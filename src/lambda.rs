//! serve on Lambda, for the router: each request sent as a function URL event with `InvokeWithResponseStream`, as the
//! app's tenant, and the answer read from the event stream, as a function URL reads it: a prelude, eight NULs, then the
//! body.
use crate::aws::{Aws, query};
use crate::tric::{Request, Response, status};
use aws_smithy_eventstream::frame::read_message_from;
use aws_smithy_types::event_stream::Message;
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::{Bytes, BytesMut};
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
    let res = aws.send("lambda", Method::POST, &path, &[("x-amz-tenant-id", tenant)], event, None).await?;
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
    aws.send("lambda", Method::POST, &path, &[("x-amz-invocation-type", "Event")], event.into(), None).await?;
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
                match (header(&m, ":message-type"), header(&m, ":event-type")) {
                    (Some("event"), Some("PayloadChunk")) => return Ok(Some(m.payload().clone())),
                    (Some("event"), Some("InvokeComplete")) => {
                        let done: Complete = serde_json::from_slice(m.payload())?;
                        let Some(code) = done.error_code else { return Ok(None) };
                        bail!("the function failed: {code}: {}", done.error_details.unwrap_or_default());
                    }
                    (Some("event"), _) => {}
                    _ => {
                        let kind = header(&m, ":exception-type").or(header(&m, ":error-code")).unwrap_or("an error");
                        bail!("Lambda answered {kind}: {}", String::from_utf8_lossy(m.payload()));
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

/// Takes the first message of an event stream (`application/vnd.amazon.eventstream`) off `buf`, if `buf` holds all of
/// it, with its CRCs checked; an error if it is over `MESSAGE_MAX`, which is refused before it is held.
fn message(buf: &mut BytesMut) -> Result<Option<Message>> {
    let Some(len) = buf.first_chunk().map(|n| u32::from_be_bytes(*n) as usize) else { return Ok(None) };
    ensure!(len <= MESSAGE_MAX, "an event stream message of {len} bytes");
    if buf.len() < len {
        return Ok(None);
    }
    Ok(Some(read_message_from(buf.split_to(len))?))
}

/// The value of `m`'s header `name`, if it has one and it is a string, the one type Lambda sends.
fn header<'a>(m: &'a Message, name: &str) -> Option<&'a str> {
    let header = m.headers().iter().find(|h| h.name().as_str() == name)?;
    header.value().as_string().ok().map(|v| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::{Header, HeaderValue};

    /// A message as AWS frames it, its headers strings.
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let headers =
            headers.iter().map(|(k, v)| Header::new(k.to_string(), HeaderValue::String(v.to_string().into())));
        let mut m = vec![];
        write_message_to(&Message::new_from_parts(headers.collect(), Bytes::copy_from_slice(payload)), &mut m).unwrap();
        m
    }

    /// An answer's event stream, cut into pieces of `n` bytes.
    fn stream(payloads: &[&[u8]], complete: &str, n: usize) -> BoxStream<'static, Result<Bytes, HttpError>> {
        let event = |kind: &str, p: &[u8]| frame(&[(":event-type", kind), (":message-type", "event")], p);
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
        // A message whose CRC is wrong is refused, as is one over the limit, before it is held.
        let mut bad = frame(&[(":message-type", "event")], b"hi");
        let last = bad.len() - 5;
        bad[last] ^= 1; // the payload's last byte
        assert!(message(&mut BytesMut::from(&bad[..])).is_err());
        let long = ((MESSAGE_MAX + 1) as u32).to_be_bytes();
        assert!(message(&mut BytesMut::from(&long[..])).is_err());
    }
}
