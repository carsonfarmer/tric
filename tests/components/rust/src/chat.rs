//! The `/chat` route: a room over Pushpin's WebSocket-over-HTTP, where the room is the name (`/@room/chat`) and its
//! channel. Only a `POST` of `application/websocket-events` with a `Connection-Id` is a socket's; any other is 400.
//! - `OPEN`: accepted, with GRIP, the socket subscribed to the room; with `plain` in the query, without GRIP, so nothing
//!   is broadcast to it; with `deny`, refused with 403.
//! - `TEXT say T`: the room's `count`, in the name's state, goes up by one, and `<count>: T` is published to the room,
//!   held until the turn commits. `count`: the count. `id`: `id <the socket's id>`. `boom T`: as `say`, but the answer
//!   is 500. `garbage`: an answer that is not events. `bye`: closed. Other text comes back as it is.
//! - `CLOSE`: closed back, and the name's `last` is `close`; `DISCONNECT`: `last` is `disconnect`.
use crate::kv;
use serde_json::json;
use std::collections::HashMap;
use wasip3::http::client;
use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::http_compat::http_into_wasi_request;
use wasip3::{wit_future, wit_stream};

const EVENTS: &str = "application/websocket-events";

pub async fn respond(request: Request, name: &str, q: &HashMap<String, String>) -> Result<Response, ErrorCode> {
    let headers = request.get_headers().copy_all();
    let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| String::from_utf8_lossy(v).into_owned());
    let authority = request.get_authority().unwrap_or_default();
    let (body, _) = Request::consume_body(request, wit_future::new(|| Ok(())).1);
    let body = body.collect().await;
    let Some(id) = header("connection-id").filter(|_| header("content-type").is_some_and(|t| t == EVENTS)) else {
        return Ok(reply(400, &[], b"not a socket".to_vec()));
    };
    let (grip, err) = (!q.contains_key("plain"), |e| ErrorCode::InternalError(Some(e)));
    let text = |t: &str| event("TEXT", format!("{}{t}", if grip { "m:" } else { "" }).as_bytes());
    let (mut status, mut out) = (200, vec![]);
    for (kind, content) in parse(&body) {
        match kind.as_str() {
            "OPEN" if q.contains_key("deny") => return Ok(reply(403, &[], b"no".to_vec())),
            "OPEN" => {
                out.extend(b"OPEN\r\n");
                if grip {
                    out.extend(event("TEXT", format!(r#"c:{{"type":"subscribe","channel":"{name}"}}"#).as_bytes()));
                }
            }
            "TEXT" => {
                let said = String::from_utf8_lossy(&content).into_owned();
                match said.split_once(' ') {
                    Some((command @ ("say" | "boom"), said)) => {
                        let n = kv::incr(name, "count", 1).map_err(err)?;
                        publish(&authority, name, &format!("{n}: {said}")).await?;
                        status = if command == "boom" { 500 } else { 200 };
                    }
                    _ if said == "count" => out.extend(text(&kv::incr(name, "count", 0).map_err(err)?.to_string())),
                    _ if said == "id" => out.extend(text(&format!("id {id}"))),
                    _ if said == "bye" => out.extend(b"CLOSE\r\n"),
                    _ if said == "garbage" => out.extend(b"TEXT 99\r\nx"),
                    _ => out.extend(text(&said)),
                }
            }
            "CLOSE" => {
                kv::keep(name, "last", b"close").map_err(err)?;
                out.extend(event("CLOSE", &[0x03, 0xe8]));
            }
            _ => kv::keep(name, "last", b"disconnect").map_err(err)?,
        }
    }
    let mut headers = vec![("content-type", EVENTS)];
    if out.starts_with(b"OPEN") && grip {
        headers.push(("sec-websocket-extensions", "grip"));
    }
    Ok(reply(status, &headers, out))
}

fn reply(status: u16, headers: &[(&str, &str)], body: Vec<u8>) -> Response {
    let headers: Vec<_> = headers.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect();
    let (mut tx, rx) = wit_stream::new();
    let (done, trailers) = wit_future::new(|| Ok(None));
    wasip3::wit_bindgen::spawn_local(async move {
        tx.write_all(body).await;
        drop(tx);
        _ = done.write(Ok(None)).await;
    });
    let response = Response::new(Fields::from_list(&headers).unwrap(), Some(rx), trailers).0;
    response.set_status_code(status).unwrap();
    response
}

/// Publishes `message` to `channel`, held until the turn commits.
async fn publish(authority: &str, channel: &str, message: &str) -> Result<(), ErrorCode> {
    let items = json!({ "items": [{ "channel": channel, "formats": { "ws-message": { "content": message } } }] });
    let request = http::Request::builder().method("POST").uri(format!("http://{authority}/publish/"));
    let request = request.header("prefer", "respond-async").body(items.to_string()).unwrap();
    client::send(http_into_wasi_request(request)?).await.map(|_| ())
}

/// `KIND HEXLEN\r\n`, the content and `\r\n`.
fn event(kind: &str, content: &[u8]) -> Vec<u8> {
    [format!("{kind} {:x}\r\n", content.len()).as_bytes(), content, b"\r\n"].concat()
}

/// The (kind, content) of each event in `body`, which is well formed.
fn parse(mut body: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut events = vec![];
    while let Some(i) = body.windows(2).position(|w| w == b"\r\n") {
        let head = String::from_utf8_lossy(&body[..i]).into_owned();
        let (kind, len) = head.split_once(' ').map_or((&*head, None), |(k, n)| (k, usize::from_str_radix(n, 16).ok()));
        let rest = &body[i + 2..];
        let (content, rest) = len.map_or((&[][..], rest), |n| (&rest[..n], &rest[n + 2..]));
        events.push((kind.to_owned(), content.to_vec()));
        body = rest;
    }
    events
}
