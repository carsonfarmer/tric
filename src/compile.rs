//! Native code: the compile function compiles each component once, and hosts load what it made rather than compile it
//! themselves. The bucket of native code is trusted, as loading its code runs it: only the compile function writes it,
//! and only from a component it checked against its hash.
use crate::state::{self, BLOBS};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body, Bytes};
use hyper::{Method, Request, StatusCode};
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutPayload};
use serde_json::Value;
use std::{error::Error as StdError, sync::Arc};
use tempfile::NamedTempFile;
use tokio::task::spawn_blocking;
use torpor::Engine;
use wasmtime::component::Component;
use wasmtime::{Error, Result, ensure, error::Context};
use wasmtime_wasi_http::handler::Response;

/// Where the Lambda Web Adapter posts the events that are not HTTP requests, such as the bucket's.
pub const EVENTS: &str = "/events";
const EVENT_MAX: usize = 1 << 20; // S3 sends a record of about 1 KiB per event
const ZSTD_LEVEL: i32 = 3; // zstd's own default

/// The code of `app`'s component `hash`: its native code from `native`, or else a compile here, which then asks the
/// compile function for native code, so the next host to load it need not compile it.
pub async fn component(
    store: &dyn ObjectStore,
    native: Option<&dyn ObjectStore>,
    engine: &Arc<Engine>,
    app: &str,
    hash: &str,
) -> Result<Component> {
    if let Some(native) = native {
        match load(native, engine, app, hash).await {
            Ok(code) => return Ok(code),
            Err(e) => tracing::warn!(app, "compiling it here, as its native code did not load: {e}"),
        }
    }
    let wasm = state::fetch(store, app, BLOBS, hash).await?;
    let engine = engine.clone();
    let code = spawn_blocking(move || engine.compile(&wasm)).await??;
    // Only once it compiles here, so the compile function is never asked for what cannot compile.
    if native.is_some()
        && let Err(e) = store.put(&state::marker(app, hash)?.into(), PutPayload::new()).await
    {
        tracing::warn!(app, "could not ask for its native code: {e}");
    }
    Ok(code)
}

/// Loads the native code of `app`'s component `hash` from `native`.
async fn load(native: &dyn ObjectStore, engine: &Arc<Engine>, app: &str, hash: &str) -> Result<Component> {
    let zst = native.get(&state::native(app, hash, &engine.compat())?.into()).await?.bytes().await?;
    let engine = engine.clone();
    spawn_blocking(move || {
        let mut file = NamedTempFile::new()?;
        zstd::stream::copy_decode(&*zst, &mut file)?;
        // SAFETY: the code is what `precompile` made, as only the compile function writes `native`. The file is new and
        // this host's own, and is deleted on return, which leaves the component's mapping of it in place.
        unsafe { engine.native(file.path()) }
    })
    .await?
}

/// The compile function, which makes the native code that markers ask for.
pub struct Worker {
    store: Arc<dyn ObjectStore>,
    native: Arc<dyn ObjectStore>,
    engine: Arc<Engine>,
}

impl Worker {
    pub fn new(store: Arc<dyn ObjectStore>, native: Arc<dyn ObjectStore>) -> Result<Self> {
        Ok(Self { store, native, engine: Engine::new()?.into() })
    }

    /// Serves the Lambda Web Adapter, which posts the bucket's events to `EVENTS`. Anything else, like its readiness
    /// check, gets a 404.
    pub async fn handle<B>(self: Arc<Self>, req: Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send,
        B::Error: Into<Box<dyn StdError + Send + Sync>>,
    {
        let code = if req.method() != Method::POST || req.uri().path() != EVENTS {
            StatusCode::NOT_FOUND
        } else if let Err(e) = self.events(req.into_body()).await {
            tracing::warn!("{e}");
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        };
        hyper::Response::builder().status(code).body(Default::default()).unwrap()
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
                tracing::warn!(key, "{e}");
                failed += 1;
            }
        }
        ensure!(failed == 0, "{failed} of {} markers failed", records.len());
        Ok(())
    }

    /// Makes the native code that the marker `key` asks for, unless it is there already, then deletes the marker. A
    /// failure leaves the marker, so the next host that compiles the component asks again.
    async fn work(&self, key: &str) -> Result<()> {
        let (app, hash) = state::marked(key).with_context(|| format!("{key:?} is not a marker"))?;
        let path = state::native(app, hash, &self.engine.compat())?.into();
        match self.native.head(&path).await {
            Ok(_) => {}
            Err(E::NotFound { .. }) => {
                let wasm = state::fetch(&*self.store, app, BLOBS, hash).await?;
                let engine = self.engine.clone();
                let zst =
                    spawn_blocking(move || Ok::<_, Error>(zstd::encode_all(&*engine.precompile(&wasm)?, ZSTD_LEVEL)?));
                self.native.put(&path, zst.await??.into()).await?;
                tracing::info!(app, hash, "compiled");
            }
            Err(e) => return Err(e.into()),
        }
        self.store.delete(&key.into()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;
    use futures_util::StreamExt;
    use http_body_util::Empty;
    use object_store::memory::InMemory;
    use std::{collections::BTreeMap, fs, time::Duration};
    use tokio::time::sleep;

    /// What `app` answers at `/` when a host loads its component `hash`.
    async fn serve(
        store: &Arc<InMemory>,
        native: &Arc<InMemory>,
        engine: &Arc<Engine>,
        app: &str,
        hash: &str,
    ) -> String {
        let code = component(&**store, Some(&**native), engine, app, hash).await.unwrap();
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

    /// A host loads only native code from the native bucket, made from the app's own component by an engine like its
    /// own. Missing that, it compiles the component itself and asks for native code, which the next load uses.
    #[tokio::test]
    async fn loads_only_its_own_native_code() {
        let (store, native) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let (app, id) = cli::publish(&*store, "tests/app".as_ref(), false).await.unwrap();
        let hash = state::release(&*store, &app, &id).await.unwrap().component;
        let engine = Arc::new(Engine::new().unwrap());
        let probe = engine.precompile(&fs::read("tests/fixtures/probe-p3.wasm").unwrap()).unwrap();
        let probe = Bytes::from(zstd::encode_all(&*probe, ZSTD_LEVEL).unwrap());
        let other = state::native(&app, &hash, "0000000000000000").unwrap(); // as an older engine's
        native.put(&other.into(), probe.clone().into()).await.unwrap();
        let ours = state::native(&app, &hash, &engine.compat()).unwrap();
        store.put(&ours.as_str().into(), probe.into()).await.unwrap(); // in the app bucket, which teams write
        assert_eq!(serve(&store, &native, &engine, &app, &hash).await, "hello"); // not the probe's "ok"

        let marker = state::marker(&app, &hash).unwrap();
        store.head(&marker.as_str().into()).await.unwrap();
        Worker::new(store.clone(), native.clone()).unwrap().work(&marker).await.unwrap();
        native.head(&ours.as_str().into()).await.unwrap();
        assert!(store.head(&marker.as_str().into()).await.is_err());
        store.delete(&state::object(&app, BLOBS, &hash).unwrap().into()).await.unwrap(); // so only native code serves it
        assert_eq!(serve(&store, &native, &engine, &app, &hash).await, "hello");
        assert!(store.head(&marker.as_str().into()).await.is_err()); // and it asks for nothing
    }

    /// `precompile` waits until the compile function has made the native code, which an event asked it for.
    #[tokio::test]
    async fn publish_waits_for_native_code() {
        let (store, native) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let (app, id) = cli::publish(&*store, "tests/app".as_ref(), false).await.unwrap();
        let hash = state::release(&*store, &app, &id).await.unwrap().component;
        let marker = state::marker(&app, &hash).unwrap();
        let worker = Arc::new(Worker::new(store.clone(), native.clone()).unwrap());
        let (s, m) = (store.clone(), marker.clone());
        tokio::spawn(async move {
            while s.head(&m.as_str().into()).await.is_err() {
                sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(worker.handle(event(&[&m])).await.status(), StatusCode::OK);
        });
        cli::precompile(&*store, &app, &id).await.unwrap();
        native.head(&state::native(&app, &hash, &Engine::new().unwrap().compat()).unwrap().into()).await.unwrap();
    }

    /// A failed compile leaves its marker, so the next host to compile the component asks again, and makes nothing.
    #[tokio::test]
    async fn a_failed_compile_leaves_its_marker() {
        let (store, native) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let hash = state::add(&*store, "app", BLOBS, "not wasm".into()).await.unwrap();
        let marker = state::marker("app", &hash).unwrap();
        store.put(&marker.as_str().into(), PutPayload::new()).await.unwrap();
        let worker = Arc::new(Worker::new(store.clone(), native.clone()).unwrap());
        for keys in [&[marker.as_str()][..], &["compile/app"], &["apps/app/current"]] {
            assert_eq!(worker.clone().handle(event(keys)).await.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        store.head(&marker.as_str().into()).await.unwrap();
        assert!(native.list(None).next().await.is_none());
        assert_eq!(worker.handle(Request::get("/").body(String::new()).unwrap()).await.status(), StatusCode::NOT_FOUND);
    }
}
