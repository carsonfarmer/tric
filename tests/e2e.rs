//! End to end: tric serving the fixtures in tests/fixtures, which tests/components/build.sh builds. Each test of an
//! app's semantics runs twice: on `tric dev`, with its state in memory, and on `tric route` in front of `tric serve`,
//! with its state in the compose stack's MinIO, as an app of its own. Each test runs its own processes, at ports of
//! their own.
use bytes::Bytes;
use futures_util::TryStreamExt;
use http::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::service::service_fn;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, path::Path as Key};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use wasmtime_wasi_http::io::TokioIo;

const TRIC: &str = env!("CARGO_BIN_EXE_tric");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.wasm"))
}

/// A name that no other test, nor any earlier run, has; a DNS label, as an app's name must be.
fn unique() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    format!("t{t:x}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// A directory of its own with a tric.toml of `toml`, in which `APP` and `GUARD` are the fixtures' paths, and `DIGEST`
/// the guard's.
fn manifest(toml: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique());
    std::fs::create_dir_all(&dir).unwrap();
    let path = |name| format!("{:?}", fixture(name).display().to_string());
    let digest = format!("sha256:{:x}", Sha256::digest(std::fs::read(fixture("guard")).unwrap()));
    let toml = toml.replace("APP", &path("app")).replace("GUARD", &path("guard")).replace("DIGEST", &digest);
    std::fs::write(dir.join("tric.toml"), toml).unwrap();
    dir
}

/// Ports that were free, all at once.
fn ports<const N: usize>() -> [u16; N] {
    let held = [(); N].map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}

/// The bucket, as its owner, which deploys.
fn owner() -> AmazonS3 {
    let var = |k| std::env::var(k).unwrap_or_else(|_| panic!("{k}: run the tests in compose, which sets it"));
    let s3 = AmazonS3Builder::from_env().with_bucket_name(var("TRIC_BUCKET"));
    s3.with_access_key_id(var("MINIO_ROOT_USER")).with_secret_access_key(var("MINIO_ROOT_PASSWORD")).build().unwrap()
}

/// Deploys the app at `path`, with `args`, as an app of its own, named after its directory, and returns that name.
async fn deploy(path: &Path, args: &[&str]) -> String {
    let dir = if path.is_dir() { path.to_owned() } else { manifest(&format!("component = {:?}\n", path.display())) };
    let mut deploy = Command::new(TRIC);
    deploy.arg("deploy").arg(&dir).args(args);
    let out = deploy.env("AWS_ACCESS_KEY_ID", std::env::var("MINIO_ROOT_USER").unwrap_or_default());
    let out = out.env("AWS_SECRET_ACCESS_KEY", std::env::var("MINIO_ROOT_PASSWORD").unwrap_or_default());
    let out = out.output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    dir.file_name().unwrap().to_str().unwrap().to_owned()
}

/// Deletes every object of `app`, so no later router lists it, or fires its cron.
async fn forget(app: &str) {
    let s3 = owner();
    for prefix in [format!("apps/{app}"), format!("native/{app}")] {
        let keys: Vec<_> =
            s3.list(Some(&Key::from(prefix))).map_ok(|m| m.location).try_collect().await.unwrap_or_default();
        for key in keys {
            _ = s3.delete(&key).await;
        }
    }
}

type Log = Arc<Mutex<String>>;

/// Runs `cmd`, whose log goes to `log` after `tag`, and returns it once it says where it is, with that address.
async fn start(cmd: &mut Command, log: &Log, tag: &str) -> (Child, String) {
    let mut child = cmd.stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let started = timeout(Duration::from_secs(120), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            log.lock().unwrap().push_str(&format!("{tag}{line}\n"));
            if let Some((_, addr)) = line.split_once(" at http://") {
                return addr.to_owned();
            }
        }
        panic!("{tag}exited:\n{}", log.lock().unwrap());
    });
    let addr = started.await.unwrap_or_else(|_| panic!("{tag}did not start:\n{}", log.lock().unwrap()));
    let (log, tag) = (log.clone(), tag.to_owned());
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            log.lock().unwrap().push_str(&format!("{tag}{line}\n"));
        }
    });
    (child, addr)
}

/// `tric serve`, for apps at `<app>.<domain>`, handing events to the outbox at `outbox`; with no storage credentials of
/// its own.
async fn serve(domain: &str, outbox: &str, log: &Log) -> (Child, String) {
    let mut serve = Command::new(TRIC);
    serve.args(["serve", "--listen", "127.0.0.1:0", "--domain", domain, "--outbox", outbox]);
    for var in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "MINIO_ROOT_USER", "MINIO_ROOT_PASSWORD"] {
        serve.env_remove(var);
    }
    start(&mut serve, log, "serve: ").await
}

/// `tric route` at `port`, for apps at `<app>.localhost:<port>`, with its outbox at `outbox`, sending to `serve`.
async fn route(port: u16, outbox: &str, serve: &str, log: &Log) -> (Child, String) {
    let (listen, domain) = (format!("127.0.0.1:{port}"), format!("localhost:{port}"));
    let mut route = Command::new(TRIC);
    route.args(["route", "--listen", &listen, "--outbox-listen", outbox, "--domain", &domain, "--serve", serve]);
    start(&mut route, log, "route: ").await
}

#[derive(Clone, Copy)]
enum Kind {
    Dev,
    Stack,
}

/// tric, as one process or more, which print their logs if a test fails.
struct Tric {
    /// Where to connect, and the `Host` to send.
    addr: String,
    host: String,
    /// The router's outbox, in a stack.
    outbox: String,
    /// The apps to forget when done.
    apps: Vec<String>,
    log: Log,
    _children: Vec<Child>,
}

struct Res {
    /// How long the answer's head took.
    head: Duration,
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

/// Sends `method` `target` with `body` to `addr`, as `host`.
async fn exchange(addr: &str, host: &str, method: &str, target: &str, headers: &[(&str, &str)], body: Bytes) -> Res {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let mut req = http::Request::builder().method(method).uri(target).header("host", host);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let start = Instant::now();
    let res = sender.send_request(req.body(Full::new(body)).unwrap()).await.unwrap();
    let head = start.elapsed();
    let (parts, body) = res.into_parts();
    let body = String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned();
    Res { head, status: parts.status.as_u16(), headers: parts.headers, body }
}

impl Tric {
    /// The app at `path`, with `args` as `tric dev` and `tric deploy` take them: on `tric dev`, or deployed and on a
    /// stack of its own.
    async fn new(kind: Kind, path: &Path, args: &[&str]) -> Self {
        let log = Log::default();
        let Kind::Stack = kind else {
            let mut dev = Command::new(TRIC);
            dev.arg("dev").arg(path).args(["--listen", "127.0.0.1:0"]).args(args);
            let (dev, addr) = start(&mut dev, &log, "").await;
            return Self { host: addr.clone(), addr, outbox: String::new(), apps: vec![], log, _children: vec![dev] };
        };
        let app = deploy(path, args).await;
        let [port, outbox] = ports();
        let (domain, outbox) = (format!("localhost:{port}"), format!("127.0.0.1:{outbox}"));
        let (serve, at) = serve(&domain, &outbox, &log).await;
        let (route, addr) = route(port, &outbox, &at, &log).await;
        let host = format!("{app}.{domain}");
        Self { addr, host, outbox, apps: vec![app], log, _children: vec![serve, route] }
    }

    /// The app fixture, as a file.
    async fn app(kind: Kind) -> Self {
        Self::new(kind, &fixture("app"), &[]).await
    }

    async fn send(&self, method: &str, target: &str, headers: &[(&str, &str)]) -> Res {
        exchange(&self.addr, &self.host, method, target, headers, Bytes::new()).await
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

    /// Waits up to `limit` for the log to have `line`.
    async fn logged(&self, line: &str, limit: Duration) -> bool {
        let start = Instant::now();
        while !self.log.lock().unwrap().contains(line) {
            if start.elapsed() > limit {
                return false;
            }
            sleep(Duration::from_millis(100)).await;
        }
        true
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
        let apps = std::mem::take(&mut self.apps);
        if !apps.is_empty() {
            let forgotten = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async { futures_util::future::join_all(apps.iter().map(|app| forget(app))).await });
            });
            _ = forgotten.join();
        }
    }
}

/// Each test, on `tric dev` and on a stack.
macro_rules! both {
    ($($test:ident),* $(,)?) => {
        mod dev {
            $(#[tokio::test] async fn $test() { super::$test(super::Kind::Dev).await })*
        }
        mod stack {
            $(#[tokio::test] async fn $test() { super::$test(super::Kind::Stack).await })*
        }
    };
}

both!(
    serves_and_forwards,
    confines_the_guest,
    turns_commit_or_discard,
    turns_on_one_name_serialize,
    delivers_the_outbox_once_committed,
    fetches_itself,
    runs_middleware,
    confines_outbound_requests,
    fires_cron,
);

async fn serves_and_forwards(kind: Kind) {
    let tric = Tric::new(kind, &fixture("app"), &["-e", "GREETING=hi"]).await;
    let res = tric.get("/").await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    assert_eq!(tric.get("/?status=418").await.status, 418);
    let start = Instant::now();
    let res = tric.get("/stream?n=3").await;
    assert_eq!(res.body, "0\n1\n2\n");
    assert!(res.head < Duration::from_secs(1) && start.elapsed() > Duration::from_secs(2), "the body streams");

    // Whatever a client says of where it came from, or of whose it is, `Forwarded` is tric's, from the socket.
    let claims = [
        ("forwarded", "for=_cron"),
        ("x-forwarded-for", "9.9.9.9"),
        ("x-forwarded-host", "example.com"),
        ("x-tric-credentials", "{}"),
        ("x-amz-tenant-id", "other"),
    ];
    let echo = tric.send("GET", "/echo?a=1", &claims).await.json();
    assert_eq!(echo["uri"], format!("http://{}/echo?a=1", tric.host));
    let headers = echo["headers"].as_array().unwrap();
    let forwarded: Vec<_> = headers.iter().filter(|h| h[0] == "forwarded").collect();
    assert_eq!(forwarded, [&json!(["forwarded", format!(r#"for=127.0.0.1;host="{}";proto=http"#, tric.host)])]);
    let hop = |h: &&Value| ["x-forwarded-", "x-amz", "x-tric-"].iter().any(|p| h[0].as_str().unwrap().starts_with(p));
    assert!(!headers.iter().any(|h| hop(&h)), "{headers:?}");

    assert_eq!(tric.get("/env").await.json()["env"]["GREETING"], "hi");
    assert_eq!(tric.get("/print").await.body, "printed");
}

async fn confines_the_guest(kind: Kind) {
    let tric = Tric::app(kind).await;
    let fs = tric.get("/fs").await.json();
    for (what, outcome) in fs.as_object().unwrap() {
        assert!(outcome.get("err").is_some(), "{what}: {outcome}");
    }
    assert_eq!(tric.get("/hog?mb=10").await.body, "hogged 10 MiB");
    assert_eq!(tric.get("/hog?mb=300").await.status, 500);
    let start = Instant::now();
    assert_eq!(tric.get("/loop").await.status, 500);
    assert!((Duration::from_secs(9)..Duration::from_secs(20)).contains(&start.elapsed()), "{:?}", start.elapsed());
    assert_eq!(tric.get("/").await.body, "hello", "a guest that ran away costs the host nothing more");
}

async fn turns_commit_or_discard(kind: Kind) {
    let tric = Tric::app(kind).await;
    let set = tric.post("/@a/kv?op=set&key=k&value=1").await;
    assert_eq!((set.status, set.json()), (200, json!({ "ok": null })));
    let etag = set.header("etag").expect("a turn's answer has the name's ETag").to_owned();
    let get = tric.get("/@a/kv?op=get&key=k").await;
    assert_eq!(get.json()["ok"], "1");
    assert_eq!(get.header("etag"), Some(&*etag), "a snapshot has its name's ETag");

    // Preconditions, on turns only.
    assert_eq!(tric.send("POST", "/@a/kv?op=set&key=k&value=2", &[("if-match", "\"nope\"")]).await.status, 412);
    assert_eq!(tric.send("POST", "/@a/kv?op=set&key=k&value=2", &[("if-none-match", "*")]).await.status, 412);
    assert_eq!(tric.send("GET", "/@a/kv?op=get&key=k", &[("if-match", "\"nope\"")]).await.status, 200);
    let res = tric.send("PUT", "/@a/kv?op=set&key=k&value=2", &[("if-match", &etag)]).await;
    assert_eq!(res.status, 200);
    assert_ne!(res.header("etag"), Some(&*etag));
    assert_eq!(tric.send("POST", "/@new/kv?op=set&key=k&value=1", &[("if-none-match", "*")]).await.status, 200);

    // A 5xx discards; anything else commits.
    assert_eq!(tric.post("/@a/kv?op=set&key=k&value=3&status=500").await.status, 500);
    assert_eq!(tric.value("a", "k").await, "2");
    assert_eq!(tric.post("/@a/kv?op=set&key=k&value=4&status=409").await.status, 409);
    assert_eq!(tric.value("a", "k").await, "4");

    // Outside a turn on it, a name is read-only.
    for (method, target) in [("GET", "/@a/kv?"), ("POST", "/@b/kv?store=a&"), ("POST", "/kv?store=a&")] {
        let err = tric.send(method, &format!("{target}op=set&key=k&value=5"), &[]).await.json()["err"].clone();
        assert!(err.as_str().is_some_and(|e| e.contains("AccessDenied")), "{method} {target}: {err}");
    }
    assert_eq!(tric.value("a", "k").await, "4");

    // The rest of wasi:keyvalue, in one turn each.
    assert_eq!(tric.post("/@c/kv?op=set-many&keys=x,y,z&value=v").await.status, 200);
    let list = tric.get("/@c/kv?op=list").await.json();
    assert_eq!(list["ok"]["keys"], json!(["x", "y", "z"]));
    assert_eq!(tric.get("/@c/kv?op=exists&key=y").await.json()["ok"], true);
    assert_eq!(tric.get("/@c/kv?op=get-many&keys=x,w").await.json()["ok"], json!([["x", "v"], ["w", null]]));
    assert_eq!(tric.post("/@c/kv?op=delete-many&keys=x,y").await.status, 200);
    assert_eq!(tric.get("/@c/kv?op=list").await.json()["ok"]["keys"], json!(["z"]));
    let cas = tric.post("/@c/kv?op=cas&key=z&value=w&between=u").await.json();
    assert_eq!(cas["ok"], json!({ "seen": "v", "swapped": false, "latest": "u" }));
    assert_eq!(tric.post("/@c/kv?op=rmw&key=n&n=3").await.json()["ok"], 0);
    assert_eq!(tric.value("c", "n").await, "3");

    // A value too big to inline is a value of its own, which a discarded turn leaves as it was.
    let big = |c: &str| c.repeat(2000);
    for c in ["x", "y"] {
        assert_eq!(tric.post(&format!("/@d/kv?op=set&key=k&value={}", big(c))).await.status, 200);
        assert_eq!(tric.value("d", "k").await, big(c));
    }
    assert_eq!(tric.post(&format!("/@d/kv?op=set&key=k&value={}&status=500", big("z"))).await.status, 500);
    assert_eq!(tric.value("d", "k").await, big("y"));
    assert_eq!(tric.post("/@d/kv?op=delete&key=k").await.status, 200);
    assert_eq!(tric.value("d", "k").await, Value::Null);

    assert_eq!(tric.get("/@-bad/").await.status, 404, "not a name");
    assert_eq!(tric.get(&format!("/@{}/", "n".repeat(129))).await.status, 404, "too long a name");
}

async fn turns_on_one_name_serialize(kind: Kind) {
    let tric = Arc::new(Tric::app(kind).await);
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let tric = tric.clone();
            tokio::spawn(async move { tric.post("/@counter/kv?op=incr&key=n").await })
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
    let n = tric.post("/@counter/kv?op=incr&key=n&delta=0").await.json()["ok"].as_i64().unwrap();
    assert_eq!(n, ok, "every turn that answered 200 counted once, and no other");

    // A turn that has run long enough claims its name: one that comes while it runs waits, and then is turned away.
    let slow = tokio::spawn({
        let tric = tric.clone();
        async move { tric.post("/@held/kv?op=set&key=k&value=1&sleep=8000").await }
    });
    sleep(Duration::from_millis(1500)).await; // past CLAIM_AFTER
    let busy = tric.post("/@held/kv?op=set&key=k&value=2").await;
    assert_eq!((busy.status, busy.header("retry-after")), (429, Some("1")));
    assert_eq!(slow.await.unwrap().status, 200);
    assert_eq!(tric.value("held", "k").await, "1");
}

async fn delivers_the_outbox_once_committed(kind: Kind) {
    let tric = Tric::app(kind).await;
    let me = format!("http://{}", tric.host);
    let res = tric.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await;
    assert_eq!((res.status, &*res.body), (200, "202 "));
    let echo = tric.wait_for("g", "echo", Duration::from_secs(10)).await.expect("delivered");
    let key = header(&echo, "idempotency-key").expect("an Idempotency-Key");
    assert!(key.ends_with("/0"), "{key}");
    assert_eq!(header(&echo, "prefer"), None);
    assert_eq!(header(&echo, "forwarded"), Some("for=_tric"));

    // A turn that is discarded sends nothing.
    let res = tric.post(&format!("/@f/fetch?method=POST&async=1&status=500&url={me}/@h/echo")).await;
    assert_eq!(res.status, 500);
    assert_eq!(tric.wait_for("h", "echo", Duration::from_secs(3)).await, None);
    // Without a turn there is no commit to hold a request for: it goes at once, and is answered.
    assert!(tric.get(&format!("/fetch?method=POST&async=1&url={me}/@i/echo")).await.body.starts_with("200 "));
}

async fn fetches_itself(kind: Kind) {
    let tric = Tric::app(kind).await;
    let me = format!("http://{}", tric.host);
    assert_eq!(tric.get(&format!("/fetch?url={me}/")).await.body, "200 hello");
    let echo = tric.get(&format!("/fetch?url={me}/echo")).await.body;
    assert!(echo.starts_with("200 ") && echo.contains("for=_tric"), "{echo}");
    // From inside a turn: an unsafe request to its own name would wait for itself, so it is refused; another goes.
    assert_eq!(tric.post(&format!("/@f/fetch?method=POST&url={me}/@f/echo")).await.body, "508 ");
    assert!(tric.post(&format!("/@f/fetch?method=POST&url={me}/@g/echo")).await.body.starts_with("200 "));
    assert!(tric.value("g", "echo").await.as_str().unwrap().contains("for=_tric"));
}

async fn runs_middleware(kind: Kind) {
    let dir = manifest("component = APP\nmiddleware = [{ url = GUARD, digest = \"DIGEST\" }]\n");
    let tric = Tric::new(kind, &dir, &["-e", "GUARD_TOKEN=t"]).await;
    assert_eq!(tric.get("/").await.status, 401);
    assert_eq!(tric.send("GET", "/", &[("authorization", "Bearer x")]).await.status, 401);
    let res = tric.send("GET", "/", &[("authorization", "Bearer t")]).await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    let env = tric.send("GET", "/env", &[("authorization", "Bearer t")]).await.json();
    assert_eq!(env["env"]["GUARD_TOKEN"], "t", "the app and its middleware share one environment");
}

#[tokio::test]
async fn refuses_forged_middleware() {
    let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let dir = manifest(&format!("component = APP\nmiddleware = [{{ url = GUARD, digest = \"{digest}\" }}]\n"));
    let out = Command::new(TRIC).arg("dev").arg(&dir).output().await.unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("does not match its digest"), "{err}");
}

async fn confines_outbound_requests(kind: Kind) {
    let tric = Tric::app(kind).await;
    assert!(tric.get("/fetch?url=http://example.com/").await.body.contains("HttpRequestDenied"));
    let tric = Tric::new(kind, &fixture("app"), &["--allow", "*://*:*"]).await;
    for url in ["http://127.0.0.2:9/", "http://10.0.0.1/", "http://169.254.169.254/", "http://[::1]:9/"] {
        let body = tric.get(&format!("/fetch?url={url}")).await.body;
        assert!(body.contains("DestinationIpProhibited"), "{url}: {body}");
    }
}

async fn fires_cron(kind: Kind) {
    let tric = Tric::new(kind, &manifest("component = APP\n[cron]\n\"* * * * *\" = \"/@ticks/echo\"\n"), &[]).await;
    let echo = tric.wait_for("ticks", "echo", Duration::from_secs(75)).await.expect("a tick within a minute");
    assert_eq!(echo["method"], "POST");
    // On a stack, any router on the bucket may fire it: each test's does, at a port of its own.
    let uri = echo["uri"].as_str().unwrap();
    assert!(uri.starts_with(&format!("http://{}", tric.host.split(':').next().unwrap())), "{uri}");
    assert!(uri.ends_with("/@ticks/echo"), "{uri}");
    assert_eq!(header(&echo, "forwarded"), Some("for=_cron"));
}

/// A stand-in for serve, which answers 204 to every request, and keeps their headers.
async fn stand_in() -> (String, Arc<Mutex<Vec<HeaderMap>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::<Mutex<Vec<HeaderMap>>>::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let kept = kept.clone();
            let svc = service_fn(move |req: hyper::Request<Incoming>| {
                kept.lock().unwrap().push(req.headers().clone());
                let res = hyper::Response::builder().status(204).body(Full::new(Bytes::new()));
                async { Ok::<_, Infallible>(res.unwrap()) }
            });
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), svc));
        }
    });
    (addr, seen)
}

/// The router sends serve only requests for apps that have a release, each with credentials that reach that app's
/// objects only; and serve runs a request only for the app its tenant id names, with credentials that read that app.
/// Acceptance criteria 1, 3, 4 and 7.
#[tokio::test]
async fn isolates_apps() {
    let s3 = owner();
    let (a, b) = (deploy(&fixture("app"), &[]).await, deploy(&fixture("app"), &[]).await);
    let key = |s: String| Key::from(s);
    for k in ["names/n", "values/v"] {
        s3.put(&key(format!("apps/{b}/{k}")), PutPayload::from_static(b"{}")).await.unwrap();
    }
    s3.put(&key(format!("native/{b}/x/y")), PutPayload::from_static(b"{}")).await.unwrap();

    // The router, in front of a stand-in for serve.
    let (fake, seen) = stand_in().await;
    let log = Log::default();
    let [port, outbox] = ports();
    let (route, addr) = route(port, &format!("127.0.0.1:{outbox}"), &fake, &log).await;
    let mut tric = Tric {
        addr,
        host: format!("{a}.localhost:{port}"),
        outbox: String::new(),
        apps: vec![a.clone(), b.clone()],
        log: log.clone(),
        _children: vec![route],
    };
    let claims = [("x-tric-credentials", r#"{"AccessKeyId":"forged"}"#), ("x-amz-tenant-id", b.as_str())];
    assert_eq!(tric.send("GET", "/", &claims).await.status, 204);
    let sent = seen.lock().unwrap().pop().expect("the router called serve");
    let one = |name: &str| {
        let all: Vec<_> = sent.get_all(name).iter().collect();
        assert_eq!(all.len(), 1, "{name}: {all:?}");
        all[0].to_str().unwrap().to_owned()
    };
    assert_eq!(one("x-amz-tenant-id"), a);
    assert_eq!(one("forwarded"), format!(r#"for=127.0.0.1;host="{a}.localhost:{port}";proto=http"#));
    let creds = one("x-tric-credentials");
    let c: Value = serde_json::from_str(&creds).unwrap();
    assert_eq!(c["Version"], 1);

    // 4: an app with no release, or a host that is not one, never reaches serve.
    for host in [format!("{}.localhost:{port}", unique()), format!("{a}.elsewhere:{port}"), format!("localhost:{port}")]
    {
        tric.host = host;
        assert_eq!(tric.get("/").await.status, 404, "{}", tric.host);
    }
    assert!(seen.lock().unwrap().is_empty());

    // 1: a's credentials reach a's names, values and native code, read the rest of a, and nothing of b.
    let as_a = AmazonS3Builder::from_env().with_bucket_name(std::env::var("TRIC_BUCKET").unwrap());
    let as_a = as_a.with_access_key_id(c["AccessKeyId"].as_str().unwrap());
    let as_a = as_a.with_secret_access_key(c["SecretAccessKey"].as_str().unwrap());
    let as_a = as_a.with_token(c["SessionToken"].as_str().unwrap()).build().unwrap();
    let denied = |r: object_store::Result<()>| matches!(r, Err(object_store::Error::PermissionDenied { .. }));
    let put = |k: String| {
        let as_a = &as_a;
        async move { as_a.put(&key(k), PutPayload::from_static(b"{}")).await.map(|_| ()) }
    };
    let get = |k: String| {
        let as_a = &as_a;
        async move { as_a.get(&key(k)).await.map(|_| ()) }
    };
    for k in [format!("apps/{b}/current"), format!("apps/{b}/names/n"), format!("apps/{b}/values/v")] {
        assert!(denied(get(k.clone()).await), "read {k}");
        assert!(denied(put(k.clone()).await), "write {k}");
    }
    assert!(denied(get(format!("native/{b}/x/y")).await) && denied(put(format!("native/{b}/x/y")).await));
    get(format!("apps/{a}/current")).await.unwrap();
    for k in [format!("apps/{a}/current"), format!("apps/{a}/components/x")] {
        assert!(denied(put(k.clone()).await), "write {k}");
    }
    for k in [format!("apps/{a}/names/n"), format!("apps/{a}/values/v"), format!("native/{a}/x/y")] {
        put(k.clone()).await.unwrap_or_else(|e| panic!("write {k}: {e}"));
        get(k.clone()).await.unwrap_or_else(|e| panic!("read {k}: {e}"));
        as_a.delete(&key(k.clone())).await.unwrap_or_else(|e| panic!("delete {k}: {e}"));
    }

    // 3: serve, with no credentials of its own, runs a request only as the tenant its host names, with credentials.
    let domain = format!("localhost:{port}");
    let (serve, at) = serve(&domain, "127.0.0.1:1", &log).await;
    tric._children.push(serve);
    tric.addr = at;
    tric.host = format!("{a}.{domain}");
    let from = ("forwarded", "for=127.0.0.1");
    for headers in [vec![from], vec![from, ("x-amz-tenant-id", &a)], vec![from, ("x-amz-tenant-id", &b)]] {
        assert_eq!(tric.send("GET", "/", &headers).await.status, 403, "{headers:?}");
    }
    let wrong = [from, ("x-amz-tenant-id", &b), ("x-tric-credentials", &creds)];
    assert_eq!(tric.send("GET", "/", &wrong).await.status, 403, "a's credentials, as b");
    let right = [from, ("x-amz-tenant-id", &a), ("x-tric-credentials", &creds)];
    assert_eq!(tric.send("GET", "/", &right).await.body, "hello");
    // a's credentials, at b's host and as b's tenant, cannot read b's release.
    tric.host = format!("{b}.{domain}");
    let as_b = [from, ("x-amz-tenant-id", &b), ("x-tric-credentials", &creds)];
    assert_eq!(tric.send("GET", "/", &as_b).await.status, 404);

    // 7: serve wrote a's native code, which only a's credentials could have.
    let listed = |app: &str| s3.list(Some(&key(format!("native/{app}")))).map_ok(|m| m.location.to_string());
    let sha = format!("{:x}", Sha256::digest(std::fs::read(fixture("app")).unwrap()));
    let built: Vec<_> = listed(&a).try_collect().await.unwrap();
    assert!(matches!(&built[..], [k] if k.ends_with(&sha)), "{built:?}");
    assert_eq!(listed(&b).try_collect::<Vec<_>>().await.unwrap(), [format!("native/{b}/x/y")]);
}

/// An event that names an app, but whose commit is not pending in that app's head with the event's digest, is dropped:
/// so neither an app, nor anyone who learns a commit id, can send requests as another. Acceptance criterion 5.
#[tokio::test]
async fn drops_forged_events() {
    let tric = Tric::app(Kind::Stack).await;
    let me = format!("http://{}", tric.host);
    assert_eq!(tric.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await.body, "202 ");
    let echo = tric.wait_for("g", "echo", Duration::from_secs(10)).await.expect("delivered");
    let key = header(&echo, "idempotency-key").expect("an Idempotency-Key");
    let (commit, _) = key.rsplit_once('/').unwrap();

    // The commit the receiver learnt, with a request of the forger's choosing, handed to the router's outbox.
    let event = json!({
        "app": tric.apps[0],
        "host": tric.host,
        "name": "f",
        "base": "\"forged\"",
        "commit": commit,
        "requests": [{ "method": "POST", "uri": format!("{me}/@h/echo"), "headers": [], "body": "" }],
    });
    let body = Bytes::from(event.to_string());
    assert_eq!(exchange(&tric.outbox, &tric.outbox, "POST", "/", &[], body).await.status, 202);
    let line = "outbox: dropped an event, as its commit is not pending";
    assert!(tric.logged(line, Duration::from_secs(10)).await, "dropped");
    assert_eq!(tric.value("h", "echo").await, Value::Null);
}
