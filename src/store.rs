//! The bucket: one object store, in memory for `tric dev` and S3 everywhere else. S3 answers 403 for a missing key to a
//! caller that cannot list, so a 403 reads as not found.
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use object_store::aws::{AmazonS3, AmazonS3Builder, AwsCredential};
use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{
    Certificate, ClientOptions, ListResult, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, RetryConfig,
    StaticCredentialProvider,
};
use object_store::{UpdateVersion, memory::InMemory, path::Path};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tric_tree::Cache;
use wasmtime::{Result, ensure, error::Context};
use webpki_root_certs::TLS_SERVER_ROOT_CERTS;

#[derive(Clone)]
pub struct Store {
    pub(crate) inner: Arc<dyn ObjectStore>,
    /// The client for conditional puts, where it is not `inner`: on S3, one that sends each once (see [`S3`]).
    pub(crate) once: Option<AmazonS3>,
    /// Whether the bucket keeps versions, so a value read by its version id outlives its delete. The memory store
    /// keeps none, and never deletes.
    pub versioned: bool,
    /// The tree nodes read, which a process keeps for the next request: those of S3 are shared by all its stores.
    pub(crate) cache: Arc<Cache>,
}

/// `apps/<app>/<parts>`.
pub fn app(app: &str, parts: &[&str]) -> Path {
    Path::from_iter(["apps", app].into_iter().chain(parts.iter().copied()))
}

/// The SHA-256 of `bytes`, in hex.
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 128 random bits, in hex.
pub fn random() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// How many times [`Store::put`] tries a conditional put, while it fails for a reason that is not its condition.
pub(crate) const TRIES: u32 = 4;

/// The bucket `bucket` on S3, as the environment's `AWS_*` variables configure it, with `creds` if given, and else
/// the environment's.
pub fn s3(bucket: &str, creds: Option<AwsCredential>) -> Result<S3> {
    let mut s3 = AmazonS3Builder::from_env().with_bucket_name(bucket);
    if let Some(creds) = creds {
        s3 = s3.with_credentials(Arc::new(StaticCredentialProvider::new(creds)));
    }
    S3::new(s3)
}

/// A bucket on S3, with a second client for its conditional puts, which sends each once; the rest go as object_store
/// sends them, retries and all. object_store tries a put again after a 5xx, a 429, a 408 or a connection that failed,
/// and a conditional put that landed but whose answer was lost then fails its own condition: the 412 would read as
/// another's write. So [`Store::put`] tries those again itself, and knows which failure came after a try that may have
/// landed.
pub struct S3 {
    s3: AmazonS3,
    once: AmazonS3, // with no retries, and the same credentials and connections
}

impl S3 {
    pub fn new(s3: AmazonS3Builder) -> Result<Self> {
        let s3 = s3.with_http_connector(Shared);
        let (many, once) = (s3.clone().build()?, s3);
        let once = once.with_credentials(many.credentials().clone());
        let once = once.with_retry(RetryConfig { max_retries: 0, ..Default::default() }).build()?;
        Ok(Self { s3: many, once })
    }
}

/// The client that retries, for what reads its configuration, as the credentials.
impl std::ops::Deref for S3 {
    type Target = AmazonS3;

    fn deref(&self) -> &AmazonS3 {
        &self.s3
    }
}

/// One HTTP client for each set of options, so everything at one endpoint shares its connections, whatever the
/// credentials, and the roots are loaded once. `ClientOptions` has no `Eq`, so its `Debug` is the key: that has every
/// field but a certificate added in code, and the roots are added after.
#[derive(Debug)]
pub struct Shared;

static CLIENTS: LazyLock<Mutex<HashMap<String, HttpClient>>> = LazyLock::new(Mutex::default);

impl HttpConnector for Shared {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let key = format!("{options:?}");
        let mut clients = CLIENTS.lock().unwrap();
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        // Mozilla's roots, as `outbound` trusts, not the system's: those took ~30 ms more of a cold start on Lambda.
        let mut tls = options.clone().with_no_system_certificates(true);
        for der in TLS_SERVER_ROOT_CERTS {
            tls = tls.with_root_certificate(Certificate::from_der(der)?);
        }
        let client = ReqwestConnector::default().connect(&tls)?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// `r`, with a missing object as `None`.
fn found<T>(r: object_store::Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(object_store::Error::NotFound { .. } | object_store::Error::PermissionDenied { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Store {
    pub fn memory() -> Self {
        Self { inner: Arc::new(InMemory::new()), once: None, versioned: false, cache: Arc::default() }
    }

    /// `s3`, whose bucket must keep versions.
    pub fn s3(s3: S3) -> Self {
        static SHARED: LazyLock<Arc<Cache>> = LazyLock::new(Default::default);
        Self { inner: Arc::new(s3.s3), once: Some(s3.once), versioned: true, cache: SHARED.clone() }
    }

    /// Whether there is an object at `path`.
    pub async fn head(&self, path: &Path) -> Result<bool> {
        Ok(found(self.inner.head(path).await)?.is_some())
    }

    /// What is under `prefix`, one level down.
    pub async fn list(&self, prefix: &Path) -> Result<ListResult> {
        Ok(self.inner.list_with_delimiter(Some(prefix)).await?)
    }

    /// The object at `path`, if there is one; it must be `max` bytes or less.
    pub async fn get(&self, path: &Path, max: u64) -> Result<Option<(Bytes, UpdateVersion)>> {
        let Some(got) = found(self.inner.get(path).await)? else { return Ok(None) };
        ensure!(got.meta.size <= max, "{path} is over {max} bytes");
        let version = UpdateVersion { e_tag: got.meta.e_tag.clone(), version: got.meta.version.clone() };
        Ok(Some((got.bytes().await?, version)))
    }

    pub async fn json<T: DeserializeOwned>(&self, path: &Path, max: u64) -> Result<Option<(T, UpdateVersion)>> {
        let Some((bytes, version)) = self.get(path, max).await? else { return Ok(None) };
        Ok(Some((serde_json::from_slice(&bytes).with_context(|| format!("{path}"))?, version)))
    }

    /// Writes `bytes` at `path` as `mode` allows, and returns the new version, or `None` if `mode`'s condition failed,
    /// or another conditional write was under way (a 409), and nothing was written. A conditional put that fails
    /// otherwise, as on a 5xx, may have landed: it is tried again, as it was, up to `TRIES` times. Once a try may have
    /// landed, a later one that is refused may have been refused for that try's write, so that is an error too, and
    /// what the put did is for the caller to read.
    pub async fn put(&self, path: &Path, bytes: Bytes, mode: PutMode) -> Result<Option<UpdateVersion>> {
        let (client, tries): (&dyn ObjectStore, _) = match (&mode, &self.once) {
            (PutMode::Overwrite, _) => (&*self.inner, 1),
            (_, Some(once)) => (once, TRIES),
            (_, None) => (&*self.inner, TRIES),
        };
        let mut tried = 0;
        loop {
            tried += 1;
            match client.put_opts(path, bytes.clone().into(), mode.clone().into()).await {
                Ok(r) => return Ok(Some(UpdateVersion { e_tag: r.e_tag, version: r.version })),
                Err(
                    e @ (object_store::Error::Precondition { .. }
                    | object_store::Error::AlreadyExists { .. }
                    | object_store::Error::NotFound { .. }),
                ) => {
                    return match tried {
                        1 => Ok(None),
                        _ => Err(e).with_context(|| format!("{path}: try {tried}, after one that may have landed")),
                    };
                }
                Err(e @ object_store::Error::Generic { .. }) if tried < tries => {
                    tracing::warn!(%path, tried, "put: {e}");
                    let ms = rand::random_range(50..=100) << (tried - 1); // 50 to 100 ms, then twice as long each time
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Deletes the object at `path`, in a versioned bucket only: elsewhere a snapshot may still read it.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        match self.versioned {
            true => found(self.inner.delete(path).await).map(drop),
            false => Ok(()),
        }
    }

    /// Deletes the objects at `paths`, in a versioned bucket only: elsewhere a snapshot may still read them. One that
    /// is gone already, or that the role cannot see, is as good as deleted. The others are logged, not returned: what
    /// a delete leaves behind is only space, and the sweep reclaims it. Returns how many were deleted.
    pub async fn delete_many(&self, paths: Vec<Path>) -> usize {
        if !self.versioned || paths.is_empty() {
            return 0;
        }
        let mut gone = self.inner.delete_stream(stream::iter(paths.into_iter().map(Ok)).boxed());
        let mut deleted = 0;
        while let Some(r) = gone.next().await {
            match r {
                Ok(_) => deleted += 1,
                Err(object_store::Error::NotFound { .. } | object_store::Error::PermissionDenied { .. }) => {}
                Err(e) => tracing::warn!("delete: {e}"),
            }
        }
        deleted
    }

    /// Every object under `prefix`, however deep, in key order.
    pub fn objects(&self, prefix: &Path) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(Some(prefix)).map(|r| r.map_err(Into::into)).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::counting::counting;
    use futures_util::TryStreamExt;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    async fn put(store: &Store, path: &Path) {
        store.put(path, Bytes::from_static(b"x"), PutMode::Overwrite).await.unwrap();
    }

    #[tokio::test]
    async fn delete_many_counts_what_it_deleted_and_only_in_a_versioned_bucket() {
        let paths: Vec<_> = (0..5).map(|i| app("a", &["values", "n", &format!("{i:032x}")])).collect();
        let (store, counts) = counting();
        for path in &paths {
            put(&store, path).await;
        }
        assert_eq!(store.delete_many(vec![]).await, 0);
        assert_eq!(counts.deletes.load(SeqCst), 0, "nothing asked of the store for nothing");
        assert_eq!(store.delete_many(paths[..3].to_vec()).await, 3);
        assert_eq!(store.objects(&app("a", &["values", "n"])).try_collect::<Vec<_>>().await.unwrap().len(), 2);

        // The memory store keeps no versions, so a snapshot may still read what it holds: it deletes nothing.
        let memory = Store::memory();
        put(&memory, &paths[0]).await;
        assert_eq!(memory.delete_many(vec![paths[0].clone()]).await, 0);
        assert!(memory.head(&paths[0]).await.unwrap());
    }

    /// An S3 whose puts answer the statuses of `plan` in turn, and 200 after, and the puts it has had.
    async fn s3(plan: &[u16]) -> (Store, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let (plan, puts) = (Arc::new(Mutex::new(VecDeque::from(plan.to_vec()))), Arc::new(AtomicUsize::new(0)));
        let counted = puts.clone();
        tokio::spawn(crate::tric::listen(listener, move |_, req| {
            let (plan, puts) = (plan.clone(), counted.clone());
            async move {
                let put = req.method() == http::Method::PUT;
                _ = http_body_util::BodyExt::collect(req.into_body()).await;
                let code = if put { plan.lock().unwrap().pop_front().unwrap_or(200) } else { 404 };
                puts.fetch_add(put as usize, SeqCst);
                let mut res = crate::tric::status(http::StatusCode::from_u16(code).unwrap());
                res.headers_mut().insert(http::header::ETAG, http::HeaderValue::from_static("\"new\""));
                res
            }
        }));
        let s3 = AmazonS3Builder::new().with_endpoint(format!("http://{at}")).with_allow_http(true);
        let s3 = s3.with_region("us-east-1").with_bucket_name("b").with_access_key_id("k").with_secret_access_key("s");
        (Store::s3(S3::new(s3).unwrap()), puts)
    }

    #[tokio::test]
    async fn a_conditional_put_refused_after_a_try_that_may_have_landed_is_an_error_not_a_conflict() {
        let update = PutMode::Update(UpdateVersion { e_tag: Some("\"old\"".into()), version: None });
        let tries = TRIES as usize;
        // What the put came to: whether it landed, if that is known, and how many puts S3 had.
        let cases: [(&[u16], PutMode, Option<bool>, usize); 6] = [
            (&[412], update.clone(), Some(false), 1), // another write landed first
            (&[409], update.clone(), Some(false), 1), // another write was under way
            (&[500, 412], update.clone(), None, 2),   // the first may have landed, and the second failed on it
            (&[500, 200], update.clone(), Some(true), 2),
            (&[503; TRIES as usize], update, None, tries),
            (&[500, 200], PutMode::Overwrite, Some(true), 2), // tried again by object_store, as before
        ];
        let (mut got, mut want) = (vec![], vec![]);
        for (plan, mode, landed, puts) in cases {
            let (store, counted) = s3(plan).await;
            let put = store.put(&app("a", &["names", "n"]), Bytes::from_static(b"{}"), mode).await;
            got.push((plan, put.map(|v| v.is_some()).ok(), counted.load(SeqCst)));
            want.push((plan, landed, puts));
        }
        assert_eq!(got, want);
    }
}
