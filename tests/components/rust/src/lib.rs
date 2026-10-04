//! One source for the fixtures. Without `probe` or `kv` every request gets 200 `hello`; `probe` adds the routes below that
//! exercise the host, and `kv` those in kv.rs.
use serde_json::{Value, json};
use std::{fmt::Debug, fs, hint::black_box, io};

#[cfg(feature = "kv")]
mod kv;
#[cfg(feature = "kv")]
wit_bindgen::generate!({
    inline: "package tric:fixture; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["../../../wit/keyvalue", "../../../wit/config"],
    generate_all,
});

async fn respond(target: &str) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    #[cfg(feature = "kv")]
    if let Some(body) = kv::respond(path, query) {
        return body;
    }
    if cfg!(not(feature = "probe")) {
        return "hello".into();
    }
    let n: usize = query.split_once('=').and_then(|(_, v)| v.parse().ok()).unwrap_or(1); // `mb=` or `n=`
    match path {
        "/loop" => loop {
            std::hint::spin_loop()
        },
        "/hog" => format!("hogged {} MiB", black_box(vec![1u8; n << 20]).len() >> 20),
        "/fields" => format!("held {} fields", black_box((0..n).map(|_| world::Fields::new()).collect::<Vec<_>>()).len()),
        "/env" => json!({
            "env": std::env::vars().collect::<std::collections::BTreeMap<_, _>>(),
            "args": std::env::args().collect::<Vec<_>>(),
        })
        .to_string(),
        "/fs" => {
            let ls = |dir| fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>());
            json!({
                "read /etc/passwd": outcome(fs::read_to_string("/etc/passwd")),
                "list /": outcome(ls("/")),
                "list .": outcome(ls(".")),
            })
            .to_string()
        }
        // The whole rest of the query, not decoded: `/fetch?url=http://host:port/p?a=1&b=2`. The reply is `<status> <body>` or the
        // Debug of the `ErrorCode`.
        "/fetch" => match world::fetch(query.strip_prefix("url=").unwrap_or_default()).await {
            Ok((status, body)) => format!("{status} {}", String::from_utf8_lossy(&body)),
            Err(e) => format!("{e:?}"),
        },
        "/print" => {
            println!("probe-stdout");
            eprintln!("probe-stderr");
            "printed".into()
        }
        _ => "ok".into(),
    }
}

fn outcome<T: Debug>(result: io::Result<T>) -> Value {
    match result {
        Ok(v) => json!({ "ok": format!("{v:?}") }),
        Err(e) => json!({ "err": e.to_string() }),
    }
}

/// `http://host:port/p?q` as (is https, `host:port`, `/p?q`).
fn split(url: &str) -> (bool, &str, &str) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    (scheme == "https", authority, path)
}

#[cfg(feature = "p2")]
mod world {
    use std::{future::Future, io::Write, pin::pin, task::{Context, Poll, Waker}};
    pub use wasip2::http::types::Fields;
    use wasip2::http::{outgoing_handler, types::{ErrorCode, IncomingRequest, OutgoingBody, OutgoingRequest, OutgoingResponse, ResponseOutparam, Scheme}};

    /// Everything in p2 blocks, so a future never suspends.
    fn block_on<T>(future: impl Future<Output = T>) -> T {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => unreachable!(),
        }
    }

    pub async fn fetch(url: &str) -> Result<(u16, Vec<u8>), ErrorCode> {
        let (https, authority, path) = crate::split(url);
        let request = OutgoingRequest::new(Fields::new());
        request.set_scheme(Some(&if https { Scheme::Https } else { Scheme::Http })).unwrap();
        request.set_authority(Some(authority)).unwrap();
        request.set_path_with_query(Some(path)).unwrap();
        let pending = outgoing_handler::handle(request, None)?;
        pending.subscribe().block();
        let response = pending.get().unwrap().unwrap()?;
        let body = response.consume().unwrap();
        let stream = body.stream().unwrap();
        let mut bytes = vec![];
        while let Ok(chunk) = stream.blocking_read(1 << 16) {
            bytes.extend(chunk);
        }
        Ok((response.status(), bytes))
    }

    wasip2::http::proxy::export!(Fixture);
    struct Fixture;

    impl wasip2::exports::http::incoming_handler::Guest for Fixture {
        fn handle(request: IncomingRequest, out: ResponseOutparam) {
            let body = block_on(crate::respond(&request.path_with_query().unwrap_or_default()));
            let response = OutgoingResponse::new(Fields::new());
            let stream = response.body().unwrap();
            ResponseOutparam::set(out, Ok(response));
            let mut w = stream.write().unwrap();
            w.write_all(body.as_bytes()).unwrap();
            w.flush().unwrap();
            drop(w);
            OutgoingBody::finish(stream, None).unwrap();
        }
    }
}

#[cfg(feature = "p3")]
mod world {
    pub use wasip3::http::types::Fields;
    use wasip3::http::{client, types::{ErrorCode, Request, Response, Scheme}};
    use wasip3::{http_compat::http_into_wasi_response, wit_future};

    pub async fn fetch(url: &str) -> Result<(u16, Vec<u8>), ErrorCode> {
        let (https, authority, path) = crate::split(url);
        let (request, _) = Request::new(Fields::new(), None, wit_future::new(|| Ok(None)).1, None);
        request.set_scheme(Some(&if https { Scheme::Https } else { Scheme::Http })).unwrap();
        request.set_authority(Some(authority)).unwrap();
        request.set_path_with_query(Some(path)).unwrap();
        let response = client::send(request).await?;
        let status = response.get_status_code();
        let (body, _) = Response::consume_body(response, wit_future::new(|| Ok(())).1);
        Ok((status, body.collect().await))
    }

    wasip3::http::service::export!(Fixture);
    struct Fixture;

    impl wasip3::exports::http::handler::Guest for Fixture {
        async fn handle(request: Request) -> Result<Response, ErrorCode> {
            let target = request.get_path_with_query().unwrap_or_default();
            http_into_wasi_response(http::Response::new(crate::respond(&target).await))
        }
    }
}
