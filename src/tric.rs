//! Running an app: each request in a fresh instance. A request with an unsafe method to `/@<name>` is a turn on the
//! name: its writes, and the requests it holds, commit if it answers anything but 5xx. A conflict runs it again.
use crate::engine::App;
use crate::name::{self, BUSY, Committed, Conditions, Refused, Snap, Turn};
use crate::outbound::{self, Allow, Fut, Sent};
use crate::outbox::{self, Held, Sink};
use crate::store::Store;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::header::{ETAG, FORWARDED, RETRY_AFTER};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::{BodyExt, BodyStream, StreamBody};
use hyper::body::Frame;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{sync::Mutex, time::sleep};
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks};

pub type Request = http::Request<WasiBody>;
pub type Response = http::Response<WasiBody>;

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

/// `Forwarded` (RFC 7239) for a request from `ip` to `host` over `proto`.
pub fn forwarded(ip: IpAddr, host: &str, proto: &str) -> Option<HeaderValue> {
    let r#for = match ip.to_canonical() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("\"[{ip}]\""),
    };
    HeaderValue::try_from(format!("for={for};host=\"{host}\";proto={proto}")).ok()
}

/// Makes `from` the only word on where a request came from: no other `Forwarded`, no `X-Forwarded-*` of a proxy, and
/// none of the `X-Amz*` and `X-Tric-*` that AWS and tric pass between themselves.
pub fn forward(headers: &mut HeaderMap, from: HeaderValue) {
    let hop = |k: &&HeaderName| ["x-forwarded-", "x-amz", "x-tric-"].iter().any(|p| k.as_str().starts_with(p));
    let names: Vec<_> = headers.keys().filter(hop).cloned().collect();
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

    /// Runs `req` in an instance, as a turn if it is one.
    async fn dispatch(self: &Arc<Self>, req: Request, host: &str, chain: Vec<String>, depth: usize) -> Response {
        let ctx = |turn, chain| {
            let (tric, snaps, host) = (self.clone(), Mutex::default(), host.into());
            Arc::new(Ctx { tric, turn, snaps, host, chain, depth })
        };
        let Some(name) = name::of(req.uri().path()).map(str::to_owned) else {
            return self.call(req, ctx(None, chain)).await;
        };
        if !name::is_name(&name) {
            return status(StatusCode::NOT_FOUND);
        }
        if req.method().is_safe() {
            let ctx = ctx(None, chain);
            return match ctx.snap(&name).await {
                Ok(snap) => tag(self.call(req, ctx).await, snap.etag.clone()),
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
                Ok(Err(Refused::Busy)) => return later(StatusCode::TOO_MANY_REQUESTS),
                Ok(Err(Refused::Precondition)) => return status(StatusCode::PRECONDITION_FAILED),
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
            let answer = self.code.call(req, ctx(Some(turn.clone()), chain.clone())).await;
            if turn.doomed() {
                // A claim failed, so another turn committed first: whatever this one answered, it runs again.
                if let Ok((_, instance)) = &answer {
                    instance.abort();
                }
                turn.discard().await;
                claim = true;
                continue;
            }
            let (res, instance) = match answer {
                Ok(answer) => answer,
                Err(e) => {
                    tracing::warn!(app = self.app, name, "{e:#}");
                    turn.discard().await;
                    return status(StatusCode::INTERNAL_SERVER_ERROR);
                }
            };
            if res.status().is_server_error() {
                turn.discard().await;
                return res;
            }
            match turn.commit(host, &self.sink).await {
                Ok(Committed::Done(etag)) => return tag(res, etag),
                Ok(Committed::Conflict) => {
                    instance.abort();
                    claim = true;
                }
                Err(e) => {
                    instance.abort();
                    tracing::warn!(app = self.app, name, "{e:#}");
                    return later(StatusCode::SERVICE_UNAVAILABLE);
                }
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

impl Ctx {
    /// The name `name` as this request first read it.
    pub async fn snap(&self, name: &str) -> wasmtime::Result<Arc<Snap>> {
        let mut snaps = self.snaps.lock().await;
        if let Some(snap) = snaps.get(name) {
            return Ok(snap.clone());
        }
        let snap = Arc::new(Snap::read(&self.tric.store, &self.tric.app, name).await?);
        snaps.insert(name.into(), snap.clone());
        Ok(snap)
    }

    /// The instance answered: its turn may write no more.
    pub fn answered(&self) {
        if let Some(turn) = &self.turn {
            turn.answer();
        }
    }

    /// Sends an outbound request: held for the outbox if it asks to be and the turn is open; to the app itself
    /// in-process; and, if it is unsafe and the request a turn, once the turn's name is claimed.
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
    fn send_request(
        &mut self,
        request: Request,
        _: Option<RequestOptions>,
        _: Fut<()>,
    ) -> Box<dyn Future<Output = Sent> + Send> {
        Box::new(self.0.clone().send(request))
    }
}
