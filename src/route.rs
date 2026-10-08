//! `tric route`: the router, tric's trusted core, which runs no app code. It takes the app from the `Host`, mints the
//! app's storage credentials with STS, and sends serve the request with them, as the app's tenant. Locally, it ticks
//! cron too, and relays the delivery events serve hands its outbox, which is a listener of its own. On Lambda, cron and
//! the outbox are events, which the Lambda Web Adapter makes requests of, as it does a function URL's.
use crate::aws::{Aws, LIFETIME};
use crate::cron::{self, Cron};
use crate::deploy::{RELEASE_MAX, Release};
use crate::lambda;
use crate::outbound;
use crate::outbox::{self, Delivered};
use crate::store::{self, Store};
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
use wasmtime::{Result, format_err};

/// How long whether an app has a release is taken as known.
const KNOWN_FOR: Duration = Duration::from_secs(5);
/// Credentials are used until 15 minutes before they expire.
const CREDS_FOR: Duration = Duration::from_secs(LIFETIME - 15 * 60);
const EVENT_MAX: usize = 2 << 20;

struct Route {
    domain: String,
    bucket: String,
    serve: String,
    role: Option<String>,
    /// On Lambda, the secret CloudFront sends with every request, as `X-Tric-Origin`.
    origin: Option<String>,
    store: Store,
    aws: Aws,
    known: Mutex<HashMap<String, (bool, Instant)>>,
    creds: Mutex<HashMap<String, (HeaderValue, Instant)>>,
}

/// Routes requests for `<app>.<domain>` on `listen` to serve, with credentials for `bucket` minted as `role`. serve is
/// a Lambda function, by its ARN, or else at `host:port`, and then the router takes delivery events on `outbox`.
pub async fn run(
    listen: SocketAddr,
    outbox: SocketAddr,
    domain: String,
    bucket: String,
    serve: String,
    role: Option<String>,
    origin: Option<String>,
) -> Result<()> {
    let s3 = store::s3(&bucket, None)?;
    let aws = Aws::new(&s3)?;
    let (known, creds) = (Mutex::default(), Mutex::default());
    let route = Arc::new(Route { domain, bucket, serve, role, origin, store: Store::s3(s3), aws, known, creds });
    let clients = TcpListener::bind(listen).await?;
    eprintln!("routing at http://{}", clients.local_addr()?);
    if route.serve.starts_with("arn:") {
        return tric::listen(clients, move |_, req| route.clone().lambda(req)).await;
    }
    let ticker = route.clone();
    tokio::spawn(cron::tick(move |t| _ = tokio::spawn(ticker.clone().cron(t))));
    let r = route.clone();
    let events = tric::listen(TcpListener::bind(outbox).await?, move |_, req| r.clone().outbox(req, false));
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
        let creds = match self.known(&app).await {
            Ok(true) => self.creds(&app).await,
            Ok(false) => return status(StatusCode::NOT_FOUND),
            Err(e) => Err(e),
        };
        let creds = match creds {
            Ok(creds) => creds,
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

    /// On Lambda, a request the Lambda Web Adapter makes of an invocation. A router with the origin secret is the
    /// function whose URL CloudFront calls, with that secret, the viewer's host and the viewer's address, and takes
    /// nothing else. One without takes only events: a delivery event if it came to the `outbox` alias, which only serve
    /// may invoke, and a cron job if not.
    async fn lambda(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let context = |name: &str| serde_json::from_str::<Value>(header(name)).unwrap_or_default();
        let http = context("x-amzn-request-context").get("http").is_some();
        let Some(origin) = &self.origin else {
            let outbox =
                context("x-amzn-lambda-context")["invoked_function_arn"].as_str().map(|a| a.ends_with(":outbox"));
            return match (http, outbox) {
                (false, Some(true)) => self.outbox(req, true).await,
                (false, Some(false)) => self.job(req).await,
                _ => status(StatusCode::FORBIDDEN),
            };
        };
        let hash = |s: &str| Sha256::digest(s); // compared as digests, which takes no longer for a closer guess
        if !http || hash(origin) != hash(header("x-tric-origin")) {
            return status(StatusCode::FORBIDDEN);
        }
        let viewer = header("cloudfront-viewer-address").rsplit_once(':');
        let viewer = viewer.and_then(|(ip, _)| ip.trim_start_matches('[').trim_end_matches(']').parse().ok());
        let Some(ip) = viewer else { return status(StatusCode::BAD_REQUEST) };
        let host = header("x-forwarded-host").to_owned();
        self.handle(ip, host, "https", req).await
    }

    /// Whether `app` has a release, as it did up to `KNOWN_FOR` ago.
    async fn known(&self, app: &str) -> Result<bool> {
        if let Some((known, at)) = self.known.lock().unwrap().get(app)
            && at.elapsed() < KNOWN_FOR
        {
            return Ok(*known);
        }
        let known = self.store.head(&store::app(app, &["current"])).await?;
        let mut all = self.known.lock().unwrap();
        all.retain(|_, (_, at)| at.elapsed() < KNOWN_FOR);
        all.insert(app.into(), (known, Instant::now()));
        Ok(known)
    }

    /// `app`'s credentials, as serve takes them: minted with a session policy that reaches only `app`'s objects, and
    /// used for `CREDS_FOR`.
    async fn creds(&self, app: &str) -> Result<HeaderValue> {
        if let Some((creds, at)) = self.creds.lock().unwrap().get(app)
            && at.elapsed() < CREDS_FOR
        {
            return Ok(creds.clone());
        }
        let minted = Instant::now();
        let creds = self.aws.assume(self.role.as_deref(), app, &policy(&self.bucket, app)).await?;
        let mut creds = HeaderValue::try_from(serde_json::to_string(&creds)?)?;
        creds.set_sensitive(true);
        let mut all = self.creds.lock().unwrap();
        all.retain(|_, (_, at)| at.elapsed() < CREDS_FOR);
        all.insert(app.into(), (creds.clone(), minted));
        Ok(creds)
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

    /// Sends serve a `POST` of `body` to `path` of `app`, from tric itself, as `from` says.
    async fn event(&self, app: &str, path: &str, from: &'static str, body: Bytes) -> Result<Response> {
        let creds = self.creds(app).await?;
        let req = http::Request::post(path).header(HOST, format!("{app}.{}", self.domain));
        let req = req.header(FORWARDED, from).body(Full::new(body).map_err(|n| match n {}).boxed_unsync())?;
        Ok(self.send(app, creds, req).await)
    }

    /// Fires the cron jobs, of every app, that match the minute `t`.
    async fn cron(self: Arc<Self>, t: u64) {
        let apps = match self.store.dirs(&Path::from("apps")).await {
            Ok(apps) => apps,
            Err(e) => return tracing::warn!("cron: {e:#}"),
        };
        for app in apps.into_iter().filter(|app| label(app)) {
            let Ok(Some((release, _))) = self.store.json::<Release>(&store::app(&app, &["current"]), RELEASE_MAX).await
            else {
                continue;
            };
            let due = release.cron.into_iter().filter(|(fields, _)| Cron::parse(fields).is_ok_and(|c| c.matches(t)));
            for (_, path) in due {
                let (route, app) = (self.clone(), app.clone());
                tokio::spawn(async move {
                    match route.event(&app, &path, "for=_cron", Bytes::new()).await {
                        Ok(res) => tracing::info!(app, path, status = res.status().as_u16(), "cron"),
                        Err(e) => tracing::warn!(app, path, "cron: {e:#}"),
                    }
                });
            }
        }
    }

    /// Fires the cron job, `{app, path}`, that Scheduler sends.
    async fn job(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        #[derive(Deserialize)]
        struct Job {
            app: String,
            path: String,
        }
        let job = match body::<Job>(req).await {
            Ok((_, job)) if label(&job.app) && job.path.starts_with('/') => job,
            Ok(_) => return status(StatusCode::BAD_REQUEST),
            Err(code) => return status(code),
        };
        match self.event(&job.app, &job.path, "for=_cron", Bytes::new()).await {
            Ok(res) => tracing::info!(app = job.app, path = job.path, status = res.status().as_u16(), "cron"),
            Err(e) => tracing::warn!(app = job.app, path = job.path, "cron: {e:#}"),
        }
        status(StatusCode::NO_CONTENT)
    }

    /// Takes a delivery event that serve hands over, and relays it to the app it names, as Lambda does an event: at
    /// once, and twice more if that fails. On Lambda, Lambda does: this delivers it once, and answers 503 if that
    /// failed, which fails the invocation.
    async fn outbox(self: Arc<Self>, req: hyper::Request<Incoming>, lambda: bool) -> Response {
        #[derive(Deserialize)]
        struct Of {
            app: String,
        }
        let (event, app) = match body::<Of>(req).await {
            Ok((event, Of { app })) if label(&app) => (event, app),
            Ok(_) => return status(StatusCode::BAD_REQUEST),
            Err(code) => return status(code),
        };
        if lambda {
            return match self.deliver(&app, event).await {
                Delivered::Done => status(StatusCode::NO_CONTENT),
                Delivered::Retry(_) => status(StatusCode::SERVICE_UNAVAILABLE),
            };
        }
        tokio::spawn(outbox::relay(move || {
            let (route, app, event) = (self.clone(), app.clone(), event.clone());
            async move { route.deliver(&app, event).await }
        }));
        status(StatusCode::ACCEPTED)
    }

    /// Sends serve `app`'s delivery event: done once serve has it delivered, or dropped, and to be tried again after a
    /// 429, a 5xx or a failed exchange.
    async fn deliver(&self, app: &str, event: Bytes) -> Delivered {
        let res = match self.known(app).await {
            Ok(true) => self.event(app, "/", "for=_tric", event).await,
            Ok(false) => {
                tracing::info!(app, "outbox: dropped an event, as its app has no release");
                return Delivered::Done;
            }
            Err(e) => Err(e),
        };
        match res {
            Ok(res) if res.status() == StatusCode::TOO_MANY_REQUESTS || res.status().is_server_error() => {
                tracing::info!(app, "outbox: serve answered {}", res.status());
                Delivered::Retry(outbox::retry_after(res.headers()))
            }
            Ok(_) => Delivered::Done,
            Err(e) => {
                tracing::info!(app, "outbox: {e:#}");
                Delivered::Retry(None)
            }
        }
    }
}

/// A `POST`'s body, of up to `EVENT_MAX`, and the `T` it is as JSON; else the status to answer with.
async fn body<T: DeserializeOwned>(req: hyper::Request<Incoming>) -> Result<(Bytes, T), StatusCode> {
    if req.method() != Method::POST {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Ok(body) = Limited::new(req.into_body(), EVENT_MAX).collect().await else {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let body = body.to_bytes();
    let t = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((body, t))
}

/// The session policy of `app`'s credentials: its names, values and native code to read and write, and the rest of its
/// objects to read.
fn policy(bucket: &str, app: &str) -> String {
    let arn = |p: &str| format!("arn:aws:s3:::{bucket}/{p}");
    let rw = ["apps/{app}/names/*", "apps/{app}/values/*", "native/{app}/*"].map(|p| arn(&p.replace("{app}", app)));
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
        ],
    })
    .to_string()
}
