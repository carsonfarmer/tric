//! serve on Lambda, for the router: each request sent as a function URL event with `InvokeWithResponseStream`, as the
//! app's tenant, and the answer read from the event stream, as a function URL reads it: a prelude, eight NULs, then the
//! body.
use crate::aws::{Aws, query};
use crate::tric::{Request, Response, status};
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::{Buf, Bytes, BytesMut};
use futures_util::stream::{self, BoxStream, StreamExt};
use http::header::{COOKIE, SET_COOKIE};
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::Frame;
use object_store::client::HttpError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use wasmtime::{Result, bail, ensure};
use wasmtime_wasi_http::Error;

/// The most an event stream message may be.
const MESSAGE_MAX: usize = 16 << 20;
/// The most a response's prelude may be.
const PRELUDE_MAX: usize = 1 << 20;
/// The most a request's body may be: Lambda takes events of up to 6 MB, and the body goes in base64.
const BODY_MAX: usize = 4 << 20;

/// The start of a function URL's answer: what goes before the eight NULs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Prelude {
    status_code: u16,
    #[serde(default)]
    headers: BTreeMap<String, Value>, // a value, or a list of them
    #[serde(default)]
    cookies: Vec<String>,
}

/// The end of an `InvokeWithResponseStream` answer.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Complete {
    error_code: Option<String>,
    error_details: Option<String>,
}

/// Sends `req` to the function `function`, as the tenant `tenant`, and streams back its answer.
pub async fn invoke(aws: &Aws, function: &str, tenant: &str, req: Request) -> Result<Response> {
    let Some(event) = event(req).await? else { return Ok(status(StatusCode::PAYLOAD_TOO_LARGE)) };
    let path = format!("/2021-11-15/functions/{}/response-streaming-invocations", query(function));
    let res = aws.send("lambda", Method::POST, &path, &[("x-amz-tenant-id", tenant)], event).await?;
    if !res.status().is_success() {
        let status = res.status();
        bail!("Lambda answered {status}: {}", String::from_utf8_lossy(&res.into_body().bytes().await?));
    }
    answer(res.into_body().bytes_stream()).await
}

/// The response that `stream`, an `InvokeWithResponseStream` answer, carries, its body streaming on.
async fn answer(stream: BoxStream<'static, Result<Bytes, HttpError>>) -> Result<Response> {
    let mut chunks = Chunks { stream, buf: BytesMut::new() };
    let mut head = BytesMut::new();
    let body = loop {
        let Some(chunk) = chunks.next().await? else { bail!("the function answered nothing") };
        head.extend_from_slice(&chunk);
        if let Some(at) = head.windows(8).position(|w| w == [0; 8]) {
            break head.split_off(at).split_off(8).freeze();
        }
        ensure!(head.len() <= PRELUDE_MAX, "the function's prelude is over {PRELUDE_MAX} bytes");
    };
    let prelude: Prelude = serde_json::from_slice(&head)?;
    let mut res = http::Response::builder().status(prelude.status_code);
    for (name, value) in &prelude.headers {
        for value in value.as_array().map_or(std::slice::from_ref(value), Vec::as_slice) {
            res = res.header(name, value.as_str().unwrap_or_default());
        }
    }
    for cookie in &prelude.cookies {
        res = res.header(SET_COOKIE, cookie);
    }
    let rest = stream::try_unfold(chunks, |mut c| async move { Ok(c.next().await?.map(|b| (b, c))) });
    let body = stream::once(async { Ok(body) }).chain(rest);
    let body =
        body.map(|b: Result<Bytes>| b.map(Frame::data).map_err(|e| Error::InternalError(Some(format!("{e:#}")))));
    Ok(res.body(StreamBody::new(body).boxed_unsync())?)
}

/// `req` as a function URL event (payload format 2.0, with only what a runtime needs), with its body in base64; `None`
/// if that is over `BODY_MAX`. JSON holds text only, so a header value that is not UTF-8 is read as lossily.
async fn event(req: Request) -> Result<Option<Vec<u8>>> {
    let (parts, body) = req.into_parts();
    let Ok(body) = Limited::new(body, BODY_MAX).collect().await else { return Ok(None) };
    let mut headers = BTreeMap::<&str, String>::new();
    for (name, value) in &parts.headers {
        let (value, sep) = (String::from_utf8_lossy(value.as_bytes()), if name == COOKIE { "; " } else { "," });
        headers
            .entry(name.as_str())
            .and_modify(|all| *all += &format!("{sep}{value}"))
            .or_insert_with(|| value.to_string());
    }
    Ok(Some(serde_json::to_vec(&json!({
        "rawPath": parts.uri.path(),
        "rawQueryString": parts.uri.query().unwrap_or_default(),
        "headers": headers,
        "requestContext": { "http": { "method": parts.method.as_str() } },
        "body": B64.encode(body.to_bytes()),
        "isBase64Encoded": true,
    }))?))
}

/// Hands `event` to the function `function`, which is to run it once Lambda gets to it.
pub async fn hand(aws: &Aws, function: &str, event: Bytes) -> Result<()> {
    let path = format!("/2015-03-31/functions/{}/invocations", query(function));
    let res = aws.send("lambda", Method::POST, &path, &[("x-amz-invocation-type", "Event")], event.into()).await?;
    ensure!(res.status() == StatusCode::ACCEPTED, "Lambda answered {}", res.status());
    Ok(())
}

/// The payloads of an `InvokeWithResponseStream` answer, in order, until the function returns; an error if it fails.
struct Chunks {
    stream: BoxStream<'static, Result<Bytes, HttpError>>,
    buf: BytesMut,
}

impl Chunks {
    async fn next(&mut self) -> Result<Option<Bytes>> {
        loop {
            while let Some(m) = message(&mut self.buf)? {
                match (m.header(":message-type"), m.header(":event-type")) {
                    (Some("event"), Some("PayloadChunk")) => return Ok(Some(m.payload)),
                    (Some("event"), Some("InvokeComplete")) => {
                        let done: Complete = serde_json::from_slice(&m.payload)?;
                        let Some(code) = done.error_code else { return Ok(None) };
                        bail!("the function failed: {code}: {}", done.error_details.unwrap_or_default());
                    }
                    (Some("event"), _) => {}
                    _ => {
                        let kind = m.header(":exception-type").or(m.header(":error-code")).unwrap_or("an error");
                        bail!("Lambda answered {kind}: {}", String::from_utf8_lossy(&m.payload));
                    }
                }
            }
            match self.stream.next().await {
                Some(bytes) => self.buf.extend_from_slice(&bytes?),
                None => bail!("the answer ended before the function returned"),
            }
        }
    }
}

/// One message of an event stream (`application/vnd.amazon.eventstream`): its headers, which Lambda sends as strings
/// only, and its payload.
#[derive(Debug, PartialEq)]
struct Message {
    headers: Vec<(String, String)>,
    payload: Bytes,
}

impl Message {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Takes the first message off `buf`, if `buf` holds all of it: a prelude of its length, its headers' length and a
/// CRC of those, then the headers, the payload and a CRC of it all. The CRCs go unchecked: TLS refuses corruption.
fn message(buf: &mut BytesMut) -> Result<Option<Message>> {
    let word = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    if buf.len() < 12 {
        return Ok(None);
    }
    let (len, headers_len) = (word(buf, 0) as usize, word(buf, 4) as usize);
    ensure!((16 + headers_len..=MESSAGE_MAX).contains(&len), "an event stream message of {len} bytes");
    if buf.len() < len {
        return Ok(None);
    }
    let mut m = buf.split_to(len).freeze();
    m.advance(12);
    let (mut h, payload) = (m.split_to(headers_len), m.slice(..len - 16 - headers_len));
    let mut headers = vec![];
    while !h.is_empty() {
        let n = take(&mut h, 1)?[0] as usize;
        let (name, kind) = (take(&mut h, n)?, take(&mut h, 1)?[0]);
        ensure!(kind == 7, "an event stream header of type {kind}, not a string");
        let len = take(&mut h, 2).map(|n| u16::from_be_bytes([n[0], n[1]]) as usize)?;
        let value = take(&mut h, len)?;
        headers.push((String::from_utf8(name.into())?, String::from_utf8(value.into())?));
    }
    Ok(Some(Message { headers, payload }))
}

/// The first `n` bytes of a message's headers, taken off them.
fn take(h: &mut Bytes, n: usize) -> Result<Bytes> {
    ensure!(h.len() >= n, "an event stream header is cut short");
    Ok(h.split_to(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message as AWS frames it, its headers strings, but with no CRCs, which go unchecked.
    fn frame(headers: &[(&str, &[u8])], payload: &[u8]) -> Vec<u8> {
        let mut h = vec![];
        for (name, value) in headers {
            h.extend([name.len() as u8]);
            h.extend(name.as_bytes());
            h.extend([7]);
            h.extend((value.len() as u16).to_be_bytes());
            h.extend(*value);
        }
        let mut m = ((16 + h.len() + payload.len()) as u32).to_be_bytes().to_vec();
        m.extend((h.len() as u32).to_be_bytes());
        m.extend([0; 4]);
        m.extend(h);
        m.extend(payload);
        m.extend([0; 4]);
        m
    }

    #[test]
    fn reads_messages_as_they_arrive() {
        let chunk = frame(&[(":event-type", b"PayloadChunk"), (":message-type", b"event")], b"hi");
        let done = frame(&[(":event-type", b"InvokeComplete")], b"{}");
        let all = [chunk.clone(), done.clone()].concat();
        // Fed a byte at a time, each message comes whole once its last byte is in, and not before.
        let (mut buf, mut got) = (BytesMut::new(), vec![]);
        for (i, b) in all.iter().enumerate() {
            buf.extend([*b]);
            if let Some(m) = message(&mut buf).unwrap() {
                got.push((i + 1, m));
            }
        }
        assert!(buf.is_empty());
        let [(at, m), (end, done)] = &got[..] else { panic!("{got:?}") };
        assert_eq!((*at, *end), (chunk.len(), all.len()));
        assert_eq!(
            (m.header(":event-type"), m.header(":message-type"), &m.payload[..]),
            (Some("PayloadChunk"), Some("event"), &b"hi"[..])
        );
        assert_eq!((done.header(":event-type"), &done.payload[..]), (Some("InvokeComplete"), &b"{}"[..]));
        // A header that is not a string, which Lambda never sends, is refused, not skipped.
        let mut bad = frame(&[("n", b"1")], b"");
        bad[14] = 4; // the header's type: a 32-bit integer
        assert!(message(&mut BytesMut::from(&bad[..])).is_err());
    }

    /// An answer's event stream, cut into pieces of `n` bytes.
    fn stream(payloads: &[&[u8]], complete: &str, n: usize) -> BoxStream<'static, Result<Bytes, HttpError>> {
        let event = |kind: &str, p: &[u8]| frame(&[(":event-type", kind.as_bytes()), (":message-type", b"event")], p);
        let mut all: Vec<u8> = payloads.iter().flat_map(|p| event("PayloadChunk", p)).collect();
        all.extend(event("InvokeComplete", complete.as_bytes()));
        stream::iter(all.chunks(n).map(|c| Ok(Bytes::copy_from_slice(c))).collect::<Vec<_>>()).boxed()
    }

    #[tokio::test]
    async fn answers_as_a_function_url_does() {
        let prelude = br#"{"statusCode":201,"headers":{"a":"1","b":["2","3"]},"cookies":["c=4","d=5"]}"#;
        for n in [1, 7, 1 << 20] {
            let s = stream(
                &[&prelude[..30], &[&prelude[30..], &[0; 5][..]].concat(), &[0, 0, 0, b'h', b'i'], b"!"],
                "{}",
                n,
            );
            let res = answer(s).await.unwrap();
            let all =
                |k: &str| res.headers().get_all(k).iter().map(|v| v.to_str().unwrap().to_owned()).collect::<Vec<_>>();
            assert_eq!(
                (res.status().as_u16(), all("a"), all("b"), all("set-cookie")),
                (201, vec!["1".into()], vec!["2".into(), "3".into()], vec!["c=4".into(), "d=5".into()])
            );
            assert_eq!(&res.into_body().collect().await.unwrap().to_bytes()[..], b"hi!");
        }
        // A failure before the prelude fails the answer; one after it, the body.
        let failed = r#"{"ErrorCode":"Unhandled","ErrorDetails":"oops"}"#;
        assert!(answer(stream(&[], failed, 3)).await.is_err());
        let res = answer(stream(&[br#"{"statusCode":200}"#, &[0; 8], b"part"], failed, 3)).await.unwrap();
        assert!(res.into_body().collect().await.is_err());
    }
}
