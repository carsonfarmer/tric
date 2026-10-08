//! `tric route`: the router, tric's trusted core, which runs no app code. It takes the app from the `Host`, mints the
//! app's storage credentials with STS, and sends serve the request with them, as the app's tenant. It ticks cron too,
//! and relays the delivery events serve hands its outbox, which is a listener of its own.
use crate::aws::{Aws, LIFETIME};
use crate::cron::{self, Cron};
use crate::deploy::{RELEASE_MAX, Release};
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
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use wasmtime::Result;

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
    store: Store,
    aws: Aws,
    known: Mutex<HashMap<String, (bool, Instant)>>,
    creds: Mutex<HashMap<String, (HeaderValue, Instant)>>,
}

/// Routes requests for `<app>.<domain>` on `listen` to serve at `serve`, with credentials for `bucket` minted as
/// `role`, and takes delivery events on `outbox`.
pub async fn run(
    listen: SocketAddr,
    outbox: SocketAddr,
    domain: String,
    bucket: String,
    serve: String,
    role: Option<String>,
) -> Result<()> {
    let s3 = store::s3(&bucket, None)?;
    let aws = Aws::new(&s3)?;
    let (known, creds) = (Mutex::default(), Mutex::default());
    let route = Arc::new(Route { domain, bucket, serve, role, store: Store::s3(s3), aws, known, creds });
    let (clients, events) = (TcpListener::bind(listen).await?, TcpListener::bind(outbox).await?);
    let ticker = route.clone();
    tokio::spawn(cron::tick(move |t| _ = tokio::spawn(ticker.clone().cron(t))));
    eprintln!("routing at http://{}", clients.local_addr()?);
    let r = route.clone();
    let events = tric::listen(events, move |_, req| r.clone().outbox(req));
    let clients = tric::listen(clients, move |peer, req| route.clone().handle(peer, req));
    tokio::try_join!(clients, events).map(|_| ())
}

impl Route {
    /// Sends a client's request to serve, for the app its `Host` names, if that app has a release.
    async fn handle(self: Arc<Self>, peer: SocketAddr, req: hyper::Request<Incoming>) -> Response {
        let start = Instant::now();
        let (mut parts, body) = req.into_parts();
        let host = parts.headers.get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default().to_owned();
        let Some(app) = app_at(&host, &self.domain) else { return status(StatusCode::NOT_FOUND) };
        let Some(from) = forwarded(peer.ip(), &host, "http") else { return status(StatusCode::BAD_REQUEST) };
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
        let body = body.map_err(wasmtime_wasi_http::Error::from).boxed_unsync();
        let res = self.send(&app, creds, http::Request::from_parts(parts, body)).await;
        tracing::info!(app, status = res.status().as_u16(), ms = start.elapsed().as_millis() as u64, "route");
        res
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
        let creds = self.aws.assume(self.role.as_deref(), &format!("app-{app}"), &policy(&self.bucket, app)).await?;
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
        match outbound::internal(&self.serve, req).await {
            Ok((res, _)) => res,
            Err(e) => {
                tracing::warn!(app, "serve: {e:?}");
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

    /// Takes a delivery event that serve hands over, and relays it to the app it names, as Lambda does an event: at
    /// once, and twice more if that fails.
    async fn outbox(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        #[derive(Deserialize)]
        struct Of {
            app: String,
        }
        if req.method() != Method::POST {
            return status(StatusCode::METHOD_NOT_ALLOWED);
        }
        let Ok(event) = Limited::new(req.into_body(), EVENT_MAX).collect().await else {
            return status(StatusCode::PAYLOAD_TOO_LARGE);
        };
        let event = event.to_bytes();
        let Some(app) = serde_json::from_slice::<Of>(&event).ok().map(|of| of.app).filter(|app| label(app)) else {
            return status(StatusCode::BAD_REQUEST);
        };
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
