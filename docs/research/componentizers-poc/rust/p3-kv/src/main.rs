use wasip3::http::types::{self, ErrorCode};
use wasip3::http_compat::{http_from_wasi_request, http_into_wasi_response, IncomingRequestBody};

wit_bindgen::generate!({ world: "app", path: "wit", generate_all });

use wasi::config::store as config;
use wasi::keyvalue::store as kv;

wasip3::http::service::export!(App);

struct App;

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(request: types::Request) -> Result<types::Response, ErrorCode> {
        let request = http_from_wasi_request(request)?;
        http_into_wasi_response(serve(request).await)
    }
}

fn e<E: std::fmt::Debug>(x: E) -> String {
    format!("{x:?}")
}

fn run() -> Result<String, String> {
    let greeting = config::get("greeting").map_err(e)?.unwrap_or_else(|| "(unset)".into());
    let bucket = kv::open("").map_err(e)?;
    let seed = bucket.get("seed").map_err(e)?.map(|v| String::from_utf8_lossy(&v).into_owned());
    bucket.set("k", b"v1").map_err(e)?;
    let k = bucket.get("k").map_err(e)?.map(|v| String::from_utf8_lossy(&v).into_owned());
    Ok(format!("config.greeting={greeting} kv.seed={seed:?} kv.k={k:?}\n"))
}

async fn serve(_req: http::Request<IncomingRequestBody>) -> http::Response<String> {
    match run() {
        Ok(s) => http::Response::new(s),
        Err(m) => http::Response::builder().status(500).body(m).unwrap(),
    }
}

fn main() {}
