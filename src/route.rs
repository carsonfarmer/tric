//! `tric route`: the router, tric's trusted core, which runs no app code. It takes the app from the `Host`, mints the
//! app's storage credentials with STS, and sends serve the request with them, as the app's tenant. On Lambda, cron
//! jobs, delivery events, their retries and sockets' events invoke aliases, which the Lambda Web Adapter makes requests
//! of, as it does a function URL's. Locally, the router ticks cron itself, and takes the invocations on a listener of
//! its own.
use crate::aws::{Aws, LIFETIME};
use crate::cron::{self, Cron};
use crate::deploy::{RELEASE_MAX, Release, Scheduler};
use crate::lambda;
use crate::outbound;
use crate::outbox::{self, Delivered};
use crate::retry::Waiting;
use crate::store::{self, Store};
use crate::sweep;
use crate::tric::{self, CREDENTIALS, Request, Response, TENANT, app_at, forward, forwarded, label, status};
use bytes::Bytes;
use http::header::{FORWARDED, HOST};
use http::{HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use object_store::path::Path;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use wasmtime::{Result, ensure, format_err};

/// How long whether an app has a release is taken as known.
const KNOWN_FOR: Duration = Duration::from_secs(5);
/// Credentials are used until 15 minutes before they expire.
const CREDS_FOR: Duration = Duration::from_secs(LIFETIME - 15 * 60);

pub(crate) struct Route {
    pub(crate) domain: String,
    bucket: String,
    serve: String,
    role: Option<String>,
    /// The secret CloudFront sends with every request, and every socket's opening, as `X-Tric-Origin`.
    origin: Option<String>,
    /// On Lambda, where a delivery that fails waits to be tried again.
    pub(crate) retries: Option<Scheduler>,
    pub(crate) store: Store,
    pub(crate) aws: Aws,
    known: Cache<bool>,
    creds: Cache<HeaderValue>,
}

/// A value per app, and when it was begun.
type Cache<V> = Mutex<HashMap<String, (V, Instant)>>;

/// `app`'s value in `cache` if it was begun less than `ttl` ago, else the one `make` gives, which is kept.
async fn cached<V: Clone>(
    cache: &Cache<V>,
    ttl: Duration,
    app: &str,
    make: impl Future<Output = Result<V>>,
) -> Result<V> {
    if let Some((v, at)) = cache.lock().unwrap().get(app)
        && at.elapsed() < ttl
    {
        return Ok(v.clone());
    }
    let begun = Instant::now();
    let v = make.await?;
    let mut all = cache.lock().unwrap();
    all.retain(|_, (_, at)| at.elapsed() < ttl);
    all.insert(app.into(), (v.clone(), begun));
    Ok(v)
}

/// Routes requests for `<app>.<domain>` on `listen` to serve, with credentials for `bucket` minted as `role`. serve is
/// a Lambda function, by its ARN, and then `retries` is where deliveries wait, or else at `host:port`, and then the
/// router takes invocations on `outbox`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    listen: SocketAddr,
    outbox: SocketAddr,
    domain: String,
    bucket: String,
    serve: String,
    role: Option<String>,
    origin: Option<String>,
    retries: Option<Scheduler>,
) -> Result<()> {
    ensure!(
        serve.starts_with("arn:") == retries.is_some(),
        "a router has retries if, and only if, its serve is a Lambda function"
    );
    let s3 = store::s3(&bucket, None)?;
    let aws = Aws::new(&s3)?;
    let (known, creds) = (Mutex::default(), Mutex::default());
    let route =
        Arc::new(Route { domain, bucket, serve, role, origin, retries, store: Store::s3(s3), aws, known, creds });
    let clients = TcpListener::bind(listen).await?;
    // Locally, events have a listener of their own, which is up before the router says it is.
    let events = match route.serve.starts_with("arn:") {
        true => None,
        false => Some(TcpListener::bind(outbox).await?),
    };
    eprintln!("routing at http://{}", clients.local_addr()?);
    let Some(events) = events else {
        return tric::listen(clients, move |_, req| route.clone().lambda(req)).await;
    };
    let ticker = route.clone();
    tokio::spawn(cron::tick(move |t| _ = tokio::spawn(ticker.clone().cron(t))));
    let r = route.clone();
    let events = tric::listen(events, move |_, req| r.clone().lambda(req));
    let clients = tric::listen(clients, move |peer, req| {
        let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default().to_owned();
        route.clone().handle(peer.ip(), host, "http", req)
    });
    tokio::try_join!(clients, events).map(|_| ())
}

impl Route {
    /// Sends a client's request, from `ip` for `host` over `proto`, to serve, for the app `host` names, if that app has
    /// a release.
    async fn handle(self: Arc<Self>, ip: IpAddr, host: String, proto: &str, req: hyper::Request<Incoming>) -> Response {
        let start = Instant::now();
        let (mut parts, body) = req.into_parts();
        let Some(app) = app_at(&host, &self.domain) else { return status(StatusCode::NOT_FOUND) };
        let (Some(from), Ok(host)) = (forwarded(ip, &host, proto), HeaderValue::try_from(host)) else {
            return status(StatusCode::BAD_REQUEST);
        };
        let creds = match self.tenant(&app).await {
            Ok(Some(creds)) => creds,
            Ok(None) => return status(StatusCode::NOT_FOUND),
            Err(e) => {
                tracing::warn!(app, "{e:#}");
                return status(StatusCode::SERVICE_UNAVAILABLE);
            }
        };
        forward(&mut parts.headers, from);
        parts.headers.insert(HOST, host);
        let body = body.map_err(wasmtime_wasi_http::Error::from).boxed_unsync();
        let res = self.send(&app, creds, http::Request::from_parts(parts, body)).await;
        tracing::info!(app, status = res.status().as_u16(), ms = start.elapsed().as_millis() as u64, "route");
        res
    }

    /// A request the Lambda Web Adapter makes of an invocation, or one of the local listener that stands in for them.
    /// A function URL's is CloudFront's, with the origin secret, the viewer's host and the viewer's address. Any other
    /// is an event, by the alias invoked: `outbox`, which only serve may invoke; `cron` and `retry`, which only
    /// Scheduler may, each as a role of its own; and `ws`, which only API Gateway may.
    async fn lambda(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let context = |name: &str| serde_json::from_str::<Value>(header(name)).unwrap_or_default();
        let http = context("x-amzn-request-context").get("http").is_some();
        let (ok, ip) = (self.is_origin(header("x-tric-origin")), viewer(header("cloudfront-viewer-address")));
        let host = header("x-forwarded-host").to_owned();
        let arn = context("x-amzn-lambda-context")["invoked_function_arn"].as_str().unwrap_or_default().to_owned();
        match (http, arn.splitn(8, ':').nth(7), ip) {
            (true, ..) if !ok => status(StatusCode::FORBIDDEN),
            (true, _, Some(ip)) => self.handle(ip, host, "https", req).await,
            (true, _, None) => status(StatusCode::BAD_REQUEST),
            (false, Some("outbox"), _) => self.outbox(req).await,
            (false, Some("cron"), _) => self.job(req).await,
            (false, Some("retry"), _) => self.retry(req).await,
            #[cfg(feature = "ws")]
            (false, Some("ws"), _) => self.ws(req).await,
            _ => status(StatusCode::FORBIDDEN),
        }
    }

    /// Whether `secret` is the origin secret, which only CloudFront sends: compared as digests, which takes no longer
    /// for a closer guess.
    pub(crate) fn is_origin(&self, secret: &str) -> bool {
        self.origin.as_deref().is_some_and(|origin| Sha256::digest(origin) == Sha256::digest(secret))
    }

    /// `app`'s credentials, as serve takes them, if it has a release, as it did up to `KNOWN_FOR` ago: minted with a
    /// session policy that reaches only `app`'s objects, and used for `CREDS_FOR`.
    async fn tenant(&self, app: &str) -> Result<Option<HeaderValue>> {
        if !cached(&self.known, KNOWN_FOR, app, self.store.head(&store::app(app, &["current"]))).await? {
            return Ok(None);
        }
        let creds = cached(&self.creds, CREDS_FOR, app, async {
            let creds = self.aws.assume(self.role.as_deref(), app, &policy(&self.bucket, app)).await?;
            let mut creds = HeaderValue::try_from(serde_json::to_string(&creds)?)?;
            creds.set_sensitive(true);
            Ok(creds)
        });
        creds.await.map(Some)
    }

    /// Sends `req` to serve, as `app`'s tenant, with `app`'s credentials.
    async fn send(&self, app: &str, creds: HeaderValue, mut req: Request) -> Response {
        req.headers_mut().insert(CREDENTIALS, creds);
        if let Ok(app) = HeaderValue::try_from(app) {
            req.headers_mut().insert(TENANT, app);
        }
        let res = match self.serve.starts_with("arn:") {
            true => lambda::invoke(&self.aws, &self.serve, app, req).await,
            false => outbound::internal(&self.serve, req).await.map(|(res, _)| res).map_err(|e| format_err!("{e:?}")),
        };
        match res {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(app, "serve: {e:#}");
                status(StatusCode::BAD_GATEWAY)
            }
        }
    }

    /// Sends serve a `POST` of `body` to `path` of `app`, from tric itself, as `from` says; `None` if `app` has no
    /// release.
    pub async fn event(&self, app: &str, path: &str, from: &'static str, body: Bytes) -> Result<Option<Response>> {
        let Some(creds) = self.tenant(app).await? else { return Ok(None) };
        let req = http::Request::post(path).header(HOST, format!("{app}.{}", self.domain));
        let req = req.header(FORWARDED, from).body(Full::new(body).map_err(|n| match n {}).boxed_unsync())?;
        Ok(Some(self.send(app, creds, req).await))
    }

    /// Fires the cron jobs, of every app, that match the minute `t`, and sweeps the apps whose day it is.
    async fn cron(self: Arc<Self>, t: u64) {
        let apps = match self.store.list(&Path::from("apps")).await {
            Ok(list) => list.common_prefixes,
            Err(e) => return tracing::warn!("cron: {e:#}"),
        };
        for app in apps.iter().filter_map(Path::filename).filter(|app| label(app)) {
            let Ok(Some((release, _))) = self.store.json::<Release>(&store::app(app, &["current"]), RELEASE_MAX).await
            else {
                continue;
            };
            if Cron::parse(&sweep::schedule(app)).is_ok_and(|c| c.matches(t)) {
                tracing::info!(app, "cron: sweep due");
                let (route, app) = (self.clone(), app.to_owned());
                tokio::spawn(async move { route.sweep(&app).await });
            }
            let due = release.cron.into_iter().filter(|(fields, _)| Cron::parse(fields).is_ok_and(|c| c.matches(t)));
            for (_, path) in due {
                let (route, app) = (self.clone(), app.to_owned());
                tokio::spawn(async move { route.fire(&app, &path).await });
            }
        }
    }

    /// Fires the cron job, `{app, path}`, or the sweep, `{app, sweep: true}`, that Scheduler sends.
    async fn job(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        let job = match body::<Job>(req).await {
            Ok((_, job)) if job.valid() => job,
            Ok(_) => return status(StatusCode::BAD_REQUEST),
            Err(code) => return status(code),
        };
        let fired = match &job {
            Job::Cron(Fire { app, path }) => self.fire(app, path).await,
            Job::Sweep(Sweep { app, .. }) => self.sweep(app).await,
        };
        match fired {
            true => status(StatusCode::NO_CONTENT),
            false => status(StatusCode::NOT_FOUND), // an app with no release has no tenant
        }
    }

    /// Fires the cron job at `path` of `app`, and logs how it went; false if `app` has no release.
    async fn fire(&self, app: &str, path: &str) -> bool {
        match self.event(app, path, "for=_cron", Bytes::new()).await {
            Ok(Some(res)) => tracing::info!(app, path, status = res.status().as_u16(), "cron"),
            Ok(None) => return false,
            Err(e) => tracing::warn!(app, path, "cron: {e:#}"),
        }
        true
    }

    /// Has serve sweep `app` (see `sweep`), and logs how it went; false if `app` has no release. The sweep runs in
    /// serve, with `app`'s own credentials, as `app`'s tenant; serve answers when it is done, and it is not tried
    /// again if it fails: tomorrow's sweep does what this one did not.
    async fn sweep(&self, app: &str) -> bool {
        match self.event(app, "/", "for=_sweep", Bytes::new()).await {
            Ok(Some(res)) => tracing::info!(app, status = res.status().as_u16(), "sweep"),
            Ok(None) => return false,
            Err(e) => tracing::warn!(app, "sweep: {e:#}"),
        }
        true
    }

    /// Takes a delivery event that serve hands over, and delivers it to the app it names, and sends the messages it
    /// published. On Lambda, that is once; if it fails, the event waits to be tried again, as `retry.rs` says, and 503
    /// answers only a failure of the router itself. Locally, the router tries again itself, in a task.
    async fn outbox(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        let (event, e) = match body::<outbox::Event>(req).await {
            Ok((event, e)) if label(&e.app) => (event, e),
            Ok(_) => return status(StatusCode::BAD_REQUEST),
            Err(code) => return status(code),
        };
        if let Some(retries) = &self.retries {
            let w = Waiting::new(e.app.clone(), e.commit);
            if !w.valid() {
                return status(StatusCode::BAD_REQUEST);
            }
            return self.answer(&e.app, self.first(retries, w, event).await).await;
        }
        let app = e.app;
        tokio::spawn(async move {
            let _published = outbox::relay(|| {
                let (route, app, event) = (self.clone(), app.clone(), event.clone());
                async move { route.deliver(&app, event).await }
            })
            .await;
            #[cfg(feature = "ws")]
            crate::ws::Hub::Aws(&self).publish(&app, _published).await;
        });
        status(StatusCode::ACCEPTED)
    }

    /// Sends serve `app`'s delivery event: done once serve has it delivered, or dropped, and to be tried again after a
    /// 429, a 5xx or a failed exchange.
    pub(crate) async fn deliver(&self, app: &str, event: Bytes) -> Delivered {
        match self.event(app, "/", "for=_tric", event).await {
            Ok(Some(res)) if res.status() == StatusCode::TOO_MANY_REQUESTS || res.status().is_server_error() => {
                tracing::info!(app, "outbox: serve answered {}", res.status());
                Err(outbox::retry_after(res.headers()))
            }
            Ok(Some(res)) => Ok(outbox::published(res).await),
            Ok(None) => {
                tracing::info!(app, "outbox: dropped an event, as its app has no release");
                Ok(vec![])
            }
            Err(e) => {
                tracing::info!(app, "outbox: {e:#}");
                Err(None)
            }
        }
    }
}

/// What Scheduler sends the `cron` alias: an app's cron job, `{app, path}`, or its sweep, `{app, sweep: true}`. A body
/// of any other shape, one with a field of the other's included, is no job.
#[derive(Deserialize)]
#[serde(untagged)]
enum Job {
    Cron(Fire),
    Sweep(Sweep),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fire {
    app: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sweep {
    app: String,
    sweep: bool,
}

impl Job {
    /// Whether the job is for an app that can exist, and is what it says.
    fn valid(&self) -> bool {
        match self {
            Job::Cron(Fire { app, path }) => label(app) && path.starts_with('/'),
            Job::Sweep(Sweep { app, sweep }) => label(app) && *sweep,
        }
    }
}

/// The address in a `CloudFront-Viewer-Address`, `ip:port`, where an IPv6 one may be in brackets.
pub(crate) fn viewer(address: &str) -> Option<IpAddr> {
    let (ip, _) = address.rsplit_once(':')?;
    ip.trim_start_matches('[').trim_end_matches(']').parse().ok()
}

/// A `POST`'s body, of up to `outbox::EVENT_MAX`, and the `T` it is as JSON; else the status to answer with.
pub(crate) async fn body<T: DeserializeOwned>(req: hyper::Request<Incoming>) -> Result<(Bytes, T), StatusCode> {
    if req.method() != Method::POST {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    let body = Limited::new(req.into_body(), outbox::EVENT_MAX).collect().await;
    let body = body.map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?.to_bytes();
    let t = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((body, t))
}

/// The session policy of `app`'s credentials: its names, values and native code to read and write, the rest of its
/// objects to read, and to list its names and its values, and nothing else of the bucket: a list names the keys under
/// its prefix, which here are `app`'s own. The sweep lists, and that is all it lists for.
fn policy(bucket: &str, app: &str) -> String {
    let arn = |p: &str| format!("arn:aws:s3:::{bucket}/{p}");
    let rw = ["apps/{app}/names/*", "apps/{app}/values/*", "native/{app}/*"].map(|p| arn(&p.replace("{app}", app)));
    let listed = ["apps/{app}/names/*", "apps/{app}/values/*"].map(|p| p.replace("{app}", app));
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:GetObjectVersion", "s3:PutObject", "s3:DeleteObject"],
                "Resource": rw,
            },
            {
                "Effect": "Allow",
                "Action": ["s3:GetObject", "s3:GetObjectVersion"],
                "Resource": [arn(&format!("apps/{app}/*"))],
            },
            {
                "Effect": "Allow",
                "Action": ["s3:ListBucket"],
                "Resource": [format!("arn:aws:s3:::{bucket}")],
                "Condition": { "StringLike": { "s3:prefix": listed } },
            },
        ],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(json: &str) -> Option<Job> {
        serde_json::from_str(json).ok().filter(Job::valid)
    }

    #[test]
    fn a_job_is_a_cron_job_or_a_sweep_and_nothing_else() {
        assert!(matches!(job(r#"{"app":"shop","path":"/run"}"#), Some(Job::Cron(_))));
        assert!(matches!(job(r#"{"app":"shop","sweep":true}"#), Some(Job::Sweep(_))));
        // A sweep that is not one, a shape of both, a shape of neither, and an app that cannot be.
        assert!(job(r#"{"app":"shop","sweep":false}"#).is_none());
        assert!(job(r#"{"app":"shop","path":"/run","sweep":true}"#).is_none());
        assert!(job(r#"{"app":"shop","path":"/run","sweep":false}"#).is_none());
        assert!(job(r#"{"app":"shop"}"#).is_none());
        assert!(job(r#"{"app":"shop","sweep":"true"}"#).is_none());
        assert!(job(r#"{"app":"shop","sweep":true,"more":1}"#).is_none());
        assert!(job(r#"{"app":"shop","path":"run"}"#).is_none());
        assert!(job(r#"{"app":"Shop","sweep":true}"#).is_none());
        assert!(job(r#"{"app":"a/b","sweep":true}"#).is_none());
        assert!(job(r#"{"app":"*","sweep":true}"#).is_none());
        assert!(job(r#"{"sweep":true}"#).is_none());
    }

    #[test]
    fn credentials_list_the_apps_own_names_and_values_and_nothing_else() {
        let policy: Value = serde_json::from_str(&policy("b", "shop")).unwrap();
        let statements = policy["Statement"].as_array().unwrap();
        let lists = |s: &&Value| s["Action"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().contains("List"));
        let listing: Vec<_> = statements.iter().filter(lists).collect();
        // Exactly one statement lists: on the bucket, for those two prefixes only.
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0]["Effect"], "Allow");
        assert_eq!(listing[0]["Action"], serde_json::json!(["s3:ListBucket"]));
        assert_eq!(listing[0]["Resource"], serde_json::json!(["arn:aws:s3:::b"]));
        assert_eq!(
            listing[0]["Condition"],
            serde_json::json!({ "StringLike": { "s3:prefix": ["apps/shop/names/*", "apps/shop/values/*"] } })
        );
        // The statements that read and write are as they were: objects of the app, and no list.
        for s in statements.iter().filter(|s| !lists(s)) {
            assert!(s["Condition"].is_null());
            let resources = s["Resource"].as_array().unwrap();
            assert!(resources.iter().all(|r| r.as_str().unwrap().starts_with("arn:aws:s3:::b/")));
        }
    }
}
