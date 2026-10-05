//! Native code: the compile function compiles each release's component once, into the release's folder, and hosts load
//! what it made rather than compile it themselves. Loading native code runs it, so only the compile function may write
//! it, and an app loads only what was made from its own folder: its own code, run in the sandbox it was compiled with.
use crate::serve::status;
use crate::state::{self, COMPONENT, COMPONENT_MAX, Entry};
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body, Bytes};
use hyper::{Method, Request, StatusCode};
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutPayload};
use serde_json::Value;
use std::{error::Error as StdError, io, sync::Arc, time::Duration};
use tempfile::NamedTempFile;
use tokio::{task::spawn_blocking, time::timeout};
use tokio_util::io::{StreamReader, SyncIoBridge};
use tric::Engine;
use wasmtime::component::Component;
use wasmtime::{Error, Result, ensure, error::Context};
use wasmtime_wasi_http::handler::Response;

/// Where the Lambda Web Adapter posts the events that are not HTTP requests, such as the bucket's.
pub const EVENTS: &str = "/events";
const EVENT_MAX: usize = 1 << 20; // S3 sends a record of about 1 KiB per event
const ZSTD_LEVEL: i32 = 3; // zstd's own default
const ASK_MAX: Duration = Duration::from_secs(1); // as the load that asks waits for it

/// The code of `app`'s release `live`: with `native`, the native code that the compile function made of it, or else a
/// compile here, which then asks the compile function for native code, so the next host to load it need not compile it.
pub async fn component(
    store: &dyn ObjectStore,
    native: bool,
    engine: &Arc<Engine>,
    app: &str,
    live: &Entry,
) -> Result<Component> {
    if native {
        match load(store, engine, app, live.n).await {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => tracing::info!(app, "compiling it here, as it has no native code yet"),
            Err(e) => tracing::warn!(app, "compiling it here, as its native code did not load: {e:#}"),
        }
    }
    let wasm = state::component(store, app, live).await?;
    let engine = engine.clone();
    let code = spawn_blocking(move || engine.compile(&wasm)).await??;
    // Only once it compiles here, so the compile function is never asked for what cannot compile.
    if native && let Err(e) = ask(store, app, live.n).await {
        tracing::warn!(app, "could not ask for its native code: {e:#}");
    }
    Ok(code)
}

/// Asks the compile function for the native code of `app`'s folder `n`, giving up after `ASK_MAX`.
async fn ask(store: &dyn ObjectStore, app: &str, n: u64) -> Result<()> {
    timeout(ASK_MAX, store.put(&state::marker(app, n)?.into(), PutPayload::new())).await??;
    Ok(())
}

/// The native code of `app`'s folder `n`, or `None` if it has none. It is decoded as it arrives.
async fn load(store: &dyn ObjectStore, engine: &Arc<Engine>, app: &str, n: u64) -> Result<Option<Component>> {
    let zst = match store.get(&state::native(app, n, &engine.compat())?.into()).await {
        Err(E::NotFound { .. }) => return Ok(None),
        r => r?.into_stream().map_err(io::Error::other),
    };
    let mut zst = SyncIoBridge::new(StreamReader::new(zst));
    let engine = engine.clone();
    let code = spawn_blocking(move || {
        let mut file = NamedTempFile::new()?;
        zstd::stream::copy_decode(&mut zst, &mut file)?;
        // SAFETY: this is what `Worker::work` made, as only the compile function can write native code (see `state`).
        // The file is new and this host's own, and is deleted on return, which leaves the component's mapping in place.
        unsafe { engine.native(file.path()) }
    });
    Ok(Some(code.await??))
}

/// The compile function, which makes the native code that markers ask for.
pub struct Worker {
    store: Arc<dyn ObjectStore>,
    engine: Arc<Engine>,
}

impl Worker {
    pub fn new(store: Arc<dyn ObjectStore>) -> Result<Self> {
        Ok(Self { store, engine: Engine::new()?.into() })
    }

    /// Serves the Lambda Web Adapter, which posts the bucket's events to `EVENTS`. Anything else, like its readiness
    /// check, gets a 404.
    pub async fn handle<B>(self: Arc<Self>, req: Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        status(if req.method() != Method::POST || req.uri().path() != EVENTS {
            StatusCode::NOT_FOUND
        } else if let Err(e) = self.events(req.into_body()).await {
            tracing::warn!("{e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        })
    }

    /// Works through each marker that the bucket event `body` names, and fails if any did.
    async fn events<B>(&self, body: B) -> Result<()>
    where
        B: Body<Data = Bytes>,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        let body = Limited::new(body, EVENT_MAX).collect().await.map_err(Error::from_boxed)?.to_bytes();
        let event: Value = serde_json::from_slice(&body)?;
        let records = event["Records"].as_array().context("an event has `Records`")?;
        let mut failed = 0;
        for record in records {
            let key = record.pointer("/s3/object/key").and_then(Value::as_str).unwrap_or_default();
            if let Err(e) = self.work(key).await {
                tracing::warn!(key, "{e:#}");
                failed += 1;
            }
        }
        ensure!(failed == 0, "{failed} of {} markers failed", records.len());
        Ok(())
    }

    /// Makes the native code that the marker `key` asks for, unless it is there already or its folder is gone, then
    /// deletes the marker. A failure leaves the marker, so the next host that compiles the component asks again. The
    /// component is not checked against its hash: any Wasm is safe to compile, and what it makes runs only as its app.
    async fn work(&self, key: &str) -> Result<()> {
        let (app, n) = state::marked(key).with_context(|| format!("{key:?} is not a marker"))?;
        let path = state::native(app, n, &self.engine.compat())?.into();
        if !state::exists(&*self.store, &path).await? {
            match state::read(&*self.store, &state::file(app, n, COMPONENT)?, COMPONENT_MAX).await? {
                None => tracing::info!(app, n, "made nothing, as its folder is gone"),
                Some((wasm, _)) => {
                    let engine = self.engine.clone();
                    let zst = spawn_blocking(move || {
                        Ok::<_, Error>(zstd::encode_all(&*engine.precompile(&wasm)?, ZSTD_LEVEL)?)
                    });
                    self.store.put(&path, zst.await??.into()).await?;
                    tracing::info!(app, n, "compiled");
                }
            }
        }
        Ok(self.store.delete(&key.into()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
    use futures_util::StreamExt;
    use http_body_util::Empty;
    use object_store::memory::InMemory;
    use std::{collections::BTreeMap, fs};
    use tokio::time::sleep;

    /// What `app` answers at `/` when a host loads its release `live`.
    async fn serve(store: &InMemory, engine: &Arc<Engine>, app: &str, live: &Entry) -> String {
        let code = component(store, true, engine, app, live).await.unwrap();
        let app = engine.load(app, Arc::new(InMemory::new()), &code, BTreeMap::new(), &[]).unwrap();
        let res = app.handle(Request::get("http://app/").body(Empty::<Bytes>::new()).unwrap()).await;
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().into()).unwrap()
    }

    /// The bucket event that S3 sends for `keys`.
    fn event(keys: &[&str]) -> Request<String> {
        let records: Vec<_> =
            keys.iter().map(|key| serde_json::json!({ "s3": { "object": { "key": key } } })).collect();
        Request::post(EVENTS).body(serde_json::json!({ "Records": records }).to_string()).unwrap()
    }

    /// A host loads only sound native code, made by an engine like its own in its own release's folder. Missing that,
    /// it compiles the component itself and asks for native code, which the next load uses.
    #[tokio::test]
    async fn loads_only_its_own_native_code() {
        let store = Arc::new(InMemory::new());
        let (app, live) = cli::publish(&*store, "tests/app".as_ref(), false).await.unwrap();
        let engine = Arc::new(Engine::new().unwrap());
        let probe = engine.precompile(&fs::read("tests/fixtures/probe-p3.wasm").unwrap()).unwrap();
        let probe = Bytes::from(zstd::encode_all(&*probe, ZSTD_LEVEL).unwrap());
        let native = |app, n, compat: &str| state::native(app, n, compat).unwrap();
        let ours = native(&app, live.n, &engine.compat());
        let planted = [
            native(&app, live.n, "0000000000000000"),   // an older engine's
            native(&app, live.n + 1, &engine.compat()), // another release's
            native("other", live.n, &engine.compat()),  // another app's
        ];
        for key in planted {
            store.put(&key.into(), probe.clone().into()).await.unwrap();
        }
        assert_eq!(serve(&store, &engine, &app, &live).await, "hello"); // not the probe's "ok"
        store.put(&ours.as_str().into(), "not zstd".into()).await.unwrap();
        assert_eq!(serve(&store, &engine, &app, &live).await, "hello");
        store.delete(&ours.as_str().into()).await.unwrap(); // which is how bad native code is fixed

        let marker = state::marker(&app, live.n).unwrap();
        store.head(&marker.as_str().into()).await.unwrap();
        let worker = Worker::new(store.clone()).unwrap();
        worker.work(&marker).await.unwrap();
        store.head(&ours.as_str().into()).await.unwrap();
        assert!(store.head(&marker.as_str().into()).await.is_err());
        store.delete(&state::file(&app, live.n, COMPONENT).unwrap().into()).await.unwrap(); // leaving only native code
        assert_eq!(serve(&store, &engine, &app, &live).await, "hello");
        assert!(store.head(&marker.as_str().into()).await.is_err()); // and it asks for nothing

        // as a publish again would ask, and as a marker of a folder that is gone does
        for marker in [marker, state::marker(&app, live.n + 2).unwrap()] {
            store.put(&marker.as_str().into(), PutPayload::new()).await.unwrap();
            worker.work(&marker).await.unwrap(); // with no component to read
            assert!(store.head(&marker.as_str().into()).await.is_err());
        }
        assert!(store.head(&native(&app, live.n + 2, &engine.compat()).into()).await.is_err());
    }

    /// `precompile` waits until the compile function has made the native code, which an event asked it for.
    #[tokio::test]
    async fn publish_waits_for_native_code() {
        let store = Arc::new(InMemory::new());
        let (app, live) = cli::publish(&*store, "tests/app".as_ref(), false).await.unwrap();
        let marker = state::marker(&app, live.n).unwrap();
        let worker = Arc::new(Worker::new(store.clone()).unwrap());
        let work = async {
            while store.head(&marker.as_str().into()).await.is_err() {
                sleep(Duration::from_millis(10)).await;
            }
            let status = worker.handle(event(&[&marker])).await.status();
            ensure!(status == StatusCode::OK, "the event got a {status}"); // which ends the wait at once
            Ok(())
        };
        tokio::try_join!(cli::precompile(&*store, &app, &live), work).unwrap();
        store.head(&state::native(&app, live.n, &Engine::new().unwrap().compat()).unwrap().into()).await.unwrap();
    }

    /// A failed compile leaves its marker, so the next host to compile the component asks again, and makes nothing.
    #[tokio::test]
    async fn a_failed_compile_leaves_its_marker() {
        let store = Arc::new(InMemory::new());
        store.put(&state::file("app", 0, COMPONENT).unwrap().into(), "not wasm".into()).await.unwrap();
        let marker = state::marker("app", 0).unwrap();
        store.put(&marker.as_str().into(), PutPayload::new()).await.unwrap();
        let worker = Arc::new(Worker::new(store.clone()).unwrap());
        for keys in [&[marker.as_str()][..], &["compile/app"], &["apps/app/current"]] {
            assert_eq!(worker.clone().handle(event(keys)).await.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        store.head(&marker.as_str().into()).await.unwrap();
        assert_eq!(store.list(None).count().await, 2); // the component and the marker
        assert_eq!(worker.handle(Request::get("/").body(String::new()).unwrap()).await.status(), StatusCode::NOT_FOUND);
    }
}
