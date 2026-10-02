//! `torpor serve`: runs the app in a directory, or every app of an install, over HTTP/1.
use crate::cli;
use crate::state::{self, BLOB_MAX, Current, FRESH, INDEX, Index};
use age::x25519::Identity;
use hyper::body::{Body, Bytes, Incoming};
use hyper::{Request, StatusCode, header::HOST, server::conn::http1, service::service_fn};
use object_store::{ObjectStore, UpdateVersion, memory::InMemory};
use serde::de::DeserializeOwned;
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, collections::HashMap, convert::Infallible, env, path::Path, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex, task::spawn_blocking, time::timeout};
use torpor::{App, Engine};
use tracing::Instrument;
use wasmtime::{Error, Result, bail};
use wasmtime_wasi_http::{handler::Response, io::TokioIo};

const VAR_PREFIX: &str = "TORPOR_VAR_";
/// The longest one recheck may hold up requests, and a request can meet two: the index's and its app's. The S3 client
/// on its own retries for up to 3 minutes.
const RECHECK_MAX: Duration = Duration::from_secs(1);

/// What a host serves.
pub enum Apps {
    Dir(App),
    Install(Box<Install>),
}

/// Every app of an install, each served at `<app>.<any domain>`, and loaded at its first request.
pub struct Install {
    store: Arc<dyn ObjectStore>,
    engine: Arc<Engine>,
    identity: Option<Identity>,                       // decrypts secrets
    index: Mutex<(Index, Option<String>, Instant)>,   // with its ETag and when it was read
    apps: Mutex<HashMap<String, Arc<Mutex<Served>>>>, // by name
}

/// One app as a host last read it, and the app loaded from its release or why that failed. A failure stands until the
/// next recheck, so a broken release costs one load per `FRESH`, not one per request.
#[derive(Default)]
struct Served {
    team: String,
    current: Current,
    etag: Option<String>,
    read: Option<Instant>,
    app: Option<Result<App, String>>,
}

impl Apps {
    /// The app in `dir`, at the root. `TORPOR_VAR_<KEY>` sets `key`, overriding `[config]`, so secrets stay out of files.
    pub fn dir(dir: &Path) -> Result<Self> {
        let (mut m, wasm) = cli::read(dir)?;
        m.config.extend(env::vars().filter_map(|(k, v)| Some((k.strip_prefix(VAR_PREFIX)?.to_lowercase(), v))));
        let engine = Engine::new(Arc::new(InMemory::new()))?;
        Ok(Self::Dir(engine.load(&m.name, "", wasm, m.config, &m.allowed_outbound_hosts)?))
    }

    /// The install in `store`. Its index is read now, so a host that can't read it never starts.
    pub async fn install(store: Arc<dyn ObjectStore>, identity: Option<Identity>) -> Result<Self> {
        let (i, etag) = state::read(&*store, INDEX, None).await?.map_or_else(Default::default, |(i, v)| (i, v.e_tag));
        let (index, apps) = (Mutex::new((i, etag, Instant::now())), Mutex::default());
        let engine = Engine::new(store.clone())?.into();
        Ok(Self::Install(Box::new(Install { engine, store, identity, index, apps })))
    }

    async fn handle<B>(&self, req: Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        let install = match self {
            Self::Dir(app) => return app.handle(req).await,
            Self::Install(install) => install,
        };
        match install.route(&req).await.inspect_err(|e| tracing::warn!("{e}")) {
            Ok(Some((name, app))) => {
                // A span, so a filter like `warn,[request{app=NAME}]=info` turns on one app's request lines.
                let span = tracing::info_span!("request", app = name);
                span.in_scope(|| tracing::info!(method = %req.method(), path = req.uri().path()));
                app.handle(req).instrument(span).await
            }
            Ok(None) => status(StatusCode::NOT_FOUND),
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

impl Install {
    /// The app `req` is for, named by the first label of its host, loading it if it is new or its release changed.
    async fn route<B>(&self, req: &Request<B>) -> Result<Option<(String, App)>> {
        let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
        let name = host.split(['.', ':']).next().unwrap_or_default().to_ascii_lowercase();
        let team = {
            let mut i = self.index.lock().await;
            if i.2.elapsed() >= FRESH {
                match recheck(&*self.store, INDEX, i.1.clone()).await {
                    Ok(Some((index, v))) => (i.0, i.1) = (index, v.e_tag),
                    Ok(None) => {}
                    Err(e) => tracing::warn!("serving the last index read: {e}"), // and trying again at `FRESH`
                }
                i.2 = Instant::now();
                self.apps.lock().await.retain(|app, _| i.0.contains_key(app));
            }
            let Some(team) = i.0.get(&name) else { return Ok(None) };
            team.clone()
        };
        let served = self.apps.lock().await.entry(name.clone()).or_default().clone();
        let s = &mut *served.lock().await;
        if s.team != team {
            *s = Served { team, ..Default::default() }; // new, or moved to another team
        }
        if s.read.is_none_or(|r| r.elapsed() >= FRESH) {
            match recheck(&*self.store, &state::current(&s.team, &name), s.etag.clone()).await {
                Ok(Some((current, v))) => (s.current, s.etag, s.app) = (current, v.e_tag, None),
                Ok(None) => {}
                Err(e) if s.read.is_some() => tracing::warn!("serving {name} as last read: {e}"),
                Err(e) => return Err(e),
            }
            s.read = Some(Instant::now());
            s.app.take_if(|a| a.is_err());
        }
        let Some(release) = &s.current.release else { return Ok(None) };
        if s.app.is_none() {
            s.app = Some(self.load(&name, &s.team, release, &s.current.secrets).await.map_err(|e| e.to_string()));
        }
        Ok(Some((name, s.app.clone().expect("loaded above").map_err(Error::msg)?)))
    }

    /// The app `name` of `team` from its release `id`, with `secrets` decrypted over its config, and its component
    /// compiled on a blocking thread, so a compile never holds up the requests of apps that are already loaded.
    async fn load(&self, name: &str, team: &str, id: &str, secrets: &BTreeMap<String, String>) -> Result<App> {
        let r = state::release(&*self.store, &state::releases(team, name), id).await?;
        let mut config = r.config;
        for (key, sealed) in secrets {
            let Some(identity) = &self.identity else { bail!("{name} has secrets, and there is no identity") };
            config.insert(key.clone(), String::from_utf8(age::decrypt(identity, sealed.as_bytes())?)?);
        }
        let wasm = state::fetch(&*self.store, &state::blobs(team), &r.component, BLOB_MAX).await?;
        let (engine, name, kv) = (self.engine.clone(), name.to_owned(), state::kv(team, name));
        spawn_blocking(move || engine.load(&name, &kv, wasm, config, &r.allowed_outbound_hosts)).await?
    }
}

/// `state::read`, given up at `RECHECK_MAX`.
async fn recheck<T: DeserializeOwned>(
    s: &dyn ObjectStore,
    path: &str,
    etag: Option<String>,
) -> Result<Option<(T, UpdateVersion)>> {
    timeout(RECHECK_MAX, state::read(s, path, etag)).await.unwrap_or_else(|e| Err(e.into()))
}

fn status(code: StatusCode) -> Response {
    hyper::Response::builder().status(code).body(Default::default()).unwrap()
}

pub async fn run(apps: Apps, listener: TcpListener) -> Result<()> {
    let apps = Arc::new(apps);
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?; // otherwise Nagle plus delayed ACK stalls a response by ~40 ms
        let apps = apps.clone();
        tokio::spawn(async move {
            let svc = service_fn(|req: Request<Incoming>| async { Ok::<_, Infallible>(apps.handle(req).await) });
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

    async fn get(apps: &Apps, host: &str, uri: &str) -> (StatusCode, String) {
        let res = apps.handle(Request::get(uri).header(HOST, host).body(Empty::<Bytes>::new()).unwrap()).await;
        (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().into()).unwrap())
    }

    async fn ok(apps: &Apps, host: &str, uri: &str) -> String {
        let (status, body) = get(apps, host, uri).await;
        assert_eq!(status, 200, "{host}{uri}: {body}");
        body
    }

    /// Puts the app in `dir` in `team`, then publishes and releases it, and returns the release's id.
    async fn ship(store: &dyn ObjectStore, team: &str, dir: &str) -> String {
        cli::assign(store, &cli::read(dir.as_ref()).unwrap().0.name, Some(team)).await.unwrap();
        let (app, id) = cli::publish(store, dir.as_ref()).await.unwrap();
        cli::release(store, &app, &id).await.unwrap();
        id
    }

    /// One request over a real socket checks `torpor.toml` is read and its component loaded.
    #[tokio::test]
    async fn reads_the_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut conn = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        tokio::spawn(run(Apps::dir("tests/app".as_ref()).unwrap(), listener));
        conn.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut res = String::new();
        conn.read_to_string(&mut res).await.unwrap();
        assert!(res.starts_with("HTTP/1.1 200") && res.contains("hello"), "{res}");
    }

    /// Two apps of two teams, served by subdomain, one with a secret. Then one gets a new release and the other a new
    /// team, and then the first goes back to its old release and the second out of the install.
    #[tokio::test]
    async fn publishes_releases_and_serves() {
        let store = Arc::new(InMemory::new());
        assert!(cli::publish(&*store, "tests/app".as_ref()).await.is_err()); // not in the index
        ship(&*store, "a", "tests/app").await;
        let first = ship(&*store, "b", "tests/kv").await;
        let identity = Identity::generate();
        cli::set_secret(&*store, "kv", "token", "s3cret", &identity.to_public().to_string()).await.unwrap();
        assert_eq!(cli::secrets(&*store, "kv").await.unwrap(), ["token"]); // with no identity at all

        let apps = Apps::install(store.clone(), Some(identity)).await.unwrap();
        let etag = state::read::<Index>(&*store, INDEX, None).await.unwrap().unwrap().1.e_tag;
        assert!(state::read::<Index>(&*store, INDEX, etag).await.unwrap().is_none()); // a recheck of no change reads nothing
        assert_eq!(ok(&apps, "hello.localhost:3000", "/").await, "hello");
        assert_eq!(ok(&apps, "KV.example.com", "/config?key=token").await, r#"{"ok":"s3cret"}"#);
        ok(&apps, "kv.localhost", "/kv?op=set&store=s&key=k&value=v").await;
        store.head(&"kv/b/kv/s/k".into()).await.unwrap();
        for host in ["nope.localhost", "localhost", ""] {
            assert_eq!(get(&apps, host, "/").await.0, 404);
        }

        let dir = state::releases("b", "kv");
        let mut r = state::release(&*store, &dir, &first).await.unwrap();
        r.config.insert("greeting".into(), "bye".into());
        let second = state::add(&*store, &dir, serde_json::to_vec(&r).unwrap().into(), state::JSON_MAX).await.unwrap();
        cli::release(&*store, "kv", &second).await.unwrap();
        cli::assign(&*store, "hello", Some("c")).await.unwrap();
        sleep(FRESH).await;
        assert_eq!(ok(&apps, "kv.localhost", "/config?key=greeting").await, r#"{"ok":"bye"}"#);
        assert_eq!(ok(&apps, "kv.localhost", "/config?key=token").await, r#"{"ok":"s3cret"}"#);
        assert_eq!(get(&apps, "hello.localhost", "/").await.0, 404); // team c has published nothing

        cli::release(&*store, "kv", &first).await.unwrap();
        assert!(cli::release(&*store, "kv", "nope").await.is_err());
        cli::assign(&*store, "hello", None).await.unwrap();
        assert!(cli::secrets(&*store, "hello").await.is_err());
        sleep(FRESH).await;
        assert_eq!(ok(&apps, "kv.localhost", "/config?key=greeting").await, r#"{"ok":"hi"}"#);
        let releases = cli::releases(&*store, "kv").await.unwrap();
        assert_eq!(releases.iter().map(|r| r.split(' ').next().unwrap()).collect::<Vec<_>>(), [&second, &first]);
    }

    #[tokio::test]
    async fn refuses_a_blob_that_does_not_match_its_hash() {
        let store = Arc::new(InMemory::new());
        let id = ship(&*store, "a", "tests/app").await;
        let component = state::release(&*store, &state::releases("a", "hello"), &id).await.unwrap().component;
        let blob = format!("{}/{component}", state::blobs("a")).as_str().into();
        let good = store.get(&blob).await.unwrap().bytes().await.unwrap();
        store.put(&blob, std::fs::read("tests/fixtures/hello-p2.wasm").unwrap().into()).await.unwrap();
        let apps = Apps::install(store.clone(), None).await.unwrap();
        let Apps::Install(install) = &apps else { unreachable!() };
        let e =
            install.route(&Request::get("/").header(HOST, "hello.localhost").body(()).unwrap()).await.err().unwrap();
        assert!(e.to_string().contains("does not match its hash"), "{e}");
        store.put(&blob, good.into()).await.unwrap();
        assert_eq!(get(&apps, "hello.localhost", "/").await.0, 500); // the failure is remembered, not fetched again
        sleep(FRESH).await;
        assert_eq!(ok(&apps, "hello.localhost", "/").await, "hello"); // until the next recheck
    }

    /// A recheck that hangs gives up at `RECHECK_MAX`, and the host serves what it last read.
    #[tokio::test]
    async fn a_hung_recheck_serves_the_last_read() {
        let store = Arc::new(ThrottledStore::new(InMemory::new(), ThrottleConfig::default()));
        ship(&*store, "a", "tests/app").await;
        let apps = Apps::install(store.clone(), None).await.unwrap();
        assert_eq!(ok(&apps, "hello.localhost", "/").await, "hello");
        store.config_mut(|c| c.wait_get_per_call = Duration::from_secs(60));
        sleep(FRESH).await;
        let start = Instant::now();
        assert_eq!(ok(&apps, "hello.localhost", "/").await, "hello");
        assert!(start.elapsed() < 3 * RECHECK_MAX); // the index's recheck, then the app's
    }

    /// Calls that wait let both changes read the index before either writes it, so one loses the swap and retries.
    #[tokio::test]
    async fn concurrent_changes_both_land() {
        let ms = Duration::from_millis(10);
        let config = ThrottleConfig { wait_get_per_call: ms, wait_put_per_call: ms, ..Default::default() };
        let store = ThrottledStore::new(InMemory::new(), config);
        cli::assign(&store, "a", Some("t")).await.unwrap(); // so the race is over a conditional update, not a create
        tokio::try_join!(cli::assign(&store, "b", Some("t")), cli::assign(&store, "c", Some("t"))).unwrap();
        let index: Index = state::read(&store, INDEX, None).await.unwrap().unwrap().0;
        assert_eq!(index.keys().collect::<Vec<_>>(), ["a", "b", "c"]);
    }
}
