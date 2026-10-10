//! End to end: tric serving the fixtures in tests/fixtures, which tests/components/build.sh builds. Each test of an
//! app's semantics runs four times: on `tric dev`, with its state in memory, and on `tric route` in front of
//! `tric serve`, with its state in the compose stack's RustFS, as an app of its own; each with the Rust app and with
//! the JavaScript one. Each test runs its own processes, at ports of their own.
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

/// A directory of its own with a tric.toml of `toml`, in which `APP` is the path `app`, `GUARD` the guard fixture's and
/// `DIGEST` its digest.
fn manifest(app: &Path, toml: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique());
    std::fs::create_dir_all(&dir).unwrap();
    let path = |path: &Path| format!("{:?}", path.display().to_string());
    let digest = format!("sha256:{:x}", Sha256::digest(std::fs::read(fixture("guard")).unwrap()));
    let toml = toml.replace("APP", &path(app)).replace("GUARD", &path(&fixture("guard"))).replace("DIGEST", &digest);
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
    s3.with_access_key_id(var("RUSTFS_ACCESS_KEY")).with_secret_access_key(var("RUSTFS_SECRET_KEY")).build().unwrap()
}

/// Deploys the app at `path`, with `args`, as an app of its own, named after its directory, and returns that name.
async fn deploy(path: &Path, args: &[&str]) -> String {
    let dir = if path.is_dir() { path.to_owned() } else { manifest(path, "component = APP\n") };
    let mut deploy = Command::new(TRIC);
    deploy.arg("deploy").arg(&dir).args(args);
    let out = deploy.env("AWS_ACCESS_KEY_ID", std::env::var("RUSTFS_ACCESS_KEY").unwrap_or_default());
    let out = out.env("AWS_SECRET_ACCESS_KEY", std::env::var("RUSTFS_SECRET_KEY").unwrap_or_default());
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
    for var in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "RUSTFS_ACCESS_KEY", "RUSTFS_SECRET_KEY"] {
        serve.env_remove(var);
    }
    start(&mut serve, log, "serve: ").await
}

/// `tric route` at `port`, for apps at `<app>.localhost:<port>`, with its outbox at `outbox`, sending to `serve`, and
/// `env` in its environment.
async fn route(port: u16, outbox: &str, serve: &str, env: &[(&str, &str)], log: &Log) -> (Child, String) {
    let (listen, domain) = (format!("127.0.0.1:{port}"), format!("localhost:{port}"));
    let mut route = Command::new(TRIC);
    route.args(["route", "--listen", &listen, "--outbox-listen", outbox, "--domain", &domain, "--serve", serve]);
    start(route.envs(env.iter().copied()), log, "route: ").await
}

/// Where an app runs, and which app it is: the fixture `app`, or `js`.
#[derive(Clone, Copy)]
struct Kind {
    stack: bool,
    app: &'static str,
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
    /// The app at `path`, with `args` as `tric dev` and `tric deploy` take them: on `tric dev`, or on a stack.
    async fn new(kind: Kind, path: &Path, args: &[&str]) -> Self {
        if kind.stack {
            return Self::stack(path, args, &[]).await;
        }
        let log = Log::default();
        let mut dev = Command::new(TRIC);
        dev.arg("dev").arg(path).args(["--listen", "127.0.0.1:0"]).args(args);
        let (dev, addr) = start(&mut dev, &log, "").await;
        Self { host: addr.clone(), addr, outbox: String::new(), apps: vec![], log, _children: vec![dev] }
    }

    /// The app at `path`, deployed with `args`, on a stack of its own whose router has `env` in its environment.
    async fn stack(path: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let (app, log, [port, outbox]) = (deploy(path, args).await, Log::default(), ports());
        let (domain, outbox) = (format!("localhost:{port}"), format!("127.0.0.1:{outbox}"));
        let (serve, at) = serve(&domain, &outbox, &log).await;
        let (route, addr) = route(port, &outbox, &at, env, &log).await;
        let host = format!("{app}.{domain}");
        Self { addr, host, outbox, apps: vec![app], log, _children: vec![serve, route] }
    }

    /// The app fixture, as a file.
    async fn app(kind: Kind) -> Self {
        Self::new(kind, &fixture(kind.app), &[]).await
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
        until(limit, async || self.value(name, key).await.as_str().map(|s| serde_json::from_str(s).unwrap())).await
    }

    /// Waits up to `limit` for the log to have `line`.
    async fn logged(&self, line: &str, limit: Duration) -> bool {
        until(limit, async || self.log.lock().unwrap().contains(line).then_some(())).await.is_some()
    }
}

/// What `f` gives, once it gives something, asked every 100 ms for up to `limit`.
async fn until<T>(limit: Duration, mut f: impl AsyncFnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(t) = f().await {
            return Some(t);
        }
        sleep(Duration::from_millis(100)).await;
    }
    None
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

/// Each test, on `tric dev` and on a stack, with the Rust app and with the JavaScript one.
macro_rules! both {
    (@in $mod:ident $stack:literal $app:literal $($test:ident)*) => {
        mod $mod {
            $(#[tokio::test] async fn $test() { super::$test(super::Kind { stack: $stack, app: $app }).await })*
        }
    };
    ($($test:ident),* $(,)?) => {
        both!(@in dev false "app" $($test)*);
        both!(@in stack true "app" $($test)*);
        both!(@in js_dev false "js" $($test)*);
        both!(@in js_stack true "js" $($test)*);
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

// The files are the Rust app's: the JavaScript one has no routes for them.
both!(@in files_dev false "app"
    files_commit_or_discard files_tree files_big files_many files_race files_outlive_the_turn);
both!(@in files_stack true "app"
    files_commit_or_discard files_tree files_big files_many files_race files_outlive_the_turn);

async fn serves_and_forwards(kind: Kind) {
    let tric = Tric::new(kind, &fixture(kind.app), &["-e", "GREETING=hi"]).await;
    let res = tric.get("/").await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    assert_eq!(tric.get("/?status=418").await.status, 418);
    let start = Instant::now();
    let res = tric.get("/stream?n=3").await;
    assert_eq!(res.body, "0\n1\n2\n");
    assert!(res.head < Duration::from_secs(1) && start.elapsed() > Duration::from_secs(2), "the body streams");

    // Whatever a client says of where it came from, of whose it is, or of being a WebSocket's event, with the feature
    // `ws` or without it, `Forwarded` is tric's, from the socket.
    let claims = [
        ("forwarded", "for=_cron"),
        ("x-forwarded-for", "9.9.9.9"),
        ("x_forwarded_for", "9.9.9.9"),
        ("x-forwarded-host", "example.com"),
        ("x-tric-credentials", "{}"),
        ("x-amz-tenant-id", "other"),
        ("connection-id", "c"),
        ("connection_id", "c"),
        ("grip-hold", "stream"),
        ("meta-user", "u"),
        ("content-type", "application/websocket-events"),
    ];
    let echo = tric.send("GET", "/echo?a=1", &claims).await.json();
    assert_eq!(echo["uri"], format!("http://{}/echo?a=1", tric.host));
    let headers = echo["headers"].as_array().unwrap();
    let forwarded: Vec<_> = headers.iter().filter(|h| h[0] == "forwarded").collect();
    assert_eq!(forwarded, [&json!(["forwarded", format!(r#"for=127.0.0.1;host="{}";proto=http"#, tric.host)])]);
    let ours = ["x-forwarded-", "x-amz", "x-tric-", "connection-id", "grip-", "meta-", "content-type"];
    let hop = |h: &&Value| ours.iter().any(|p| h[0].as_str().unwrap().replace('_', "-").starts_with(p));
    assert!(!headers.iter().any(|h| hop(&h)), "{headers:?}");

    assert_eq!(tric.get("/env").await.json()["env"]["GREETING"], "hi");
    if kind.app == "app" {
        assert_eq!(tric.get("/print").await.body, "printed"); // only Rust has stdio
    }
}

async fn confines_the_guest(kind: Kind) {
    let tric = Tric::app(kind).await;
    if kind.app == "app" {
        // only Rust has files to try
        let fs = tric.get("/fs").await.json();
        for (what, outcome) in fs.as_object().unwrap() {
            assert!(outcome.get("err").is_some(), "{what}: {outcome}");
        }
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
    let big = |c: &str| c.repeat(5000);
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

impl Tric {
    /// What `/files?query` of `name` answers, as `{"ok": ..}` or `{"err": ..}`. A POST is a turn, and a GET reads.
    async fn files(&self, method: &str, name: &str, query: &str) -> Value {
        self.send(method, &format!("/@{name}/files?{query}"), &[]).await.json()
    }

    /// Whether `/files?query` of `name` fails with an error of the kind `kind`.
    async fn fails(&self, method: &str, name: &str, query: &str, kind: &str) -> bool {
        self.files(method, name, query).await["err"].as_str().is_some_and(|e| e.starts_with(kind))
    }
}

/// The bytes `big` of the fixture writes: byte `i` is `i % 251`.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// FNV-1a of `bytes`, as the fixture's `sum` has it.
fn fnv(bytes: &[u8]) -> String {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)).to_string()
}

async fn files_commit_or_discard(kind: Kind) {
    let tric = Tric::app(kind).await;
    let ok = json!({ "ok": null });
    // A turn's files commit with its keys: whole, or not at all.
    assert_eq!(tric.files("POST", "a", "op=write&path=/hello.txt&data=hi&key=k&value=v").await, ok);
    assert_eq!(tric.files("GET", "a", "op=read&path=/hello.txt").await, json!({ "ok": "hi" }));
    assert_eq!(tric.value("a", "k").await, "v");
    let write = |data: &str, status: u16| {
        format!("/@a/files?op=write&path=/hello.txt&data={data}&key=k&value={data}&status={status}")
    };
    assert_eq!(tric.post(&write("gone", 500)).await.status, 500);
    assert_eq!(tric.files("GET", "a", "op=read&path=/hello.txt").await, json!({ "ok": "hi" }));
    assert_eq!(tric.value("a", "k").await, "v");
    assert_eq!(tric.post(&write("kept", 409)).await.status, 409);
    assert_eq!(tric.files("GET", "a", "op=read&path=/hello.txt").await, json!({ "ok": "kept" }));
    assert_eq!(tric.value("a", "k").await, "kept");

    // Outside a turn on it, a name's files are read-only. They are the name's: no other has them, nor has a request
    // that is on none.
    assert!(tric.fails("GET", "a", "op=write&path=/x&data=1", "ReadOnlyFilesystem").await);
    assert!(tric.fails("GET", "a", "op=mkdir&path=/x", "ReadOnlyFilesystem").await);
    assert!(tric.fails("GET", "a", "op=rm&path=/hello.txt", "ReadOnlyFilesystem").await);
    assert_eq!(tric.files("GET", "a", "op=read&path=/hello.txt").await, json!({ "ok": "kept" }));
    assert!(tric.fails("GET", "b", "op=read&path=/hello.txt", "NotFound").await);
    let none = tric.get("/files?op=read&path=/hello.txt").await.json();
    assert!(none.get("err").is_some(), "{none}");
    assert!(tric.files("POST", "b", "op=read&path=/hello.txt").await.get("err").is_some());
}

async fn files_tree(kind: Kind) {
    let tric = Tric::app(kind).await;
    let ok = json!({ "ok": null });
    for q in [
        "op=mkdirs&path=/d/e",
        "op=write&path=/d/e/f&data=1",
        "op=ln&path=/d/e/f&to=/g",
        "op=symlink&path=/s&to=d/e",
        "op=mv&path=/d/e/f&to=/d/h",
    ] {
        assert_eq!(tric.files("POST", "t", q).await, ok, "{q}");
    }
    let stat = |kind: &str, len: u64| json!({ "ok": { "type": kind, "len": len } });
    assert_eq!(tric.files("GET", "t", "op=stat&path=/g").await, stat("file", 1));
    assert_eq!(tric.files("GET", "t", "op=stat&path=/d").await["ok"]["type"], "dir");
    assert_eq!(tric.files("GET", "t", "op=stat&path=/s").await["ok"]["type"], "symlink");
    assert_eq!(tric.files("GET", "t", "op=readlink&path=/s").await, json!({ "ok": "d/e" }));
    assert_eq!(tric.files("GET", "t", "op=ls&path=/s").await["ok"]["count"], 0, "a symlink to a directory is one");
    assert!(tric.fails("GET", "t", "op=read&path=/d/e/f", "NotFound").await, "moved");

    // A hard link is the file itself.
    assert_eq!(tric.files("POST", "t", "op=write&path=/d/h&data=22").await, ok);
    assert_eq!(tric.files("GET", "t", "op=read&path=/g").await, json!({ "ok": "22" }));
    assert!(tric.fails("POST", "t", "op=rmdir&path=/d", "DirectoryNotEmpty").await);
    assert!(tric.fails("POST", "t", "op=mkdir&path=/d", "AlreadyExists").await);
    assert_eq!(tric.files("POST", "t", "op=rmtree&path=/d").await, ok);
    assert_eq!(tric.files("GET", "t", "op=read&path=/g").await, json!({ "ok": "22" }), "its other name");
    assert_eq!(tric.files("GET", "t", "op=ls&path=/").await["ok"], json!({ "count": 2, "first": "g", "last": "s" }));

    // A rename that fails leaves nothing of its own, and a turn that fails nothing at all.
    assert!(tric.fails("POST", "t", "op=mv&path=/nope&to=/g", "NotFound").await);
    assert_eq!(tric.files("GET", "t", "op=read&path=/g").await, json!({ "ok": "22" }));
    assert_eq!(tric.post("/@t/files?op=mkdirs&path=/x/y/z&status=500").await.status, 500);
    assert!(tric.fails("GET", "t", "op=stat&path=/x", "NotFound").await);

    // Paths lead down from the root and nowhere else: not up out of it, nor along a link out of it.
    assert!(tric.fails("POST", "t", "op=symlink&path=/abs&to=/etc/passwd", "PermissionDenied").await);
    assert_eq!(tric.files("POST", "t", "op=symlink&path=/up&to=../..").await, ok);
    for q in
        ["op=ls&path=/up", "op=read&path=/up/etc/passwd", "op=read&path=/../etc/passwd", "op=read&path=/etc/passwd"]
    {
        let out = tric.files("GET", "t", q).await;
        assert!(out.get("err").is_some(), "{q}: {out}");
    }
}

async fn files_big(kind: Kind) {
    let tric = Tric::app(kind).await;
    let ok = json!({ "ok": null });
    let sum = async |name: &str| tric.files("GET", name, "op=sum&path=/big").await["ok"].clone();
    // A file of blocks, and a few bytes in the middle of it, in place.
    let mut want = pattern(1_000_000);
    assert_eq!(tric.files("POST", "a", "op=big&path=/big&n=1000000").await, json!({ "ok": 1_000_000 }));
    assert_eq!(sum("a").await, json!({ "len": 1_000_000, "fnv": fnv(&want) }));
    assert_eq!(tric.files("POST", "a", "op=patch&path=/big&at=262142&data=XYZW").await, ok);
    want[262142..262146].copy_from_slice(b"XYZW");
    assert_eq!(sum("a").await, json!({ "len": 1_000_000, "fnv": fnv(&want) }), "across the end of a block");
    let slice = tric.files("GET", "a", "op=slice&path=/big&at=262140&n=8").await;
    assert_eq!(slice["ok"], json!(want[262140..262148]));
    // A turn that is discarded leaves it as it was, and one that cuts it short, or makes it longer, makes zeros.
    assert_eq!(tric.post("/@a/files?op=patch&path=/big&at=5&data=!!&status=500").await.status, 500);
    assert_eq!(sum("a").await, json!({ "len": 1_000_000, "fnv": fnv(&want) }));
    assert_eq!(tric.files("POST", "a", "op=trunc&path=/big&n=300000").await, ok);
    want.truncate(300_000);
    assert_eq!(sum("a").await, json!({ "len": 300_000, "fnv": fnv(&want) }));
    assert_eq!(tric.files("POST", "a", "op=trunc&path=/big&n=700000").await, ok);
    want.resize(700_000, 0);
    assert_eq!(sum("a").await, json!({ "len": 700_000, "fnv": fnv(&want) }));

    // More than a turn holds in memory is sent as it goes, and is the same file once it is done; and a turn that is
    // discarded leaves nothing of what was sent.
    let n = 12 << 20;
    assert_eq!(tric.files("POST", "b", &format!("op=big&path=/big&n={n}&chunk=16384")).await, json!({ "ok": n }));
    assert_eq!(sum("b").await, json!({ "len": n, "fnv": fnv(&pattern(n)) }));
    let spilled = tric.post(&format!("/@c/files?op=big&path=/big&n={n}&chunk=65536&status=500")).await;
    assert_eq!(spilled.status, 500);
    assert!(tric.fails("GET", "c", "op=stat&path=/big", "NotFound").await);
}

async fn files_many(kind: Kind) {
    let tric = Tric::app(kind).await;
    assert_eq!(tric.files("POST", "m", "op=many&path=/dir&n=3000").await, json!({ "ok": 3000 }));
    let ls = tric.files("GET", "m", "op=ls&path=/dir").await;
    assert_eq!(ls["ok"], json!({ "count": 3000, "first": "f00000", "last": "f02999" }));
    assert_eq!(tric.files("GET", "m", "op=read&path=/dir/f02500").await, json!({ "ok": "2500" }));
    assert_eq!(tric.files("POST", "m", "op=rmtree&path=/dir").await, json!({ "ok": null }));
    assert!(tric.fails("GET", "m", "op=ls&path=/dir", "NotFound").await);
    assert_eq!(tric.files("GET", "m", "op=ls&path=/").await["ok"]["count"], 0);
}

async fn files_race(kind: Kind) {
    let tric = Arc::new(Tric::app(kind).await);
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let tric = tric.clone();
            tokio::spawn(async move { tric.post("/@counter/files?op=incr&path=/n").await })
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
    let n = tric.files("GET", "counter", "op=read&path=/n").await["ok"].as_str().unwrap().parse::<u64>().unwrap();
    assert_eq!(n, ok, "every turn that answered 200 counted once, and no other");
}

async fn files_outlive_the_turn(kind: Kind) {
    let tric = Tric::app(kind).await;
    let ok = json!({ "ok": null });
    // A file that is unlinked while it is open is there, with no name, until it is closed.
    let out = tric.files("POST", "o", "op=orphan&path=/o").await;
    assert_eq!(out["ok"], json!({ "listed": false, "text": "abcdef", "after": false }));
    assert_eq!(tric.files("GET", "o", "op=ls&path=/").await["ok"]["count"], 0);

    // Open when the turn answers, it is read after its commit has left it out, and writes are refused.
    assert_eq!(tric.files("POST", "h", "op=write&path=/h&data=hello").await, ok);
    let held = tric.post("/@h/files?op=held&path=/h").await.json();
    assert_eq!(held, json!({ "read": { "ok": "hello" }, "listed": { "ok": false } }));
    assert!(tric.fails("GET", "h", "op=read&path=/h", "NotFound").await, "the commit left it out");

    assert_eq!(tric.files("POST", "l", "op=write&path=/l&data=first").await, ok);
    let late = tric.post("/@l/files?op=late&path=/l").await.json();
    for what in ["write", "create", "mkdir", "remove"] {
        let err = late[what]["err"].as_str().unwrap_or_default();
        assert!(err.starts_with("ReadOnlyFilesystem"), "{what}: {late}");
    }
    assert_eq!(tric.files("GET", "l", "op=read&path=/l").await, json!({ "ok": "" }), "opened, to truncate, before");
    for path in ["/late-file", "/late-dir"] {
        assert!(tric.fails("GET", "l", &format!("op=stat&path={path}"), "NotFound").await);
    }
}

async fn delivers_the_outbox_once_committed(kind: Kind) {
    let tric = Tric::app(kind).await;
    let me = format!("http://{}", tric.host);
    let res = tric.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await;
    assert_eq!((res.status, &*res.body), (200, "202 "));
    let echo = tric.wait_for("g", "echo", Duration::from_secs(30)).await.expect("delivered");
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
    let dir = manifest(&fixture(kind.app), "component = APP\nmiddleware = [{ url = GUARD, digest = \"DIGEST\" }]\n");
    let tric = Tric::new(kind, &dir, &["-e", "GUARD_TOKEN=t"]).await;
    assert_eq!(tric.get("/").await.status, 401);
    assert_eq!(tric.send("GET", "/", &[("authorization", "Bearer x")]).await.status, 401);
    let res = tric.send("GET", "/", &[("authorization", "Bearer t")]).await;
    assert_eq!((res.status, &*res.body), (200, "hello"));
    let env = tric.send("GET", "/env", &[("authorization", "Bearer t")]).await.json();
    assert_eq!(env["env"]["GUARD_TOKEN"], "t", "the app and its middleware share one environment");
}

/// Middleware whose bytes are not the ones pinned, or that sits at a private address, is refused before it is run.
#[tokio::test]
async fn refuses_forged_middleware() {
    let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    for (url, why) in [("GUARD", "does not match its digest"), ("\"http://127.0.0.1:9/\"", "DestinationIpProhibited")] {
        let toml = format!("component = APP\nmiddleware = [{{ url = {url}, digest = \"{digest}\" }}]\n");
        let out = Command::new(TRIC).arg("dev").arg(manifest(&fixture("app"), &toml)).output().await.unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains(why), "{url}: {err}");
    }
}

async fn confines_outbound_requests(kind: Kind) {
    let tric = Tric::app(kind).await;
    assert!(tric.get("/fetch?url=http://example.com/").await.body.contains("HttpRequestDenied"));
    let tric = Tric::new(kind, &fixture(kind.app), &["--allow", "*://*:*"]).await;
    for url in ["http://127.0.0.2:9/", "http://10.0.0.1/", "http://169.254.169.254/", "http://[::1]:9/"] {
        let body = tric.get(&format!("/fetch?url={url}")).await.body;
        assert!(body.contains("DestinationIpProhibited"), "{url}: {body}");
    }
}

async fn fires_cron(kind: Kind) {
    let dir = manifest(&fixture(kind.app), "component = APP\n[cron]\n\"* * * * *\" = \"/@ticks/echo\"\n");
    let tric = Tric::new(kind, &dir, &[]).await;
    let echo = tric.wait_for("ticks", "echo", Duration::from_secs(75)).await.expect("a tick within a minute");
    assert_eq!(echo["method"], "POST");
    // On a stack, any router on the bucket may fire it: each test's does, at a port of its own.
    let uri = echo["uri"].as_str().unwrap();
    assert!(uri.starts_with(&format!("http://{}", tric.host.split(':').next().unwrap())), "{uri}");
    assert!(uri.ends_with("/@ticks/echo"), "{uri}");
    assert_eq!(header(&echo, "forwarded"), Some("for=_cron"));
}

type Seen = Arc<Mutex<Vec<http::Request<Bytes>>>>;

/// A stand-in for serve, or for API Gateway's `@connections`, which keeps every request, and answers 410 to one whose
/// path has `gone` in it, and 204 to the rest.
async fn stand_in() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Seen::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let kept = kept.clone();
            let svc = service_fn(move |req: hyper::Request<Incoming>| {
                let kept = kept.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let code = if parts.uri.path().contains("gone") { 410 } else { 204 };
                    let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                    kept.lock().unwrap().push(http::Request::from_parts(parts, body));
                    Ok::<_, Infallible>(hyper::Response::builder().status(code).body(Full::new(Bytes::new())).unwrap())
                }
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
    let (route, addr) = route(port, &format!("127.0.0.1:{outbox}"), &fake, &[], &log).await;
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
        let all: Vec<_> = sent.headers().get_all(name).iter().collect();
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
    let as_a = &as_a.with_token(c["SessionToken"].as_str().unwrap()).build().unwrap();
    let denied = |r: object_store::Result<()>| matches!(r, Err(object_store::Error::PermissionDenied { .. }));
    let put = move |k: String| async move { as_a.put(&key(k), PutPayload::from_static(b"{}")).await.map(|_| ()) };
    let get = move |k: String| async move { as_a.get(&key(k)).await.map(|_| ()) };
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
    // A list names the keys under its prefix, so it is a read of them: a's credentials list a's names and a's values,
    // for the sweep, and nothing else, not even what they can read: the rest of a's, b's, any other app's, the bucket.
    let list = move |prefix: Option<String>| async move {
        let prefix = prefix.map(key);
        let one = as_a.list_with_delimiter(prefix.as_ref()).await.map(|_| ());
        let all = as_a.list(prefix.as_ref()).try_collect::<Vec<_>>().await.map(|_| ());
        one.and(all)
    };
    for p in [format!("apps/{a}/names"), format!("apps/{a}/values"), format!("apps/{a}/values/n")] {
        list(Some(p.clone())).await.unwrap_or_else(|e| panic!("list {p}: {e}"));
    }
    let own = ["apps/{a}", "apps/{a}/components", "apps/{a}/current", "native/{a}", "apps", "native", "ws"];
    let other = ["apps/{b}", "apps/{b}/names", "apps/{b}/values", "apps/{b}/values/v", "native/{b}"];
    for p in own.iter().chain(&other).map(|p| p.replace("{a}", &a).replace("{b}", &b)).map(Some).chain([None]) {
        let refused = list(p.clone()).await.err().map(|e| e.to_string());
        assert!(refused.is_some_and(|e| e.contains("403")), "list {p:?}");
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
    // The word that asks serve for a sweep is no way in: it takes a sweep as it takes any request, as a tenant, with
    // credentials, and so only from the router. The tenant of a request is the app its host names.
    let sweep = ("forwarded", "for=_sweep");
    for headers in [
        vec![sweep],
        vec![sweep, ("x-amz-tenant-id", &a)],
        vec![sweep, ("x-amz-tenant-id", &b), ("x-tric-credentials", &creds)],
    ] {
        assert_eq!(tric.send("POST", "/", &headers).await.status, 403, "{headers:?}");
    }
    assert!(!log.lock().unwrap().contains("swept"), "serve swept");
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
    let tric = Tric::app(Kind { stack: true, app: "app" }).await;
    let me = format!("http://{}", tric.host);
    assert_eq!(tric.post(&format!("/@f/fetch?method=POST&async=1&url={me}/@g/echo")).await.body, "202 ");
    let echo = tric.wait_for("g", "echo", Duration::from_secs(30)).await.expect("delivered");
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
    let outbox =
        [("x-amzn-lambda-context", r#"{"invoked_function_arn":"arn:aws:lambda:local:0:function:events:outbox"}"#)];
    assert_eq!(exchange(&tric.outbox, &tric.outbox, "POST", "/", &outbox, body).await.status, 202);
    let line = "outbox: dropped an event, as its commit is not pending";
    assert!(tric.logged(line, Duration::from_secs(30)).await, "dropped");
    assert_eq!(tric.value("h", "echo").await, Value::Null);

    // A sweep lists an app's names and deletes objects, and serve takes the router's `Forwarded: for=_sweep` as the ask
    // for one. Only the `cron` alias's `{app, sweep}`, which only Scheduler may invoke, makes the router say it, so
    // nothing may carry it in: not a client's request, whose `Forwarded` the router replaces with its own, nor an
    // event, for which the router writes its own, whatever alias it is for. (The router's own daily sweep, locally,
    // could fall in this test's seconds, at about one run in ten thousand, and would show in the counts below.)
    // What serve and the router log of a sweep, once each, whether it went well or not.
    let sweeps = || {
        let log = plain(&tric.log.lock().unwrap());
        ["tric::route: sweep", "tric::serve: swept", "tric::serve: sweep:", "tric::sweep"]
            .map(|l| log.matches(l).count())
    };
    let app = &tric.apps[0];
    let ask = json!({ "app": app, "sweep": true }).to_string();
    let forged = ("forwarded", "for=_sweep");
    for method in ["GET", "POST"] {
        let res = tric.send(method, "/", &[forged]).await;
        assert_eq!((res.status, &*res.body), (200, "hello"), "{method}: a request like any other");
    }
    let alias = |alias: &str| format!(r#"{{"invoked_function_arn":"arn:aws:lambda:local:0:function:events:{alias}"}}"#);
    let events = async |context: Option<&str>, body: &str| {
        let mut headers = vec![forged];
        headers.extend(context.map(|c| ("x-amzn-lambda-context", c)));
        exchange(&tric.outbox, &tric.outbox, "POST", "/", &headers, Bytes::from(body.to_owned())).await.status
    };
    let (outbox, retry, nobody) = (alias("outbox"), alias("retry"), alias("nobody"));
    assert_eq!(events(None, &ask).await, 403, "no alias");
    assert_eq!(events(Some(&nobody), &ask).await, 403, "an alias that is none");
    assert_eq!(events(Some(&retry), &ask).await, 403, "an alias that is not here");
    assert_eq!(events(Some(&outbox), &ask).await, 400, "a delivery that is none");
    assert_eq!(events(Some(&outbox), &event.to_string()).await, 202, "a delivery, which is dropped as above");

    // The `cron` alias takes a sweep as it is, and refuses what is none.
    let cron = alias("cron");
    for body in [json!({ "app": app }), json!({ "app": app, "sweep": false }), json!({ "app": "A/b", "sweep": true })] {
        assert_eq!(events(Some(&cron), &body.to_string()).await, 400, "{body}");
    }
    assert_eq!(events(Some(&cron), &json!({ "app": app, "sweep": true, "path": "/" }).to_string()).await, 400);
    assert_eq!(events(Some(&cron), &json!({ "app": unique(), "sweep": true }).to_string()).await, 404, "no release");
    assert_eq!(sweeps(), [0; 4], "nothing swept, and nothing tried to");

    // And the sweep that is asked for is one, and the answer comes after serve has logged it.
    assert_eq!(events(Some(&cron), &ask).await, 204);
    let once = until(Duration::from_secs(30), async || (sweeps() == [1, 1, 0, 0]).then_some(())).await;
    assert!(once.is_some(), "{:?}", sweeps());
}

/// What a sweep keeps. It deletes what no tree names that is older than an hour, and nothing here is: so what the
/// sweep shows is that it goes through every name, whole, and takes nothing that it should not; what it deletes, and
/// when, is for the unit tests, which have a clock of their own.
#[tokio::test]
async fn sweeps_the_names() {
    let tric = Tric::app(Kind { stack: true, app: "app" }).await;
    let (app, s3) = (&tric.apps[0], owner());
    let n = 1_000_000;
    assert_eq!(tric.files("POST", "s", &format!("op=big&path=/big&n={n}")).await, json!({ "ok": n }));
    assert_eq!(tric.post("/@s/kv?op=set&key=k&value=v").await.status, 200);
    // Objects that no tree names: as an app's turn that died would leave, at a name with a head and at one with none.
    let id = "0123456789abcdef0123456789abcdef";
    let strays = [format!("apps/{app}/values/s/{id}"), format!("apps/{app}/values/ghost/{id}")];
    for k in &strays {
        s3.put(&Key::from(k.as_str()), PutPayload::from_static(b"{}")).await.unwrap();
    }
    let all = async || {
        let mut keys: Vec<_> =
            s3.list(Some(&Key::from(format!("apps/{app}")))).map_ok(|m| m.location).try_collect().await.unwrap();
        keys.sort();
        keys
    };
    let before = all().await;
    for k in &strays {
        assert!(before.contains(&Key::from(k.as_str())), "{k}");
    }

    let cron = r#"{"invoked_function_arn":"arn:aws:lambda:local:0:function:events:cron"}"#;
    let body = Bytes::from(json!({ "app": app, "sweep": true }).to_string());
    let res = exchange(&tric.outbox, &tric.outbox, "POST", "/", &[("x-amzn-lambda-context", cron)], body).await;
    assert_eq!(res.status, 204);
    assert!(tric.logged("swept", Duration::from_secs(30)).await, "swept");
    let log = plain(&tric.log.lock().unwrap());
    for field in ["swept=2", "deleted=0", "skipped=0", "failed=0", "done=true"] {
        assert!(log.contains(field), "{field}: {log}");
    }
    assert_eq!(all().await, before, "all there, the cursor not left");
    assert_eq!(tric.files("GET", "s", "op=sum&path=/big").await["ok"]["len"], n);
    assert_eq!(tric.value("s", "k").await, "v");
}

/// `log` without the escape sequences that colour it.
fn plain(log: &str) -> String {
    let mut plain = String::new();
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => _ = chars.by_ref().find(|c| *c == 'm'),
            c => plain.push(c),
        }
    }
    plain
}

/// WebSockets, with the feature `ws`: real clients of `tric dev`, and the router as API Gateway invokes it; each with
/// the app's `/chat` route.
#[cfg(feature = "ws")]
mod ws {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Error, Message, client::IntoClientRequest};
    use tokio_tungstenite::{WebSocketStream, client_async};

    type Client = WebSocketStream<TcpStream>;
    const EVENTS: &str = "application/websocket-events";

    async fn dev() -> Tric {
        Tric::app(Kind { stack: false, app: "app" }).await
    }

    /// The path of a room of its own.
    fn room() -> String {
        format!("/@{}/chat", unique())
    }

    /// A socket at `target`, sent with `headers`, with the id the app knows it by; or why it was refused.
    async fn open(tric: &Tric, target: &str, headers: &[(&str, &str)]) -> Result<(Client, String), Error> {
        let stream = TcpStream::connect(&tric.addr).await.unwrap();
        let mut req = format!("ws://{}{target}", tric.addr).into_client_request()?;
        for (k, v) in headers {
            req.headers_mut().append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        let (mut ws, _) = client_async(req, stream).await?;
        let id = ask(&mut ws, "id").await;
        Ok((ws, id.strip_prefix("id ").expect("an id").to_owned()))
    }

    /// Whether `ws` is closed with `code`, next.
    async fn closed(ws: &mut Client, code: u16) -> bool {
        let next = timeout(Duration::from_secs(10), ws.next()).await.expect("a close");
        matches!(next, Some(Ok(Message::Close(Some(f)))) if u16::from(f.code) == code)
    }

    /// The status that refused a socket.
    fn refused(res: Result<(Client, String), Error>) -> u16 {
        match res.err() {
            Some(Error::Http(res)) => res.status().as_u16(),
            got => panic!("{got:?}"),
        }
    }

    /// The next text message, which comes within ten seconds.
    async fn next(ws: &mut Client) -> String {
        match timeout(Duration::from_secs(10), ws.next()).await.expect("a message") {
            Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
            got => panic!("{got:?}"),
        }
    }

    /// Sends `text`, and gives the next message.
    async fn ask(ws: &mut Client, text: &str) -> String {
        ws.send(Message::text(text)).await.unwrap();
        next(ws).await
    }

    /// The name of the room at `path`.
    fn name(path: &str) -> &str {
        path[2..].split('/').next().unwrap()
    }

    /// Waits for the last event of the room at `path` to be `want`.
    async fn last_is(tric: &Tric, path: &str, want: &str) {
        let name = name(path);
        let last =
            until(Duration::from_secs(10), async || (tric.value(name, "last").await == want).then_some(())).await;
        assert!(last.is_some(), "{name}: not {want}");
    }

    #[tokio::test]
    async fn echoes_counts_and_broadcasts() {
        let (tric, path) = (dev().await, room());
        let ((mut a, _), (mut b, _)) = (open(&tric, &path, &[]).await.unwrap(), open(&tric, &path, &[]).await.unwrap());
        assert_eq!(ask(&mut a, "hello").await, "hello");
        assert_eq!(ask(&mut a, "count").await, "0");

        // What is said counts in the name's state, and goes to every socket in the room, the sender's too.
        a.send(Message::text("say hi")).await.unwrap();
        assert_eq!((next(&mut a).await, next(&mut b).await), ("1: hi".into(), "1: hi".into()));
        assert_eq!(ask(&mut b, "say yo").await, "2: yo");
        assert_eq!(next(&mut a).await, "2: yo");
        assert_eq!(ask(&mut a, "count").await, "2");

        // Another room has its own count, and hears nothing. Binary messages are not taken, as on AWS.
        let (mut c, _) = open(&tric, &room(), &[]).await.unwrap();
        assert_eq!(ask(&mut c, "count").await, "0");
        c.send(Message::Binary(vec![0, 255].into())).await.unwrap();
        assert!(closed(&mut c, 1003).await);

        // An answer that is a failure is not committed, so is not counted, nor said; and it ends the socket.
        a.send(Message::text("boom no")).await.unwrap();
        assert!(closed(&mut a, 1000).await);
        assert_eq!(ask(&mut b, "count").await, "2");
        b.send(Message::text("bye")).await.unwrap();
        assert!(closed(&mut b, 1000).await);
    }

    #[tokio::test]
    async fn plain_sockets_are_not_subscribed() {
        let (tric, path) = (dev().await, room());
        let (mut plain, _) = open(&tric, &format!("{path}?plain"), &[]).await.unwrap();
        let (mut a, _) = open(&tric, &path, &[]).await.unwrap();
        plain.send(Message::text("say hi")).await.unwrap();
        assert_eq!(next(&mut a).await, "1: hi");
        assert_eq!(ask(&mut plain, "ping").await, "ping");
    }

    #[tokio::test]
    async fn refuses_what_the_app_refuses() {
        let (tric, path) = (dev().await, room());
        assert_eq!(refused(open(&tric, &format!("{path}?deny"), &[]).await), 403);
        // An app that takes no socket there answers as it does any request, which API Gateway could not tell from
        // accepting, so it is refused; and a path not a name's is not asked.
        assert_eq!(refused(open(&tric, &path.replace("/chat", "/"), &[]).await), 502);
        assert_eq!(refused(open(&tric, "/chat", &[]).await), 400);
    }

    #[tokio::test]
    async fn tells_the_app_how_a_socket_ends() {
        let tric = dev().await;
        let (shut, gone, broken, large) = (room(), room(), room(), room());
        let (mut a, _) = open(&tric, &shut, &[]).await.unwrap();
        a.close(None).await.unwrap();
        last_is(&tric, &shut, "close").await;

        drop(open(&tric, &gone, &[]).await.unwrap());
        last_is(&tric, &gone, "disconnect").await;

        // An answer that is not events ends it.
        let (mut b, _) = open(&tric, &broken, &[]).await.unwrap();
        b.send(Message::text("garbage")).await.unwrap();
        assert!(closed(&mut b, 1000).await);
        last_is(&tric, &broken, "disconnect").await;

        // So does a message past API Gateway's limit, which is not read.
        let (mut c, _) = open(&tric, &large, &[]).await.unwrap();
        _ = c.send(Message::text("x".repeat((128 << 10) + 1))).await;
        while let Ok(Some(Ok(_))) = timeout(Duration::from_secs(10), c.next()).await {}
        last_is(&tric, &large, "close").await;
    }

    #[tokio::test]
    async fn holds_so_many_sockets() {
        let (tric, path) = (dev().await, room());
        let mut held = vec![];
        for _ in 0..256 {
            held.push(open(&tric, &path, &[]).await.unwrap());
        }
        assert_eq!(refused(open(&tric, &path, &[]).await), 503);
    }

    /// What a client says is never what tric says: not that a request is events, nor a socket's, nor tric's publish.
    #[tokio::test]
    async fn is_not_fooled_by_clients() {
        let (tric, path) = (dev().await, room());
        let (mut a, id) = open(&tric, &path, &[]).await.unwrap();

        // Posing as events, and as the socket `a`, to say something: the app takes it for any request, as it is.
        let say = Bytes::from("TEXT 5\r\nsay x\r\n");
        let claims: [&[(&str, &str)]; 5] = [
            &[("content-type", EVENTS)],
            &[("content-type", EVENTS), ("connection-id", &id)],
            &[
                ("content-type", "text/plain"),
                ("content-type", "Application/WebSocket-Events; x=y"),
                ("connection-id", &id),
            ],
            &[("content-type", EVENTS), ("connection_id", &id), ("grip-hold", "stream"), ("meta-user", "a")],
            &[("connection-id", &id)],
        ];
        for claim in claims {
            let res = exchange(&tric.addr, &tric.host, "POST", &path, claim, say.clone()).await;
            assert_eq!((res.status, &*res.body), (400, "not a socket"), "{claim:?}");
        }
        // Nor is a socket's id the client's to choose.
        let (_, mine) = open(&tric, &path, &[("connection-id", &id), ("content-type", EVENTS)]).await.unwrap();
        assert_ne!(mine, id);

        // Publishing is the app's, to itself, and the client has no way to say that it is.
        let item = json!({ "channel": name(&path), "formats": { "ws-message": { "content": "x" } } });
        let publish = json!({ "items": [item] });
        for from in [&[][..], &[("forwarded", "for=_tric")]] {
            let mut headers = vec![("content-type", "application/json")];
            headers.extend(from);
            let res = exchange(&tric.addr, &tric.host, "POST", "/publish/", &headers, publish.to_string().into()).await;
            assert_eq!((res.status, &*res.body), (200, "hello"));
        }

        // None of that was counted, nor said.
        assert_eq!(ask(&mut a, "count").await, "0");
    }

    /// The router, as API Gateway invokes its `ws` alias, in front of serve and the app as on AWS, and of a stand-in
    /// for `@connections` at `TRIC_WS`: what it is sent, as (method, the socket's id, body).
    struct Gateway {
        tric: Tric,
        seen: Seen,
    }

    impl Gateway {
        async fn new() -> Self {
            let (fake, seen) = stand_in().await;
            let ws = format!("http://{fake}/ws");
            let tric = Tric::stack(&fixture("app"), &[], &[("TRIC_ORIGIN", "secret"), ("TRIC_WS", &ws)]).await;
            Self { tric, seen }
        }

        /// Invokes the alias `alias` with API Gateway's event `kind` of the socket `id`, with `more`: the router's
        /// status, and its answer's `statusCode`.
        async fn invoke(&self, alias: &str, kind: &str, id: &str, more: Value) -> (u16, Value) {
            let mut event = json!({ "requestContext": { "eventType": kind, "connectionId": id, "stage": "ws" } });
            more.as_object().unwrap().iter().for_each(|(k, v)| event[k] = v.clone());
            let arn = format!(r#"{{"invoked_function_arn":"arn:aws:lambda:local:0:function:events:{alias}"}}"#);
            let at = &self.tric.outbox;
            let res = exchange(at, at, "POST", "/", &[("x-amzn-lambda-context", &arn)], event.to_string().into()).await;
            (res.status, if res.status == 200 { res.json()["statusCode"].clone() } else { Value::Null })
        }

        /// A socket `id` opening at `target` of `host`, with `origin` as the origin secret: its `statusCode`. Its query
        /// is not in its path, but parsed, as API Gateway's is.
        async fn connect(&self, id: &str, host: &str, target: &str, origin: &str) -> Value {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let query: serde_json::Map<_, _> =
                query.split('&').filter(|q| !q.is_empty()).map(|q| (q.to_owned(), json!([""]))).collect();
            let headers = json!({
                "Host": ["abc.execute-api.us-west-2.amazonaws.com"],
                "X-Tric-Origin": [origin],
                "X-Forwarded-Host": [host],
                "X-Forwarded-Path": [path],
                "CloudFront-Viewer-Address": ["203.0.113.7:50000"],
                "Sec-WebSocket-Key": ["dGhlIHNhbXBsZSBub25jZQ=="],
                "Connection-Id": ["forged"],
            });
            let mut more = json!({ "multiValueHeaders": headers });
            if !query.is_empty() {
                more["multiValueQueryStringParameters"] = query.into();
            }
            self.invoke("ws", "CONNECT", id, more).await.1
        }

        async fn message(&self, id: &str, text: &str) -> Value {
            self.invoke("ws", "MESSAGE", id, json!({ "body": text })).await.1
        }

        /// Waits up to ten seconds for the stand-in to have been sent `want`, in any order, which it then forgets.
        async fn sent(&self, want: &[(&str, &str, &str)]) {
            let mut want: Vec<_> =
                want.iter().map(|&(method, id, body)| [method, id, body].map(str::to_owned)).collect();
            want.sort();
            let got = |seen: &[http::Request<Bytes>]| {
                let id = |r: &http::Request<Bytes>| r.uri().path().replace("/ws/@connections/", "").replace("%3D", "=");
                let body = |r: &http::Request<Bytes>| String::from_utf8_lossy(r.body()).into_owned();
                let mut got: Vec<_> = seen.iter().map(|r| [r.method().to_string(), id(r), body(r)]).collect();
                got.sort();
                got
            };
            let sent = until(Duration::from_secs(10), async || {
                let mut seen = self.seen.lock().unwrap();
                (got(&seen) == want).then(|| seen.clear())
            });
            assert!(sent.await.is_some(), "sent {:?}, not {want:?}", got(&self.seen.lock().unwrap()));
        }
    }

    #[tokio::test]
    async fn routes_api_gateways_events() {
        let g = Gateway::new().await;
        let (tric, room) = (&g.tric, unique());
        let (host, path) = (tric.host.as_str(), format!("/@{room}/chat"));
        let [a, b, c] = [0, 1, 2].map(|_| format!("{}=", unique()));
        let gone = format!("gone{}=", unique());

        // Only CloudFront's, with the secret, at a name of an app that has a release, and that the app accepts.
        assert_eq!(g.connect(&a, host, &path, "wrong").await, 403);
        assert_eq!(g.connect(&a, host, &format!("{path}?deny"), "secret").await, 403);
        let other = host.replacen(&tric.apps[0], &unique(), 1);
        assert_eq!(g.connect(&a, &other, &path, "secret").await, 404);
        assert_eq!(g.connect(&a, host, "/chat", "secret").await, 404);
        assert_eq!(g.connect(&a, host, &path.replace("/chat", "/"), "secret").await, 502);
        assert_eq!(g.invoke("nope", "CONNECT", &a, json!({})).await.0, 403);
        for id in [&a, &b, &c, &gone] {
            assert_eq!(g.connect(id, host, &path, "secret").await, 200);
        }

        // A message is sent to the app as the socket's, and its answer to the socket; one of a socket that never
        // opened goes nowhere.
        assert_eq!(g.message(&a, "id").await, 200);
        g.sent(&[("POST", &a, &format!("id {a}"))]).await;
        assert_eq!(g.message("never", "hello").await, 200);
        assert_eq!(g.message(&a, "hello").await, 200);
        g.sent(&[("POST", &a, "hello")]).await;

        // What is said goes to every socket in the room, once its turn commits; a socket that is gone is forgotten.
        assert_eq!(g.message(&a, "say hi").await, 200);
        g.sent(&[("POST", &a, "1: hi"), ("POST", &b, "1: hi"), ("POST", &c, "1: hi"), ("POST", &gone, "1: hi")]).await;
        assert_eq!(g.message(&b, "say yo").await, 200);
        g.sent(&[("POST", &a, "2: yo"), ("POST", &b, "2: yo"), ("POST", &c, "2: yo")]).await;

        // The app closes a socket, which is then gone; an answer not events ends one, and the app is told.
        assert_eq!(g.message(&a, "bye").await, 200);
        g.sent(&[("DELETE", &a, "")]).await;
        assert_eq!(g.message(&a, "hello").await, 200);
        assert_eq!(g.message(&b, "garbage").await, 200);
        g.sent(&[("DELETE", &b, "")]).await;
        assert_eq!(tric.value(&room, "last").await, "disconnect");

        // A socket's end is told the app once.
        let end =
            json!({ "requestContext": { "eventType": "DISCONNECT", "connectionId": c, "disconnectStatusCode": 1000 } });
        assert_eq!(g.invoke("ws", "DISCONNECT", &c, end.clone()).await.1, 200);
        assert_eq!(tric.value(&room, "last").await, "close");
        assert_eq!(g.message(&b, "garbage").await, 200);
        assert_eq!(g.invoke("ws", "DISCONNECT", &c, end).await.1, 200);
        assert_eq!(g.message(&c, "say late").await, 200);
        g.sent(&[]).await;
    }
}
