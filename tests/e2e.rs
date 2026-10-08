//! End to end: tric serving the fixtures in tests/fixtures, which tests/components/build.sh builds. Each test runs its
//! own host, at a port of its own: `tric dev`, with its state in memory, and, given a bucket in `TRIC_TEST_STORE` (as
//! `docker compose run --rm test` has, in MinIO), `tric dev` and `tric serve` with their state there.
use bytes::Bytes;
use http::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use wasmtime_wasi_http::io::TokioIo;

const TRIC: &str = env!("CARGO_BIN_EXE_tric");

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

/// The bucket to test against, if there is one.
fn s3() -> Option<String> {
    std::env::var("TRIC_TEST_STORE").ok().filter(|s| !s.is_empty())
}

/// An app name no other test or run has: the bucket outlives a run.
fn fresh(prefix: &str) -> String {
    format!("{prefix}-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

/// The test app, served by `tric dev`, in memory or in `store`.
async fn app(store: Option<&str>) -> Tric {
    match store {
        None => Tric::dev(fixture("app"), &[], None).await,
        Some(store) => {
            let name = fresh("app");
            Tric::dev(manifest(&name, &format!("name = \"{name}\"\ncomponent = APP\n")), &[], Some(store)).await
        }
    }
}

/// Runs the command `args` on the install in `store`, and returns what it prints.
async fn cli(store: &str, args: &[&str]) -> String {
    let out = Command::new(TRIC).args(["--store", store]).args(args).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Waits up to 15 s for `f` to hold, as a change to an app takes up to 5 s to reach a host.
async fn until(what: &str, f: impl AsyncFn() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < Duration::from_secs(15), "{what}");
        sleep(Duration::from_millis(250)).await;
    }
}

/// A host, which prints its log if a test fails.
struct Tric {
    addr: String,
    /// What requests have in `Host`.
    host: String,
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

impl Tric {
    /// `tric dev` of the app at `path`.
    async fn dev(path: PathBuf, env: &[&str], store: Option<&str>) -> Self {
        let mut args = vec!["dev".into(), path.into_os_string(), "--listen".into(), "127.0.0.1:0".into()];
        env.iter().for_each(|e| args.extend(["-e".into(), e.into()]));
        store.iter().for_each(|s| args.extend(["--store".into(), s.into()]));
        Self::start(&args).await
    }

    /// `tric serve` of every app in `store`, at `<app>.localhost`.
    async fn serve(store: &str) -> Self {
        Self::start(&["serve", "--store", store, "--listen", "127.0.0.1:0", "--domain", "localhost"]).await
    }

    async fn start<S: AsRef<OsStr>>(args: &[S]) -> Self {
        let mut cmd = Command::new(TRIC);
        cmd.args(args).env_remove("TRIC_STORE").env_remove("TRIC_DOMAIN").env_remove("TRIC_LISTEN");
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
            panic!("tric exited:\n{}", log.lock().unwrap());
        });
        let addr = started.await.expect("tric did not start");
        let kept = log.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                kept.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        Self { host: addr.clone(), addr, log, _child: child }
    }

    async fn send(&self, method: &str, target: &str, headers: &[(&str, &str)]) -> Res {
        let stream = TcpStream::connect(&self.addr).await.unwrap();
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.unwrap();
        tokio::spawn(conn);
        let mut req = http::Request::builder().method(method).uri(target).header("host", &self.host);
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

    /// Waits up to `limit` for `key` in `name` to hold an echo, which it returns.
    async fn wait_for(&self, name: &str, key: &str, limit: Duration) -> Option<Value> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Value::String(s) = self.value(name, key).await {
                return Some(serde_json::from_str(&s).unwrap());
            }
            sleep(Duration::from_millis(200)).await;
        }
        None
    }
}

/// The value of the first header `name` that an echo reports.
fn header<'a>(echo: &'a Value, name: &str) -> Option<&'a str> {
    echo["headers"].as_array().unwrap().iter().find(|h| h[0] == name).map(|h| h[1].as_str().unwrap())
}

impl Drop for Tric {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("--- tric at {}:\n{}", self.addr, self.log.lock().unwrap());
        }
    }
}

#[tokio::test]
async fn serves_and_forwards() {
    let dev = Tric::dev(fixture("app"), &["GREETING=hi"], None).await;
    let res = dev.get("/").await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    assert_eq!(dev.get("/?status=418").await.status, 418);

    let fwd = [
        ("x-forwarded-for", "9.9.9.9, 1.2.3.4"),
        ("x-forwarded-host", "example.com"),
        ("x-forwarded-proto", "https"),
        ("x-amzn-lambda-context", "{}"),
    ];
    let echo = dev.send("GET", "/echo?a=1", &fwd).await.json();
    assert_eq!(echo["uri"], "https://example.com/echo?a=1");
    assert_eq!(header(&echo, "forwarded"), Some(r#"for=1.2.3.4;host="example.com";proto=https"#));
    let headers = echo["headers"].as_array().unwrap();
    let hop = |h: &Value| ["x-forwarded-", "x-amzn-"].iter().any(|p| h[0].as_str().unwrap().starts_with(p));
    assert!(!headers.iter().any(hop), "{headers:?}");
    // A proto that is neither http nor https counts for nothing; the peer is `for` when there is no X-Forwarded-For.
    let echo = dev.send("GET", "/echo", &[("x-forwarded-proto", "gopher")]).await.json();
    assert_eq!(echo["uri"], format!("http://{}/echo", dev.addr));
    assert_eq!(header(&echo, "forwarded"), Some(&*format!(r#"for=127.0.0.1;host="{}";proto=http"#, dev.addr)));

    let env = dev.get("/env").await.json();
    assert_eq!(env["env"]["GREETING"], "hi");
    assert_eq!(dev.get("/print").await.body, "printed");
}

#[tokio::test]
async fn confines_the_guest() {
    let dev = Tric::dev(fixture("app"), &[], None).await;
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

/// For each scenario, a module of two tests: `in_memory`, and `in_s3`, which passes if there is no bucket to test.
macro_rules! in_memory_and_s3 {
    ($($scenario:ident),*) => {$(
        mod $scenario {
            #[tokio::test]
            async fn in_memory() {
                super::$scenario(super::app(None).await).await
            }

            #[tokio::test]
            async fn in_s3() {
                if let Some(s3) = super::s3() {
                    super::$scenario(super::app(Some(&s3)).await).await
                }
            }
        }
    )*};
}

in_memory_and_s3!(turns_commit_or_discard, turns_on_one_name_serialize, delivers_the_outbox_once_committed);

async fn turns_commit_or_discard(dev: Tric) {
    let set = dev.post("/@a/kv?op=set&key=k&value=1").await;
    assert_eq!((set.status, set.json()), (200, json!({ "ok": null })));
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
    assert_eq!(list["ok"]["keys"], json!(["x", "y", "z"]));
    assert_eq!(dev.get("/@c/kv?op=exists&key=y").await.json()["ok"], true);
    assert_eq!(dev.get("/@c/kv?op=get-many&keys=x,w").await.json()["ok"], json!([["x", "v"], ["w", null]]));
    assert_eq!(dev.post("/@c/kv?op=delete-many&keys=x,y").await.status, 200);
    assert_eq!(dev.get("/@c/kv?op=list").await.json()["ok"]["keys"], json!(["z"]));
    let cas = dev.post("/@c/kv?op=cas&key=z&value=w&between=u").await.json();
    assert_eq!(cas["ok"], json!({ "seen": "v", "swapped": false, "latest": "u" }));
    assert_eq!(dev.post("/@c/kv?op=rmw&key=n&n=3").await.json()["ok"], 0);
    assert_eq!(dev.value("c", "n").await, "3");

    // A value over 1 KiB is an object of its own, which the head names by version in a versioned bucket.
    let big = |c: &str| c.repeat(2000);
    for c in ["x", "y"] {
        assert_eq!(dev.post(&format!("/@d/kv?op=set&key=k&value={}", big(c))).await.status, 200);
        assert_eq!(dev.value("d", "k").await, big(c));
    }
    assert_eq!(dev.post(&format!("/@d/kv?op=set&key=k&value={}&status=500", big("z"))).await.status, 500);
    assert_eq!(dev.value("d", "k").await, big("y"));
    assert_eq!(dev.post("/@d/kv?op=delete&key=k").await.status, 200);
    assert_eq!(dev.value("d", "k").await, Value::Null);

    assert_eq!(dev.get("/@-bad/").await.status, 404, "not a name");
    assert_eq!(dev.get(&format!("/@{}/", "n".repeat(129))).await.status, 404, "too long a name");
}

async fn turns_on_one_name_serialize(dev: Tric) {
    let dev = Arc::new(dev);
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

async fn delivers_the_outbox_once_committed(dev: Tric) {
    let me = format!("http://{}", dev.addr);
    let res = dev.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await;
    assert_eq!((res.status, &*res.body), (200, "202 "));
    let echo = dev.wait_for("g", "echo", Duration::from_secs(10)).await.expect("delivered");
    let key = header(&echo, "idempotency-key").expect("an Idempotency-Key");
    assert!(key.ends_with("/0"), "{key}");
    assert_eq!(header(&echo, "prefer"), None);
    assert_eq!(header(&echo, "forwarded"), Some("for=_tric"));

    // A turn that is discarded sends nothing.
    let res = dev.post(&format!("/@f/fetch?method=POST&async=1&status=500&url={me}/@h/echo")).await;
    assert_eq!(res.status, 500);
    assert_eq!(dev.wait_for("h", "echo", Duration::from_secs(3)).await, None);
    // Nor is there an outbox outside a turn: the request goes, and is answered.
    assert!(dev.get(&format!("/fetch?method=POST&async=1&url={me}/@i/echo")).await.body.starts_with("200 "));
}

#[tokio::test]
async fn fetches_itself() {
    let dev = Tric::dev(fixture("app"), &[], None).await;
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
async fn runs_middleware() {
    let dir = manifest("guarded", "name = \"guarded\"\ncomponent = APP\nmiddleware = [{ path = GUARD }]\n");
    let dev = Tric::dev(dir, &["GUARD_TOKEN=t"], None).await;
    assert_eq!(dev.get("/").await.status, 401);
    assert_eq!(dev.send("GET", "/", &[("authorization", "Bearer x")]).await.status, 401);
    let res = dev.send("GET", "/", &[("authorization", "Bearer t")]).await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    let env = dev.send("GET", "/env", &[("authorization", "Bearer t")]).await.json();
    assert_eq!(env["env"]["GUARD_TOKEN"], "t", "the app and its middleware share one environment");
}

#[tokio::test]
async fn confines_outbound_requests() {
    let dev = Tric::dev(fixture("app"), &[], None).await;
    assert!(dev.get("/fetch?url=http://example.com/").await.body.contains("HttpRequestDenied"));
    let dir = manifest("open", "name = \"open\"\ncomponent = APP\nallowed_outbound_hosts = [\"*://*:*\"]\n");
    let dev = Tric::dev(dir, &[], None).await;
    for url in ["http://127.0.0.2:9/", "http://10.0.0.1/", "http://169.254.169.254/", "http://[::1]:9/"] {
        let body = dev.get(&format!("/fetch?url={url}")).await.body;
        assert!(body.contains("DestinationIpProhibited"), "{url}: {body}");
    }
}

#[tokio::test]
async fn fires_cron() {
    let dir = manifest("cron", "name = \"cron\"\ncomponent = APP\n[cron]\n\"* * * * *\" = \"/@ticks/echo\"\n");
    let dev = Tric::dev(dir, &[], None).await;
    let echo = dev.wait_for("ticks", "echo", Duration::from_secs(75)).await.expect("a tick within a minute");
    assert_eq!(echo["method"], "POST");
    assert_eq!(echo["uri"], "http://cron.localhost/@ticks/echo");
    assert_eq!(header(&echo, "forwarded"), Some("for=_cron"));
}

#[tokio::test]
async fn serves_an_install() {
    let Some(s3) = s3() else { return };
    let name = fresh("site");
    let toml = format!("name = \"{name}\"\ncomponent = APP\n[cron]\n\"* * * * *\" = \"/@ticks/echo\"\n");
    let plain = manifest(&name, &toml);
    let first = cli(&s3, &["deploy", plain.to_str().unwrap()]).await.trim().to_owned();

    let mut tric = Tric::serve(&s3).await;
    assert_eq!(tric.get("/").await.status, 404, "{} is no app's host", tric.host);
    tric.host = "nope.localhost".into();
    assert_eq!(tric.get("/").await.status, 404, "nope has not been deployed");
    tric.host = format!("{name}.localhost");
    assert_eq!(tric.get("/").await.body, "hello");

    // A change reaches a host in at most `FRESH`.
    assert_eq!(cli(&s3, &["env", &name, "GREETING=hi"]).await, "GREETING\n");
    until("the new environment", async || tric.get("/env").await.json()["env"]["GREETING"] == "hi").await;
    let toml = format!("name = \"{name}\"\ncomponent = APP\nmiddleware = [{{ path = GUARD }}]\n");
    let guarded = manifest(&format!("{name}-guarded"), &toml);
    let second = cli(&s3, &["deploy", guarded.to_str().unwrap(), "-e", "GUARD_TOKEN=t"]).await.trim().to_owned();
    until("the guarded release", async || tric.get("/").await.status == 401).await;
    assert_eq!(cli(&s3, &["releases", &name]).await, format!("{second} running\n{first}\n"));
    assert_eq!(cli(&s3, &["env", &name]).await, "GREETING\nGUARD_TOKEN\n");
    cli(&s3, &["release", &name, &first]).await;
    until("the first release again", async || tric.get("/").await.status == 200).await;

    // The outbox and cron, at `<app>.<domain>`.
    let res = tric.post(&format!("/@f/fetch?method=POST&async=1&url=http://{name}.localhost/@g/echo")).await;
    assert_eq!((res.status, &*res.body), (200, "202 "));
    assert!(tric.wait_for("g", "echo", Duration::from_secs(10)).await.is_some(), "delivered");
    let echo = tric.wait_for("ticks", "echo", Duration::from_secs(75)).await.expect("a tick within a minute");
    assert_eq!(echo["uri"], format!("http://{name}.localhost/@ticks/echo"));
}
