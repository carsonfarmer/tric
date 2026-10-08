//! The bucket: one object store, in memory for `tric dev` and S3 everywhere else. S3 answers 403 for a missing key to a
//! caller that cannot list, so a 403 reads as not found.
use bytes::Bytes;
use object_store::{GetOptions, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, memory::InMemory, path::Path};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use wasmtime::{Result, ensure, error::Context};

#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
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

fn missing(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::NotFound { .. } | object_store::Error::PermissionDenied { .. })
}

impl Store {
    pub fn memory() -> Self {
        Self { inner: Arc::new(InMemory::new()), versioned: false }
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
