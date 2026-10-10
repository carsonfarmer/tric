//! Running an app: each request in a fresh instance. A request with an unsafe method to `/@<name>` is a turn on the
//! name: its writes, and the requests it holds, commit if it answers anything but 5xx. A conflict runs it again.
use crate::engine::App;
use crate::name::{self, BUSY, Committed, Conditions, TooLarge, Turn};
use crate::outbound::{self, Allow, Fut, Sent};
use crate::outbox::{self, Held, Sink};
use crate::store::Store;
use bytes::{Bytes, BytesMut};
use futures_util::{FutureExt, StreamExt, future::join_all, stream};
use http::header::{CONTENT_TYPE, ETAG, FORWARDED, RETRY_AFTER};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::{BodyExt, BodyStream, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{server::conn::http1, service::service_fn};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::{sync::Mutex, task::JoinHandle, time::sleep};
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks, io::TokioIo};

pub type Request = http::Request<WasiBody>;
pub type Response = http::Response<WasiBody>;

/// The app a request to serve is for, which the router sets, as Lambda names a tenant.
pub const TENANT: HeaderName = HeaderName::from_static("x-amz-tenant-id");
/// The app's storage credentials, which the router sets, as JSON that `credential_process` would print.
pub const CREDENTIALS: HeaderName = HeaderName::from_static("x-tric-credentials");

/// A turn's request body is kept up to this, so the turn can run again; past it, the turn claims its name first.
const REPLAY_MAX: usize = 6 << 20;
/// A turn that has run this long claims its name, so that shorter ones do not keep it from committing.
const CLAIM_AFTER: Duration = Duration::from_secs(1);
const DEPTH_MAX: usize = 16; // requests to itself, one inside another

/// An app, ready to run requests: its code, its state, where it may send requests and where its commits hand their
/// background requests.
pub struct Tric {
    pub app: String,
    pub store: Store,
    pub code: Arc<App>,
    pub allow: Arc<[Allow]>,
    pub sink: Sink,
}

/// One request's state: its turn, if it is one, the names it has read, and where it is in a chain of requests to
/// itself.
pub struct Ctx {
    tric: Arc<Tric>,
    pub turn: Option<Arc<Turn>>,
    name: Option<String>, // the name the request addresses, whose files it sees
    snaps: Mutex<HashMap<String, Arc<Turn>>>,
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

/// `Forwarded` (RFC 7239) for a request from `ip` to `host` over `proto`.
pub fn forwarded(ip: IpAddr, host: &str, proto: &str) -> Option<HeaderValue> {
    let r#for = match ip.to_canonical() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("\"[{ip}]\""),
    };
    HeaderValue::try_from(format!("for={for};host=\"{host}\";proto={proto}")).ok()
}

/// Whether `s` is an app's name: a DNS label in lowercase, of 2 to 63 `a-z`, `0-9` and `-`, with no `-` at either end.
/// 2 at least, as it is the app's STS session name.
pub fn label(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    (2..=63).contains(&s.len()) && s.bytes().all(ok) && !s.starts_with('-') && !s.ends_with('-')
}

/// The app at `host`, which must be exactly a label, a dot and `domain`, port and all, in any case.
pub fn app_at(host: &str, domain: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    let app = host.strip_suffix(&domain.to_ascii_lowercase())?.strip_suffix('.')?;
    label(app).then(|| app.to_owned())
}

/// Serves HTTP/1.1 on `listener`, answering each request with `handle`, which is given the client's address.
pub async fn listen<F, R>(listener: TcpListener, handle: F) -> wasmtime::Result<()>
where
    F: Fn(SocketAddr, hyper::Request<Incoming>) -> R + Clone + Send + 'static,
    R: Future<Output = Response> + Send + 'static,
{
    loop {
        let (tcp, peer) = listener.accept().await?;
        _ = tcp.set_nodelay(true); // otherwise Nagle and delayed ACKs hold a response up by ~40 ms
        let handle = handle.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(peer, req).map(Ok::<_, Infallible>));
            http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).with_upgrades().await.ok();
        });
    }
}

/// Makes `from` the only word on where a request came from: no other `Forwarded`, no `X-Forwarded-*` of a proxy, none
/// of the `X-Amz*` and `X-Tric-*` that AWS and tric pass between themselves, and nothing that marks a WebSocket's event
/// (`Connection-Id`, `Grip-*`, `Meta-*`, a content type of `websocket-events`), with the feature `ws` or without it.
/// A name matches in either spelling of its dashes.
pub fn forward(headers: &mut HeaderMap, from: HeaderValue) {
    const OURS: [&str; 6] = ["x-forwarded-", "x-amz", "x-tric-", "connection-id", "grip-", "meta-"];
    let ours = |k: &&HeaderName| OURS.iter().any(|p| k.as_str().replace('_', "-").starts_with(p));
    let names: Vec<_> = headers.keys().filter(ours).cloned().collect();
    for name in names {
        headers.remove(name);
    }
    let events = |v: &HeaderValue| v.as_bytes().to_ascii_lowercase().windows(16).any(|w| w == b"websocket-events");
    if headers.get_all(CONTENT_TYPE).iter().any(events) {
        headers.remove(CONTENT_TYPE);
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
    /// Runs `req`, from tric itself, as `from` says, at the host its URI names: cron's, and background requests'.
    pub async fn fetch(self: &Arc<Self>, mut req: Request, from: &'static str) -> Response {
        let Some(host) = req.uri().authority().map(ToString::to_string) else {
            return status(StatusCode::BAD_REQUEST);
        };
        forward(req.headers_mut(), HeaderValue::from_static(from));
        self.run(req, &host).await
    }

    /// Runs `req`, asked at `host`, once tric has set its `Forwarded`.
    pub async fn run(self: &Arc<Self>, req: Request, host: &str) -> Response {
        self.dispatch(req, host, vec![], 0).await
    }

    /// Runs `req` in an instance, as a turn if it is one. The response ends once the objects that turns left behind are
    /// deleted, so a Lambda that is frozen at the end of the response does not leave the deletes half done.
    async fn dispatch(self: &Arc<Self>, req: Request, host: &str, chain: Vec<String>, depth: usize) -> Response {
        let mut reaping = vec![];
        let res = self.attempts(req, host, chain, depth, &mut reaping).await;
        match reaping.is_empty() {
            true => res,
            false => res.map(|body| {
                let end = stream::once(join_all(reaping)).filter_map(|_| std::future::ready(None));
                StreamBody::new(BodyStream::new(body).chain(end)).boxed_unsync()
            }),
        }
    }

    /// Runs `req` as often as a turn on its name takes, with the deletes it starts in `reaping`.
    async fn attempts(
        self: &Arc<Self>,
        req: Request,
        host: &str,
        chain: Vec<String>,
        depth: usize,
        reaping: &mut Vec<JoinHandle<()>>,
    ) -> Response {
        let named = name::of(req.uri().path()).map(str::to_owned);
        let ctx = |turn, chain| {
            let (tric, name, snaps, host) = (self.clone(), named.clone(), Mutex::default(), host.into());
            Arc::new(Ctx { tric, turn, name, snaps, host, chain, depth })
        };
        let Some(name) = named.clone() else {
            return self.call(req, ctx(None, chain)).await;
        };
        if !name::is_name(&name) {
            return status(StatusCode::NOT_FOUND);
        }
        if req.method().is_safe() {
            let ctx = ctx(None, chain);
            return match ctx.snap(&name).await {
                Ok(snap) => tag(self.call(req, ctx).await, snap.etag()),
                Err(e) => {
                    tracing::warn!(app = self.app, name, "{e:#}");
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
            let turn = match Turn::open(&self.store, &self.app, &name, claim, &conditions, until).await {
                Ok(Ok(turn)) => turn,
                Ok(Err(StatusCode::TOO_MANY_REQUESTS)) => return later(StatusCode::TOO_MANY_REQUESTS),
                Ok(Err(refused)) => return status(refused),
                Err(e) => {
                    tracing::warn!(app = self.app, name, "{e:#}");
                    return later(StatusCode::SERVICE_UNAVAILABLE);
                }
            };
            let timer = turn.clone();
            tokio::spawn(async move {
                sleep(CLAIM_AFTER).await;
                timer.claim().await;
            });
            let req = http::Request::from_parts(parts.clone(), body);
            let attempt = self.attempt(&turn, req, ctx(Some(turn.clone()), chain.clone()), host).await;
            reaping.extend(turn.reaped());
            match attempt {
                Some(res) => return res,
                None => claim = true,
            }
        }
    }

    /// Runs `req` as the turn `turn`, and commits or discards it. The answer is none if the turn must run again.
    async fn attempt(
        &self,
        turn: &Arc<Turn>,
        req: http::Request<WasiBody>,
        ctx: Arc<Ctx>,
        host: &str,
    ) -> Option<Response> {
        let name = &turn.name;
        let answer = self.code.call(req, ctx).await;
        if turn.doomed() {
            // A claim failed, so another turn committed first: whatever this one answered, it runs again.
            if let Ok((_, instance)) = &answer {
                instance.abort();
            }
            turn.discard().await;
            return None;
        }
        let (res, instance) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                tracing::warn!(app = self.app, name, "{e:#}");
                turn.discard().await;
                return Some(status(StatusCode::INTERNAL_SERVER_ERROR));
            }
        };
        if res.status().is_server_error() {
            turn.discard().await;
            return Some(res);
        }
        match turn.commit(host, &self.sink).await {
            Ok(Committed::Done(etag)) => Some(tag(res, etag)),
            Ok(Committed::Conflict) => {
                instance.abort();
                None
            }
            Err(e) => {
                instance.abort();
                tracing::warn!(app = self.app, name, "{e:#}");
                Some(match e.is::<TooLarge>() {
                    true => status(StatusCode::INTERNAL_SERVER_ERROR),
                    false => later(StatusCode::SERVICE_UNAVAILABLE),
                })
            }
        }
    }

    /// Runs a request that is not a turn.
    async fn call(&self, req: Request, ctx: Arc<Ctx>) -> Response {
        match self.code.call(req, ctx).await {
            Ok((res, _)) => res,
            Err(e) => {
                tracing::warn!(app = self.app, "{e:#}");
                status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

#[cfg(test)]
impl Ctx {
    /// The state of a command, which is not a request, and runs in `turn`, which is on the name `name` of `tric`.
    pub fn of_command(tric: Arc<Tric>, turn: Arc<Turn>, name: &str) -> Arc<Self> {
        let (name, snaps, host) = (Some(name.into()), Mutex::default(), String::new());
        Arc::new(Self { tric, turn: Some(turn), name, snaps, host, chain: vec![], depth: 0 })
    }
}

impl Ctx {
    /// The name `name` as this request first read it.
    pub async fn snap(&self, name: &str) -> wasmtime::Result<Arc<Turn>> {
        let mut snaps = self.snaps.lock().await;
        if let Some(snap) = snaps.get(name) {
            return Ok(snap.clone());
        }
        let snap = Turn::snap(&self.tric.store, &self.tric.app, name).await?;
        snaps.insert(name.into(), snap.clone());
        Ok(snap)
    }

    /// The turn whose tree the guest sees as files: the request's own if it is one, else a snapshot of the name it
    /// addresses, and none if it addresses none.
    pub async fn mount(&self) -> wasmtime::Result<Option<Arc<Turn>>> {
        match (&self.turn, &self.name) {
            (Some(turn), _) => Ok(Some(turn.clone())),
            (None, Some(name)) => self.snap(name).await.map(Some),
            (None, None) => Ok(None),
        }
    }

    /// The instance answered: its turn may write no more.
    pub fn answered(&self) {
        if let Some(turn) = &self.turn {
            turn.answer();
        }
    }

    /// Sends an outbound request: held for the outbox if it asks to be and the turn is open, or else refused with 503
    /// if the name has too many commits pending; to the app itself in-process; and, if it is unsafe and the request a
    /// turn, once the turn's name is claimed.
    async fn send(self: Arc<Self>, mut req: Request) -> Sent {
        let me = req.uri().authority().is_some_and(|a| a.as_str().eq_ignore_ascii_case(&self.host));
        if !me {
            outbound::allowed(&self.tric.allow, req.uri())?;
        }
        let internal = |e: &str| wasmtime_wasi_http::Error::InternalError(Some(e.into()));
        let done: Fut<()> = Box::new(std::future::ready(Ok(())));
        if let Some(turn) = &self.turn
            && turn.is_open()
            && outbox::respond_async(req.headers())
        {
            if turn.backlogged() {
                return Ok((status(StatusCode::SERVICE_UNAVAILABLE), done)); // refused, and not sent at once
            }
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
        Ok((self.tric.dispatch(req, &self.host, self.chain.clone(), self.depth + 1).await, done))
    }
}

impl WasiHttpHooks for Outbound {
    fn send_request(&mut self, request: Request, _: Option<RequestOptions>, _: Fut<()>) -> Fut<(Response, Fut<()>)> {
        Box::new(self.0.clone().send(request))
    }
}
