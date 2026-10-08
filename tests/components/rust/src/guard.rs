//! The test middleware: with `GUARD_TOKEN` set, a request without `Authorization: Bearer <it>` gets 401, and every other
//! goes on to the app.
use exports::wasi::http::handler::Guest;
use wasi::http::handler;
use wasi::http::types::{ErrorCode, Fields, Request, Response};

wit_bindgen::generate!({ path: "wit", world: "guard", generate_all });

export!(Guard);
struct Guard;

impl Guest for Guard {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let want = std::env::var("GUARD_TOKEN").ok().map(|t| format!("Bearer {t}").into_bytes());
        if want.is_none_or(|w| request.get_headers().get("authorization").contains(&w)) {
            return handler::handle(request).await;
        }
        let (response, _) = Response::new(Fields::new(), None, wit_future::new(|| Ok(None)).1);
        response.set_status_code(401).map_err(|()| ErrorCode::InternalError(None))?;
        Ok(response)
    }
}
