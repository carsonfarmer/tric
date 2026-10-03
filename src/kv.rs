//! `wasi:keyvalue@0.2.0-draft2` over an object store: one object per key and no cache, so every call is the store's.
use crate::engine::Host;
use futures_util::{StreamExt, TryStreamExt};
use hyper::body::Bytes;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path, prefix::PrefixStore};
use percent_encoding::percent_decode_str;
use std::sync::Arc;
use wasmtime::component::Resource;

const KEY_MAX: usize = 256; // bytes, before percent-encoding
const VALUE_MAX: usize = 1 << 20;
pub const NAME_MAX: usize = 63; // bytes in a bucket or app name: a DNS label, as an app's is
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

/// Whether `s` can name a bucket or an app, which both become one segment of a key: 1 to 63 of `a-z`, `0-9` and `-`.
pub fn is_name(s: &str) -> bool {
    (1..=NAME_MAX).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// An opened bucket: a store that holds its keys alone.
#[derive(Clone)]
pub struct Bucket(Arc<dyn ObjectStore>);

/// What `cas::new` saw of a key: its value and the version (ETag, or generation on GCS) that `swap` sends back as the
/// condition of its write. Both `None` when there is no such key.
pub struct Cas {
    bucket: Bucket,
    key: Path,
    seen: Option<(Bytes, UpdateVersion)>,
}

/// `key` as one segment, percent-encoded, so `/` stays inside it.
fn path(key: &str) -> R<Path> {
    if key.is_empty() || key.len() > KEY_MAX {
        return Err(other(format!("a key is 1 to {KEY_MAX} bytes"))); // an empty key would be the bucket itself
    }
    Ok(Path::from_iter([key]))
}

/// `None` for a key that is not there.
fn found<T>(r: object_store::Result<T>) -> R<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(E::NotFound { .. }) => Ok(None),
        Err(e) => Err(other(e)),
    }
}

impl Bucket {
    /// The key as the store has it, with the version a conditional write needs.
    async fn read(&self, p: &Path) -> R<Option<(Bytes, UpdateVersion)>> {
        let Some(r) = found(self.0.get(p).await)? else { return Ok(None) };
        let v = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
        Ok(Some((r.bytes().await.map_err(other)?, v)))
    }

    async fn get(&self, key: &str) -> R<Option<Vec<u8>>> {
        Ok(self.read(&path(key)?).await?.map(|(v, _)| v.into()))
    }

    async fn exists(&self, key: &str) -> R<bool> {
        Ok(found(self.0.head(&path(key)?).await)?.is_some())
    }

    /// `false` when the condition in `mode` failed, which includes a key deleted since it was read.
    async fn put(&self, p: &Path, v: Bytes, mode: PutMode) -> R<bool> {
        if v.len() > VALUE_MAX {
            return Err(other(format!("a value is {VALUE_MAX} bytes or less")));
        }
        let conditional = mode != PutMode::Overwrite;
        match self.0.put_opts(p, v.into(), mode.into()).await {
            Ok(_) => Ok(true),
            Err(E::Precondition { .. } | E::AlreadyExists { .. } | E::NotFound { .. }) if conditional => Ok(false),
            Err(e) => Err(other(e)),
        }
    }

    async fn set(&self, key: &str, v: Vec<u8>) -> R<()> {
        self.put(&path(key)?, v.into(), PutMode::Overwrite).await.map(drop)
    }

    /// A missing key is not an error, though GCS and Azure answer 404 for one.
    async fn delete(&self, key: &str) -> R<()> {
        found(self.0.delete(&path(key)?).await).map(drop)
    }

    async fn cas(&self, key: &str) -> R<Cas> {
        let key = path(key)?;
        Ok(Cas { seen: self.read(&key).await?, key, bucket: self.clone() })
    }

    /// One page of keys in order: the first `PAGE` after `cursor`, which is the last key of the page before.
    async fn list(&self, cursor: Option<String>) -> R<KeyResponse> {
        let after = cursor.map_or(Ok(Path::default()), |k| path(&k))?; // every key is after the bucket itself
        let keys: Vec<String> = (self.0.list_with_offset(None, &after).take(PAGE))
            .map_ok(|m| percent_decode_str(m.location.as_ref()).decode_utf8_lossy().into_owned())
            .map_err(other)
            .try_collect()
            .await?;
        Ok(KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys }) // may end on an empty page
    }
}

impl Cas {
    /// `None` when the swap won, or else a handle that sees what the winner wrote.
    async fn swap(mut self, v: Bytes) -> R<Option<Cas>> {
        let mode = self.seen.take().map_or(PutMode::Create, |(_, version)| PutMode::Update(version));
        if self.bucket.put(&self.key, v, mode).await? {
            return Ok(None);
        }
        Ok(Some(Cas { seen: self.bucket.read(&self.key).await?, ..self }))
    }
}

impl Host {
    fn bucket(&self, b: &Resource<Bucket>) -> R<&Bucket> {
        self.table.get(b).map_err(other)
    }
}

impl store::Host for Host {
    async fn open(&mut self, name: String) -> R<Resource<Bucket>> {
        if !is_name(&name) {
            return Err(Error::NoSuchStore);
        }
        self.table.push(Bucket(Arc::new(PrefixStore::new(self.app.kv.clone(), name)))).map_err(other)
    }
}

impl store::HostBucket for Host {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> R<Option<Vec<u8>>> {
        self.bucket(&b)?.get(&key).await
    }
    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> R<()> {
        self.bucket(&b)?.set(&key, value).await
    }
    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> R<()> {
        self.bucket(&b)?.delete(&key).await
    }
    async fn exists(&mut self, b: Resource<Bucket>, key: String) -> R<bool> {
        self.bucket(&b)?.exists(&key).await
    }
    async fn list_keys(&mut self, b: Resource<Bucket>, cursor: Option<String>) -> R<KeyResponse> {
        self.bucket(&b)?.list(cursor).await
    }
    async fn drop(&mut self, b: Resource<Bucket>) -> wasmtime::Result<()> {
        Ok(self.table.delete(b).map(drop)?)
    }
}

impl wasi::keyvalue::batch::Host for Host {
    async fn get_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<Vec<(String, Option<Vec<u8>>)>> {
        let (mut out, mut bytes) = (vec![], 0);
        for key in keys {
            let value = self.bucket(&b)?.get(&key).await?;
            bytes += value.as_ref().map_or(0, Vec::len);
            if bytes > BATCH_MAX {
                return Err(other(format!("get-many returns {BATCH_MAX} bytes or less")));
            }
            out.push((key, value));
        }
        Ok(out)
    }
    async fn set_many(&mut self, b: Resource<Bucket>, items: Vec<(String, Vec<u8>)>) -> R<()> {
        for (key, value) in items {
            self.bucket(&b)?.set(&key, value).await?;
        }
        Ok(())
    }
    async fn delete_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<()> {
        for key in keys {
            self.bucket(&b)?.delete(&key).await?;
        }
        Ok(())
    }
}

impl atomics::Host for Host {
    async fn increment(&mut self, b: Resource<Bucket>, key: String, delta: i64) -> R<i64> {
        let mut cas = self.bucket(&b)?.cas(&key).await?;
        for _ in 0..RETRIES {
            let now = cas.seen.as_ref().map_or(Ok(0), |(v, _)| v.as_ref().try_into().map(i64::from_le_bytes));
            let now = now.map_err(|_| other("not a counter"))?; // 8 bytes, little-endian, as Spin stores it
            let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
            match cas.swap(Bytes::copy_from_slice(&next.to_le_bytes())).await? {
                None => return Ok(next),
                Some(fresh) => cas = fresh,
            }
        }
        Err(other("too much contention"))
    }
    async fn swap(&mut self, c: Resource<Cas>, value: Vec<u8>) -> Result<(), CasError> {
        let cas = self.table.delete(c).map_err(|e| CasError::StoreError(other(e)))?;
        let Some(fresh) = cas.swap(value.into()).await.map_err(CasError::StoreError)? else { return Ok(()) };
        Err(self.table.push(fresh).map_or_else(|e| CasError::StoreError(other(e)), CasError::CasFailed))
    }
}

impl atomics::HostCas for Host {
    async fn new(&mut self, b: Resource<Bucket>, key: String) -> R<Resource<Cas>> {
        let cas = self.bucket(&b)?.cas(&key).await?;
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

    fn bucket() -> Bucket {
        Bucket(Arc::new(InMemory::new()))
    }

    /// Too big for the guest tests, as a `Uri` of 64 KiB is the most their fixture can pass.
    #[tokio::test]
    async fn value_limit() {
        let b = bucket();
        assert!(b.set("k", vec![0; VALUE_MAX]).await.is_ok());
        assert!(b.set("k", vec![0; VALUE_MAX + 1]).await.is_err());
        assert_eq!(b.get("k").await.unwrap().unwrap().len(), VALUE_MAX); // the failed write changed nothing
    }

    /// A listing is the store's: a write is in it at once, and a failed write is not.
    #[tokio::test]
    async fn listing_is_the_stores() {
        let b = bucket();
        assert!(b.list(None).await.unwrap().keys.is_empty());
        b.set("a", "1".into()).await.unwrap();
        assert!(b.set("z", vec![0; VALUE_MAX + 1]).await.is_err());
        assert_eq!(b.list(None).await.unwrap().keys, ["a"]);
        b.delete("a").await.unwrap();
        b.delete("a").await.unwrap(); // a missing key is not an error
        assert!(b.list(None).await.unwrap().keys.is_empty());
    }

    /// The condition of a swap is the version it read, so the second of two swaps from one read loses.
    #[tokio::test]
    async fn swap_is_conditional() {
        let b = bucket();
        let (first, second) = (b.cas("k").await.unwrap(), b.cas("k").await.unwrap());
        assert!(first.swap("1".into()).await.unwrap().is_none());
        let lost = second.swap("2".into()).await.unwrap().expect("the key was created since");
        assert_eq!(lost.seen.as_ref().map(|(v, _)| v.as_ref()), Some(b"1".as_ref()));
        assert!(lost.swap("2".into()).await.unwrap().is_none());
        let stale = b.cas("k").await.unwrap();
        b.delete("k").await.unwrap();
        assert!(stale.swap("3".into()).await.unwrap().is_some(), "a swap on a deleted key loses");
    }
}
