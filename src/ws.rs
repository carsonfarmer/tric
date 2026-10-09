//! WebSockets, behind the feature `ws`, as Pushpin speaks them to an app. In WebSocket-over-HTTP
//! (https://pushpin.org/docs/protocols/websocket-over-http/), each event of a socket opened at `/@name/…` is a turn on
//! the name: a `POST` of `application/websocket-events` with its `Connection-Id`, answered with events, `OPEN` first to
//! accept it. In GRIP (https://pushpin.org/docs/protocols/grip/), an app's message starts `m:`, for the client, or
//! `c:`, JSON that subscribes the socket to a channel or unsubscribes it; a publish is a `POST` to `/publish/` at the
//! app's own host, held until its turn commits. API Gateway holds the sockets on AWS, where the router sends serve each
//! event as `Forwarded: for=_ws`; `tric dev` holds them itself. Either keeps a socket's record and subscriptions under
//! `ws/`, which no app reaches. Every size and count is bounded. Not here: binary messages, `PING`, `PONG`, `detach`,
//! `Meta-` and `Grip-` headers, a publish's other formats, and an idle timeout.
use crate::tric::{Response, Tric, app_at, forward, forwarded, status};
use crate::{aws, engine::ANSWER, name, route::Route, route::body, route::viewer, store::Store, store::random};
use base64::{Engine as _, prelude::BASE64_URL_SAFE_NO_PAD as B64};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt, future, stream};
use http::header::UPGRADE;
use http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_PROTOCOL};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, request::Parts};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, upgrade::OnUpgrade, upgrade::Upgraded};
use object_store::{PutMode, path::Path};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, sync::LazyLock, sync::Mutex, time::Duration};
use tokio::{sync::Semaphore, sync::mpsc, time::timeout};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, Role, WebSocketConfig, frame::coding::CloseCode};
use tokio_tungstenite::{WebSocketStream, tungstenite, tungstenite::handshake::server::create_response_with_body};

const EVENTS: &str = "application/websocket-events";
const CONNECTION_ID: HeaderName = HeaderName::from_static("connection-id");
/// API Gateway's sizes: a client's frame is this long at most, and a message from either side.
const FRAME_MAX: usize = 32 << 10;
const MESSAGE_MAX: usize = 128 << 10;
/// An answer, or an event serve is sent, is this long at most, with `ITEMS_MAX` events; a publish, that many items.
const BODY_MAX: usize = 2 << 20;
const ITEMS_MAX: usize = 64;
/// `tric dev` holds this many sockets, runs this many messages of one at once, and gives up a write after `STALL`.
const SOCKETS_MAX: usize = 256;
const RUNNING_MAX: usize = 16;
const STALL: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Event {
    Open,
    Text(String),
    Close(Option<u16>),
    Disconnect,
}

impl Event {
    /// `KIND\r\n`, or `KIND HEXLEN\r\n`, the content and `\r\n`.
    fn encode(&self) -> Bytes {
        let of = |k: &str, c: &[u8]| Bytes::from([format!("{k} {:x}\r\n", c.len()).as_bytes(), c, b"\r\n"].concat());
        match self {
            Self::Text(t) => of("TEXT", t.as_bytes()),
            Self::Close(Some(c)) => of("CLOSE", &c.to_be_bytes()),
            Self::Open => "OPEN\r\n".into(),
            Self::Close(None) => "CLOSE\r\n".into(),
            Self::Disconnect => "DISCONNECT\r\n".into(),
        }
    }
}

/// A socket, as the app is told of it, which is its record: its app, where it opened, which each event is sent to, the
/// headers each is sent with, and whether it speaks GRIP.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Conn {
    app: String,
    uri: String,
    headers: Vec<(String, String)>,
    grip: bool,
}

impl Conn {
    /// A socket of `app` opened at `uri` with `headers`, of which each event carries those that are text, less the
    /// upgrade's own, but for the subprotocols the client offers, and those each event sets.
    fn of(app: &str, uri: String, headers: &HeaderMap) -> Self {
        let ours = [HOST, CONNECTION, UPGRADE, CONTENT_TYPE, CONTENT_LENGTH, CONNECTION_ID];
        let upgrade = |k: &HeaderName| k.as_str().starts_with("sec-websocket-") && k != SEC_WEBSOCKET_PROTOCOL;
        let headers = headers.iter().filter(|(k, _)| !ours.contains(k) && !upgrade(k));
        let headers = headers.filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_owned()))).collect();
        Self { app: app.into(), uri, headers, grip: false }
    }

    /// Sends `tric`'s app `event` of the socket `id`: a turn on the name the socket opened at.
    async fn send(&self, tric: &Arc<Tric>, id: &str, event: &Event) -> Response {
        let host = Uri::try_from(&self.uri).ok().and_then(|u| Some(u.authority()?.to_string())).unwrap_or_default();
        let (body, req) = (event.encode(), http::Request::post(&self.uri).header(HOST, &host));
        let req = self.headers.iter().fold(req, |req, (k, v)| req.header(k, v)).header(CONTENT_TYPE, EVENTS);
        let req = req.header(CONNECTION_ID, id).header(CONTENT_LENGTH, body.len());
        match req.body(Full::new(body).map_err(|n| match n {}).boxed_unsync()) {
            Ok(req) if !host.is_empty() => tric.run(req, &host).await,
            _ => status(StatusCode::BAD_REQUEST),
        }
    }
}

/// The events of the app's answer `res`, if it is `200` of exactly their type, in time, and each is well formed. Else
/// the status to refuse a socket with: the app's own 4xx or 5xx, or else 502.
async fn answer(res: Response) -> Result<Vec<Event>, StatusCode> {
    let (code, of) = (res.status(), res.headers().get(CONTENT_TYPE).is_some_and(|v| v == EVENTS));
    let refuse = if code.is_client_error() || code.is_server_error() { code } else { StatusCode::BAD_GATEWAY };
    (code == StatusCode::OK && of).then_some(()).ok_or(refuse)?;
    let body = timeout(ANSWER, Limited::new(res.into_body(), BODY_MAX).collect()).await;
    body.ok().and_then(|b| parse(&b.ok()?.to_bytes())).ok_or(StatusCode::BAD_GATEWAY)
}

/// The events in `body`, if it is nothing else: `KIND\r\n`, or `KIND HEXLEN\r\n`, as many bytes and `\r\n`, with 1 to 8
/// hex digits. Only kinds tric takes, in capitals; the content of `OPEN` and `DISCONNECT` is ignored, as it must be.
fn parse(mut body: &[u8]) -> Option<Vec<Event>> {
    let mut events = vec![];
    while !body.is_empty() && events.len() <= ITEMS_MAX {
        let end = body.windows(2).position(|w| w == b"\r\n")?;
        let head = std::str::from_utf8(&body[..end]).ok()?;
        body = &body[end + 2..];
        let (kind, content) = match head.split_once(' ') {
            None => (head, None),
            Some((kind, l)) if l.len() <= 8 && l.bytes().all(|b| b.is_ascii_hexdigit()) => {
                let n = usize::from_str_radix(l, 16).ok().filter(|n| *n <= MESSAGE_MAX)?;
                let content = body.get(..n)?;
                body = body.get(n..)?.strip_prefix(b"\r\n")?;
                (kind, Some(content))
            }
            Some(_) => return None,
        };
        events.push(match (kind, content) {
            ("OPEN", _) => Event::Open,
            ("DISCONNECT", _) => Event::Disconnect,
            ("TEXT", Some(c)) => Event::Text(String::from_utf8(c.to_vec()).ok()?),
            ("CLOSE", None | Some([])) => Event::Close(None),
            ("CLOSE", Some(&[a, b])) => Event::Close(Some(u16::from_be_bytes([a, b]))),
            _ => return None,
        });
    }
    (events.len() <= ITEMS_MAX).then_some(events)
}

/// What an event of the app's answer asks of a socket.
#[derive(Debug, PartialEq)]
enum Act {
    Send(String),
    Sub(String),
    Unsub(String),
    Close,
}

/// What `event`, in an answer on a socket that speaks GRIP if `grip`, asks; `None` for a second `OPEN`, a `DISCONNECT`,
/// or GRIP tric does not take: a control message is `{"type", "channel"}` alone. `CLOSE` closes with 1000, as on AWS.
fn act(grip: bool, event: Event) -> Option<Act> {
    Some(match event {
        Event::Text(t) if !grip => Act::Send(t),
        Event::Text(t) => match t.split_at_checked(2)? {
            ("m:", m) => Act::Send(m.into()),
            ("c:", c) => {
                let c = serde_json::from_str::<BTreeMap<String, Value>>(c).ok().filter(|c| c.len() == 2)?;
                let channel = c.get("channel")?.as_str().filter(|c| name::is_name(c))?.into();
                match c.get("type")?.as_str()? {
                    "subscribe" => Act::Sub(channel),
                    "unsubscribe" => Act::Unsub(channel),
                    _ => return None,
                }
            }
            _ => return None,
        },
        Event::Close(_) => Act::Close,
        _ => return None,
    })
}

/// The (channel, message) of each item of the publish `json` that has a message for a socket; `None` unless it is well
/// formed, of `ITEMS_MAX` items at most, each to a name, and each message its text alone, of `MESSAGE_MAX` at most.
pub fn messages(json: &[u8]) -> Option<Vec<(String, String)>> {
    let publish = serde_json::from_slice::<Value>(json).ok()?;
    let items = publish.get("items")?.as_array().filter(|items| items.len() <= ITEMS_MAX)?;
    let message = |item: &Value| -> Option<Option<(String, String)>> {
        let channel = item.get("channel")?.as_str().filter(|c| name::is_name(c))?;
        let Some(m) = item.get("formats")?.as_object()?.get("ws-message") else { return Some(None) };
        let m = m.as_object().filter(|m| m.len() == 1)?.get("content")?.as_str().filter(|m| m.len() <= MESSAGE_MAX)?;
        Some(Some((channel.into(), m.into())))
    };
    items.iter().map(message).collect::<Option<Vec<_>>>().map(|m| m.into_iter().flatten().collect())
}

/// Where the sockets are: `tric dev` holds them, or API Gateway does, for the router.
pub(crate) enum Hub<'a> {
    Dev(&'a Arc<Tric>),
    Aws(&'a Route),
}

impl Hub<'_> {
    /// Where the records and subscriptions are kept.
    fn store(&self) -> &Store {
        if let Self::Aws(route) = self { &route.store } else { &KEPT }
    }

    /// Tells the app `event` of the socket `id`: its answer, else 404 if it has no release, or 503; on AWS, by serve.
    async fn tell(&self, conn: &Conn, id: &str, event: &Event) -> Result<Response, StatusCode> {
        let route = match self {
            Self::Dev(tric) => return Ok(conn.send(tric, id, event).await),
            Self::Aws(route) => route,
        };
        match route.event(&conn.app, "/", "for=_ws", json!([conn, id, event]).to_string().into()).await {
            Ok(res) => res.ok_or(StatusCode::NOT_FOUND),
            Err(e) => Err((tracing::warn!(app = conn.app, "ws: {e:#}"), StatusCode::SERVICE_UNAVAILABLE).1),
        }
    }

    /// Sends the socket `id` the message `m`, or closes it if there is none: whether the socket is gone. On AWS, by
    /// `@connections` at `TRIC_WS`, the stage's URL.
    async fn post(&self, id: &str, m: Option<String>) -> bool {
        let Self::Aws(route) = self else {
            let mut open = OPEN.lock().unwrap();
            let Some(tx) = open.get(id).filter(|_| m.is_some()) else { return open.remove(id).is_none() };
            _ = tx.try_send(m.unwrap_or_default()).inspect_err(|_| tracing::info!(id, "ws: a socket is behind"));
            return false;
        };
        let Some(ws) = std::env::var("TRIC_WS").ok().filter(|ws| !ws.is_empty()) else {
            return (tracing::warn!("ws: TRIC_WS is not set"), false).1;
        };
        let (method, body) = m.map_or((Method::DELETE, vec![]), |m| (Method::POST, m.into_bytes()));
        let url = format!("{}/@connections/{}", ws.trim_end_matches('/'), aws::query(id));
        match route.aws.send("execute-api", method, &url, &[], body).await {
            Ok(res) if res.status().is_success() => false,
            Ok(res) => (tracing::info!(id, "ws: @connections answered {}", res.status()), res.status() == 410).1,
            Err(e) => (tracing::warn!(id, "ws: {e:#}"), false).1,
        }
    }

    /// Asks the app if the socket `id`, of `conn`, may open: if so, keeps its record and subscriptions and gives the
    /// subprotocol chosen; else the status to refuse with, 502 for an answer with a message API Gateway can't send.
    async fn open(&self, id: &str, mut conn: Conn) -> Result<Option<HeaderValue>, StatusCode> {
        let res = self.tell(&conn, id, &Event::Open).await?;
        let [protocol, ext] = [SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_EXTENSIONS].map(|k| res.headers().get(k).cloned());
        let mut events = answer(res).await?.into_iter();
        conn.grip = ext.as_ref().is_some_and(|e| e == "grip");
        let subscribe = |e| if let Some(Act::Sub(channel)) = act(conn.grip, e) { Some(channel) } else { None };
        let opens = events.next() == Some(Event::Open) && (conn.grip || ext.is_none());
        let subs: Vec<_> = opens.then(|| events.map(subscribe).collect()).flatten().ok_or(StatusCode::BAD_GATEWAY)?;
        let keep = async {
            future::try_join_all(subs.iter().map(|c| self.sub(&conn.app, c, id, true))).await?;
            self.store().put(&record(id), serde_json::to_vec(&conn)?.into(), PutMode::Overwrite).await
        };
        let Err(e) = keep.await else { return Ok(protocol) };
        tracing::warn!(app = conn.app, "ws: {e:#}");
        Err((self.tell(&conn, id, &Event::Disconnect).await, StatusCode::SERVICE_UNAVAILABLE).1)
    }

    /// A client's message on the socket `id`: sent to the app, and what it answers done, unless the socket is gone. An
    /// answer that is not events the app may send ends the socket.
    async fn message(&self, id: &str, text: String) {
        let Some(conn) = self.conn(id).await else { return };
        let res = self.tell(&conn, id, &Event::Text(text)).await;
        let acts = async { answer(res.ok()?).await.ok()?.into_iter().map(|e| act(conn.grip, e)).collect() };
        let Some(acts): Option<Vec<_>> = acts.await else {
            return self.end(&conn, id, true, Some(Event::Disconnect)).await;
        };
        for act in acts {
            let done = match act {
                Act::Send(m) => Ok(_ = self.post(id, Some(m)).await),
                Act::Sub(c) => self.sub(&conn.app, &c, id, true).await,
                Act::Unsub(c) => self.sub(&conn.app, &c, id, false).await,
                Act::Close => return self.end(&conn, id, true, None).await,
            };
            done.unwrap_or_else(|e| tracing::warn!(app = conn.app, "ws: {e:#}"));
        }
    }

    /// The socket `id` has ended, with the code it closed with: the app is told, unless the socket is gone already.
    async fn disconnect(&self, id: &str, code: Option<u16>) {
        let Some(conn) = self.conn(id).await else { return };
        let close = Event::Close(code.filter(|c| *c != 1005));
        self.end(&conn, id, false, Some(if code.is_none_or(|c| c == 1006) { Event::Disconnect } else { close })).await
    }

    /// Ends the socket `id`: forgets it, closes it if `close`, and tells the app `event`, if any.
    async fn end(&self, conn: &Conn, id: &str, close: bool, event: Option<Event>) {
        self.store().delete(&record(id)).await.unwrap_or_else(|e| tracing::warn!(app = conn.app, "ws: {e:#}"));
        _ = close && self.post(id, None).await;
        if let Some(event) = event {
            _ = self.tell(conn, id, &event).await;
        }
    }

    /// The record of the socket `id`, unless it is gone.
    async fn conn(&self, id: &str) -> Option<Conn> {
        let read = self.store().json::<Conn>(&record(id), BODY_MAX as u64).await;
        let conn = read.inspect_err(|e| tracing::warn!(id, "ws: {e:#}")).ok().flatten().map(|(conn, _)| conn);
        conn.or_else(|| (tracing::info!(id, "ws: an event of a socket that is gone"), None).1)
    }

    /// Subscribes the socket `id` to `channel` of `app`, or unsubscribes it.
    async fn sub(&self, app: &str, channel: &str, id: &str, on: bool) -> wasmtime::Result<()> {
        let (store, at) = (self.store(), Path::from_iter(["ws", "channels", app, channel, &B64.encode(id)]));
        if on { store.put(&at, Bytes::new(), PutMode::Overwrite).await.map(drop) } else { store.delete(&at).await }
    }

    /// Sends the messages `app` published to the sockets subscribed to their channels, `ITEMS_MAX` at once, and forgets
    /// a subscription whose socket is gone.
    pub(crate) async fn publish(&self, app: &str, messages: Vec<(String, String)>) {
        for (channel, m) in messages.iter().filter(|(c, m)| name::is_name(c) && m.len() <= MESSAGE_MAX) {
            let list = self.store().list(&Path::from_iter(["ws", "channels", app, channel])).await;
            let subs = list.inspect_err(|e| tracing::warn!(app, "ws: {e:#}")).map(|l| l.objects).unwrap_or_default();
            let ids = subs.iter().filter_map(|o| String::from_utf8(B64.decode(o.location.filename()?).ok()?).ok());
            let send = |id: String| async move {
                _ = self.post(&id, Some(m.clone())).await && self.sub(app, channel, &id, false).await.is_ok();
            };
            stream::iter(ids).for_each_concurrent(ITEMS_MAX, send).await;
        }
    }
}

fn record(id: &str) -> Path {
    Path::from_iter(["ws", "connections", &B64.encode(id)])
}

/// `tric dev`'s sockets, by id: where each is sent its messages, until it is removed, which closes it.
static OPEN: Mutex<BTreeMap<String, mpsc::Sender<String>>> = Mutex::new(BTreeMap::new());
static SOCKETS: Semaphore = Semaphore::const_new(SOCKETS_MAX);
/// `tric dev`'s records and subscriptions, in a store of their own where a delete deletes, as no snapshot reads them.
static KEPT: LazyLock<Store> = LazyLock::new(|| Store { versioned: true, ..Store::memory() });

/// Whether `parts` ask to upgrade, at a name.
pub fn wants(parts: &Parts) -> bool {
    parts.extensions.get::<OnUpgrade>().is_some() && name::of(parts.uri.path()).is_some_and(name::is_name)
}

/// Opens a socket for the upgrade in `parts`, in `tric dev`: asks the app, and answers `101` if it accepts.
pub async fn open(tric: Arc<Tric>, mut parts: Parts) -> Response {
    let upgrade = parts.extensions.remove::<OnUpgrade>();
    let req = http::Request::from_parts(parts, ());
    let (Some(upgrade), Ok(mut res)) = (upgrade, create_response_with_body(&req, Default::default)) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let Ok(permit) = SOCKETS.try_acquire() else { return status(StatusCode::SERVICE_UNAVAILABLE) };
    let (id, (tx, rx)) = (random(), mpsc::channel(ITEMS_MAX));
    OPEN.lock().unwrap().insert(id.clone(), tx);
    let protocol = match Hub::Dev(&tric).open(&id, Conn::of(&tric.app, req.uri().to_string(), req.headers())).await {
        Ok(protocol) => protocol,
        Err(code) => return (OPEN.lock().unwrap().remove(&id), status(code)).1,
    };
    tokio::spawn(async move {
        let code = if let Ok(io) = upgrade.await { run(&tric, &id, io, rx).await } else { None };
        OPEN.lock().unwrap().remove(&id);
        (Hub::Dev(&tric).disconnect(&id, code).await, permit)
    });
    if let Some(protocol) = protocol {
        res.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, protocol);
    }
    res
}

/// Serves `tric dev`'s socket `id` until it ends: each message of the client as a turn of its own, `RUNNING_MAX` at
/// once, unordered, as API Gateway runs them; and what `rx` is sent. Closes it, and gives the code it closed with.
async fn run(tric: &Arc<Tric>, id: &str, io: Upgraded, mut rx: mpsc::Receiver<String>) -> Option<u16> {
    let config = WebSocketConfig::default().max_frame_size(Some(FRAME_MAX)).max_message_size(Some(MESSAGE_MAX));
    let io = hyper_util::rt::TokioIo::new(io);
    let (mut sink, mut stream) = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await.split();
    let running = Arc::new(Semaphore::new(RUNNING_MAX));
    let (close, code) = loop {
        tokio::select! {
            (permit, frame) = async { (running.clone().acquire_owned().await, stream.next().await) } => match frame {
                Some(Ok(Message::Text(text))) => {
                    let (tric, id) = (tric.clone(), id.to_owned());
                    tokio::spawn(async move { (Hub::Dev(&tric).message(&id, text.as_str().into()).await, permit) });
                }
                Some(Ok(Message::Binary(_))) => break (CloseCode::Unsupported, Some(1003)),
                Some(Ok(Message::Close(f))) => break (CloseCode::Normal, Some(f.map_or(1005, |f| f.code.into()))),
                Some(Ok(_)) => {} // a ping or a pong, which the socket has answered
                Some(Err(tungstenite::Error::Capacity(_))) => break (CloseCode::Size, Some(1009)),
                _ => break (CloseCode::Away, None),
            },
            m = rx.recv() => {
                let Some(m) = m else { break (CloseCode::Normal, Some(1000)) };
                let sent = timeout(STALL, sink.send(Message::text(m))).await;
                let Ok(Ok(())) = sent else { break (CloseCode::Away, None) };
            }
        }
    };
    _ = timeout(STALL, sink.send(Message::Close(Some(CloseFrame { code: close, reason: "".into() })))).await;
    code
}

/// Sends `tric`'s app the event of a socket the router sent serve, `[conn, id, event]`, and answers with its answer.
pub async fn serve(tric: &Arc<Tric>, body: wasmtime_wasi_http::WasiBody) -> Response {
    let body = Limited::new(body, BODY_MAX).collect().await.ok().map(|b| b.to_bytes());
    match body.and_then(|b| serde_json::from_slice::<(Conn, String, Event)>(&b).ok()) {
        Some((conn, id, event)) if conn.app == tric.app => conn.send(tric, &id, &event).await,
        _ => status(StatusCode::BAD_REQUEST),
    }
}

impl Route {
    /// Takes an event of a socket API Gateway holds, which it invokes the `ws` alias with, and answers as API Gateway
    /// takes it: `{"statusCode"}`, with the subprotocol the app chose if it accepts a `CONNECT`.
    pub(crate) async fn ws(&self, req: hyper::Request<Incoming>) -> Response {
        let Ok((_, e)) = body::<Value>(req).await else { return status(StatusCode::BAD_REQUEST) };
        let (hub, cx, text) = (Hub::Aws(self), &e["requestContext"], e["body"].as_str().unwrap_or_default());
        let closed = cx["disconnectStatusCode"].as_u64().and_then(|c| c.try_into().ok());
        let answer = match (cx["eventType"].as_str(), cx["connectionId"].as_str()) {
            (Some("CONNECT"), Some(id)) => self.connect(id, &e).await,
            (Some("MESSAGE"), Some(id)) => Ok((hub.message(id, text.into()).await, None).1),
            (Some("DISCONNECT"), Some(id)) => Ok((hub.disconnect(id, closed).await, None).1),
            _ => Err(StatusCode::BAD_REQUEST),
        };
        let (code, p) = answer.map_or_else(|code| (code, None), |p| (StatusCode::OK, p));
        let headers = p.and_then(|p| Some(json!({ "Sec-WebSocket-Protocol": p.to_str().ok()? })));
        let answer = json!({ "statusCode": code.as_u16(), "headers": headers.unwrap_or(json!({})) }).to_string();
        http::Response::new(Full::new(Bytes::from(answer)).map_err(|n| match n {}).boxed_unsync())
    }

    /// A socket opens, if CloudFront sent it, at a name of an app that accepts it; its query is rebuilt from the event.
    async fn connect(&self, id: &str, e: &Value) -> Result<Option<HeaderValue>, StatusCode> {
        let pairs = move |k: &str| {
            let map = e[k].as_object().into_iter().flatten();
            map.flat_map(|(k, vs)| vs.as_array().into_iter().flatten().filter_map(move |v| Some((k, v.as_str()?))))
        };
        let pair = |(k, v): (&String, &str)| Some((HeaderName::try_from(k).ok()?, HeaderValue::try_from(v).ok()?));
        let mut headers: HeaderMap = pairs("multiValueHeaders").filter_map(pair).collect();
        let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let [host, path, ip] = ["x-forwarded-host", "x-forwarded-path", "cloudfront-viewer-address"].map(header);
        if !self.is_origin(&header("x-tric-origin")) {
            return Err(StatusCode::FORBIDDEN);
        }
        let at_name = path.starts_with("/@") && name::of(&path).is_some_and(name::is_name);
        let app = app_at(&host, &self.domain).filter(|_| at_name).ok_or(StatusCode::NOT_FOUND)?;
        let query = pairs("multiValueQueryStringParameters").map(|(k, v)| aws::query(k) + "=" + &aws::query(v));
        let query = query.collect::<Vec<_>>().join("&");
        let uri = format!("https://{host}{path}{}{query}", if query.is_empty() { "" } else { "?" });
        let from = viewer(&ip).and_then(|ip| forwarded(ip, &host, "https"));
        let (Ok(_), Some(from)) = (uri.parse::<Uri>(), from) else { return Err(StatusCode::BAD_REQUEST) };
        forward(&mut headers, from);
        Hub::Aws(self).open(id, Conn::of(&app, uri, &headers)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Event {
        Event::Text(s.into())
    }

    #[test]
    fn events_parse_strictly_and_encode_as_parsed() {
        let ok = |body: &[u8], want: &[Event]| assert_eq!(parse(body).as_deref(), Some(want), "{body:?}");
        ok(b"", &[]);
        ok(b"OPEN\r\nTEXT 5\r\nhello\r\nDISCONNECT 2\r\nhi\r\n", &[Event::Open, text("hello"), Event::Disconnect]);
        ok(b"TEXT A\r\n0123456789\r\nTEXT 4\r\na\r\nb\r\n", &[text("0123456789"), text("a\r\nb")]);
        ok(b"CLOSE\r\nCLOSE 0\r\n\r\n", &[Event::Close(None), Event::Close(None)]);
        // With no line end; not CRLF; not in capitals; not kinds tric takes; TEXT with no length; no line end after the
        // content; too short; too long; not hex digits; 9 digits; past a message; not UTF-8; not a code; a blank line.
        let bad = b"OPEN|OPEN\n|open\r\n|PING\r\n|BINARY 1\r\nx\r\n|TEXT\r\n|TEXT 5\r\nhello|TEXT 5\r\nhell\r\n|\
            TEXT 4\r\nhello\r\n|TEXT +1\r\na\r\n|TEXT 000000001\r\na\r\n|TEXT 20001\r\n|TEXT 1\r\n\xff\r\n|\
            CLOSE 1\r\n\x03\r\n|OPEN\r\n\r\n";
        bad.split(|b| *b == b'|').for_each(|body| assert!(parse(body).is_none(), "{}", body.escape_ascii()));
        let many = |n| parse(&b"TEXT 0\r\n\r\n".repeat(n)).map(|e| e.len());
        assert_eq!([ITEMS_MAX, ITEMS_MAX + 1].map(many), [Some(ITEMS_MAX), None]);
        let events = [Event::Open, text("héllo"), Event::Close(None), Event::Close(Some(4001)), Event::Disconnect];
        events.into_iter().for_each(|e| assert_eq!(parse(&e.encode()).as_deref(), Some(&[e][..])));
        assert_eq!(Event::Close(Some(1000)).encode().as_ref(), b"CLOSE 2\r\n\x03\xe8\r\n");
    }

    #[test]
    fn acts_are_only_what_tric_takes() {
        let control = |json: &str| act(true, text(&format!("c:{json}")));
        assert_eq!(act(false, text("c:x")), Some(Act::Send("c:x".into())));
        assert_eq!(act(true, text("m:hi")), Some(Act::Send("hi".into())));
        assert_eq!(act(false, Event::Close(Some(4000))), Some(Act::Close));
        assert_eq!(control(r#"{"type":"subscribe","channel":"room"}"#), Some(Act::Sub("room".into())));
        assert_eq!(control(r#"{"channel":"room","type":"unsubscribe"}"#), Some(Act::Unsub("room".into())));
        let bad = r#"{"type":"detach"}|{"type":"subscribe"}|{"type":"subscribe","channel":"a b"}|subscribe
            {"type":"unsubscribe","channel":"../x"}|{"type":"subscribe","channel":1}|{"type":1,"channel":"x"}
            {"type":"subscribe","channel":"x","filters":[]}"#;
        bad.split(['|', '\n']).map(str::trim).for_each(|bad| assert_eq!(control(bad), None, "{bad}"));
        [text("x"), text("m"), Event::Open, Event::Disconnect].into_iter().for_each(|e| assert_eq!(act(true, e), None));
    }

    #[test]
    fn publishes_are_only_what_tric_takes() {
        let one = |format: &str| format!(r#"{{"items":[{{"channel":"room","formats":{{"ws-message":{format}}}}}]}}"#);
        let got = |json: &str| messages(json.as_bytes());
        assert_eq!(got(&one(r#"{"content":"hi"}"#)), Some(vec![("room".into(), "hi".into())]));
        assert_eq!(got(r#"{"items":[{"channel":"room","formats":{"http-stream":{}}}],"x":1}"#), Some(vec![]));
        let long = format!(r#"{{"content":"{}"}}"#, "x".repeat(MESSAGE_MAX + 1));
        let bad = [r#"{"content":"hi","action":"close"}"#, r#"{"content-bin":"AP8="}"#, "{}", &long].map(one);
        let bad = bad.into_iter().chain([one(r#"{"content":"hi"}"#).replace("room", "a/b"), "[]".into()]);
        bad.for_each(|bad| assert!(got(&bad).is_none(), "{}", &bad[..bad.len().min(80)]));
        let item = r#"{"channel":"room","formats":{"ws-message":{"content":"x"}}}"#;
        let items = |n: usize| got(&format!(r#"{{"items":[{}]}}"#, vec![item; n].join(","))).is_some();
        assert_eq!([ITEMS_MAX, ITEMS_MAX + 1].map(items), [true, false]);
    }
}
