//! `tric serve`: the runtime. Each request comes from the router, for the app its `Host` names, as that app's tenant and
//! with that app's storage credentials, which are the only ones serve has: its own role has no storage access.
use crate::aws::{Aws, Credentials};
use crate::deploy::{RELEASE_MAX, Release};
use crate::engine::Engine;
use crate::lambda;
use crate::outbound::{self, Allow};
use crate::outbox::{self, Sink};
use crate::store::{self, Store};
use crate::sweep;
use crate::tric::{self, CREDENTIALS, Response, TENANT, Tric, app_at, forward, status};
use bytes::Bytes;
use futures_util::FutureExt;
use http::header::{CONTENT_TYPE, FORWARDED, HOST, RETRY_AFTER};
use http::{HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use object_store::{PutMode, path::Path};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};
use wasmtime::component::Component;
use wasmtime::{Result, ensure, error::Context, format_err};
use wasmtime_wasi_http::WasiBody;

/// How long a release read is taken to be the app's current one.
const FRESH: Duration = Duration::from_secs(5);
const COMPONENT_MAX: u64 = 64 << 20;
const NATIVE_MAX: u64 = 256 << 20;
/// How long the router's outbox has to take an event.
const HAND: Duration = Duration::from_secs(10);
/// The context of an invocation of the `outbox` alias, which the router's local events listener stands in for.
const OUTBOX: &str = r#"{"invoked_function_arn":"arn:aws:lambda:local:0:function:events:outbox"}"#;

struct Serve {
    engine: Engine,
    domain: String,
    bucket: String,
    sink: Sink,
    /// A lock per app, so that one app's loading holds up no other's.
    apps: std::sync::Mutex<HashMap<String, Arc<Mutex<Option<Loaded>>>>>,
}

/// An app as loaded: with the credentials it was given and the release it read, and when it read it.
struct Loaded {
    creds: Credentials,
    release: Bytes,
    tric: Arc<Tric>,
    read: Instant,
}

/// Serves the apps at `<app>.<domain>` on `listen`, from `bucket`, handing delivery events to the router's outbox at
/// `outbox`: the `outbox` alias of the router's function, by its ARN, or `host:port`.
pub async fn run(listen: SocketAddr, domain: String, bucket: String, outbox: String) -> Result<()> {
    let sink: Sink = match outbox.starts_with("arn:") {
        true => {
            let aws = Arc::new(Aws::new(&store::s3(&bucket, None)?)?);
            Arc::new(move |event| {
                let (aws, outbox) = (aws.clone(), outbox.clone());
                async move { timeout(HAND, lambda::hand(&aws, &outbox, event)).await? }.boxed()
            })
        }
        false => Arc::new(move |event| hand(outbox.clone(), event).boxed()),
    };
    let serve = Arc::new(Serve { engine: Engine::new()?, domain, bucket, sink, apps: Default::default() });
    let listener = TcpListener::bind(listen).await?;
    eprintln!("serving at http://{}", listener.local_addr()?);
    tric::listen(listener, move |_, req| serve.clone().handle(req)).await
}

impl Serve {
    /// Runs a request the router sent: one for the app it is the tenant of, with that app's credentials, which the app
    /// never sees, and the router's `Forwarded`.
    async fn handle(self: Arc<Self>, req: hyper::Request<Incoming>) -> Response {
        let (mut parts, body) = req.into_parts();
        let host = parts.headers.get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        let Some(app) = app_at(&host, &self.domain) else { return status(StatusCode::NOT_FOUND) };
        let creds = parts.headers.remove(CREDENTIALS).and_then(|c| serde_json::from_slice(c.as_bytes()).ok());
        let (Some(creds), true) = (creds, parts.headers.get(TENANT).is_some_and(|t| t == app.as_str())) else {
            return status(StatusCode::FORBIDDEN);
        };
        // A sweep needs the store and none of the app's code, so it reads no release and runs no guest.
        if parts.method == Method::POST && parts.headers.get(FORWARDED).is_some_and(|f| f == "for=_sweep") {
            return self.sweep(&app, creds).await;
        }
        let tric = match self.load(&app, creds).await {
            Ok(Some(tric)) => tric,
            Ok(None) => return status(StatusCode::NOT_FOUND),
            Err(e) => {
                tracing::warn!(app, "{e:#}");
                return status(StatusCode::SERVICE_UNAVAILABLE);
            }
        };
        let Some(from) = parts.headers.get(FORWARDED).cloned() else { return status(StatusCode::BAD_REQUEST) };
        let body = body.map_err(wasmtime_wasi_http::Error::from).boxed_unsync();
        if parts.method == Method::POST && from == "for=_tric" {
            return deliver(&tric, body).await;
        }
        #[cfg(feature = "ws")]
        if parts.method == Method::POST && from == "for=_ws" {
            return crate::ws::serve(&tric, body).await;
        }
        let https = from.to_str().is_ok_and(|f| f.split(';').any(|p| p.trim().eq_ignore_ascii_case("proto=https")));
        let pq = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
        let Ok(uri) = format!("{}://{host}{pq}", if https { "https" } else { "http" }).parse() else {
            return status(StatusCode::BAD_REQUEST);
        };
        parts.uri = uri;
        forward(&mut parts.headers, from);
        tric.run(http::Request::from_parts(parts, body), &host).await
    }

    /// Sweeps `app`'s names for the objects that nothing names (see `sweep`), as far as `sweep::RUN` takes: 204 when it
    /// went well, whether or not it went through every name; else 503, which the router does not retry: a sweep that
    /// fails is tomorrow's.
    async fn sweep(&self, app: &str, creds: Credentials) -> Response {
        let store = match store::s3(&self.bucket, Some(creds.into())) {
            Ok(s3) => Store::s3(s3),
            Err(e) => {
                tracing::warn!(app, "sweep: {e:#}");
                return status(StatusCode::SERVICE_UNAVAILABLE);
            }
        };
        match sweep::run(&store, app, SystemTime::now(), tokio::time::Instant::now() + sweep::RUN).await {
            Ok(r) => {
                let (swept, deleted, bytes) = (r.swept, r.deleted, r.bytes);
                let (skipped, failed, done) = (r.skipped, r.failed, r.done);
                tracing::info!(app, swept, deleted, bytes, skipped, failed, done, "swept");
                status(StatusCode::NO_CONTENT)
            }
            Err(e) => {
                tracing::warn!(app, "sweep: {e:#}");
                status(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
    }

    /// `app`, with `creds`: as loaded, if it read its release less than `FRESH` ago with these credentials; else with
    /// its release read again, and its code loaded again if that changed. `None` if it has no release.
    async fn load(&self, app: &str, creds: Credentials) -> Result<Option<Arc<Tric>>> {
        let slot = self.apps.lock().unwrap().entry(app.into()).or_default().clone();
        let mut slot = slot.lock().await;
        if let Some(l) = &*slot
            && l.creds == creds
            && l.read.elapsed() < FRESH
        {
            return Ok(Some(l.tric.clone()));
        }
        let store = Store::s3(store::s3(&self.bucket, Some(creds.clone().into()))?);
        let Some((release, _)) = store.get(&store::app(app, &["current"]), None, RELEASE_MAX).await? else {
            *slot = None;
            return Ok(None);
        };
        let (code, allow) = match &*slot {
            Some(l) if l.release == release => (l.tric.code.clone(), l.tric.allow.clone()),
            _ => {
                let r: Release = serde_json::from_slice(&release)?;
                let allow = r.allowed_outbound_hosts.iter().map(|a| Allow::parse(a)).collect::<Result<_>>()?;
                let component = self.component(&store, app, &r.component).await?;
                (Arc::new(self.engine.load(&component, r.env.into_iter().collect())?), allow)
            }
        };
        let tric = Arc::new(Tric { app: app.into(), store, code, allow, sink: self.sink.clone() });
        *slot = Some(Loaded { creds, release, tric: tric.clone(), read: Instant::now() });
        Ok(Some(tric))
    }

    /// `app`'s component `sha`: from its native code, if that is there and loads; else compiled, and its native code
    /// written for next time.
    async fn component(&self, store: &Store, app: &str, sha: &str) -> Result<Component> {
        let native = Path::from_iter(["native", app, &self.engine.compat(), sha]);
        if let Some((bytes, _)) = store.get(&native, None, NATIVE_MAX).await? {
            // SAFETY: only `app`'s credentials write under `native/<app>/`, so the worst this code can do is what an
            // escape from `app`'s sandbox could, in `app`'s own tenant. A build of another `compat` fails to load.
            match unsafe { self.engine.deserialize(&bytes) } {
                Ok(component) => return Ok(component),
                Err(e) => tracing::warn!(app, "native code: {e:#}"),
            }
        }
        let wasm = store.get(&store::app(app, &["components", sha]), None, COMPONENT_MAX).await?;
        let (wasm, _) = wasm.context("the component is missing")?;
        ensure!(store::hash(&wasm) == sha, "the component does not match its digest");
        let component = tokio::task::block_in_place(|| self.engine.compile(&wasm))?;
        let put = async { store.put(&native, component.serialize()?.into(), PutMode::Overwrite).await };
        if let Err(e) = put.await {
            tracing::warn!(app, "native code: {e:#}");
        }
        Ok(component)
    }
}

/// Hands a commit's delivery event to the router's outbox at `outbox`, which must take it within `HAND`, as Lambda
/// would invoke its `outbox` alias.
async fn hand(outbox: String, event: Bytes) -> Result<()> {
    let body = Full::new(event).map_err(|n| match n {}).boxed_unsync();
    let req = http::Request::post("/").header(HOST, &outbox).header("x-amzn-lambda-context", OUTBOX).body(body)?;
    let (res, _) = timeout(HAND, outbound::internal(&outbox, req)).await?.map_err(|e| format_err!("outbox: {e:?}"))?;
    ensure!(res.status().is_success(), "the outbox answered {}", res.status());
    Ok(())
}

/// Delivers the event in `body`: when that is done, 200 and the messages it published, as JSON; else 503, so the router
/// tries again.
async fn deliver(tric: &Arc<Tric>, body: WasiBody) -> Response {
    let Ok(event) = Limited::new(body, outbox::EVENT_MAX).collect().await else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    match outbox::deliver(tric, &event.to_bytes()).await {
        Ok(m) => {
            let body = Full::new(Bytes::from(serde_json::to_vec(&m).unwrap_or_default())).map_err(|n| match n {});
            http::Response::builder().header(CONTENT_TYPE, "application/json").body(body.boxed_unsync()).unwrap()
        }
        Err(after) => {
            let mut res = status(StatusCode::SERVICE_UNAVAILABLE);
            if let Some(after) = after {
                res.headers_mut().insert(RETRY_AFTER, HeaderValue::from(after.as_secs()));
            }
            res
        }
    }
}
