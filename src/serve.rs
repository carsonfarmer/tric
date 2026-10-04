//! `torpor serve`: runs every app of an install, or the app in a directory as an install of one, over HTTP/1.
use crate::state::{self, Current};
use crate::{cli, compile};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use hyper::body::{Body, Bytes, Incoming};
use hyper::{Request, StatusCode, header::HOST, server::conn::http1, service::service_fn};
use object_store::{ObjectStore, memory::InMemory, prefix::PrefixStore};
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, collections::HashMap, convert::Infallible, env, path::Path, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};
use torpor::{App, Engine, is_name};
use wasmtime::{Error, Result};
use wasmtime_wasi_http::{handler::Response, io::TokioIo};

const VAR_PREFIX: &str = "TORPOR_VAR_";
/// The viewer's host, which a proxy in front passes on in this header when its origin needs its own `Host`.
const FORWARDED_HOST: &str = "x-forwarded-host";
/// How stale a host's view may get, so also how long a change takes to be live everywhere.
const FRESH: Duration = Duration::from_secs(5);
/// The longest a read of `current` may hold up a request. The S3 client on its own retries for up to 3 minutes.
const RECHECK_MAX: Duration = Duration::from_secs(1);

/// Every app of an install, each served at `<app>.<any domain>`, and loaded at its first request.
pub struct Install {
    store: Arc<dyn ObjectStore>,
    native: Option<Arc<dyn ObjectStore>>,
    kv: Arc<dyn ObjectStore>,
    engine: Arc<Engine>,
    apps: Mutex<HashMap<String, Arc<Mutex<Served>>>>, // by name, each once it has had a release
}

/// One app as a host last read it, and the load of its release. A failed load stands until the next recheck, so a broken
/// release costs one load per `FRESH`, not one per request.
struct Served {
    current: Current,
    read: Instant,
    app: Option<Load>,
}

/// A load, shared by every request that waits for it.
type Load = Shared<BoxFuture<'static, Result<App, String>>>;

impl Install {
    /// The install in `store`, with its bucket of native code, if it has one, and the bucket of its apps' KV data.
    pub fn new(
        store: Arc<dyn ObjectStore>,
        native: Option<Arc<dyn ObjectStore>>,
        kv: Arc<dyn ObjectStore>,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self { engine: Engine::new()?.into(), store, native, kv, apps: Mutex::default() }))
    }

    /// An install in memory of just the app in `dir`, released, with `TORPOR_VAR_<KEY>` as its secret `key`, so secrets
    /// stay out of files. It loads the app before returning, so one that a host would refuse fails here.
    pub async fn dev(dir: &Path) -> Result<Arc<Self>> {
        let store = Arc::new(InMemory::new());
        let (app, id) = cli::publish(&*store, dir, false).await?; // unchecked, as the load below checks it
        let secrets = env::vars().filter_map(|(k, v)| Some((k.strip_prefix(VAR_PREFIX)?.to_lowercase(), v))).collect();
        state::update(&*store, &app, |c| *c = Current { release: Some(id), secrets }).await?;
        let install = Self::new(store.clone(), None, store)?;
        install.app(&app).await?;
        Ok(install)
    }

    pub async fn handle<B>(self: Arc<Self>, mut req: Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        // The viewer's host names the app, and is the one the app sees.
        if let Some(host) = req.headers_mut().remove(FORWARDED_HOST) {
            req.headers_mut().insert(HOST, host);
        }
        let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
        let name = host.split(['.', ':']).next().unwrap_or_default().to_ascii_lowercase(); // the first label
        match self.app(&name).await.inspect_err(|e| tracing::warn!(app = name, "{e:#}")) {
            Ok(Some(app)) => app.handle(req).await,
            Ok(None) => status(StatusCode::NOT_FOUND),
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    /// The app `name`, loading it if it is new or its `current` changed. A name is kept only once it has had a release,
    /// so a made-up name costs a read each, and no memory.
    async fn app(self: &Arc<Self>, name: &str) -> Result<Option<App>> {
        if !is_name(name) {
            return Ok(None);
        }
        let known = self.apps.lock().await.get(name).cloned(); // apart, as the `match` would hold the lock into its arms
        let served = match known {
            Some(served) => served,
            None => {
                let current = self.current(name).await?;
                if current.release.is_none() {
                    return Ok(None);
                }
                let served = Arc::new(Mutex::new(Served { current, read: Instant::now(), app: None }));
                self.apps.lock().await.entry(name.into()).or_insert(served).clone()
            }
        };
        let load = {
            let s = &mut *served.lock().await;
            if s.read.elapsed() >= FRESH {
                match self.current(name).await {
                    Ok(current) if current != s.current => (s.current, s.app) = (current, None),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(app = name, "serving it as last read: {e:#}"),
                }
                s.read = Instant::now();
                s.app.take_if(|a| a.peek().is_some_and(Result::is_err));
            }
            let Some(release) = &s.current.release else { return Ok(None) };
            s.app.get_or_insert_with(|| self.load(name, release, &s.current.secrets)).clone()
        };
        Ok(Some(load.await.map_err(Error::msg)?))
    }

    /// `name`'s `current`, or an error once `RECHECK_MAX` has passed.
    async fn current(&self, name: &str) -> Result<Current> {
        Ok(timeout(RECHECK_MAX, state::current(&*self.store, name)).await??.0)
    }

    /// Loads the app `name` from its release `id`, with `secrets` over its config. The load is a task of its own, so it
    /// runs to its end even if every request waiting for it goes away.
    fn load(self: &Arc<Self>, name: &str, id: &str, secrets: &BTreeMap<String, String>) -> Load {
        let (install, name, id, secrets) = (self.clone(), name.to_owned(), id.to_owned(), secrets.clone());
        let task = tokio::spawn(async move {
            let Install { store, native, kv, engine, .. } = &*install;
            let start = Instant::now();
            let mut r = state::release(&**store, &name, &id).await?;
            r.config.extend(secrets);
            let code = compile::component(&**store, native.as_deref(), engine, &name, &r.component).await?;
            let kv = Arc::new(PrefixStore::new(kv.clone(), state::kv(&name)));
            let app = engine.load(&name, kv, &code, r.config, &r.allowed_outbound_hosts);
            app.inspect(|_| tracing::info!(app = name, ms = start.elapsed().as_millis(), "loaded"))
        });
        task.map(|r| r.unwrap_or_else(|e| Err(e.into())).map_err(|e| format!("{e:#}"))).boxed().shared()
    }
}

/// An empty response of `code`.
pub fn status(code: StatusCode) -> Response {
    hyper::Response::builder().status(code).body(Default::default()).unwrap()
}

/// Serves HTTP/1 on `listener`, each request with `handle`.
pub async fn run<H, F>(listener: TcpListener, handle: H) -> Result<()>
where
    H: Fn(Request<Incoming>) -> F + Clone + Send + 'static,
    F: Future<Output = Response> + Send + 'static,
{
    loop {
        let (stream, _) = listener.accept().await?;
        _ = stream.set_nodelay(true); // best effort; otherwise Nagle plus delayed ACK stalls a response by ~40 ms
        let handle = handle.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req).map(Ok::<_, Infallible>));
            http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await.ok();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty};
    use object_store::ObjectStoreExt;
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::sleep;

    async fn get(install: &Arc<Install>, host: &str, uri: &str) -> (StatusCode, String) {
        let res =
            install.clone().handle(Request::get(uri).header(HOST, host).body(Empty::<Bytes>::new()).unwrap()).await;
        (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().into()).unwrap())
    }

    async fn ok(install: &Arc<Install>, host: &str, uri: &str) -> String {
        let (status, body) = get(install, host, uri).await;
        assert_eq!(status, 200, "{host}{uri}: {body}");
        body
    }

    /// Publishes and releases the app in `dir`, and returns the release's id.
    async fn ship(store: &dyn ObjectStore, dir: &str) -> String {
        let (app, id) = cli::publish(store, dir.as_ref(), true).await.unwrap();
        cli::release(store, &app, &id).await.unwrap();
        id
    }

    /// One request over a real socket checks that `torpor serve DIR` serves the app in `DIR` at its name.
    #[tokio::test]
    async fn serves_a_dir() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut conn = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let install = Install::dev("tests/app".as_ref()).await.unwrap();
        tokio::spawn(run(listener, move |req| install.clone().handle(req)));
        conn.write_all(b"GET / HTTP/1.1\r\nHost: hello.localhost\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut res = String::new();
        conn.read_to_string(&mut res).await.unwrap();
        assert!(res.starts_with("HTTP/1.1 200") && res.contains("hello"), "{res}");
    }

    /// Two apps served by subdomain, one with a secret. Then one gets a new release and the other is taken offline, and
    /// then the first goes back to its old release.
    #[tokio::test]
    async fn publishes_releases_and_serves() {
        let store = Arc::new(InMemory::new());
        ship(&*store, "tests/app").await;
        let first = ship(&*store, "tests/kv").await;
        for (name, value) in [("token", "s3cret"), ("gone", "x"), ("gone", "")] {
            cli::set_secret(&*store, "kv", name, value).await.unwrap();
        }
        assert_eq!(cli::secrets(&*store, "kv").await.unwrap(), ["token"]);

        let kv = Arc::new(InMemory::new());
        let install = Install::new(store.clone(), None, kv.clone()).unwrap();
        assert_eq!(ok(&install, "hello.localhost:3000", "/").await, "hello");
        assert_eq!(ok(&install, "KV.example.com", "/config?key=token").await, r#"{"ok":"s3cret"}"#);
        ok(&install, "kv.localhost", "/kv?op=set&store=s&key=k&value=v").await;
        kv.head(&"kv/kv/s/k".into()).await.unwrap();
        assert!(store.head(&"kv/kv/s/k".into()).await.is_err());
        let req = Request::get("/")
            .header(HOST, "abc.lambda-url.us-west-2.on.aws")
            .header(FORWARDED_HOST, "hello.tric.works");
        assert_eq!(install.clone().handle(req.body(Empty::<Bytes>::new()).unwrap()).await.status(), 200);
        for host in ["nope.localhost", "localhost", "", "a/b.localhost"] {
            assert_eq!(get(&install, host, "/").await.0, 404);
        }
        assert_eq!(install.apps.lock().await.len(), 2); // the names with nothing released are not kept

        let mut r = state::release(&*store, "kv", &first).await.unwrap();
        r.config.insert("greeting".into(), "bye".into());
        let second = state::add(&*store, "kv", state::RELEASES, serde_json::to_vec(&r).unwrap().into()).await.unwrap();
        cli::release(&*store, "kv", &second).await.unwrap();
        store.delete(&"apps/hello/current".into()).await.unwrap();
        sleep(FRESH).await;
        assert_eq!(ok(&install, "kv.localhost", "/config?key=greeting").await, r#"{"ok":"bye"}"#);
        assert_eq!(ok(&install, "kv.localhost", "/config?key=token").await, r#"{"ok":"s3cret"}"#);
        assert_eq!(get(&install, "hello.localhost", "/").await.0, 404);

        cli::release(&*store, "kv", &first).await.unwrap();
        assert!(cli::release(&*store, "kv", "nope").await.is_err());
        sleep(FRESH).await;
        assert_eq!(ok(&install, "kv.localhost", "/config?key=greeting").await, r#"{"ok":"hi"}"#);
        let releases = cli::releases(&*store, "kv").await.unwrap();
        assert_eq!(releases.iter().map(|r| r.split(' ').next().unwrap()).collect::<Vec<_>>(), [&second, &first]);
    }

    #[tokio::test]
    async fn refuses_a_blob_that_does_not_match_its_hash() {
        let store = Arc::new(InMemory::new());
        let id = ship(&*store, "tests/app").await;
        let component = state::release(&*store, "hello", &id).await.unwrap().component;
        let blob = format!("apps/hello/blobs/sha256/{component}").as_str().into();
        let good = store.get(&blob).await.unwrap().bytes().await.unwrap();
        store.put(&blob, std::fs::read("tests/fixtures/hello-p2.wasm").unwrap().into()).await.unwrap();
        let install = Install::new(store.clone(), None, store.clone()).unwrap();
        let e = install.app("hello").await.err().unwrap();
        assert!(e.to_string().contains("does not match its hash"), "{e}");
        store.put(&blob, good.into()).await.unwrap();
        assert_eq!(get(&install, "hello.localhost", "/").await.0, 500); // the failure is remembered, not fetched again
        sleep(FRESH).await;
        assert_eq!(ok(&install, "hello.localhost", "/").await, "hello"); // until the next recheck
    }

    /// A recheck that hangs gives up at `RECHECK_MAX`, and the host serves what it last read.
    #[tokio::test]
    async fn a_hung_recheck_serves_the_last_read() {
        let store = Arc::new(ThrottledStore::new(InMemory::new(), ThrottleConfig::default()));
        ship(&*store, "tests/app").await;
        let install = Install::new(store.clone(), None, store.clone()).unwrap();
        assert_eq!(ok(&install, "hello.localhost", "/").await, "hello");
        store.config_mut(|c| c.wait_get_per_call = Duration::from_secs(60));
        sleep(FRESH).await;
        let start = Instant::now();
        assert_eq!(ok(&install, "hello.localhost", "/").await, "hello");
        assert!(start.elapsed() < 2 * RECHECK_MAX);
    }

    /// A load runs to its end even if every request waiting for it goes away, so the next request does not start over.
    #[tokio::test]
    async fn a_load_outlives_its_requests() {
        let store = Arc::new(ThrottledStore::new(InMemory::new(), ThrottleConfig::default()));
        ship(&*store, "tests/app").await;
        let install = Install::new(store.clone(), None, store.clone()).unwrap();
        store.config_mut(|c| c.wait_get_per_call = Duration::from_millis(200));
        assert!(timeout(Duration::from_millis(300), install.app("hello")).await.is_err()); // gone while it fetches
        sleep(Duration::from_secs(2)).await; // the load's fetches finish meanwhile
        store.config_mut(|c| c.wait_get_per_call = Duration::from_secs(60)); // so fetching again would hang
        assert_eq!(timeout(Duration::from_secs(30), ok(&install, "hello.localhost", "/")).await.unwrap(), "hello");
    }

    /// Calls that wait let both changes read `current` before either writes it, so one loses the swap and fails.
    #[tokio::test]
    async fn concurrent_changes_one_lands() {
        let ms = Duration::from_millis(10);
        let config = ThrottleConfig { wait_get_per_call: ms, wait_put_per_call: ms, ..Default::default() };
        let store = ThrottledStore::new(InMemory::new(), config);
        cli::set_secret(&store, "app", "a", "1").await.unwrap(); // so the race is over a conditional update, not a create
        let (b, c) = tokio::join!(cli::set_secret(&store, "app", "b", "2"), cli::set_secret(&store, "app", "c", "3"));
        assert_ne!(b.is_ok(), c.is_ok());
        assert!(b.and(c).unwrap_err().to_string().contains("app changed while this ran"));
        assert_eq!(cli::secrets(&store, "app").await.unwrap().len(), 2);
    }
}
