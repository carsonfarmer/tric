//! `wasi:keyvalue@0.2.0-draft2` over an object store: one object per key and no cache, so every call is the store's.
use crate::engine::Host;
use futures_util::{StreamExt, TryStreamExt};
use hyper::body::Bytes;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use percent_encoding::percent_decode_str;
use std::sync::Arc;
use wasmtime::component::Resource;

const KEY_MAX: usize = 256; // bytes, before percent-encoding
const VALUE_MAX: usize = 1 << 20;
pub const NAME_MAX: usize = 63; // bytes in a bucket, app or team name: a DNS label, as an app's is
const PAGE: usize = 1000; // keys per `list-keys`, the most S3 gives for one LIST
const BATCH_MAX: usize = 16 << 20; // bytes of values in one `get-many` reply
const RETRIES: usize = 16; // CAS attempts for one `increment`

wasmtime::component::bindgen!({
    path: "wit/keyvalue",
    world: "imports",
    imports: { default: async },
    with: { "wasi:keyvalue/store.bucket": Bucket, "wasi:keyvalue/atomics.cas": Cas },
});
use wasi::keyvalue::atomics::{self, CasError};
use wasi::keyvalue::store::{self, Error, KeyResponse};
type R<T> = Result<T, Error>;

fn other(e: impl ToString) -> Error {
    Error::Other(e.to_string())
}

/// Whether `s` can name a bucket, an app or a team, which all become one segment of a key: 1 to 63 of `a-z`, `0-9` and
/// `-`.
pub fn is_name(s: &str) -> bool {
    (1..=NAME_MAX).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// The handle for an opened bucket: the prefix of its keys.
pub struct Bucket(Path);

/// What `cas::new` saw of a key: its value and the version (ETag, or generation on GCS) that `swap` sends back as the
/// condition of its write. Both `None` when there is no such key.
pub struct Cas {
    path: Path,
    seen: Option<(Bytes, UpdateVersion)>,
}

/// One app's view of the object store: its buckets under `root`.
pub(crate) struct Kv {
    store: Arc<dyn ObjectStore>,
    root: Path,
}

fn path(bucket: &Path, key: &str) -> R<Path> {
    if key.is_empty() || key.len() > KEY_MAX {
        return Err(Error::Other(format!("a key is 1 to {KEY_MAX} bytes"))); // an empty key would be the bucket's prefix
    }
    Ok(bucket.clone().join(key)) // percent-encoded, so `/` stays inside it
}

impl Kv {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, root: &str) -> Self {
        Self { store, root: root.into() }
    }

    /// The key as the store has it, with the version a conditional write needs.
    async fn read(&self, p: &Path) -> R<Option<(Bytes, UpdateVersion)>> {
        match self.store.get(p).await {
            Ok(r) => {
                let v = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
                Ok(Some((r.bytes().await.map_err(other)?, v)))
            }
            Err(E::NotFound { .. }) => Ok(None),
            Err(e) => Err(other(e)),
        }
    }

    async fn get(&self, b: &Path, key: &str) -> R<Option<Vec<u8>>> {
        Ok(self.read(&path(b, key)?).await?.map(|(v, _)| v.into()))
    }

    /// `false` when the condition in `mode` failed, which includes a key deleted since it was read.
    async fn put(&self, p: &Path, v: Bytes, mode: PutMode) -> R<bool> {
        if v.len() > VALUE_MAX {
            return Err(Error::Other(format!("a value is {VALUE_MAX} bytes or less")));
        }
        let conditional = mode != PutMode::Overwrite;
        match self.store.put_opts(p, v.into(), mode.into()).await {
            Ok(_) => Ok(true),
            Err(E::Precondition { .. } | E::AlreadyExists { .. } | E::NotFound { .. }) if conditional => Ok(false),
            Err(e) => Err(other(e)),
        }
    }

    async fn set(&self, b: &Path, key: &str, v: Vec<u8>) -> R<()> {
        self.put(&path(b, key)?, v.into(), PutMode::Overwrite).await.map(drop)
    }

    /// A missing key is not an error, though GCS and Azure answer 404 for one.
    async fn delete(&self, b: &Path, key: &str) -> R<()> {
        match self.store.delete(&path(b, key)?).await {
            Ok(()) | Err(E::NotFound { .. }) => Ok(()),
            Err(e) => Err(other(e)),
        }
    }

    async fn cas(&self, b: &Path, key: &str) -> R<Cas> {
        let path = path(b, key)?;
        Ok(Cas { seen: self.read(&path).await?, path })
    }

    /// `None` when the swap won, or else a handle that sees what the winner wrote.
    async fn swap(&self, c: Cas, v: Bytes) -> R<Option<Cas>> {
        let mode = c.seen.as_ref().map_or(PutMode::Create, |(_, version)| PutMode::Update(version.clone()));
        if self.put(&c.path, v, mode).await? {
            return Ok(None);
        }
        Ok(Some(Cas { seen: self.read(&c.path).await?, ..c }))
    }

    /// One page of keys in order: the first `PAGE` after `cursor`, which is the last key of the page before.
    async fn list(&self, b: &Path, cursor: Option<String>) -> R<KeyResponse> {
        let after = cursor.map_or(Ok(b.clone()), |k| path(b, &k))?; // everything is after the prefix itself
        let keys: Vec<String> = (self.store.list_with_offset(Some(b), &after).take(PAGE))
            .map_ok(|m| percent_decode_str(m.location.filename().unwrap_or_default()).decode_utf8_lossy().into_owned())
            .map_err(other)
            .try_collect()
            .await?;
        Ok(KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys }) // may end on an empty page
    }
}

impl Host {
    fn bucket(&self, b: &Resource<Bucket>) -> R<&Path> {
        Ok(&self.table.get(b).map_err(other)?.0)
    }
}

impl store::Host for Host {
    async fn open(&mut self, name: String) -> R<Resource<Bucket>> {
        let b = is_name(&name).then(|| Bucket(self.app.kv.root.clone().join(name))).ok_or(Error::NoSuchStore)?;
        self.table.push(b).map_err(other)
    }
}

impl store::HostBucket for Host {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> R<Option<Vec<u8>>> {
        self.app.kv.get(self.bucket(&b)?, &key).await
    }
    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> R<()> {
        self.app.kv.set(self.bucket(&b)?, &key, value).await
    }
    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> R<()> {
        self.app.kv.delete(self.bucket(&b)?, &key).await
    }
    async fn exists(&mut self, b: Resource<Bucket>, key: String) -> R<bool> {
        Ok(self.app.kv.read(&path(self.bucket(&b)?, &key)?).await?.is_some())
    }
    async fn list_keys(&mut self, b: Resource<Bucket>, cursor: Option<String>) -> R<KeyResponse> {
        self.app.kv.list(self.bucket(&b)?, cursor).await
    }
    async fn drop(&mut self, b: Resource<Bucket>) -> wasmtime::Result<()> {
        Ok(self.table.delete(b).map(drop)?)
    }
}

impl wasi::keyvalue::batch::Host for Host {
    async fn get_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<Vec<(String, Option<Vec<u8>>)>> {
        let (mut out, mut bytes) = (vec![], 0);
        for key in keys {
            let value = self.app.kv.get(self.bucket(&b)?, &key).await?;
            bytes += value.as_ref().map_or(0, Vec::len);
            if bytes > BATCH_MAX {
                return Err(Error::Other(format!("get-many returns {BATCH_MAX} bytes or less")));
            }
            out.push((key, value));
        }
        Ok(out)
    }
    async fn set_many(&mut self, b: Resource<Bucket>, items: Vec<(String, Vec<u8>)>) -> R<()> {
        for (key, value) in items {
            self.app.kv.set(self.bucket(&b)?, &key, value).await?;
        }
        Ok(())
    }
    async fn delete_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<()> {
        for key in keys {
            self.app.kv.delete(self.bucket(&b)?, &key).await?;
        }
        Ok(())
    }
}

impl atomics::Host for Host {
    async fn increment(&mut self, b: Resource<Bucket>, key: String, delta: i64) -> R<i64> {
        let mut cas = self.app.kv.cas(self.bucket(&b)?, &key).await?;
        for _ in 0..RETRIES {
            let now = cas.seen.as_ref().map_or(Ok(0), |(v, _)| v.as_ref().try_into().map(i64::from_le_bytes));
            let now = now.map_err(|_| other("not a counter"))?; // 8 bytes, little-endian, as Spin stores it
            let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
            match self.app.kv.swap(cas, Bytes::copy_from_slice(&next.to_le_bytes())).await? {
                None => return Ok(next),
                Some(fresh) => cas = fresh,
            }
        }
        Err(other("too much contention"))
    }
    async fn swap(&mut self, c: Resource<Cas>, value: Vec<u8>) -> Result<(), CasError> {
        let cas = self.table.delete(c).map_err(|e| CasError::StoreError(other(e)))?;
        let lost = self.app.kv.swap(cas, value.into()).await.map_err(CasError::StoreError)?;
        let Some(fresh) = lost else { return Ok(()) };
        Err(self.table.push(fresh).map_or_else(|e| CasError::StoreError(other(e)), CasError::CasFailed))
    }
}

impl atomics::HostCas for Host {
    async fn new(&mut self, b: Resource<Bucket>, key: String) -> R<Resource<Cas>> {
        let cas = self.app.kv.cas(self.bucket(&b)?, &key).await?;
        self.table.push(cas).map_err(other)
    }
    async fn current(&mut self, c: Resource<Cas>) -> R<Option<Vec<u8>>> {
        Ok(self.table.get(&c).map_err(other)?.seen.as_ref().map(|(v, _)| v.to_vec()))
    }
    async fn drop(&mut self, c: Resource<Cas>) -> wasmtime::Result<()> {
        Ok(self.table.delete(c).map(drop)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn kv() -> (Kv, Path) {
        (Kv::new(Arc::new(InMemory::new()), "kv/app"), "kv/app/b".into())
    }

    /// Too big for the guest tests, as a `Uri` of 64 KiB is the most their fixture can pass.
    #[tokio::test]
    async fn value_limit() {
        let (kv, b) = kv();
        assert!(kv.set(&b, "k", vec![0; VALUE_MAX]).await.is_ok());
        assert!(kv.set(&b, "k", vec![0; VALUE_MAX + 1]).await.is_err());
        assert_eq!(kv.get(&b, "k").await.unwrap().unwrap().len(), VALUE_MAX); // the failed write changed nothing
    }

    /// A listing is the store's: a write is in it at once, and a failed write is not.
    #[tokio::test]
    async fn listing_is_the_stores() {
        let (kv, b) = kv();
        assert!(kv.list(&b, None).await.unwrap().keys.is_empty());
        kv.set(&b, "a", "1".into()).await.unwrap();
        assert!(kv.set(&b, "z", vec![0; VALUE_MAX + 1]).await.is_err());
        assert_eq!(kv.list(&b, None).await.unwrap().keys, ["a"]);
        kv.delete(&b, "a").await.unwrap();
        kv.delete(&b, "a").await.unwrap(); // a missing key is not an error
        assert!(kv.list(&b, None).await.unwrap().keys.is_empty());
    }

    /// The condition of a swap is the version it read, so the second of two swaps from one read loses.
    #[tokio::test]
    async fn swap_is_conditional() {
        let (kv, b) = kv();
        let (first, second) = (kv.cas(&b, "k").await.unwrap(), kv.cas(&b, "k").await.unwrap());
        assert!(kv.swap(first, "1".into()).await.unwrap().is_none());
        let lost = kv.swap(second, "2".into()).await.unwrap().expect("the key was created since");
        assert_eq!(lost.seen.as_ref().map(|(v, _)| v.as_ref()), Some(b"1".as_ref()));
        assert!(kv.swap(lost, "2".into()).await.unwrap().is_none());
        let stale = kv.cas(&b, "k").await.unwrap();
        kv.delete(&b, "k").await.unwrap();
        assert!(kv.swap(stale, "3".into()).await.unwrap().is_some(), "a swap on a deleted key loses");
    }
}
