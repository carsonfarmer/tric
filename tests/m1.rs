//! The M1 "done when" list, through the public API only: requests go straight to `App::handle`, with no sockets.
use http_body_util::{BodyExt, Empty};
use hyper::{StatusCode, body::Bytes};
use object_store::memory::InMemory;
use std::{fs, sync::Arc, time::Duration, time::Instant};
use torpor::{App, Engine};

fn load(name: &str) -> App {
    let wasm = fs::read(format!("tests/fixtures/{name}.wasm")).unwrap();
    Engine::new().unwrap().load(name, Arc::new(InMemory::new()), wasm, Default::default(), &[]).unwrap()
}

async fn get(app: &App, path: &str) -> (StatusCode, String) {
    let res = app.handle(hyper::Request::get(format!("http://app{path}")).body(Empty::<Bytes>::new()).unwrap()).await;
    (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
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
    assert_eq!(ok(&app, "/hog?mb=240").await, "hogged 240 MiB"); // just under the cap
    assert_eq!(get(&app, "/fields?n=300").await.0, 500, "{name}");
    assert_eq!(ok(&app, "/fields?n=200").await, "held 200 fields");
    assert_eq!(ok(&app, "/").await, "ok");
    assert_eq!(ok(&app, "/env").await, r#"{"args":[],"env":{}}"#);
    assert_eq!(ok(&app, "/fs").await.matches(r#""err""#).count(), 3);
}

#[tokio::test(flavor = "multi_thread")] // each probe compiles its app while the other is timing
async fn probes() {
    tokio::join!(probe("probe-p2"), probe("probe-p3"));
}

/// How long a proxy component takes to give a 500 when its start function spins forever. Its handler returns at once,
/// so the 500 can only come from instantiation: the deadline, or a limit on `memories`.
async fn spin(memories: &str) -> Duration {
    let wat = format!(
        r#"(component
            (import "wasi:http/types@0.2.12" (instance $types
                (export "incoming-request" (type (sub resource)))
                (export "response-outparam" (type (sub resource)))))
            (alias export $types "incoming-request" (type $req))
            (alias export $types "response-outparam" (type $out))
            (core module $m
                {memories}
                (func $spin (loop (br 0)))
                (start $spin)
                (func (export "handle") (param i32 i32)))
            (core instance $i (instantiate $m))
            (func $handle (param "request" (own $req)) (param "response-out" (own $out)) (canon lift (core func $i "handle")))
            (instance $h (export "handle" (func $handle)))
            (export "wasi:http/incoming-handler@0.2.12" (instance $h)))"#
    );
    let app = Engine::new().unwrap().load("wat", Arc::new(InMemory::new()), wat, Default::default(), &[]).unwrap();
    let start = Instant::now();
    assert_eq!(get(&app, "/").await.0, 500);
    start.elapsed()
}

#[tokio::test(flavor = "multi_thread")]
async fn instantiation_is_limited() {
    let (fits, over) = tokio::join!(
        spin("(memory 1600) (memory 1600)"), // 100 MiB twice
        spin("(memory 3200) (memory 3200)"), // 200 MiB twice: 400 MiB in all
    );
    assert!((9..12).contains(&fits.as_secs()), "spins until the deadline: {fits:?}");
    assert!(over < Duration::from_secs(5), "refused at once: {over:?}");
}
