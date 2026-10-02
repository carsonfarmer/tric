//! The M1 "done when" list, through the public API only: requests go straight to `App::handle`, with no sockets.
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use std::{fs, time::Instant};
use torpor::{App, Engine};

fn load(name: &str) -> App {
    let wasm = fs::read(format!("tests/fixtures/{name}.wasm")).unwrap();
    Engine::new().unwrap().load(name, wasm, Default::default()).unwrap()
}

async fn get(app: &App, path: &str) -> (u16, String) {
    let res = app.handle(hyper::Request::get(format!("http://app{path}")).body(Empty::<Bytes>::new()).unwrap()).await;
    (res.status().as_u16(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
}

async fn ok(app: &App, path: &str) -> String {
    let (status, body) = get(app, path).await;
    assert_eq!(status, 200, "{path}");
    body
}

#[tokio::test]
async fn hello() {
    for name in ["hello-p2", "hello-p3", "hello-js"] {
        assert_eq!(ok(&load(name), "/").await, "hello", "{name}");
    }
}

async fn probe(name: &str) {
    let app = load(name);
    let start = Instant::now();
    assert_eq!(get(&app, "/loop").await.0, 500, "{name}");
    assert!((9..12).contains(&start.elapsed().as_secs()), "{name}: {:?}", start.elapsed());
    assert_eq!(ok(&app, "/").await, "ok");
    assert_eq!(get(&app, "/hog?mb=300").await.0, 500, "{name}");
    assert_eq!(ok(&app, "/hog?mb=8").await, "hogged 8 MiB");
    assert_eq!(ok(&app, "/").await, "ok");
    assert_eq!(ok(&app, "/env").await, r#"{"args":[],"env":{}}"#);
    assert_eq!(ok(&app, "/fs").await.matches(r#""err""#).count(), 3);
}

#[tokio::test]
async fn probes() {
    tokio::join!(probe("probe-p2"), probe("probe-p3"));
}
