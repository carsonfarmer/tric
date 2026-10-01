use wasip3::http::types::{self, ErrorCode};
use wasip3::http_compat::{http_from_wasi_request, http_into_wasi_response, IncomingRequestBody};

wasip3::http::service::export!(App);

struct App;

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(request: types::Request) -> Result<types::Response, ErrorCode> {
        let request = http_from_wasi_request(request)?;
        http_into_wasi_response(serve(request).await?)
    }
}

async fn serve(
    _req: http::Request<IncomingRequestBody>,
) -> Result<http::Response<String>, ErrorCode> {
    Ok(http::Response::new("hello from rust p3 (wasip3)\n".to_string()))
}

// `cargo build` for a bin target on wasm32-wasip2 needs a main symbol.
fn main() {}
