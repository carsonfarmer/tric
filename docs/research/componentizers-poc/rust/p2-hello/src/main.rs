use wstd::http::{Body, Error, Request, Response};

#[wstd::http_server]
async fn main(_req: Request<Body>) -> Result<Response<Body>, Error> {
    Ok(Response::new("hello from rust p2 (wstd)\n".to_owned().into()))
}
