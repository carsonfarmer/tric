//! Background requests: those a turn sends with `Prefer: respond-async` (RFC 7240) are held, and go out once, and only
//! if, the turn commits, in order, each with an `Idempotency-Key`. A commit hands them to a sink as one delivery event
//! before it writes the head, and keeps the event's SHA-256 in the head's `pending`. Delivery waits for the head to move
//! past the version the commit wrote over, and goes ahead only if the commit is pending there with that digest: an event
//! of a turn that did not commit is dropped, as is one forged by someone who learnt a commit id.
use crate::name::{self, Head};
use crate::outbound;
use crate::store;
use crate::tric::{Request, Response, Tric, status};
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http::header::RETRY_AFTER;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};
use wasmtime::Result;
use wasmtime_wasi_http::Error;

const PREFER: HeaderName = HeaderName::from_static("prefer");
/// How long delivery waits for the commit to land: a commit is under way when it hands over its event.
const LANDING: Duration = Duration::from_secs(30);
/// One request's whole exchange, its response's body included.
const EXCHANGE: Duration = Duration::from_secs(60);
const BODY_MAX: usize = 1 << 20;
/// The most a delivery event, as JSON, may be.
pub const EVENT_MAX: usize = 2 << 20;
/// How long a delivery is tried again, from its first failure.
pub const WINDOW: Duration = Duration::from_secs(24 * 3600);
/// The wait after a delivery's first failure, which doubles with each, up to `WAIT_MAX`. EventBridge Scheduler keeps
/// time to the minute, so a shorter one would be no shorter.
const FIRST: Duration = Duration::from_secs(60);
const WAIT_MAX: Duration = Duration::from_secs(3600);

/// Where a commit hands its delivery event, the event's JSON, before it writes the head.
pub type Sink = Arc<dyn Fn(Bytes) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// The requests one turn holds, and where they came from.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub app: String,
    /// The host the turn was asked at, so a request to it runs in-process.
    pub host: String,
    pub name: String,
    /// The head's version, as the store's `ETag`, that the commit writes over; none if there was no head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub commit: String,
    pub requests: Vec<Held>,
}

/// A request, as held.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Held {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: String, // base64
}

/// Whether `headers` ask for `respond-async`.
pub fn respond_async(headers: &HeaderMap) -> bool {
    let token = |p: &str| p.split([';', '=']).next().unwrap_or_default().trim().eq_ignore_ascii_case("respond-async");
    headers.get_all(PREFER).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(token)
}

/// The response that stands in for one the app gets when its request is held.
pub fn accepted() -> Response {
    let mut res = status(StatusCode::ACCEPTED);
    res.headers_mut().insert("preference-applied", HeaderValue::from_static("respond-async"));
    res
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
            uri: parts.uri.to_string(),
            headers,
            body: B64.encode(body.to_bytes()),
        })
    }

    /// The request, as the `n`th of the commit `commit`.
    fn request(&self, commit: &str, n: usize) -> Result<Request> {
        let mut req = http::Request::builder().method(Method::from_bytes(self.method.as_bytes())?).uri(&self.uri);
        for (k, v) in &self.headers {
            req = req.header(HeaderName::try_from(k)?, HeaderValue::try_from(v)?);
        }
        let body = Full::new(Bytes::from(B64.decode(&self.body)?)).map_err(|n| match n {}).boxed_unsync();
        Ok(req.header("idempotency-key", format!("{commit}/{n}")).body(body)?)
    }

    /// If this is a publish, a `POST` to `/publish/` at `host`, the app's own: the messages it publishes, which are
    /// none if it is not well formed.
    #[cfg(feature = "ws")]
    fn publish(&self, host: &str) -> Option<Vec<(String, String)>> {
        let uri = self.uri.parse::<http::Uri>().ok().filter(|u| self.method == "POST" && u.path() == "/publish/")?;
        uri.authority().filter(|a| a.as_str().eq_ignore_ascii_case(host))?;
        let messages = B64.decode(&self.body).ok().and_then(|body| crate::ws::messages(&body));
        messages.or_else(|| (tracing::warn!("outbox: dropped a publish, as it is not well formed"), Some(vec![])).1)
    }
}

/// How a delivery went: `Ok` if every request was delivered, or the event was dropped, with the messages its publishes,
/// which delivery takes instead of sending, publish to channels of the app's sockets; `Err` if a request failed, so the
/// event is to be tried again, after the `Retry-After` given.
pub type Delivered = Result<Vec<(String, String)>, Option<Duration>>;

/// Delivers the event `bytes` of `tric`'s app, if its commit landed with its digest: each request in order, until one
/// fails, which a response of 5xx or 429 or a failed exchange is. A request to a host the app may not reach is done.
/// With the feature `ws`, a publish is taken, not sent, so it reaches sockets once, if at all, after all else.
pub async fn deliver(tric: &Arc<Tric>, bytes: &[u8]) -> Delivered {
    let e = match serde_json::from_slice::<Event>(bytes) {
        Ok(e) if e.app == tric.app => e,
        _ => return dropped("not an event of this app"),
    };
    let path = name::path(&e.app, &e.name);
    let (start, mut wait) = (Instant::now(), Duration::from_millis(50));
    let head: Option<Head> = loop {
        match name::read(&tric.store, &path).await {
            Ok(read) if read.as_ref().and_then(|(_, v)| v.e_tag.as_ref()) != e.base.as_ref() => {
                break read.map(|(head, _)| head);
            }
            Ok(_) if start.elapsed() > LANDING => return dropped("its commit never landed"),
            Ok(_) => {}
            Err(err) => {
                tracing::info!(app = e.app, commit = e.commit, "outbox: {err:#}");
                return Err(None);
            }
        }
        sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(1));
    };
    let digest = store::hash(bytes);
    if !head.is_some_and(|h| h.pending.get(&e.commit).is_some_and(|p| p.digest == digest)) {
        return dropped("its commit is not pending");
    }
    #[cfg_attr(not(feature = "ws"), allow(unused_mut))]
    let mut published = vec![];
    for n in 0..e.requests.len() {
        #[cfg(feature = "ws")]
        if let Some(messages) = e.requests[n].publish(&e.host) {
            published.extend(messages);
            continue;
        }
        send(tric, &e, n).await?;
    }
    name::settle(&tric.store, &e.app, &e.name, &e.commit).await;
    Ok(published)
}

fn dropped(why: &str) -> Delivered {
    tracing::info!("outbox: dropped an event, as {why}");
    Ok(vec![])
}

/// Sends `e`'s `n`th request: `Ok` if it is done, or else the `Retry-After` its response gave.
async fn send(tric: &Arc<Tric>, e: &Event, n: usize) -> Result<(), Option<Duration>> {
    let failed = |why: String| -> Option<Duration> {
        tracing::info!(app = e.app, commit = e.commit, n, "outbox: {why}");
        None
    };
    let exchange = async {
        let req = e.requests[n].request(&e.commit, n).map_err(|err| failed(format!("{err:#}")))?;
        let res = match req.uri().authority().is_some_and(|a| a.as_str().eq_ignore_ascii_case(&e.host)) {
            true => tric.fetch(req, "for=_tric").await,
            false => match outbound::allowed(&tric.allow, req.uri()) {
                Ok(()) => outbound::send(req).await.map_err(|err| failed(format!("{err:?}")))?.0,
                Err(err) => {
                    tracing::warn!(app = e.app, commit = e.commit, n, "outbox: not sent, as {err:?}");
                    return Ok(());
                }
            },
        };
        let (status, after) = (res.status(), retry_after(res.headers()));
        _ = res.into_body().collect().await;
        if status == StatusCode::TOO_MANY_REQUESTS || !(200..500).contains(&status.as_u16()) {
            failed(format!("answered {status}"));
            return Err(after);
        }
        Ok::<_, Option<Duration>>(())
    };
    timeout(EXCHANGE, exchange).await.unwrap_or(Err(None))
}

/// A `Retry-After` of delay-seconds.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok().map(Duration::from_secs)
}

/// How long to wait after a delivery's `tries`th failed try, the first being 1: `FIRST`, doubled for each try before,
/// up to `WAIT_MAX`, or, if it is longer, what `Retry-After` asked, up to `WINDOW`; and up to a quarter more, so that
/// the events that failed together do not come back together.
pub fn backoff(tries: u32, after: Option<Duration>) -> Duration {
    let wait = FIRST.saturating_mul(2u32.saturating_pow(tries.saturating_sub(1))).min(WAIT_MAX);
    wait.max(after.unwrap_or_default().min(WINDOW)).mul_f64(rand::random_range(1.0..=1.25))
}

/// Tries a delivery `attempt` until it is done or `WINDOW` is over, waiting as `backoff` says between tries; then logs
/// the event as lost. Gives the messages it published. On Lambda, Scheduler does this: see `retry.rs`.
pub async fn relay<F: Future<Output = Delivered>>(mut attempt: impl FnMut() -> F) -> Vec<(String, String)> {
    let deadline = Instant::now() + WINDOW;
    for tries in 1.. {
        let after = match attempt().await {
            Ok(published) => return published,
            Err(after) => after,
        };
        let wait = backoff(tries, after);
        if Instant::now() + wait > deadline {
            break;
        }
        sleep(wait).await;
    }
    tracing::warn!("outbox: an event failed every try for {WINDOW:?}, so it is lost");
    vec![]
}

/// The messages that serve's answer `res` to a delivery event says were published: no more than the event held, so
/// `EVENT_MAX` at most.
pub async fn published(res: Response) -> Vec<(String, String)> {
    let body = Limited::new(res.into_body(), EVENT_MAX).collect().await;
    body.ok().and_then(|body| serde_json::from_slice(&body.to_bytes()).ok()).unwrap_or_default()
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

    #[test]
    fn backoff_doubles_to_a_cap_and_keeps_to_retry_after() {
        let within = |wait: Duration, base: u64| (base as f64..=base as f64 * 1.25).contains(&wait.as_secs_f64());
        for (tries, base) in [(1, 60), (2, 120), (3, 240), (6, 1920), (7, 3600), (30, 3600), (u32::MAX, 3600)] {
            assert!(within(backoff(tries, None), base), "{tries}");
        }
        let hours = |h: u64| Some(Duration::from_secs(h * 3600));
        assert!(within(backoff(1, hours(2)), 7200));
        assert!(within(backoff(9, hours(0)), 3600));
        assert!(within(backoff(1, Some(Duration::MAX)), 24 * 3600)); // no more than the window
    }
}
