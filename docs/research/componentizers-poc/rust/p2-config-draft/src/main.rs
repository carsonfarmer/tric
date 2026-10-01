use wstd::http::{Body, Error, Request, Response};

wit_bindgen::generate!({ world: "app", path: "wit", generate_all });

use wasi::config::store as config;
use wasi::keyvalue::store as kv;

fn err<E: std::fmt::Debug>(e: E) -> Error {
    anyhow::anyhow!("{e:?}")
}

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, Error> {
    let greeting = config::get("greeting").map_err(err)?.unwrap_or_else(|| "(unset)".into());
    let bucket = kv::open("").map_err(err)?;
    let seed = bucket.get("seed").map_err(err)?.map(|v| String::from_utf8_lossy(&v).into_owned());
    bucket.set("k", b"v1").map_err(err)?;
    let k = bucket.get("k").map_err(err)?.map(|v| String::from_utf8_lossy(&v).into_owned());
    Ok(Response::new(
        format!("config.greeting={greeting} kv.seed={seed:?} kv.k={k:?}\n").into(),
    ))
}
