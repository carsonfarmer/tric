//! The outbox: requests a turn sends with `Prefer: respond-async` (RFC 7240) are held, and go out once, and only if, the
//! turn commits, in order, each with an `Idempotency-Key`. A commit enqueues them as an event before it writes the head,
//! with the head's version it writes over; delivery waits for the head to move past that version, and goes ahead only if
//! the commit is among the head's pending ones, so an event of a turn that did not commit is dropped.
use crate::name;
use crate::outbound;
use crate::serve::Tric;
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use object_store::ObjectStoreExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};
use wasmtime::{Result, bail};
use wasmtime_wasi_http::{Error, handler::Request};

const PREFER: HeaderName = HeaderName::from_static("prefer");

/// How long delivery waits for the commit to land, at most: it is under way when it enqueues.
const LANDING: Duration = Duration::from_secs(300);
/// Waits before each round of tries: a request that fails is tried again in the next, from where the last stopped.
const ROUNDS: [u64; 3] = [0, 60, 120];
/// One request's whole exchange, its response's body included.
const EXCHANGE: Duration = Duration::from_secs(60);
/// How long delivery tries to take its commit off the name's pending ones.
const SETTLE: Duration = Duration::from_secs(30);
pub const BODY_MAX: usize = 1 << 20;

/// The requests one turn holds, and where they came from.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outbox {
    pub app: String,
    pub name: String,
    pub commit: String,
    /// The head's `ETag`, as the store gives it, that the commit writes over; none if there was no head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The host the turn was asked at, so a request to it runs in-process.
    pub host: String,
    pub requests: Vec<Held>,
}

/// A request, as held.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Held {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String, // base64
}

/// Whether `headers` ask for `respond-async`.
pub fn respond_async(headers: &HeaderMap) -> bool {
    let token = |p: &str| p.split([';', '=']).next().unwrap_or_default().trim().eq_ignore_ascii_case("respond-async");
    headers.get_all(PREFER).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(token)
}

impl Held {
    /// `req`, less its `Prefer`, with its body, which must be `BODY_MAX` bytes or less, and its headers, which must be
    /// text, as they are kept as JSON.
    pub async fn of(req: Request) -> Result<Self, Error> {
        let (parts, body) = req.into_parts();
        let mut headers = vec![];
        for (k, v) in parts.headers.iter().filter(|(k, _)| **k != PREFER) {
            let v =
                v.to_str().map_err(|_| Error::InternalError(Some(format!("{k} is not text, so cannot be held"))))?;
            headers.push((k.to_string(), v.to_owned()));
        }
        let body = Limited::new(body, BODY_MAX).collect().await.map_err(|_| Error::HttpRequestBodySize(None))?;
        Ok(Self {
            method: parts.method.to_string(),
            url: parts.uri.to_string(),
            headers,
            body: B64.encode(body.to_bytes()),
        })
    }

    /// The request, as the `n`th of the commit `commit`.
    fn request(&self, commit: &str, n: usize) -> Result<Request> {
        let mut req = http::Request::builder().method(Method::from_bytes(self.method.as_bytes())?).uri(&self.url);
        for (k, v) in &self.headers {
            req = req.header(HeaderName::try_from(k)?, HeaderValue::try_from(v)?);
        }
        let body = Full::new(Bytes::from(B64.decode(&self.body)?)).map_err(|n| match n {}).boxed_unsync();
        Ok(req.header("idempotency-key", format!("{commit}/{n}")).body(body)?)
    }
}

/// The response that stands in for one the app gets when its request is held.
pub fn accepted() -> http::Response<wasmtime_wasi_http::WasiBody> {
    let mut res = crate::serve::status(StatusCode::ACCEPTED);
    res.headers_mut().insert("preference-applied", HeaderValue::from_static("respond-async"));
    res
}

/// Delivers `o`, if its commit landed: each request until it is done, which a response of 2xx to 4xx but 429 is, in
/// `ROUNDS`; then records it in `failed/<commit>` if any is not, and takes the commit off the name's pending ones.
pub async fn deliver(tric: &Arc<Tric>, o: Outbox) -> Result<()> {
    let path = name::path(&o.app, &o.name);
    let (start, mut wait) = (Instant::now(), Duration::from_millis(50));
    let head = loop {
        let read = name::read(&*tric.store, &path).await?;
        if read.as_ref().and_then(|(_, v)| v.e_tag.as_ref()) != o.after.as_ref() {
            break read.map(|(head, _)| head);
        }
        if start.elapsed() > LANDING {
            bail!("{}/{}: commit {} never landed", o.app, o.name, o.commit);
        }
        sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(2));
    };
    if !head.is_some_and(|h| h.pending.contains_key(&o.commit)) {
        return Ok(()); // the turn did not commit
    }
    let mut next = 0;
    for round in ROUNDS {
        sleep(Duration::from_secs(round)).await;
        while next < o.requests.len() && send(tric, &o, next).await {
            next += 1;
        }
        if next == o.requests.len() {
            break;
        }
    }
    if next < o.requests.len() {
        tracing::warn!(app = o.app, name = o.name, commit = o.commit, "outbox: request {next} failed every try");
        tric.store.put(&format!("failed/{}", o.commit).into(), serde_json::to_vec(&o)?.into()).await?;
    }
    name::settle(&*tric.store, &path, &o.commit, Instant::now() + SETTLE).await
}

/// Sends `o`'s `n`th request, and returns whether it is done.
async fn send(tric: &Arc<Tric>, o: &Outbox, n: usize) -> bool {
    let exchange = async {
        let req = o.requests[n].request(&o.commit, n).map_err(|e| format!("{e:#}"))?;
        let res = match tric.is_self(&o.app, &o.host, req.uri()) {
            true => tric.fetch(&o.app, &o.host, req, HeaderValue::from_static("for=_tric")).await,
            false => outbound::send(req).await.map(|(res, _)| res).map_err(|e| format!("{e:?}"))?,
        };
        let status = res.status();
        _ = res.into_body().collect().await;
        Ok::<_, String>(status)
    };
    match timeout(EXCHANGE, exchange).await {
        Ok(Ok(s)) if s != StatusCode::TOO_MANY_REQUESTS && (200..500).contains(&s.as_u16()) => true,
        Ok(Ok(s)) => {
            tracing::info!(app = o.app, commit = o.commit, n, "outbox: answered {s}");
            false
        }
        Ok(Err(e)) => {
            tracing::info!(app = o.app, commit = o.commit, n, "outbox: {e}");
            false
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefer() {
        let h = |v: &[&str]| {
            let mut m = HeaderMap::new();
            v.iter().for_each(|v| _ = m.append(PREFER, v.parse().unwrap()));
            respond_async(&m)
        };
        for yes in [
            &["respond-async"][..],
            &["Respond-Async"],
            &["wait=10, respond-async"],
            &["handling=lenient", "respond-async; x=1"],
        ] {
            assert!(h(yes), "{yes:?}");
        }
        for no in [&[][..], &["wait=10"], &["respond-asyncx"], &["x=respond-async"]] {
            assert!(!h(no), "{no:?}");
        }
    }
}
