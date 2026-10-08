//! End to end: `tric dev` serving the fixtures in tests/fixtures, which tests/components/build.sh builds. Each test runs
//! its own host, at a port of its own, with its state in memory.
use bytes::Bytes;
use http::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use wasmtime_wasi_http::io::TokioIo;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.wasm"))
}

/// A directory with a tric.toml of `toml`, in which `APP` and `GUARD` are the fixtures' paths.
fn manifest(dir: &str, toml: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = |name| format!("{:?}", fixture(name).display().to_string());
    std::fs::write(dir.join("tric.toml"), toml.replace("APP", &path("app")).replace("GUARD", &path("guard"))).unwrap();
    dir
}

/// A `tric dev`, which prints its log if a test fails.
struct Dev {
    addr: String,
    log: Arc<Mutex<String>>,
    _child: Child,
}

struct Res {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

impl Dev {
    async fn start(path: PathBuf, env: &[&str]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tric"));
        cmd.arg("dev").arg(path).args(["--listen", "127.0.0.1:0"]).env_remove("TRIC_STORE");
        env.iter().for_each(|e| _ = cmd.args(["-e", e]));
        let mut child = cmd.stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let log = Arc::new(Mutex::new(String::new()));
        let started = timeout(Duration::from_secs(120), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some((_, addr)) = line.split_once(" at http://") {
                    return addr.to_owned();
                }
                log.lock().unwrap().push_str(&format!("{line}\n"));
            }
            panic!("tric dev exited:\n{}", log.lock().unwrap());
        });
        let addr = started.await.expect("tric dev did not start");
        let kept = log.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                kept.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        Self { addr, log, _child: child }
    }

    async fn send(&self, method: &str, target: &str, headers: &[(&str, &str)]) -> Res {
        let stream = TcpStream::connect(&self.addr).await.unwrap();
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.unwrap();
        tokio::spawn(conn);
        let mut req = http::Request::builder().method(method).uri(target).header("host", &self.addr);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = sender.send_request(req.body(Full::new(Bytes::new())).unwrap()).await.unwrap();
        let (parts, body) = res.into_parts();
        let body = String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned();
        Res { status: parts.status.as_u16(), headers: parts.headers, body }
    }

    async fn get(&self, target: &str) -> Res {
        self.send("GET", target, &[]).await
    }

    async fn post(&self, target: &str) -> Res {
        self.send("POST", target, &[]).await
    }

    /// The value of `key` in the name `name`, from a snapshot.
    async fn value(&self, name: &str, key: &str) -> Value {
        self.get(&format!("/@{name}/kv?op=get&key={key}")).await.json()["ok"].clone()
    }

    /// Waits up to `limit` for `key` in `name` to have a value.
    async fn wait_for(&self, name: &str, key: &str, limit: Duration) -> Option<String> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Value::String(s) = self.value(name, key).await {
                return Some(s);
            }
            sleep(Duration::from_millis(200)).await;
        }
        None
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("--- tric dev at {}:\n{}", self.addr, self.log.lock().unwrap());
        }
    }
}

#[tokio::test]
async fn serves_and_forwards() {
    let dev = Dev::start(fixture("app"), &["GREETING=hi"]).await;
    let res = dev.get("/").await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    assert_eq!(dev.get("/?status=418").await.status, 418);

    let fwd =
        [("x-forwarded-for", "9.9.9.9, 1.2.3.4"), ("x-forwarded-host", "example.com"), ("x-forwarded-proto", "https")];
    let echo = dev.send("GET", "/echo?a=1", &fwd).await.json();
    assert_eq!(echo["uri"], "https://example.com/echo?a=1");
    let headers = echo["headers"].as_array().unwrap();
    let header = |name: &str| headers.iter().find(|h| h[0] == name).map(|h| h[1].as_str().unwrap().to_owned());
    assert_eq!(header("forwarded").as_deref(), Some(r#"for=1.2.3.4;host="example.com";proto=https"#));
    assert!(headers.iter().all(|h| !h[0].as_str().unwrap().starts_with("x-forwarded-")), "{headers:?}");
    // A proto that is neither http nor https counts for nothing; the peer is `for` when there is no X-Forwarded-For.
    let echo = dev.send("GET", "/echo", &[("x-forwarded-proto", "gopher")]).await.json();
    assert_eq!(echo["uri"], format!("http://{}/echo", dev.addr));
    let fwd = echo["headers"].as_array().unwrap().iter().find(|h| h[0] == "forwarded").unwrap()[1].clone();
    assert_eq!(fwd, format!(r#"for=127.0.0.1;host="{}";proto=http"#, dev.addr));

    let env = dev.get("/env").await.json();
    assert_eq!(env["env"]["GREETING"], "hi");
    assert_eq!(dev.get("/print").await.body, "printed");
}

#[tokio::test]
async fn confines_the_guest() {
    let dev = Dev::start(fixture("app"), &[]).await;
    let fs = dev.get("/fs").await.json();
    for (what, outcome) in fs.as_object().unwrap() {
        assert!(outcome.get("err").is_some(), "{what}: {outcome}");
    }
    assert_eq!(dev.get("/hog?mb=10").await.body, "hogged 10 MiB");
    assert_eq!(dev.get("/hog?mb=300").await.status, 500);
    let start = Instant::now();
    assert_eq!(dev.get("/loop").await.status, 500);
    assert!((Duration::from_secs(9)..Duration::from_secs(20)).contains(&start.elapsed()), "{:?}", start.elapsed());
    assert_eq!(dev.get("/").await.body, "hello", "a guest that ran away costs the host nothing more");
}

#[tokio::test]
async fn turns_commit_or_discard() {
    let dev = Dev::start(fixture("app"), &[]).await;
    let set = dev.post("/@a/kv?op=set&key=k&value=1").await;
    assert_eq!((set.status, set.json()), (200, serde_json::json!({ "ok": null })));
    let etag = set.header("etag").expect("a turn's answer has the name's ETag").to_owned();
    let get = dev.get("/@a/kv?op=get&key=k").await;
    assert_eq!(get.json()["ok"], "1");
    assert_eq!(get.header("etag"), Some(&*etag), "a snapshot has its name's ETag");

    // Preconditions, on turns only.
    assert_eq!(dev.send("POST", "/@a/kv?op=set&key=k&value=2", &[("if-match", "\"nope\"")]).await.status, 412);
    assert_eq!(dev.send("POST", "/@a/kv?op=set&key=k&value=2", &[("if-none-match", "*")]).await.status, 412);
    assert_eq!(dev.send("GET", "/@a/kv?op=get&key=k", &[("if-match", "\"nope\"")]).await.status, 200);
    let res = dev.send("PUT", "/@a/kv?op=set&key=k&value=2", &[("if-match", &etag)]).await;
    assert_eq!(res.status, 200);
    assert_ne!(res.header("etag"), Some(&*etag));
    assert_eq!(dev.send("POST", "/@new/kv?op=set&key=k&value=1", &[("if-none-match", "*")]).await.status, 200);

    // A 5xx discards; anything else commits.
    assert_eq!(dev.post("/@a/kv?op=set&key=k&value=3&status=500").await.status, 500);
    assert_eq!(dev.value("a", "k").await, "2");
    assert_eq!(dev.post("/@a/kv?op=set&key=k&value=4&status=409").await.status, 409);
    assert_eq!(dev.value("a", "k").await, "4");

    // Outside a turn on it, a store is read-only.
    for (method, target) in [("GET", "/@a/kv?"), ("POST", "/@b/kv?store=a&"), ("POST", "/kv?store=a&")] {
        let err = dev.send(method, &format!("{target}op=set&key=k&value=5"), &[]).await.json()["err"].clone();
        assert!(err.as_str().is_some_and(|e| e.contains("AccessDenied")), "{method} {target}: {err}");
    }
    assert_eq!(dev.value("a", "k").await, "4");

    // The rest of wasi:keyvalue, in one turn each.
    assert_eq!(dev.post("/@c/kv?op=set-many&keys=x,y,z&value=v").await.status, 200);
    let list = dev.get("/@c/kv?op=list").await.json();
    assert_eq!(list["ok"]["keys"], serde_json::json!(["x", "y", "z"]));
    assert_eq!(dev.get("/@c/kv?op=exists&key=y").await.json()["ok"], true);
    assert_eq!(dev.get("/@c/kv?op=get-many&keys=x,w").await.json()["ok"], serde_json::json!([["x", "v"], ["w", null]]));
    assert_eq!(dev.post("/@c/kv?op=delete-many&keys=x,y").await.status, 200);
    assert_eq!(dev.get("/@c/kv?op=list").await.json()["ok"]["keys"], serde_json::json!(["z"]));
    let cas = dev.post("/@c/kv?op=cas&key=z&value=w&between=u").await.json();
    assert_eq!(cas["ok"], serde_json::json!({ "seen": "v", "swapped": false, "latest": "u" }));
    assert_eq!(dev.post("/@c/kv?op=rmw&key=n&n=3").await.json()["ok"], 0);
    assert_eq!(dev.value("c", "n").await, "3");

    assert_eq!(dev.get("/@-bad/").await.status, 404, "not a name");
    assert_eq!(dev.get(&format!("/@{}/", "n".repeat(129))).await.status, 404, "too long a name");
}

#[tokio::test]
async fn turns_on_one_name_serialize() {
    let dev = Arc::new(Dev::start(fixture("app"), &[]).await);
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let dev = dev.clone();
            tokio::spawn(async move { dev.post("/@counter/kv?op=incr&key=n").await })
        })
        .collect();
    let mut ok = 0;
    for task in tasks {
        let res = task.await.unwrap();
        match res.status {
            200 => ok += 1,
            429 => assert_eq!(res.header("retry-after"), Some("1")),
            s => panic!("{s}: {}", res.body),
        }
    }
    assert!(ok > 0);
    // A counter is 8 bytes, little-endian, so not text: `incr` by 0 reads it.
    let n = dev.post("/@counter/kv?op=incr&key=n&delta=0").await.json()["ok"].as_i64().unwrap();
    assert_eq!(n, ok, "every turn that answered 200 counted once, and no other");

    // A turn holds its name: one that comes while it runs waits, and then is turned away.
    let slow = tokio::spawn({
        let dev = dev.clone();
        async move { dev.post("/@held/kv?op=set&key=k&value=1&sleep=8000").await }
    });
    sleep(Duration::from_millis(1500)).await; // past CLAIM_AFTER
    let busy = dev.post("/@held/kv?op=set&key=k&value=2").await;
    assert_eq!((busy.status, busy.header("retry-after")), (429, Some("1")));
    assert_eq!(slow.await.unwrap().status, 200);
    assert_eq!(dev.value("held", "k").await, "1");
}

#[tokio::test]
async fn fetches_itself() {
    let dev = Dev::start(fixture("app"), &[]).await;
    let me = format!("http://{}", dev.addr);
    assert_eq!(dev.get(&format!("/fetch?url={me}/")).await.body, "200 hello");
    let echo = dev.get(&format!("/fetch?url={me}/echo")).await.body;
    assert!(echo.starts_with("200 ") && echo.contains("for=_tric"), "{echo}");
    // From inside a turn: an unsafe request to its own name would wait for itself, so it is refused; another goes.
    assert_eq!(dev.post(&format!("/@f/fetch?method=POST&url={me}/@f/echo")).await.body, "508 ");
    assert!(dev.post(&format!("/@f/fetch?method=POST&url={me}/@g/echo")).await.body.starts_with("200 "));
    assert!(dev.value("g", "echo").await.as_str().unwrap().contains("for=_tric"));
}

#[tokio::test]
async fn delivers_the_outbox_once_committed() {
    let dev = Dev::start(fixture("app"), &[]).await;
    let me = format!("http://{}", dev.addr);
    let res = dev.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await;
    assert_eq!((res.status, &*res.body), (200, "202 "));
    let echo = dev.wait_for("g", "echo", Duration::from_secs(10)).await.expect("delivered");
    let echo: Value = serde_json::from_str(&echo).unwrap();
    let headers = echo["headers"].as_array().unwrap();
    let key = headers.iter().find(|h| h[0] == "idempotency-key").expect("an Idempotency-Key")[1].as_str().unwrap();
    assert!(key.ends_with("/0"), "{key}");
    assert!(headers.iter().all(|h| h[0] != "prefer"), "{headers:?}");
    assert!(headers.iter().any(|h| h[0] == "forwarded" && h[1] == "for=_tric"), "{headers:?}");

    // A turn that is discarded sends nothing.
    let res = dev.post(&format!("/@f/fetch?method=POST&async=1&status=500&url={me}/@h/echo")).await;
    assert_eq!(res.status, 500);
    assert_eq!(dev.wait_for("h", "echo", Duration::from_secs(3)).await, None);
    // Nor is there an outbox outside a turn: the request goes, and is answered.
    assert!(dev.get(&format!("/fetch?method=POST&async=1&url={me}/@i/echo")).await.body.starts_with("200 "));
}

#[tokio::test]
async fn runs_middleware() {
    let dir = manifest("guarded", "name = \"guarded\"\ncomponent = APP\nmiddleware = [{ path = GUARD }]\n");
    let dev = Dev::start(dir, &["GUARD_TOKEN=t"]).await;
    assert_eq!(dev.get("/").await.status, 401);
    assert_eq!(dev.send("GET", "/", &[("authorization", "Bearer x")]).await.status, 401);
    let res = dev.send("GET", "/", &[("authorization", "Bearer t")]).await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    let env = dev.send("GET", "/env", &[("authorization", "Bearer t")]).await.json();
    assert_eq!(env["env"]["GUARD_TOKEN"], "t", "the app and its middleware share one environment");
}

#[tokio::test]
async fn confines_outbound_requests() {
    let dev = Dev::start(fixture("app"), &[]).await;
    assert!(dev.get("/fetch?url=http://example.com/").await.body.contains("HttpRequestDenied"));
    let dir = manifest("open", "name = \"open\"\ncomponent = APP\nallowed_outbound_hosts = [\"*://*:*\"]\n");
    let dev = Dev::start(dir, &[]).await;
    for url in ["http://127.0.0.2:9/", "http://10.0.0.1/", "http://169.254.169.254/", "http://[::1]:9/"] {
        let body = dev.get(&format!("/fetch?url={url}")).await.body;
        assert!(body.contains("DestinationIpProhibited"), "{url}: {body}");
    }
}

#[tokio::test]
async fn fires_cron() {
    let dir = manifest("cron", "name = \"cron\"\ncomponent = APP\n[cron]\n\"* * * * *\" = \"/@ticks/echo\"\n");
    let dev = Dev::start(dir, &[]).await;
    let echo = dev.wait_for("ticks", "echo", Duration::from_secs(75)).await.expect("a tick within a minute");
    let echo: Value = serde_json::from_str(&echo).unwrap();
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["uri"], "http://cron.localhost/@ticks/echo");
    assert!(echo["headers"].as_array().unwrap().iter().any(|h| h[0] == "forwarded" && h[1] == "for=_cron"), "{echo}");
}
