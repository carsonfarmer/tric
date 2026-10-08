//! WebSockets, behind the feature `ws`, in `tric dev`: tric holds the sockets, and the app speaks to them as it would
//! to Pushpin, in WebSocket-over-HTTP (https://pushpin.org/docs/protocols/websocket-over-http/). A socket opened at
//! `/@name/…` has each event on it sent to that path as a `POST` of `application/websocket-events` with the socket's
//! `Connection-Id`: a turn on the name like any other, whose answer is events back down the socket. The app accepts by
//! answering `OPEN` first. `Sec-WebSocket-Extensions: grip` there turns on GRIP (https://pushpin.org/docs/protocols/
//! grip/), where a message the app sends starts `m:` and is for the client, or `c:` and is JSON that subscribes the
//! socket to a channel, or unsubscribes it. To publish to a channel, the app `POST`s Pushpin's publish API to
//! `/publish/` at its own host, which `dispatch` takes in-process.
//!
//! What only tric says to the app is never believed from anyone else: `forward` calls `scrub` on each request that
//! comes in, so a client's cannot claim to be an event, or a socket's, or GRIP's. Events are parsed whole and written
//! out again, never passed on, and every size and count is bounded. Not here: `PING` and `PONG` events, `detach`,
//! `Meta-` and `Grip-` headers, subprotocols, other formats of a publish, and an idle timeout.
use crate::engine::ANSWER;
use crate::tric::{Request, Response, Tric, status};
use crate::{name, store};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, FORWARDED, UPGRADE};
use http::header::{SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, request::Parts};
use http_body_util::{BodyExt, Full, Limited};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Semaphore, broadcast};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig, frame::coding::CloseCode};
use tokio_tungstenite::tungstenite::{self, Message, handshake::derive_accept_key};

const EVENTS: &str = "application/websocket-events";
const CONNECTION_ID: HeaderName = HeaderName::from_static("connection-id");
/// A message is this long at most, from a client or in an answer.
const MESSAGE_MAX: usize = 1 << 20;
/// An answer or a publish is this long at most, with `ITEMS_MAX` events or items.
const BODY_MAX: usize = 2 << 20;
const ITEMS_MAX: usize = 64;
/// A socket is subscribed to this many channels at most, and `tric dev` holds this many sockets.
const CHANNELS_MAX: usize = 16;
const SOCKETS_MAX: usize = 256;
/// A write to a client that has stopped reading is given up after this long.
const STALL: Duration = Duration::from_secs(30);

type Ws = WebSocketStream<TokioIo<Upgraded>>;
type Published = (String, Message);
/// How a socket ends: the code it closes with, and whether the app knows already.
type End = (CloseCode, bool);

/// What is published, once `tric dev` has called `init`: a socket that falls 256 messages behind is closed.
static BUS: OnceLock<broadcast::Sender<Published>> = OnceLock::new();
static SOCKETS: Semaphore = Semaphore::const_new(SOCKETS_MAX);

/// Starts taking sockets and publishes.
pub fn init() {
    BUS.get_or_init(|| broadcast::channel(256).0);
}

/// Removes from a request's `headers` what only `tric dev` says to the app: a `Connection-Id`, `Grip-` and `Meta-`
/// headers, in either spelling of their dash, and a content type that mentions events.
pub fn scrub(headers: &mut HeaderMap) {
    let ours = |k: &&HeaderName| {
        let k = k.as_str().replace('_', "-");
        k == CONNECTION_ID.as_str() || k.starts_with("grip-") || k.starts_with("meta-")
    };
    let names: Vec<_> = headers.keys().filter(ours).cloned().collect();
    for name in names {
        headers.remove(name);
    }
    let events =
        |v: &HeaderValue| String::from_utf8_lossy(v.as_bytes()).to_ascii_lowercase().contains("websocket-events");
    if headers.get_all(CONTENT_TYPE).iter().any(events) {
        headers.remove(CONTENT_TYPE);
    }
}

/// Whether `parts` ask to upgrade, at a name.
pub fn wants(parts: &Parts) -> bool {
    let upgrade = parts.extensions.get::<OnUpgrade>().is_some();
    BUS.get().is_some() && upgrade && name::of(parts.uri.path()).is_some_and(name::is_name)
}

/// Opens a socket for the upgrade in `parts`, at `host`: asks the app, and answers `101` if it accepts, else with its
/// refusal.
pub async fn open(tric: Arc<Tric>, host: String, mut parts: Parts) -> Response {
    let (upgrade, key, bus) = (parts.extensions.remove::<OnUpgrade>(), parts.headers.get(SEC_WEBSOCKET_KEY), BUS.get());
    let is = |name, want: &[u8]| parts.headers.get(name).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(want));
    let (Some(upgrade), Some(key), Some(bus)) = (upgrade, key, bus) else { return status(StatusCode::BAD_REQUEST) };
    if parts.method != Method::GET || !is(UPGRADE, b"websocket") || !is(SEC_WEBSOCKET_VERSION, b"13") {
        return status(StatusCode::BAD_REQUEST);
    }
    let accept = derive_accept_key(key.as_bytes());
    let Ok(permit) = SOCKETS.try_acquire() else { return status(StatusCode::SERVICE_UNAVAILABLE) };
    // What the app sees of the upgrade on each event: its headers, less the upgrade's, and the socket's.
    let upgrading = |k: &HeaderName| k == CONNECTION || k == UPGRADE || k.as_str().starts_with("sec-websocket-");
    let mut headers: HeaderMap =
        parts.headers.iter().filter(|(k, _)| !upgrading(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(EVENTS));
    headers.insert(CONNECTION_ID, HeaderValue::try_from(store::random()).unwrap());
    let mut conn = Conn { tric, host, uri: parts.uri, headers, grip: false, subs: HashSet::new() };

    let bus = bus.subscribe();
    let res = conn.send(&Event::Open).await;
    if !events(&res) {
        return res; // the app's refusal, as it is
    }
    let ext = res.headers().get(SEC_WEBSOCKET_EXTENSIONS).cloned();
    let events = match answer(res).await {
        Some(mut events) if events.first() == Some(&Event::Open) && ext.iter().all(|e| e == "grip") => {
            events.split_off(1)
        }
        _ => return status(StatusCode::BAD_GATEWAY),
    };
    conn.grip = ext.is_some();
    let config = WebSocketConfig::default().max_message_size(Some(MESSAGE_MAX)).max_frame_size(Some(MESSAGE_MAX));
    tokio::spawn(async move {
        let _permit = permit;
        match upgrade.await {
            Ok(io) => {
                let ws = WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, Some(config)).await;
                conn.run(ws, bus, events).await;
            }
            Err(_) => _ = conn.send(&Event::Disconnect).await,
        }
    });
    let res = http::Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    res.header(UPGRADE, "websocket")
        .header(CONNECTION, "upgrade")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Default::default())
        .unwrap()
}

/// A socket, as the app knows it.
struct Conn {
    tric: Arc<Tric>,
    host: String,
    /// Where the socket opened, which each event is sent to.
    uri: http::Uri,
    /// What each event is sent with.
    headers: HeaderMap,
    grip: bool,
    subs: HashSet<String>,
}

impl Conn {
    /// Sends `event` to the app, as a turn if its path is a name's.
    async fn send(&self, event: &Event) -> Response {
        let bytes = event.encode();
        let body = Full::new(bytes.clone()).map_err(|n| match n {}).boxed_unsync();
        let mut req = http::Request::builder().method(Method::POST).uri(self.uri.clone()).body(body).unwrap();
        *req.headers_mut() = self.headers.clone();
        req.headers_mut().insert(CONTENT_LENGTH, bytes.len().into());
        self.tric.run(req, &self.host).await
    }

    /// Sends `event`, and gives the events the app answers.
    async fn post(&self, event: Event) -> Result<Vec<Event>, End> {
        answer(self.send(&event).await).await.ok_or((CloseCode::Error, false))
    }

    /// Serves the socket until it ends, and closes it, telling the app unless it knows.
    async fn run(mut self, mut ws: Ws, mut bus: broadcast::Receiver<Published>, events: Vec<Event>) {
        let Err((code, told)) = self.pump(&mut ws, &mut bus, events).await;
        _ = timeout(STALL, ws.close(Some(CloseFrame { code, reason: "".into() }))).await;
        if !told {
            self.send(&Event::Disconnect).await;
        }
    }

    /// Does `events`, then each message of the client with the events the app answers, and what is published to the
    /// socket's channels, one at a time.
    async fn pump(
        &mut self,
        ws: &mut Ws,
        bus: &mut broadcast::Receiver<Published>,
        mut events: Vec<Event>,
    ) -> Result<Infallible, End> {
        loop {
            for event in events {
                self.apply(ws, event).await?;
            }
            events = tokio::select! {
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(text))) => self.post(Event::Text(text.as_str().to_owned())).await?,
                    Some(Ok(Message::Binary(data))) => self.post(Event::Binary(data)).await?,
                    Some(Ok(Message::Close(frame))) => {
                        self.send(&Event::Close(frame.map(|f| f.code.into()))).await;
                        return Err((CloseCode::Normal, true));
                    }
                    Some(Ok(_)) => vec![], // a ping or a pong, which the socket has answered
                    Some(Err(tungstenite::Error::Capacity(_))) => return Err((CloseCode::Size, false)),
                    _ => return Err((CloseCode::Away, false)),
                },
                published = bus.recv() => {
                    let (channel, message) = published.map_err(|_| (CloseCode::Again, false))?;
                    if self.subs.contains(&channel) {
                        tell(ws, message).await?;
                    }
                    vec![]
                }
            };
        }
    }

    /// Does one event of the app's answer: sends the client a message, or subscribes the socket, or ends it.
    async fn apply(&mut self, ws: &mut Ws, event: Event) -> Result<(), End> {
        const REFUSED: End = (CloseCode::Error, false);
        let message = match event {
            Event::Text(t) if !self.grip => Message::text(t),
            Event::Binary(b) if !self.grip => Message::Binary(b),
            Event::Text(t) => match t.split_at_checked(2) {
                Some(("m:", m)) => Message::text(m),
                Some(("c:", c)) if control(&mut self.subs, c) => return Ok(()),
                _ => return Err(REFUSED),
            },
            Event::Binary(b) if b.starts_with(b"m:") => Message::Binary(b.slice(2..)),
            Event::Close(code) => {
                let code = code.map_or(CloseCode::Normal, CloseCode::from);
                return Err(if code.is_allowed() { (code, true) } else { REFUSED });
            }
            _ => return Err(REFUSED), // a second `OPEN`, a `DISCONNECT`, or a binary message that GRIP does not know
        };
        tell(ws, message).await
    }
}

/// Sends `message` to the client, unless it has stopped reading.
async fn tell(ws: &mut Ws, message: Message) -> Result<(), End> {
    timeout(STALL, ws.send(message)).await.ok().and_then(Result::ok).ok_or((CloseCode::Away, false))
}

/// A GRIP control message: `{"type": "subscribe" or "unsubscribe", "channel": …}`, and no more.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Control {
    Subscribe { channel: String },
    Unsubscribe { channel: String },
}

/// Does the control message `json` on `subs`; whether it is one, and the channel a name, and not one too many.
fn control(subs: &mut HashSet<String>, json: &str) -> bool {
    match serde_json::from_str(json) {
        Ok(Control::Subscribe { channel }) if name::is_name(&channel) => {
            subs.insert(channel);
            subs.len() <= CHANNELS_MAX
        }
        Ok(Control::Unsubscribe { channel }) => {
            subs.remove(&channel);
            true
        }
        _ => false,
    }
}

/// One event.
#[derive(Debug, PartialEq)]
enum Event {
    Open,
    Text(String),
    Binary(Bytes),
    Close(Option<u16>),
    Disconnect,
}

impl Event {
    /// `KIND\r\n`, or `KIND HEXLEN\r\n`, the content and `\r\n`.
    fn encode(&self) -> Bytes {
        let code;
        let (kind, content) = match self {
            Self::Open => ("OPEN", None),
            Self::Text(t) => ("TEXT", Some(t.as_bytes())),
            Self::Binary(b) => ("BINARY", Some(&b[..])),
            Self::Close(None) => ("CLOSE", None),
            Self::Close(Some(c)) => {
                code = c.to_be_bytes();
                ("CLOSE", Some(&code[..]))
            }
            Self::Disconnect => ("DISCONNECT", None),
        };
        match content {
            Some(c) => [format!("{kind} {:x}\r\n", c.len()).as_bytes(), c, b"\r\n"].concat(),
            None => format!("{kind}\r\n").into_bytes(),
        }
        .into()
    }
}

/// Whether `res` answers with events: `200`, of exactly their type.
fn events(res: &Response) -> bool {
    res.status() == StatusCode::OK && res.headers().get(CONTENT_TYPE).is_some_and(|v| v == EVENTS)
}

/// The events `res` answers, if it is events, in time, and all of them are well formed.
async fn answer(res: Response) -> Option<Vec<Event>> {
    if !events(&res) {
        return None;
    }
    let body = timeout(ANSWER, Limited::new(res.into_body(), BODY_MAX).collect()).await.ok()?.ok()?;
    parse(&body.to_bytes())
}

/// The events in `body`, if it is nothing else: `KIND\r\n`, or `KIND HEXLEN\r\n` and that many bytes and `\r\n`, with
/// a length of 1 to 8 hex digits. Only events tric may take, in capitals; the content of `OPEN` and `DISCONNECT` is
/// ignored, as it must be.
fn parse(mut body: &[u8]) -> Option<Vec<Event>> {
    let mut events = vec![];
    while !body.is_empty() {
        let end = body.windows(2).position(|w| w == b"\r\n")?;
        let head = std::str::from_utf8(&body[..end]).ok()?;
        body = &body[end + 2..];
        let (kind, len) = head.split_once(' ').map_or((head, None), |(kind, len)| (kind, Some(len)));
        let content = match len {
            None => None,
            Some(l) if l.len() <= 8 && l.bytes().all(|b| b.is_ascii_hexdigit()) => {
                let n = usize::from_str_radix(l, 16).ok().filter(|n| *n <= MESSAGE_MAX)?;
                let content = body.get(..n)?;
                body = body.get(n..)?.strip_prefix(b"\r\n")?;
                Some(content)
            }
            Some(_) => return None,
        };
        events.push(match (kind, content) {
            ("OPEN", _) => Event::Open,
            ("DISCONNECT", _) => Event::Disconnect,
            ("TEXT", Some(c)) => Event::Text(String::from_utf8(c.to_vec()).ok()?),
            ("BINARY", Some(c)) => Event::Binary(Bytes::copy_from_slice(c)),
            ("CLOSE", c) => Event::Close(match c.unwrap_or_default() {
                [] => None,
                &[a, b] => Some(u16::from_be_bytes([a, b])),
                _ => return None,
            }),
            _ => return None,
        });
        if events.len() > ITEMS_MAX {
            return None;
        }
    }
    Some(events)
}

/// Whether `req` is a publish: Pushpin's publish API, from the app to itself, so not from a client, whose `Forwarded`
/// is its own.
pub fn publishes(req: &Request) -> bool {
    let (path, from) = (req.uri().path(), req.headers().get(FORWARDED));
    BUS.get().is_some() && req.method() == Method::POST && path == "/publish/" && from.is_some_and(|f| f == "for=_tric")
}

/// Gives a publish's messages to the sockets subscribed to their channels, or answers `400`.
pub async fn publish(req: Request) -> Response {
    let Ok(body) = Limited::new(req.into_body(), BODY_MAX).collect().await else {
        return status(StatusCode::BAD_REQUEST);
    };
    let (Some(messages), Some(bus)) = (messages(&body.to_bytes()), BUS.get()) else {
        return status(StatusCode::BAD_REQUEST);
    };
    for message in messages {
        _ = bus.send(message); // an error is no one to send to
    }
    status(StatusCode::OK)
}

#[derive(Deserialize)]
struct Publish {
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    channel: String,
    formats: Formats,
}

#[derive(Deserialize)]
struct Formats {
    #[serde(rename = "ws-message")]
    ws_message: Option<Content>,
}

/// A message's text, and no more of it: not an `action`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Content {
    content: String,
}

/// The (channel, message) of each item of the publish `json` that has a message for a socket; `None` unless it is
/// well formed, and no more than `ITEMS_MAX` items.
fn messages(json: &[u8]) -> Option<Vec<Published>> {
    let Publish { items } = serde_json::from_slice(json).ok().filter(|p: &Publish| p.items.len() <= ITEMS_MAX)?;
    let messages = items.into_iter().filter_map(|i| Some((i.channel, Message::text(i.formats.ws_message?.content))));
    Some(messages.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Event {
        Event::Text(s.into())
    }

    #[test]
    fn events_parse_strictly() {
        let ok = |body: &[u8], want: &[Event]| assert_eq!(parse(body).as_deref(), Some(want), "{body:?}");
        ok(b"", &[]);
        ok(b"OPEN\r\nTEXT 5\r\nhello\r\nDISCONNECT 2\r\nhi\r\n", &[Event::Open, text("hello"), Event::Disconnect]);
        ok(b"TEXT A\r\n0123456789\r\nTEXT 4\r\na\r\nb\r\n", &[text("0123456789"), text("a\r\nb")]);
        ok(b"CLOSE\r\nCLOSE 2\r\n\x03\xe8\r\n", &[Event::Close(None), Event::Close(Some(1000))]);
        ok(b"BINARY 3\r\n\x01\x02\x03\r\n", &[Event::Binary(Bytes::from_static(&[1, 2, 3]))]);
        for body in [
            &b"OPEN"[..],               // no line end
            b"OPEN\n",                  // not CRLF
            b"open\r\n",                // not in capitals
            b"PING\r\n",                // not an event tric takes
            b"TEXT\r\n",                // TEXT has a length
            b"TEXT 5\r\nhello",         // no line end after the content
            b"TEXT 5\r\nhell\r\n",      // too short
            b"TEXT 4\r\nhello\r\n",     // too long
            b"TEXT +1\r\na\r\n",        // not hex digits
            b"TEXT 000000001\r\na\r\n", // 9 digits
            b"TEXT 100001\r\n",         // past a message
            b"TEXT 1\r\n\xff\r\n",      // not UTF-8
            b"CLOSE 1\r\n\x03\r\n",     // not a code
            b"OPEN\r\n\r\n",            // an empty line
        ] {
            assert!(parse(body).is_none(), "{:?}", String::from_utf8_lossy(body));
        }
        let many = |n| b"TEXT 0\r\n\r\n".repeat(n);
        assert_eq!(parse(&many(ITEMS_MAX)).map(|e| e.len()), Some(ITEMS_MAX));
        assert!(parse(&many(ITEMS_MAX + 1)).is_none());
    }

    #[test]
    fn events_encode_as_parsed() {
        let all = [Event::Open, text("héllo"), Event::Binary(vec![0, 255, 13, 10].into()), Event::Close(None)];
        for event in all.into_iter().chain([Event::Close(Some(4001)), Event::Disconnect]) {
            assert_eq!(parse(&event.encode()).as_deref(), Some(&[event][..]));
        }
        assert_eq!(Event::Close(Some(1000)).encode().as_ref(), b"CLOSE 2\r\n\x03\xe8\r\n");
    }

    #[test]
    fn scrubbing() {
        let mut headers = HeaderMap::new();
        for k in "connection-id connection_id grip-sig grip_hold meta-user content-type authorization".split(' ') {
            headers.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_static("Websocket-Events"));
        }
        scrub(&mut headers);
        assert_eq!(headers.keys().map(|k| k.as_str()).collect::<Vec<_>>(), ["authorization"]);
        for types in [&["text/plain, application/websocket-events"][..], &["application/json", "x/WebSocket-Events;y"]]
        {
            headers.remove(CONTENT_TYPE);
            types.iter().for_each(|t| _ = headers.append(CONTENT_TYPE, HeaderValue::from_static(t)));
            scrub(&mut headers);
            assert!(headers.get(CONTENT_TYPE).is_none(), "{types:?}");
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        scrub(&mut headers);
        assert!(headers.get(CONTENT_TYPE).is_some());
    }

    #[test]
    fn controls() {
        let mut subs = HashSet::new();
        for ok in ["subscribe", "subscribe", "unsubscribe"] {
            assert!(control(&mut subs, &format!(r#"{{"type":"{ok}","channel":"room"}}"#)));
        }
        for bad in [
            r#"{"type":"detach"}"#,
            r#"{"type":"subscribe"}"#,
            r#"{"type":"subscribe","channel":"a b"}"#,
            r#"{"type":"subscribe","channel":"x","filters":[]}"#,
            r#"{"type":"subscribe","channel":1}"#,
            "subscribe",
        ] {
            assert!(!control(&mut subs, bad), "{bad}");
        }
        assert!(subs.is_empty());
        for n in 0..CHANNELS_MAX {
            assert!(control(&mut subs, &format!(r#"{{"type":"subscribe","channel":"c{n}"}}"#)));
        }
        assert!(!control(&mut subs, r#"{"type":"subscribe","channel":"one-too-many"}"#));
    }

    #[test]
    fn publishes_are_only_what_tric_takes() {
        let one = |format: &str| format!(r#"{{"items":[{{"channel":"room","formats":{{"ws-message":{format}}}}}]}}"#);
        let got = |json: &str| messages(json.as_bytes());
        assert_eq!(got(&one(r#"{"content":"hi"}"#)), Some(vec![("room".into(), Message::text("hi"))]));
        assert_eq!(got(r#"{"items":[{"channel":"room","formats":{"http-stream":{}}}],"x":1}"#), Some(vec![]));
        for bad in [
            one(r#"{"content":"hi","action":"close"}"#),
            one(r#"{"content-bin":"AP8="}"#), // not supported
            one("{}"),
            "[]".into(),
        ] {
            assert!(got(&bad).is_none(), "{bad}");
        }
        let item = r#"{"channel":"room","formats":{"ws-message":{"content":"x"}}}"#;
        let items = |n: usize| format!(r#"{{"items":[{}]}}"#, vec![item; n].join(","));
        assert!(got(&items(ITEMS_MAX)).is_some());
        assert!(got(&items(ITEMS_MAX + 1)).is_none());
    }
}
