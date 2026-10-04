//! For the M2 tests: a store that logs its calls, and helpers to load a fixture and call it.
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use http_body_util::{BodyExt, Empty};
use object_store::{path::Path, prefix::PrefixStore, *};
use std::sync::{Arc, LazyLock, Mutex, atomic::AtomicUsize, atomic::Ordering::Relaxed};
use std::{env, fmt, fs, time::SystemTime};
use tric::{App, Engine};

/// A store that logs its gets (`get kv/app/s/k`), puts and lists, and passes everything else through: to an `InMemory`
/// store or, with `TRIC_TEST_STORE` set to a bucket like `s3://NAME`, to a prefix of that bucket that it alone uses
/// and leaves as it is.
#[derive(Debug)]
pub struct Counting {
    pub inner: Arc<dyn ObjectStore>,
    log: Mutex<Vec<String>>,
}

impl Default for Counting {
    fn default() -> Self {
        static RUN: LazyLock<u128> = LazyLock::new(|| SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos());
        static STORES: AtomicUsize = AtomicUsize::new(0);
        let inner: Arc<dyn ObjectStore> = match env::var("TRIC_TEST_STORE") {
            Ok(url) => Arc::new(PrefixStore::new(
                aws::AmazonS3Builder::from_env().with_url(url).build().unwrap(),
                format!("test/{}-{}", *RUN, STORES.fetch_add(1, Relaxed)),
            )),
            Err(_) => Arc::new(memory::InMemory::new()),
        };
        Self { inner, log: Mutex::default() }
    }
}

impl Counting {
    /// The calls since the last `take`.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
    fn note(&self, call: String) {
        self.log.lock().unwrap().push(call);
    }
}
impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting")
    }
}

#[async_trait]
impl ObjectStore for Counting {
    async fn put_opts(&self, to: &Path, payload: PutPayload, opts: PutOptions) -> Result<PutResult> {
        self.note(format!("put {to}"));
        self.inner.put_opts(to, payload, opts).await
    }
    async fn put_multipart_opts(&self, to: &Path, opts: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(to, opts).await
    }
    async fn get_opts(&self, at: &Path, opts: GetOptions) -> Result<GetResult> {
        self.note(format!("get {at}{}", if opts.if_none_match.is_some() { " if-none-match" } else { "" }));
        self.inner.get_opts(at, opts).await
    }
    fn delete_stream(&self, at: BoxStream<'static, Result<Path>>) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(at)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.note(format!("list {}", prefix.map(Path::as_ref).unwrap_or_default()));
        self.inner.list(prefix)
    }
    // Passed through, rather than left to the default that lists from the start, so a bucket's own offset is tested.
    fn list_with_offset(&self, prefix: Option<&Path>, offset: &Path) -> BoxStream<'static, Result<ObjectMeta>> {
        self.note(format!("list {}", prefix.map(Path::as_ref).unwrap_or_default()));
        self.inner.list_with_offset(prefix, offset)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}

/// The app `app` from `tests/fixtures`, with `wasi:config` of `greeting` and `empty`, and its `wasi:keyvalue` data in
/// `store` under `kv/app`. Every app shares one engine, as they do in a host.
pub fn load(store: &Arc<Counting>, fixture: &str, allow: &[&str]) -> App {
    static ENGINE: LazyLock<Engine> = LazyLock::new(|| Engine::new().unwrap());
    let config = [("greeting".to_string(), "hi".to_string()), ("empty".to_string(), String::new())];
    let allow: Vec<String> = allow.iter().map(|a| a.to_string()).collect();
    let (kv, wasm) = (PrefixStore::new(store.clone(), "kv/app"), fs::read(format!("tests/fixtures/{fixture}.wasm")));
    ENGINE.load("app", Arc::new(kv), &ENGINE.compile(&wasm.unwrap()).unwrap(), config.into(), &allow).unwrap()
}

pub async fn get(app: &App, path: &str) -> (u16, String) {
    let res = app.handle(hyper::Request::get(format!("http://app{path}")).body(Empty::<Bytes>::new()).unwrap()).await;
    (res.status().as_u16(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
}
