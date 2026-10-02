//! `torpor serve`: runs the app in a directory, or every app of an install, over HTTP/1.
use crate::cli;
use crate::state::{self, BLOB_MAX, BLOBS, FRESH, State};
use age::x25519::Identity;
use hyper::body::{Body, Bytes, Incoming};
use hyper::{Request, StatusCode, header::HOST, server::conn::http1, service::service_fn};
use object_store::{ObjectStore, memory::InMemory};
use std::{collections::HashMap, convert::Infallible, env, path::Path, sync::Arc, time::Instant};
use tokio::{net::TcpListener, sync::Mutex};
use torpor::{App, Engine};
use tracing::Instrument;
use wasmtime::{Result, bail};
use wasmtime_wasi_http::{handler::Response, io::TokioIo};

const VAR_PREFIX: &str = "TORPOR_VAR_";
const FORWARDED_PREFIX: &str = "x-forwarded-prefix";

/// What a host serves.
pub enum Apps {
    Dir(App),
    Install(Box<Install>),
}

/// Every app of an install, each served by `domains` or else under `/<app>/`, and loaded at its first request.
pub struct Install {
    store: Arc<dyn ObjectStore>,
    engine: Engine,
    identity: Option<Identity>,                     // decrypts secrets
    state: Mutex<(State, Option<String>, Instant)>, // with its ETag and when it was read
    apps: Mutex<HashMap<String, (String, App)>>,    // by name, with the release each was loaded from
}

impl Apps {
    /// The app in `dir`, at the root. `TORPOR_VAR_<KEY>` sets `key`, overriding `[config]`, so secrets stay out of files.
    pub fn dir(dir: &Path) -> Result<Self> {
        let (mut m, wasm) = cli::read(dir)?;
        m.config.extend(env::vars().filter_map(|(k, v)| Some((k.strip_prefix(VAR_PREFIX)?.to_lowercase(), v))));
        let engine = Engine::new(Arc::new(InMemory::new()))?;
        Ok(Self::Dir(engine.load(&m.name, wasm, m.config, &m.allowed_outbound_hosts)?))
    }

    /// The install in `store`. Its state is read now, so a host that can't read it never starts.
    pub async fn install(store: Arc<dyn ObjectStore>, identity: Option<Identity>) -> Result<Self> {
        let (s, etag) = state::read(&*store, None).await?.map_or_else(Default::default, |(s, v)| (s, v.e_tag));
        let (state, apps) = (Mutex::new((s, etag, Instant::now())), Mutex::default());
        Ok(Self::Install(Box::new(Install { engine: Engine::new(store.clone())?, store, identity, state, apps })))
    }

    async fn handle<B>(&self, mut req: Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        let install = match self {
            Self::Dir(app) => return app.handle(req).await,
            Self::Install(install) => install,
        };
        match install.route(&mut req).await.inspect_err(|e| tracing::warn!("{e}")) {
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
    /// The app `req` is for, loading it if it is new. A path prefix that named it moves to `FORWARDED_PREFIX`.
    async fn route<B>(&self, req: &mut Request<B>) -> Result<Option<(String, App)>> {
        let mut s = self.state.lock().await;
        if s.2.elapsed() >= FRESH {
            match state::read(&*self.store, s.1.clone()).await {
                Ok(Some((state, v))) => (s.0, s.1) = (state, v.e_tag),
                Ok(None) => {}
                Err(e) => tracing::warn!("serving the last state read: {e}"), // and trying again at `FRESH`
            }
            s.2 = Instant::now();
        }
        req.headers_mut().remove(FORWARDED_PREFIX); // only the host sets it
        let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()?.split(':').next());
        let name = match host.and_then(|h| s.0.domains.get(h)) {
            Some(name) => name.clone(),
            None => {
                let pq = req.uri().path_and_query().map_or("", |p| p.as_str());
                let pq = pq.strip_prefix('/').unwrap_or_default();
                let (name, rest) = pq.split_at(pq.find(['/', '?']).unwrap_or(pq.len()));
                let (name, uri) = (name.to_owned(), format!("/{}", rest.strip_prefix('/').unwrap_or(rest)));
                *req.uri_mut() = uri.parse()?;
                req.headers_mut().insert(FORWARDED_PREFIX, format!("/{name}").parse()?);
                name
            }
        };
        let Some(release) = s.0.apps.get(&name) else { return Ok(None) };
        if let Some((r, app)) = self.apps.lock().await.get(&name)
            && r == release
        {
            return Ok(Some((name, app.clone())));
        }
        let release = release.clone();
        drop(s);
        let r = state::release(&*self.store, &release).await?;
        let mut config = r.config;
        for (key, sealed) in r.secrets {
            let Some(identity) = &self.identity else { bail!("{name} has secrets, and there is no identity") };
            config.insert(key, String::from_utf8(age::decrypt(identity, sealed.as_bytes())?)?);
        }
        let wasm = state::fetch(&*self.store, BLOBS, &r.component, BLOB_MAX).await?;
        let app = self.engine.load(&name, wasm, config, &r.allowed_outbound_hosts)?;
        self.apps.lock().await.insert(name.clone(), (release, app.clone()));
        Ok(Some((name, app)))
    }
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
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(apps: &Apps, req: hyper::http::request::Builder) -> (StatusCode, String) {
        let res = apps.handle(req.body(Empty::<Bytes>::new()).unwrap()).await;
        (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().into()).unwrap())
    }

    async fn ok(apps: &Apps, uri: &str) -> String {
        let (status, body) = get(apps, Request::get(uri)).await;
        assert_eq!(status, 200, "{uri}: {body}");
        body
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

    /// Two apps deployed together, served by path and by domain, with a secret, then one rolled back.
    #[tokio::test]
    async fn deploys_serves_and_rolls_back() {
        let store = Arc::new(InMemory::new());
        cli::deploy(&*store, &["tests/app", "tests/kv"]).await.unwrap();
        let identity = Identity::generate();
        let recipient = identity.to_public().to_string();
        cli::set_secret(&*store, "kv", "token", "s3cret", &recipient).await.unwrap();
        assert!(cli::set_secret(&*store, "kv", "greeting", "x", &recipient).await.is_err()); // it is config
        cli::deploy(&*store, &["tests/kv"]).await.unwrap(); // keeps the secret
        assert_eq!(cli::secrets(&*store, "kv").await.unwrap(), ["token"]); // with no identity at all
        state::update(&*store, async |s| Ok(_ = s.domains.insert("hello.test".into(), "hello".into()))).await.unwrap();

        let apps = Apps::install(store.clone(), Some(identity)).await.unwrap();
        let Apps::Install(install) = &apps else { unreachable!() };
        let etag = state::read(&*store, None).await.unwrap().unwrap().1.e_tag;
        assert!(state::read(&*store, etag).await.unwrap().is_none()); // a recheck of unchanged state reads nothing
        assert_eq!(ok(&apps, "/hello/").await, "hello");
        assert_eq!(ok(&apps, "/kv/config?key=token").await, r#"{"ok":"s3cret"}"#);
        assert_eq!(ok(&apps, "/kv/config?key=greeting").await, r#"{"ok":"hi"}"#);
        assert_eq!(get(&apps, Request::get("/kv/config").header(HOST, "hello.test:3000")).await.1, "hello");
        assert_eq!(get(&apps, Request::get("/nope/")).await.0, 404);
        let mut req = Request::get("/").header(HOST, "hello.test").header(FORWARDED_PREFIX, "/x").body(()).unwrap();
        install.route(&mut req).await.unwrap();
        assert!(req.headers().get(FORWARDED_PREFIX).is_none()); // a client can't set it
        let mut req = Request::get("/kv/config?key=k").body(()).unwrap();
        install.route(&mut req).await.unwrap();
        assert_eq!(req.uri(), "/config?key=k");
        assert_eq!(req.headers()[FORWARDED_PREFIX], "/kv");

        cli::rollback(&*store, "kv").await.unwrap(); // to before the second deploy, which still has the secret
        cli::rollback(&*store, "kv").await.unwrap(); // to before the secret
        tokio::time::sleep(state::FRESH).await;
        assert_eq!(ok(&apps, "/kv/config?key=token").await, r#"{"ok":null}"#);
        assert!(cli::rollback(&*store, "kv").await.is_err()); // the first release has no parent
    }

    #[tokio::test]
    async fn refuses_a_blob_that_does_not_match_its_hash() {
        let store = Arc::new(InMemory::new());
        cli::deploy(&*store, &["tests/app"]).await.unwrap();
        let hash = state::read(&*store, None).await.unwrap().unwrap().0.apps.remove("hello").unwrap();
        let blob = format!("{}/{}", state::BLOBS, state::release(&*store, &hash).await.unwrap().component);
        store.put(&blob.into(), std::fs::read("tests/fixtures/hello-p2.wasm").unwrap().into()).await.unwrap();
        let apps = Apps::install(store, None).await.unwrap();
        let Apps::Install(install) = &apps else { unreachable!() };
        let e = install.route(&mut Request::get("/hello/").body(()).unwrap()).await.err().unwrap();
        assert!(e.to_string().contains("does not match its hash"), "{e}");
    }

    /// Calls that wait let both deploys read the state before either writes it, so one loses the swap and retries.
    #[tokio::test]
    async fn concurrent_deploys_both_land() {
        let ms = Duration::from_millis(10);
        let config = ThrottleConfig { wait_get_per_call: ms, wait_put_per_call: ms, ..Default::default() };
        let store = ThrottledStore::new(InMemory::new(), config);
        cli::deploy(&store, &["tests/app"]).await.unwrap(); // so the race is over a conditional update, not a create
        tokio::try_join!(cli::deploy(&store, &["tests/app"]), cli::deploy(&store, &["tests/kv"])).unwrap();
        let apps = state::read(&store, None).await.unwrap().unwrap().0.apps;
        assert_eq!(apps.keys().collect::<Vec<_>>(), ["hello", "kv"]);
    }
}
