//! The test app. Every route answers 200, or the `status=N` of its query, after `sleep=MS` milliseconds if given. Under
//! `/@<name>` a route is the same, with the name as the store of `/kv` unless the query names one.
//!
//! - `/echo`: the request, as `{"method", "uri", "headers": [[name, value], ..]}`; under a name, and with an
//!   unsafe method, kept in the name's key `echo` too.
//! - `/fetch?method=M&async=1&url=U`: sends M (GET by default) to U, which is the rest of the query, not decoded, with
//!   `Prefer: respond-async` given `async`. The reply is `<status> <body>`, or the Debug of the `ErrorCode`.
//! - `/kv?…`: see kv.rs.
//! - `/chat`: a room of WebSockets, in Pushpin's WebSocket-over-HTTP: see chat.rs.
//! - `/env`: the environment; `/fs`: what reading the filesystem gets.
//! - `/stream?n=N`: N lines, a second apart, in a body that streams.
//! - `/hog?mb=N` holds N MiB; `/loop` spins; `/print` writes to stdout and stderr.
//! - anything else: `hello`.
use crate::{chat, kv};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::{fmt::Debug, fs, hint::black_box, io};
use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response, Scheme};
use wasip3::http::{client, service::export};
use wasip3::{clocks::monotonic_clock, http_compat::http_into_wasi_response, wit_future, wit_stream};

export!(App);
struct App;

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let method = http::Method::try_from(request.get_method()).map_err(|_| ErrorCode::HttpRequestMethodInvalid)?;
        let target = request.get_path_with_query().unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let (name, path) = match path.strip_prefix("/@").map(|rest| rest.split_once('/').unwrap_or((rest, ""))) {
            Some((name, rest)) => (Some(name), format!("/{rest}")),
            None => (None, path.to_owned()),
        };
        let (params, url) = query.split_once("url=").map_or((query, ""), |(p, u)| (p, u));
        let q: HashMap<String, String> = form_urlencoded::parse(params.as_bytes()).into_owned().collect();
        if let Some(ms) = q.get("sleep").and_then(|v| v.parse::<u64>().ok()) {
            monotonic_clock::wait_for(ms * 1_000_000).await;
        }
        let n: usize = q.get("mb").or(q.get("n")).and_then(|v| v.parse().ok()).unwrap_or(1);
        if path == "/stream" {
            return Ok(stream(n));
        }
        let body = match path.as_str() {
            "/echo" => {
                let echo = echo(&request, &method).to_string();
                if let Some(name) = name.filter(|_| !method.is_safe()) {
                    kv::keep(name, "echo", echo.as_bytes()).map_err(|e| ErrorCode::InternalError(Some(e)))?;
                }
                echo
            }
            "/fetch" => fetch(q.get("method").map_or("GET", |m| m), q.contains_key("async"), url).await,
            "/chat" => return chat::respond(request, name.unwrap_or_default(), &q).await,
            "/kv" => kv::respond(q.get("store").map(|s| s.as_str()).or(name).unwrap_or_default(), &q).to_string(),
            "/env" => json!({ "env": std::env::vars().collect::<BTreeMap<_, _>>() }).to_string(),
            "/fs" => {
                let ls = |dir| fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>());
                json!({
                    "read /etc/passwd": outcome(fs::read_to_string("/etc/passwd")),
                    "list /": outcome(ls("/")),
                    "list .": outcome(ls(".")),
                })
                .to_string()
            }
            "/hog" => format!("hogged {} MiB", black_box(vec![1u8; n << 20]).len() >> 20),
            "/loop" => loop {
                std::hint::spin_loop()
            },
            "/print" => {
                println!("probe-stdout");
                eprintln!("probe-stderr");
                "printed".into()
            }
            _ => "hello".into(),
        };
        let status = q.get("status").and_then(|s| s.parse::<u16>().ok()).unwrap_or(200);
        http_into_wasi_response(http::Response::builder().status(status).body(body).unwrap())
    }
}

fn echo(request: &Request, method: &http::Method) -> Value {
    let scheme = match request.get_scheme() {
        Some(Scheme::Http) => "http".into(),
        Some(Scheme::Https) => "https".into(),
        Some(Scheme::Other(s)) => s,
        None => String::new(),
    };
    let authority = request.get_authority().unwrap_or_default();
    let uri = format!("{scheme}://{authority}{}", request.get_path_with_query().unwrap_or_default());
    let headers = request.get_headers().copy_all();
    let headers: Vec<_> = headers.into_iter().map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned())).collect();
    json!({ "method": method.as_str(), "uri": uri, "headers": headers })
}

/// `n` lines, a second apart, in a body that streams.
fn stream(n: usize) -> Response {
    let (mut lines, body) = wit_stream::new();
    let (done, trailers) = wit_future::new(|| Ok(None));
    wasip3::wit_bindgen::spawn_local(async move {
        for i in 0..n {
            if i > 0 {
                monotonic_clock::wait_for(1_000_000_000).await;
            }
            lines.write_all(format!("{i}\n").into_bytes()).await;
        }
        drop(lines);
        _ = done.write(Ok(None)).await;
    });
    Response::new(Fields::new(), Some(body), trailers).0
}

async fn fetch(method: &str, respond_async: bool, url: &str) -> String {
    let (https, authority, path) = split(url);
    let prefer = [("prefer".to_owned(), b"respond-async".to_vec())];
    let headers = Fields::from_list(if respond_async { &prefer } else { &[] }).unwrap();
    let (request, _) = Request::new(headers, None, wit_future::new(|| Ok(None)).1, None);
    let method = http::Method::from_bytes(method.as_bytes()).unwrap();
    request.set_method(&Method::from(&method)).unwrap();
    request.set_scheme(Some(&if https { Scheme::Https } else { Scheme::Http })).unwrap();
    request.set_authority(Some(authority)).unwrap();
    request.set_path_with_query(Some(path)).unwrap();
    match client::send(request).await {
        Ok(response) => {
            let status = response.get_status_code();
            let (body, _) = Response::consume_body(response, wit_future::new(|| Ok(())).1);
            format!("{status} {}", String::from_utf8_lossy(&body.collect().await))
        }
        Err(e) => format!("{e:?}"),
    }
}

/// `http://host:port/p?q` as (is https, `host:port`, `/p?q`).
fn split(url: &str) -> (bool, &str, &str) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    (scheme == "https", authority, path)
}

fn outcome<T: Debug>(result: io::Result<T>) -> Value {
    match result {
        Ok(v) => json!({ "ok": format!("{v:?}") }),
        Err(e) => json!({ "err": e.to_string() }),
    }
}
