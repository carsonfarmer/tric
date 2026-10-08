//! `tric serve`: the runtime. Each request comes from the router, for the app its `Host` names, as that app's tenant and
//! with that app's storage credentials, which are the only ones serve has: its own role has no storage access.
use crate::aws::Credentials;
use crate::deploy::{RELEASE_MAX, Release};
use crate::engine::Engine;
use crate::outbound::{self, Allow};
use crate::outbox::{self, Delivered, Sink};
use crate::store::{self, Store};
use crate::tric::{self, CREDENTIALS, Response, TENANT, Tric, app_at, forward, status};
use bytes::Bytes;
use futures_util::FutureExt;
use http::header::{FORWARDED, HOST, RETRY_AFTER};
use http::{HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use object_store::{PutMode, path::Path};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};
use wasmtime::component::Component;
use wasmtime::{Result, ensure, error::Context, format_err};
use wasmtime_wasi_http::WasiBody;

/// How long a release read is taken to be the app's current one.
const FRESH: Duration = Duration::from_secs(5);
const COMPONENT_MAX: u64 = 64 << 20;
const NATIVE_MAX: u64 = 256 << 20;
const EVENT_MAX: usize = 2 << 20;
/// How long the router's outbox has to take an event.
const HAND: Duration = Duration::from_secs(10);

struct Serve {
    engine: Engine,
    domain: String,
    bucket: String,
    outbox: String,
    apps: Mutex<HashMap<String, Loaded>>,
}

/// An app as loaded: with the credentials it was given and the release it read, and when it read it.
struct Loaded {
    creds: Credentials,
    release: Bytes,
    tric: Arc<Tric>,
    read: Instant,
}

/// Serves the apps at `<app>.<domain>` on `listen`, from `bucket`, handing delivery events to the router's outbox at
/// `outbox`.
pub async fn run(listen: SocketAddr, domain: String, bucket: String, outbox: String) -> Result<()> {
    let serve = Arc::new(Serve { engine: Engine::new()?, domain, bucket, outbox, apps: Mutex::default() });
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
        let https = from.to_str().is_ok_and(|f| f.split(';').any(|p| p.trim().eq_ignore_ascii_case("proto=https")));
        let pq = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
        let Ok(uri) = format!("{}://{host}{pq}", if https { "https" } else { "http" }).parse() else {
            return status(StatusCode::BAD_REQUEST);
        };
        parts.uri = uri;
        forward(&mut parts.headers, from);
        tric.run(http::Request::from_parts(parts, body), &host).await
    }

    /// `app`, with `creds`: as loaded, if it read its release less than `FRESH` ago with these credentials; else with
    /// its release read again, and its code loaded again if that changed. `None` if it has no release.
    async fn load(&self, app: &str, creds: Credentials) -> Result<Option<Arc<Tric>>> {
        let mut apps = self.apps.lock().await;
        if let Some(l) = apps.get(app)
            && l.creds == creds
            && l.read.elapsed() < FRESH
        {
            return Ok(Some(l.tric.clone()));
        }
        let store = Store::s3(store::s3(&self.bucket, Some(creds.clone().into()))?);
        let Some((release, _)) = store.get(&store::app(app, &["current"]), None, RELEASE_MAX).await? else {
            apps.remove(app);
            return Ok(None);
        };
        let (code, allow) = match apps.get(app) {
            Some(l) if l.release == release => (l.tric.code.clone(), l.tric.allow.clone()),
            _ => {
                let r: Release = serde_json::from_slice(&release)?;
                let allow = r.allowed_outbound_hosts.iter().map(|a| Allow::parse(a)).collect::<Result<_>>()?;
                let component = self.component(&store, app, &r.component).await?;
                (Arc::new(self.engine.load(&component, r.env.into_iter().collect())?), allow)
            }
        };
        let outbox = self.outbox.clone();
        let sink: Sink = Arc::new(move |event| hand(outbox.clone(), event).boxed());
        let tric = Arc::new(Tric { app: app.into(), store, code, allow, sink });
        apps.insert(app.into(), Loaded { creds, release, tric: tric.clone(), read: Instant::now() });
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

/// Hands a commit's delivery event to the router's outbox at `outbox`, which must take it.
async fn hand(outbox: String, event: Bytes) -> Result<()> {
    let body = Full::new(event).map_err(|n| match n {}).boxed_unsync();
    let req = http::Request::post("/").header(HOST, &outbox).body(body)?;
    let (res, _) = timeout(HAND, outbound::internal(&outbox, req)).await?.map_err(|e| format_err!("outbox: {e:?}"))?;
    ensure!(res.status().is_success(), "the outbox answered {}", res.status());
    Ok(())
}

/// Delivers the event in `body`: 204 when that is done, and else 503, so the router tries again.
async fn deliver(tric: &Arc<Tric>, body: WasiBody) -> Response {
    let Ok(event) = Limited::new(body, EVENT_MAX).collect().await else {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    match outbox::deliver(tric, &event.to_bytes()).await {
        Delivered::Done => status(StatusCode::NO_CONTENT),
        Delivered::Retry(after) => {
            let mut res = status(StatusCode::SERVICE_UNAVAILABLE);
            if let Some(after) = after {
                res.headers_mut().insert(RETRY_AFTER, HeaderValue::from(after.as_secs()));
            }
            res
        }
    }
}
