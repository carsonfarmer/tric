//! One source for four fixtures. Without `probe` every request gets 200 `hello`; with it, the routes below exercise the host.
use serde_json::{Value, json};
use std::{fmt::Debug, fs, hint::black_box, io};

fn respond(target: &str) -> String {
    if cfg!(not(feature = "probe")) {
        return "hello".into();
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/loop" => loop {
            std::hint::spin_loop()
        },
        "/hog" => {
            let mb: usize = query.strip_prefix("mb=").and_then(|v| v.parse().ok()).unwrap_or(1);
            format!("hogged {} MiB", black_box(vec![1u8; mb << 20]).len() >> 20)
        }
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

#[cfg(feature = "p2")]
mod world {
    use std::io::Write;
    use wasip2::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};

    wasip2::http::proxy::export!(Fixture);
    struct Fixture;

    impl wasip2::exports::http::incoming_handler::Guest for Fixture {
        fn handle(request: IncomingRequest, out: ResponseOutparam) {
            let body = crate::respond(&request.path_with_query().unwrap_or_default());
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
    use wasip3::http::types::{ErrorCode, Request, Response};
    use wasip3::http_compat::http_into_wasi_response;

    wasip3::http::service::export!(Fixture);
    struct Fixture;

    impl wasip3::exports::http::handler::Guest for Fixture {
        async fn handle(request: Request) -> Result<Response, ErrorCode> {
            let target = request.get_path_with_query().unwrap_or_default();
            http_into_wasi_response(http::Response::new(crate::respond(&target)))
        }
    }
}
