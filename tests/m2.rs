//! M2: `wasi:keyvalue`, `wasi:config` and outbound HTTP, through the public API only, in a p2, a p3 and a Spin SDK guest.
mod common;
use common::*;
use object_store::{ObjectStoreExt, path::Path};
use serde_json::{Value, json};
use std::sync::Arc;
use torpor::App;

async fn j(app: &App, path: &str) -> Value {
    let (status, body) = get(app, path).await;
    assert_eq!(status, 200, "{path}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {e}: {body}"))
}

async fn raw(store: &Counting, key: &str) -> Vec<u8> {
    store.inner.get(&Path::from(key)).await.unwrap().bytes().await.unwrap().to_vec()
}

async fn keys(app: &App, path: &str) -> Value {
    j(app, path).await["ok"]["keys"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_op_in_every_guest() {
    for name in ["kv-p2", "kv-p3", "spin"] {
        let app = load(&Default::default(), name, &[]);
        for (path, want) in [
            ("/config?key=greeting", json!({"ok": "hi"})),
            ("/config?key=nope", json!({"ok": null})),
            ("/config?key=empty", json!({"ok": ""})),
            ("/config", json!({"ok": {"empty": "", "greeting": "hi"}})),
            ("/kv?op=open&store=s", json!({"ok": null})),
            ("/kv?op=get&store=s&key=a", json!({"ok": null})),
            ("/kv?op=exists&store=s&key=a", json!({"ok": false})),
            ("/kv?op=set&store=s&key=a&value=1", json!({"ok": null})),
            ("/kv?op=get&store=s&key=a", json!({"ok": "1"})),
            ("/kv?op=exists&store=s&key=a", json!({"ok": true})),
            ("/kv?op=set&store=s&key=b%2Fc%20d&value=2", json!({"ok": null})),
            ("/kv?op=get&store=s&key=b%2Fc%20d", json!({"ok": "2"})),
            ("/kv?op=list&store=s", json!({"ok": {"keys": ["a", "b/c d"], "cursor": null}})),
            ("/kv?op=list&store=other", json!({"ok": {"keys": [], "cursor": null}})),
            ("/kv?op=delete&store=s&key=a", json!({"ok": null})),
            ("/kv?op=delete&store=s&key=a", json!({"ok": null})), // there is no error for a missing key
            ("/kv?op=list&store=s", json!({"ok": {"keys": ["b/c d"], "cursor": null}})),
            ("/kv?op=incr&store=s&key=n", json!({"ok": 1})),
            ("/kv?op=incr&store=s&key=n&delta=41", json!({"ok": 42})),
            ("/kv?op=incr&store=s&key=n&delta=-50", json!({"ok": -8})),
            ("/kv?op=cas&store=s&key=x&value=v1", json!({"ok": {"seen": null, "swapped": true}})),
            ("/kv?op=cas&store=s&key=x&value=v2", json!({"ok": {"seen": "v1", "swapped": true}})),
            (
                "/kv?op=cas&store=s&key=x&value=v3&between=mine",
                json!({"ok": {"seen": "v2", "swapped": false, "latest": "mine"}}),
            ),
            (
                "/kv?op=cas&store=s&key=y&value=v3&between=mine",
                json!({"ok": {"seen": null, "swapped": false, "latest": "mine"}}),
            ),
            ("/kv?op=get&store=s&key=x", json!({"ok": "mine"})),
            ("/kv?op=set-many&store=m&keys=p,q,r&value=z", json!({"ok": null})),
            ("/kv?op=get-many&store=m&keys=p,nope,r", json!({"ok": [["p", "z"], ["nope", null], ["r", "z"]]})),
            ("/kv?op=delete-many&store=m&keys=p,q", json!({"ok": null})),
            ("/kv?op=list&store=m", json!({"ok": {"keys": ["r"], "cursor": null}})),
            ("/kv?op=rmw&store=s&key=r&n=20", json!({"ok": 0})),
            ("/kv?op=get&store=s&key=r", json!({"ok": "20"})),
        ] {
            assert_eq!(j(&app, path).await, want, "{name} {path}");
        }
    }
}

/// Every failure comes back as a value the guest can handle, never as a trap (which would be a 500).
#[tokio::test(flavor = "multi_thread")]
async fn errors_are_values() {
    let app = load(&Default::default(), "kv-p3", &[]);
    let (no_store, bad_key) =
        (json!({"err": "Error::NoSuchStore"}), json!({"err": r#"Error::Other("a key is 1 to 256 bytes")"#}));
    for (path, want) in [
        ("/kv?op=open&store=Bad_Name".to_string(), &no_store),
        ("/kv?op=open&store=".to_string(), &no_store),
        (format!("/kv?op=open&store={}", "a".repeat(64)), &no_store),
        (format!("/kv?op=get&store=s&key={}", "k".repeat(257)), &bad_key),
        ("/kv?op=get&store=s&key=".to_string(), &bad_key),
        ("/kv?op=set&store=s&key=&value=1".to_string(), &bad_key),
    ] {
        assert_eq!(&j(&app, &path).await, want, "{}", &path[..path.len().min(60)]);
    }
    assert_eq!(
        j(&app, &format!("/kv?op=get&store={}&key={}", "a".repeat(63), "k".repeat(256))).await,
        json!({"ok": null})
    );
    j(&app, "/kv?op=set&store=s&key=t&value=abc").await;
    assert_eq!(j(&app, "/kv?op=incr&store=s&key=t").await, json!({"err": r#"Error::Other("not a counter")"#}));
}

#[tokio::test(flavor = "multi_thread")]
async fn paging() {
    let app = load(&Default::default(), "kv-p2", &[]);
    let names: Vec<_> = (0..1500).map(|i| format!("k{i:04}")).collect();
    j(&app, &format!("/kv?op=set-many&store=big&value=v&keys={}", names.join(","))).await;
    let first = j(&app, "/kv?op=list&store=big").await["ok"].clone();
    assert_eq!((first["keys"].as_array().unwrap().len(), &first["cursor"]), (1000, &json!("k0999")));
    let second = j(&app, "/kv?op=list&store=big&cursor=k0999").await["ok"].clone();
    assert_eq!(
        (second["keys"].as_array().unwrap().len(), &second["keys"][0], &second["cursor"]),
        (500, &json!("k1000"), &Value::Null)
    );
}

/// A guest cannot make the host copy one cached value into the reply as often as it likes.
#[tokio::test(flavor = "multi_thread")]
async fn get_many_is_capped() {
    let app = load(&Default::default(), "kv-p2", &[]);
    j(&app, &format!("/kv?op=set&store=s&key=k&value={}", "v".repeat(60_000))).await;
    let err = j(&app, &format!("/kv?op=get-many&store=s&keys={}", ["k"; 300].join(","))).await;
    assert!(err["err"].as_str().is_some_and(|e| e.contains("get-many")), "{err}");
}

/// Eight guests on two apps and one store: no increment is lost, whichever way they interleave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_writers_lose_nothing() {
    let store = Arc::default();
    let (a, b) = (load(&store, "kv-p3", &[]), load(&store, "kv-p2", &[]));
    let tasks = [&a, &b].repeat(4).into_iter().cloned().map(|app| {
        tokio::spawn(async move {
            for _ in 0..10 {
                j(&app, "/kv?op=incr&store=s&key=host").await;
            }
            j(&app, "/kv?op=rmw&store=s&key=guest&n=10").await["ok"].as_i64().unwrap() // the retries
        })
    });
    let retries: i64 = futures_util::future::join_all(tasks).await.into_iter().map(Result::unwrap).sum();
    assert_eq!(raw(&store, "kv/app/s/host").await, 80i64.to_le_bytes(), "host counter");
    assert_eq!(raw(&store, "kv/app/s/guest").await, b"80", "guest counter, after {retries} retries");
}

/// Nothing is cached, so each call is one request to the store, and another host sees a write at once.
#[tokio::test(flavor = "multi_thread")]
async fn every_call_is_the_stores() {
    let store = Arc::default();
    let (a, b) = (load(&store, "kv-p3", &[]), load(&store, "kv-p3", &[]));
    j(&a, "/kv?op=set&store=s&key=k&value=1").await;
    assert_eq!(j(&b, "/kv?op=get&store=s&key=k").await["ok"], "1");
    assert_eq!(keys(&b, "/kv?op=list&store=s").await, json!(["k"]));
    assert_eq!(store.take(), ["put kv/app/s/k", "get kv/app/s/k", "list kv/app/s"]);
}

/// The allow list is checked before any lookup, and every address a name resolves to is checked after it.
#[tokio::test(flavor = "multi_thread")]
async fn outbound_is_allow_listed_and_public_only() {
    let store = Arc::default();
    for fixture in ["probe-p2", "probe-p3"] {
        let fetch = |app: App, url: &'static str| async move { get(&app, &format!("/fetch?url={url}")).await.1 };
        let none = load(&store, fixture, &[]);
        let some = load(&store, fixture, &["https://example.com", "https://*.example.org:8443"]);
        for url in
            ["http://example.com/", "https://example.com:444/", "https://example.org:8443/", "https://x.example.net/"]
        {
            assert_eq!(fetch(none.clone(), url).await, "ErrorCode::HttpRequestDenied", "{fixture} {url}");
            assert_eq!(fetch(some.clone(), url).await, "ErrorCode::HttpRequestDenied", "{fixture} {url}");
        }
        // a user name would reach the `Host` header
        assert_eq!(
            fetch(some.clone(), "https://user@example.com/").await,
            "ErrorCode::HttpRequestUriInvalid",
            "{fixture}"
        );
        let any = load(&store, fixture, &["*://*:*"]);
        for url in [
            "http://127.0.0.1/",
            "http://127.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://169.254.169.254/latest",
            "http://10.0.0.1:8080/",
        ] {
            assert_eq!(fetch(any.clone(), url).await, "ErrorCode::DestinationIpProhibited", "{fixture} {url}");
        }
        // an allowed name that resolves, by the hosts file here, to a loopback address
        let named = load(&store, fixture, &["http://localhost:8080"]);
        assert_eq!(fetch(named, "http://localhost:8080/").await, "ErrorCode::DestinationIpProhibited", "{fixture}");
    }
}
