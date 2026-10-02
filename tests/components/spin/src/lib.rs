//! A Spin SDK component: the SDK serves HTTP, and the routes are the `kv` fixture's, bound straight to the WASI interfaces.
use spin_sdk::{http::{IntoResponse, Request}, http_service, wit_bindgen};

wit_bindgen::generate!({
    inline: "package torpor:fixture; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["../../../wit/keyvalue", "../../../wit/config"],
    runtime_path: "::spin_sdk::wit_bindgen::rt",
    generate_all,
});

#[path = "../../rust/src/kv.rs"]
mod kv;

#[http_service]
async fn handle(request: Request) -> impl IntoResponse {
    kv::respond(request.uri().path(), request.uri().query().unwrap_or_default()).unwrap_or_else(|| "ok".into())
}
