//! Running an app: each request in a fresh instance. A request with an unsafe method to `/@<name>` is a turn on the
//! name: its writes, and the requests it holds, commit if it answers anything but 5xx. A conflict runs it again, if
//! it may.
use crate::engine::App;
use crate::name::{self, Answer, Committed, Conditions, Failed, Idempotency, Record, Turn};
use crate::outbound::{self, Allow, Fut, Sent};
use crate::outbox::{self, Held, Sink};
use crate::store::Store;
use futures_util::FutureExt;
use http::header::{CONTENT_TYPE, ETAG, FORWARDED, LOCATION, RETRY_AFTER};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::{server::conn::http1, service::service_fn};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks, io::TokioIo};

pub type Request = http::Request<WasiBody>;
pub type Response = http::Response<WasiBody>;

/// The app a request to serve is for, which the router sets, as Lambda names a tenant.
pub const TENANT: HeaderName = HeaderName::from_static("x-amz-tenant-id");
/// The app's storage credentials, which the router sets, as JSON that `credential_process` would print.
pub const CREDENTIALS: HeaderName = HeaderName::from_static("x-tric-credentials");
/// What marks the answer of a turn sent again with its `Idempotency-Key`, as Stripe marks one.
const IDEMPOTENT_REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

/// The most a turn's request body may be, as tric holds it whole so the turn can run again; and on Lambda the most any
/// request's may be, as Lambda takes events of up to 6 MB, and the body goes in base64.
pub const BODY_MAX: usize = 4 << 20;
/// A turn's deadline: one that another commit beat runs again until then, and after it answers 503. A run begun before
/// it has the guest's `ANSWER`, and its commit `SETTLE`, all in CloudFront's 60 s for an answer from the origin.
const DEADLINE: Duration = Duration::from_secs(5);
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

/// 503, with `Retry-After: 1`.
fn later() -> Response {
    let mut res = status(StatusCode::SERVICE_UNAVAILABLE);
    res.headers_mut().insert(RETRY_AFTER, HeaderValue::from_static("1"));
    res
}

/// The answer of a turn whose commit failed with `e`: 503, with `Retry-After: 1`, if it failed before its put
/// ([`Failed::Unsent`]), so nothing landed; else 500, so a client does not run the turn again, as it may have landed,
/// or would fail again.
pub(crate) fn failed(e: &wasmtime::Error) -> Response {
    match e.downcast_ref::<Failed>() {
        Some(Failed::Unsent) => later(),
        _ => status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// `res`, with `etag` unless it has one.
fn tag(mut res: Response, etag: Option<String>) -> Response {
    if let Some(e) = etag.and_then(|e| HeaderValue::try_from(e).ok()) {
        res.headers_mut().entry(ETAG).or_insert(e);
    }
    res
}

/// The answer of a turn sent again with its `Idempotency-Key`: the status and `Location` the turn committed with,
/// marked, with no body, which is not kept, and no `ETag`, as the head may have changed since.
fn replay(record: Record) -> Response {
    let mut res = status(StatusCode::from_u16(record.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    if let Some(location) = record.location.and_then(|l| HeaderValue::try_from(l).ok()) {
        res.headers_mut().insert(LOCATION, location);
    }
    res.headers_mut().insert(IDEMPOTENT_REPLAYED, HeaderValue::from_static("true"));
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

    /// Runs `req` in an instance, as a turn if it is one: again while another commit beats it, if it may run again and
    /// its deadline has not passed, and else answering 503, as nothing it wrote landed. A turn's body is held whole,
    /// and one over `BODY_MAX` is 413.
    async fn dispatch(self: &Arc<Self>, req: Request, host: &str, chain: Vec<String>, depth: usize) -> Response {
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
        let Ok(key) = Idempotency::key(req.headers()) else { return status(StatusCode::BAD_REQUEST) };
        let (parts, body) = req.into_parts();
        let body = match Limited::new(body, BODY_MAX).collect().await {
            Ok(body) => body.to_bytes(),
            Err(e) if e.is::<LengthLimitError>() => return status(StatusCode::PAYLOAD_TOO_LARGE),
            Err(_) => return status(StatusCode::BAD_REQUEST),
        };
        let once = key.map(|key| Idempotency::of(key, &parts.method, &parts.uri, &body));
        let until = Instant::now() + DEADLINE;
        let chain: Vec<String> = chain.into_iter().chain([name.clone()]).collect();
        loop {
            let turn = match Turn::open(&self.store, &self.app, &name, &conditions, once.as_ref()).await {
                Ok(Ok(turn)) => turn,
                Ok(Err(Answer::Replay(record))) => return replay(record),
                Ok(Err(Answer::Refused(refused))) => return status(refused),
                Err(e) => {
                    tracing::warn!(app = self.app, name, "{e:#}");
                    return later();
                }
            };
            let body = Full::new(body.clone()).map_err(|n| match n {}).boxed_unsync();
            let req = http::Request::from_parts(parts.clone(), body);
            match self.attempt(&turn, req, ctx(Some(turn.clone()), chain.clone()), host).await {
                Some(res) => return res,
                None if turn.repeatable() && Instant::now() < until => {}
                None => return later(),
            }
        }
    }

    /// Runs `req` as the turn `turn`, and commits or discards it. The answer is none if another commit beat it.
    async fn attempt(
        &self,
        turn: &Arc<Turn>,
        req: http::Request<WasiBody>,
        ctx: Arc<Ctx>,
        host: &str,
    ) -> Option<Response> {
        let name = &turn.name;
        let (res, instance) = match self.code.call(req, ctx).await {
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
        match turn.commit(host, &self.sink, res.status(), res.headers().get(LOCATION)).await {
            Ok(Committed::Done(etag)) => Some(tag(res, etag)),
            Ok(Committed::Conflict) => {
                instance.abort();
                None
            }
            Err(e) => {
                instance.abort();
                tracing::warn!(app = self.app, name, "{e:#}");
                Some(failed(&e))
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

    /// Sends an outbound request: held for the outbox if it asks to be and the turn is open; to the app itself
    /// in-process; and, if it is unsafe and the request a turn, once the turn is ready for it, as
    /// [`Turn::before_unsafe`] says.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_that_failed_is_tried_again_only_if_nothing_landed() {
        let answer = |e: wasmtime::Error| {
            let res = failed(&e);
            (res.status().as_u16(), res.headers().get(RETRY_AFTER).cloned())
        };
        let unsent = wasmtime::format_err!("the upload failed").context(Failed::Unsent);
        assert_eq!(answer(unsent), (503, Some(HeaderValue::from_static("1"))), "nothing landed");
        assert_eq!(answer(wasmtime::Error::msg(Failed::TooLarge)), (500, None), "it would fail again");
        assert_eq!(answer(wasmtime::Error::msg(Failed::Unsettled)), (500, None), "it may have landed");
        assert_eq!(answer(wasmtime::format_err!("anything else")), (500, None), "it may have landed");
    }
}
