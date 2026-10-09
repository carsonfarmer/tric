//! The bucket: one object store, in memory for `tric dev` and S3 everywhere else. S3 answers 403 for a missing key to a
//! caller that cannot list, so a 403 reads as not found.
use bytes::Bytes;
use object_store::aws::{AmazonS3, AmazonS3Builder, AwsCredential};
use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{
    Certificate, ClientOptions, GetOptions, ObjectStore, ObjectStoreExt, PutMode, StaticCredentialProvider,
};
use object_store::{UpdateVersion, memory::InMemory, path::Path};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use wasmtime::{Result, ensure, error::Context};
use webpki_root_certs::TLS_SERVER_ROOT_CERTS;

#[derive(Clone)]
pub struct Store {
    pub(crate) inner: Arc<dyn ObjectStore>,
    /// Whether the bucket keeps versions, so a value read by its version id outlives its delete. The memory store
    /// keeps none, and never deletes.
    pub versioned: bool,
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

/// The write `base` allows: over that version, or, with none, where there is nothing yet.
pub fn over(base: Option<UpdateVersion>) -> PutMode {
    base.map_or(PutMode::Create, PutMode::Update)
}

/// The bucket `bucket` on S3, as the environment's `AWS_*` variables configure it, with `creds` if given, and else
/// the environment's.
pub fn s3(bucket: &str, creds: Option<AwsCredential>) -> Result<AmazonS3> {
    let mut s3 = AmazonS3Builder::from_env().with_bucket_name(bucket).with_http_connector(Shared);
    if let Some(creds) = creds {
        s3 = s3.with_credentials(Arc::new(StaticCredentialProvider::new(creds)));
    }
    Ok(s3.build()?)
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

fn missing(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::NotFound { .. } | object_store::Error::PermissionDenied { .. })
}

impl Store {
    pub fn memory() -> Self {
        Self { inner: Arc::new(InMemory::new()), versioned: false }
    }

    /// `s3`, whose bucket must keep versions.
    pub fn s3(s3: AmazonS3) -> Self {
        Self { inner: Arc::new(s3), versioned: true }
    }

    /// Whether there is an object at `path`.
    pub async fn head(&self, path: &Path) -> Result<bool> {
        match self.inner.head(path).await {
            Ok(_) => Ok(true),
            Err(e) if missing(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The names under `prefix`, one level down.
    pub async fn dirs(&self, prefix: &Path) -> Result<Vec<String>> {
        let list = self.inner.list_with_delimiter(Some(prefix)).await?;
        Ok(list.common_prefixes.iter().filter_map(|p| p.filename().map(str::to_owned)).collect())
    }

    /// The names of the objects under `prefix`, one level down.
    #[cfg(feature = "ws")]
    pub async fn files(&self, prefix: &Path) -> Result<Vec<String>> {
        let list = self.inner.list_with_delimiter(Some(prefix)).await?;
        Ok(list.objects.iter().filter_map(|o| o.location.filename().map(str::to_owned)).collect())
    }

    /// The object at `path`, at `version` if one is given, if there is one; it must be `max` bytes or less.
    pub async fn get(&self, path: &Path, version: Option<String>, max: u64) -> Result<Option<(Bytes, UpdateVersion)>> {
        let got = match self.inner.get_opts(path, GetOptions { version, ..Default::default() }).await {
            Ok(got) => got,
            Err(e) if missing(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        ensure!(got.meta.size <= max, "{path} is over {max} bytes");
        let version = UpdateVersion { e_tag: got.meta.e_tag.clone(), version: got.meta.version.clone() };
        Ok(Some((got.bytes().await?, version)))
    }

    pub async fn json<T: DeserializeOwned>(&self, path: &Path, max: u64) -> Result<Option<(T, UpdateVersion)>> {
        let Some((bytes, version)) = self.get(path, None, max).await? else { return Ok(None) };
        Ok(Some((serde_json::from_slice(&bytes).with_context(|| format!("{path}"))?, version)))
    }

    /// Writes `bytes` at `path` as `mode` allows, and returns the new version, or `None` if `mode`'s condition failed.
    pub async fn put(&self, path: &Path, bytes: Bytes, mode: PutMode) -> Result<Option<UpdateVersion>> {
        match self.inner.put_opts(path, bytes.into(), mode.into()).await {
            Ok(r) => Ok(Some(UpdateVersion { e_tag: r.e_tag, version: r.version })),
            Err(
                object_store::Error::Precondition { .. }
                | object_store::Error::AlreadyExists { .. }
                | object_store::Error::NotFound { .. },
            ) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Deletes the object at `path`, in a versioned bucket only: elsewhere a snapshot may still read it.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        match self.versioned {
            true => self.inner.delete(path).await.or_else(|e| if missing(&e) { Ok(()) } else { Err(e.into()) }),
            false => Ok(()),
        }
    }
}
