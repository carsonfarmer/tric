//! Serving: every app of an install at `<app>.<domain>`, each request in a fresh instance. A request with an unsafe
//! method to `/@<name>` is a turn on the name: it may write it, and its writes commit if it answers 1xx to 4xx. A
//! conflict runs it again. On Lambda, an invocation that is not an HTTP request is an outbox or cron event.
use crate::aws::Aws;
use crate::engine::{App, Engine};
use crate::name::{self, BUSY, Committed, Conditions, Refused, Snap, Turn};
use crate::outbound::{self, Allow, Fut, Sent};
use crate::outbox::{self, Held, Outbox};
use crate::state::{self, Current};
use bytes::{Bytes, BytesMut};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::{StreamExt, stream};
use http::header::{ETAG, FORWARDED, HOST, RETRY_AFTER};
use http::{HeaderMap, HeaderValue, StatusCode, Uri, uri::Authority};
use http_body_util::{BodyExt, BodyStream, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{server::conn::http1, service::service_fn};
use object_store::{ObjectStore, ObjectStoreExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{net::TcpListener, sync::Mutex, time::sleep, time::timeout};
use wasmtime::component::Component;
use wasmtime::{Error, Result};
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks, io::TokioIo};

pub type Request = http::Request<WasiBody>;
pub type Response = http::Response<WasiBody>;

/// How stale a host's view of an app may get, so also how long a change takes to be live everywhere.
const FRESH: Duration = Duration::from_secs(5);
/// The longest a read of `current` may hold up a request. The S3 client on its own retries for up to 3 minutes.
const RECHECK_MAX: Duration = Duration::from_secs(1);
/// A turn's request body is kept up to this, so the turn can run again; past it, the turn claims its name first.
const REPLAY_MAX: usize = 6 << 20;
/// A turn that has run this long claims its name, so that shorter ones do not keep it from committing.
const CLAIM_AFTER: Duration = Duration::from_secs(1);
const DEPTH_MAX: usize = 16; // requests to itself, one inside another
const EVENT_MAX: usize = 2 << 20;
/// What the Lambda Web Adapter sets for an invocation that is not an HTTP request; for one that is, it is the
/// request's context, so a viewer cannot set it.
const REQUEST_CONTEXT: &str = "x-amzn-request-context";

/// An invocation that is not an HTTP request.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Outbox(Outbox),
    Cron { app: String, path: String },
}

/// A host: every app of an install, or only one.
pub struct Tric {
    pub store: Arc<dyn ObjectStore>,
    engine: Engine,
    compat: String,
    domain: String,
    only: Option<String>,
    lambda: Option<(Aws, String)>, // and the function's name, on Lambda
    apps: Mutex<HashMap<String, Arc<Mutex<Served>>>>,
}

/// An app as a host last read it, and the load of its release. A failed load stands until the next recheck, so a broken
/// release costs one load per `FRESH`, not one per request.
struct Served {
    checked: Instant,
    current: Current,
    load: Option<Load>,
}

/// A load, shared by every request that waits for it.
type Load = Shared<BoxFuture<'static, Result<Arc<Loaded>, String>>>;

/// An app, loaded: its component, ready to instantiate, and where it may send requests.
pub struct Loaded {
    app: App,
    allow: Vec<Allow>,
}

/// One request's state: its turn, if it is one, the names it has read, and where it is in a chain of requests to
/// itself.
pub struct Ctx {
    tric: Arc<Tric>,
    loaded: Arc<Loaded>,
    app: String,
    pub turn: Option<Arc<Turn>>,
    snaps: Mutex<HashMap<String, Arc<Snap>>>,
    host: String,
    chain: Vec<String>, // the names of the turns it is inside
    depth: usize,
}

/// An instance's outbound requests.
pub struct Outbound(pub Arc<Ctx>);

/// An empty response of `code`.
pub fn status(code: StatusCode) -> Response {
    http::Response::builder().status(code).body(Default::default()).unwrap()
}

/// 429, or 503, with `Retry-After: 1`.
fn later(code: StatusCode) -> Response {
    let mut res = status(code);
    res.headers_mut().insert(RETRY_AFTER, HeaderValue::from_static("1"));
    res
}

/// `res`, with `etag` unless it has one.
fn tag(mut res: Response, etag: Option<String>) -> Response {
    if let Some(e) = etag.and_then(|e| HeaderValue::try_from(e).ok()) {
        res.headers_mut().entry(ETAG).or_insert(e);
    }
    res
}

/// Says where a request came from in `Forwarded` (RFC 7239), and only there.
fn forward(headers: &mut HeaderMap, from: HeaderValue) {
    let names: Vec<_> = headers.keys().filter(|k| k.as_str().starts_with("x-forwarded-")).cloned().collect();
    for name in names {
        headers.remove(name);
    }
    headers.insert(FORWARDED, from);
}

/// A turn's request body, which can run again if it was kept whole.
enum Body {
    Whole(Bytes, Option<HeaderMap>),
    Once(Option<WasiBody>),
}

impl Body {
    /// Keeps `body` up to `REPLAY_MAX`; past that, it can be read once.
    async fn keep(mut body: WasiBody) -> Result<Self, wasmtime_wasi_http::Error> {
        let mut data = BytesMut::new();
        while let Some(frame) = body.frame().await {
            match frame?.into_data() {
                Ok(d) if data.len() + d.len() > REPLAY_MAX => {
                    data.extend_from_slice(&d);
                    let kept = stream::once(async { Ok(Frame::data(data.freeze())) });
                    return Ok(Self::Once(Some(StreamBody::new(kept.chain(BodyStream::new(body))).boxed_unsync())));
                }
                Ok(d) => data.extend_from_slice(&d),
                Err(f) => return Ok(Self::Whole(data.freeze(), f.into_trailers().ok())),
            }
        }
        Ok(Self::Whole(data.freeze(), None))
    }

    fn take(&mut self) -> Option<WasiBody> {
        match self {
            Self::Whole(data, trailers) => {
                let frames = [Some(Frame::data(data.clone())), trailers.clone().map(Frame::trailers)];
                Some(StreamBody::new(stream::iter(frames.into_iter().flatten().map(Ok))).boxed_unsync())
            }
            Self::Once(body) => body.take(),
        }
    }
}

impl Tric {
    /// A host of the install in `store`, of every app at `<app>.<domain>`, or of `only` at any host, and on Lambda when
    /// `lambda` names the function.
    pub fn new(
        store: Arc<dyn ObjectStore>,
        domain: &str,
        only: Option<String>,
        lambda: Option<(Aws, String)>,
    ) -> Result<Arc<Self>> {
        let engine = Engine::new()?;
        let (compat, domain) = (engine.compat(), domain.to_ascii_lowercase());
        Ok(Arc::new(Self { store, engine, compat, domain, only, lambda, apps: Mutex::default() }))
    }

    pub fn only(&self) -> Option<&str> {
        self.only.as_deref()
    }

    /// Whether `uri` is `app`'s own: at `host`, where the request that sends it was asked, or at `<app>.<domain>`.
    pub fn is_self(&self, app: &str, host: &str, uri: &Uri) -> bool {
        let at = |h: &str| uri.authority().is_some_and(|a| a.as_str().eq_ignore_ascii_case(h));
        at(host) || at(&format!("{app}.{}", self.domain))
    }

    /// Sends `o` on to delivery: on Lambda, as an event that Lambda queues; elsewhere, in a task.
    pub async fn enqueue(self: &Arc<Self>, o: Outbox) -> Result<()> {
        match &self.lambda {
            Some((aws, function)) => aws.invoke(function, serde_json::to_vec(&Event::Outbox(o))?).await,
            None => {
                let tric = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = outbox::deliver(&tric, o).await {
                        tracing::warn!("outbox: {e:#}");
                    }
                });
                Ok(())
            }
        }
    }

    /// Fires `app`'s cron job at `path`.
    pub async fn cron(self: Arc<Self>, app: String, path: String) {
        let host = format!("{app}.{}", self.domain);
        let proto = if self.lambda.is_some() { "https" } else { "http" };
        let req = http::Request::post(format!("{proto}://{host}{path}")).header(HOST, &host).body(Default::default());
        let res = match req {
            Ok(req) => self.fetch(&app, &host, req, HeaderValue::from_static("for=_cron")).await,
            Err(e) => return tracing::warn!(app, path, "cron: {e}"),
        };
        let status = res.status();
        _ = res.into_body().collect().await;
        tracing::info!(app, path, status = status.as_u16(), "cron");
    }

    /// Runs `req`, from tric itself, as `from` says, to `app` at `host`: the outbox's requests, and cron's.
    pub fn fetch(
        self: &Arc<Self>,
        app: &str,
        host: &str,
        mut req: Request,
        from: HeaderValue,
    ) -> BoxFuture<'static, Response> {
        let (tric, app, host) = (self.clone(), app.to_owned(), host.to_owned());
        forward(req.headers_mut(), from);
        async move {
            match tric.app(&app).await {
                Ok(Some(loaded)) => tric.dispatch(&loaded, &app, req, &host, vec![], 0).await,
                Ok(None) => status(StatusCode::NOT_FOUND),
                Err(e) => {
                    tracing::warn!(app, "{e:#}");
                    status(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        .boxed()
    }

    pub async fn handle(self: Arc<Self>, peer: SocketAddr, req: hyper::Request<Incoming>) -> Response {
        let mut req = req.map(|b| b.map_err(wasmtime_wasi_http::Error::from).boxed_unsync());
        if self.lambda.is_some() && req.headers().get(REQUEST_CONTEXT).is_some_and(|v| v == "null") {
            return self.event(req).await;
        }
        let Some((app, host)) = self.viewer(peer, &mut req) else { return status(StatusCode::NOT_FOUND) };
        match self.app(&app).await {
            Ok(Some(loaded)) => self.dispatch(&loaded, &app, req, &host, vec![], 0).await,
            Ok(None) => status(StatusCode::NOT_FOUND),
            Err(e) => {
                tracing::warn!(app, "{e:#}");
                status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    /// Makes a viewer's request what the app sees: where it came from in `Forwarded`, in place of any `X-Forwarded-*`,
    /// and its URI absolute, at the viewer's host, which a proxy in front passes in `X-Forwarded-Host`. Returns the app
    /// it is for, and the host.
    fn viewer(&self, peer: SocketAddr, req: &mut Request) -> Option<(String, String)> {
        let h = req.headers();
        let last = |name: &str| h.get_all(name).iter().next_back().and_then(|v| v.to_str().ok());
        let authority = |v: &str| v.parse::<Authority>().ok().filter(|a| !a.as_str().contains('@'));
        let host = last("x-forwarded-host").and_then(authority).or_else(|| last(HOST.as_str()).and_then(authority))?;
        let proto = last("x-forwarded-proto").filter(|p| matches!(*p, "http" | "https")).unwrap_or("http");
        let ip = last("x-forwarded-for").and_then(|v| v.rsplit(',').next()).and_then(|v| v.trim().parse().ok());
        let r#for = match ip.unwrap_or(peer.ip()) {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("\"[{ip}]\""),
        };
        let pq = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
        let uri = format!("{proto}://{host}{pq}").parse().ok()?;
        let from = HeaderValue::try_from(format!("for={for};host=\"{host}\";proto={proto}")).ok()?;
        let name = host.host().to_ascii_lowercase();
        let app = match &self.only {
            Some(app) => app.clone(),
            None => name.strip_suffix(&format!(".{}", self.domain))?.to_owned(),
        };
        let host = host.to_string();
        *req.uri_mut() = uri;
        forward(req.headers_mut(), from);
        req.headers_mut().insert(HOST, HeaderValue::try_from(&host).ok()?);
        state::is_app(&app).then_some((app, host))
    }

    /// Runs an event: delivers an outbox, or fires a cron job. A failed delivery answers 500, so Lambda tries it again.
    async fn event(self: Arc<Self>, req: Request) -> Response {
        let Ok(body) = Limited::new(req.into_body(), EVENT_MAX).collect().await else {
            return status(StatusCode::PAYLOAD_TOO_LARGE);
        };
        match serde_json::from_slice(&body.to_bytes()) {
            Ok(Event::Outbox(o)) => match outbox::deliver(&self, o).await {
                Ok(()) => status(StatusCode::OK),
                Err(e) => {
                    tracing::warn!("outbox: {e:#}");
                    status(StatusCode::INTERNAL_SERVER_ERROR)
                }
            },
            Ok(Event::Cron { app, path }) => {
                self.cron(app, path).await;
                status(StatusCode::OK)
            }
            Err(e) => {
                tracing::warn!("not an event: {e}");
                status(StatusCode::BAD_REQUEST)
            }
        }
    }

    /// Runs `req` in an instance of `loaded`, as a turn if it is one.
    async fn dispatch(
        self: &Arc<Self>,
        loaded: &Arc<Loaded>,
        app: &str,
        req: Request,
        host: &str,
        chain: Vec<String>,
        depth: usize,
    ) -> Response {
        let ctx = |turn, chain| {
            let (tric, loaded, app, host, snaps) =
                (self.clone(), loaded.clone(), app.into(), host.into(), Mutex::default());
            Arc::new(Ctx { tric, loaded, app, turn, snaps, host, chain, depth })
        };
        let Some(name) = name::of(req.uri().path()).map(str::to_owned) else {
            return call(loaded, req, ctx(None, chain)).await;
        };
        if !name::is_name(&name) {
            return status(StatusCode::NOT_FOUND);
        }
        if req.method().is_safe() {
            let ctx = ctx(None, chain);
            return match ctx.snap(&name).await {
                Ok(snap) => tag(call(loaded, req, ctx).await, snap.etag.clone()),
                Err(e) => {
                    tracing::warn!(app, name, "{e:#}");
                    status(StatusCode::INTERNAL_SERVER_ERROR)
                }
            };
        }
        let conditions = Conditions::of(req.headers());
        let (parts, body) = req.into_parts();
        let Ok(mut body) = Body::keep(body).await else { return status(StatusCode::BAD_REQUEST) };
        let (mut claim, until) = (matches!(body, Body::Once(_)), Instant::now() + BUSY);
        let chain: Vec<String> = chain.into_iter().chain([name.clone()]).collect();
        loop {
            let Some(body) = body.take() else { return later(StatusCode::SERVICE_UNAVAILABLE) }; // read, and lost
            let turn = match Turn::open(self.store.clone(), app, &name, claim, &conditions, until).await {
                Ok(Ok(turn)) => turn,
                Ok(Err(Refused::Busy)) => return later(StatusCode::TOO_MANY_REQUESTS),
                Ok(Err(Refused::Precondition)) => return status(StatusCode::PRECONDITION_FAILED),
                Err(e) => {
                    tracing::warn!(app, name, "{e:#}");
                    return status(StatusCode::INTERNAL_SERVER_ERROR);
                }
            };
            let timer = turn.clone();
            tokio::spawn(async move {
                sleep(CLAIM_AFTER).await;
                timer.claim().await;
            });
            let req = http::Request::from_parts(parts.clone(), body);
            let (res, instance) = match loaded.app.call(req, ctx(Some(turn.clone()), chain.clone())).await {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::warn!(app, name, "{e:#}");
                    turn.discard().await;
                    return status(StatusCode::INTERNAL_SERVER_ERROR);
                }
            };
            if res.status().is_server_error() {
                turn.discard().await;
                return res;
            }
            match turn.commit(self, host).await {
                Ok(Committed::Done(etag)) => return tag(res, etag),
                Ok(Committed::Conflict) => {
                    instance.abort();
                    claim = true;
                }
                Err(e) => {
                    instance.abort();
                    tracing::warn!(app, name, "{e:#}");
                    return status(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    }

    /// The app `name`, loading it if it is new or its release or environment changed. A name is kept only once it has
    /// been deployed, so a made-up name costs a read each, and no memory.
    async fn app(self: &Arc<Self>, name: &str) -> Result<Option<Arc<Loaded>>> {
        let known = self.apps.lock().await.get(name).cloned(); // apart, as the `match` would hold the lock into its arms
        let served = match known {
            Some(served) => served,
            None => {
                let Some(current) = state::current(&*self.store, name).await? else { return Ok(None) };
                let served = Arc::new(Mutex::new(Served { checked: Instant::now(), current, load: None }));
                self.apps.lock().await.entry(name.into()).or_insert(served).clone()
            }
        };
        let load = {
            let s = &mut *served.lock().await;
            if s.checked.elapsed() >= FRESH {
                match timeout(RECHECK_MAX, state::current(&*self.store, name)).await.map_err(Error::from).flatten() {
                    Ok(Some(c)) if (&c.release, &c.env) != (&s.current.release, &s.current.env) => {
                        (s.current, s.load) = (c, None)
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(app = name, "serving it as last read: {e:#}"),
                }
                s.checked = Instant::now();
                s.load.take_if(|l| l.peek().is_some_and(Result::is_err));
            }
            s.load.get_or_insert_with(|| self.load(name, &s.current)).clone()
        };
        Ok(Some(load.await.map_err(Error::msg)?))
    }

    /// Loads `app` as `current` has it. The load is a task of its own, so it runs to its end even if every request
    /// waiting for it goes away.
    fn load(self: &Arc<Self>, app: &str, current: &Current) -> Load {
        let (tric, app, current) = (self.clone(), app.to_owned(), current.clone());
        let task = tokio::spawn(async move {
            let start = Instant::now();
            let release = state::release(&*tric.store, &app, &current.release).await?;
            let component = tric.component(&app, &release.component).await?;
            let allow = release.allowed_outbound_hosts.iter().map(|a| Allow::parse(a).map_err(Error::msg));
            let allow = allow.collect::<Result<_>>()?;
            let loaded = Loaded { app: tric.engine.load(&app, &component, current.env.into_iter().collect())?, allow };
            tracing::info!(app, ms = start.elapsed().as_millis() as u64, "loaded");
            Ok(Arc::new(loaded))
        });
        task.map(|r| r.unwrap_or_else(|e| Err(e.into())).map_err(|e: Error| format!("{e:#}"))).boxed().shared()
    }

    /// The component `hash` of `app`, from native code a host of this build made, or else compiled, and its native code
    /// kept for the next.
    async fn component(self: &Arc<Self>, app: &str, hash: &str) -> Result<Component> {
        let native = state::native(&self.compat, hash);
        if let Some((bytes, _)) = state::read(&*self.store, &native, state::NATIVE_MAX).await? {
            let tric = self.clone();
            // Safety: native code is trusted as the bucket is (see `state`), and this build's `compat` named it.
            match tokio::task::spawn_blocking(move || unsafe { tric.engine.deserialize(&bytes) }).await? {
                Ok(component) => return Ok(component),
                Err(e) => tracing::warn!(app, "compiling, as its native code did not load: {e:#}"),
            }
        }
        let wasm = state::component(&*self.store, app, hash).await?;
        let tric = self.clone();
        let component = tokio::task::spawn_blocking(move || tric.engine.compile(&wasm)).await??;
        match component.serialize() {
            Ok(bytes) => {
                if let Err(e) = self.store.put(&native, bytes.into()).await {
                    tracing::warn!(app, "keeping its native code: {e}");
                }
            }
            Err(e) => tracing::warn!(app, "serializing its native code: {e:#}"),
        }
        Ok(component)
    }
}

/// Runs a request that is not a turn.
async fn call(loaded: &Loaded, req: Request, ctx: Arc<Ctx>) -> Response {
    match loaded.app.call(req, ctx).await {
        Ok((res, _)) => res,
        Err(e) => {
            tracing::warn!(app = &*loaded.app.name, "{e:#}");
            status(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

impl Ctx {
    /// The name `name` as this request first read it.
    pub async fn snap(&self, name: &str) -> Result<Arc<Snap>> {
        let mut snaps = self.snaps.lock().await;
        if let Some(snap) = snaps.get(name) {
            return Ok(snap.clone());
        }
        let snap = Arc::new(Snap::read(self.tric.store.clone(), &self.app, name).await?);
        snaps.insert(name.into(), snap.clone());
        Ok(snap)
    }

    /// The instance answered: its turn may write no more.
    pub fn answered(&self) {
        if let Some(turn) = &self.turn {
            turn.answer();
        }
    }

    /// Sends an outbound request: to the app itself in-process, held for the outbox if it asks to be and the turn is
    /// open, and, if it is unsafe and the request a turn, once the turn's name is claimed.
    async fn send(self: Arc<Self>, mut req: Request) -> Sent {
        let me = self.tric.is_self(&self.app, &self.host, req.uri());
        if !me {
            outbound::allowed(&self.loaded.allow, req.uri())?;
        }
        let internal = |e: &str| wasmtime_wasi_http::Error::InternalError(Some(e.into()));
        let done: Fut<()> = Box::new(std::future::ready(Ok(())));
        if let Some(turn) = &self.turn
            && turn.is_open()
            && outbox::respond_async(req.headers())
        {
            turn.hold(Held::of(req).await?).map_err(|e| internal(&e))?;
            return Ok((outbox::accepted(), done));
        }
        let unsafe_name = (!req.method().is_safe()).then(|| name::of(req.uri().path())).flatten();
        if me && (self.depth >= DEPTH_MAX || unsafe_name.is_some_and(|n| self.chain.iter().any(|c| c == n))) {
            return Ok((status(StatusCode::LOOP_DETECTED), done));
        }
        if let Some(turn) = &self.turn
            && !req.method().is_safe()
        {
            turn.before_unsafe().await.map_err(internal)?;
        }
        if !me {
            return outbound::send(req).await;
        }
        forward(req.headers_mut(), HeaderValue::from_static("for=_tric"));
        let res =
            self.tric.dispatch(&self.loaded, &self.app, req, &self.host, self.chain.clone(), self.depth + 1).await;
        Ok((res, done))
    }
}

impl WasiHttpHooks for Outbound {
    fn send_request(
        &mut self,
        request: Request,
        _: Option<RequestOptions>,
        _: Fut<()>,
    ) -> Box<dyn Future<Output = Sent> + Send> {
        Box::new(self.0.clone().send(request))
    }
}

/// Serves HTTP/1 on `listener`, each request with `handle`, which gets the peer's address.
pub async fn run<H, F>(listener: TcpListener, handle: H) -> Result<()>
where
    H: Fn(SocketAddr, hyper::Request<Incoming>) -> F + Clone + Send + 'static,
    F: Future<Output = Response> + Send + 'static,
{
    loop {
        let (stream, peer) = listener.accept().await?;
        _ = stream.set_nodelay(true); // best effort; otherwise Nagle plus delayed ACK stalls a response by ~40 ms
        let handle = handle.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(peer, req).map(Ok::<_, Infallible>));
            http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await.ok();
        });
    }
}
