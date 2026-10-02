//! `wasi:keyvalue@0.2.0-draft2` over an object store: one object per key, and one generation object per bucket for listing.
use crate::guest::Host;
use futures_util::{StreamExt, TryStreamExt};
use hyper::body::Bytes;
use object_store::{Error as E, GetOptions, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use percent_encoding::percent_decode_str;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use wasmtime::component::Resource;

const KEY_MAX: usize = 256; // bytes, before percent-encoding
const VALUE_MAX: usize = 1 << 20;
const BUCKET_MAX: usize = 64; // bytes in a bucket name
const FRESH: Duration = Duration::from_secs(1); // how long a cached value or generation is served without asking the store
const PAGE: usize = 1000; // keys per `list-keys`, the most S3 gives for one LIST
const CACHE_MAX: usize = 32 << 20; // bytes of cached values and pages per app
const STRING: usize = 64; // bytes a cached string costs beyond its text: header, allocation and table slot, near enough
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

/// The handle for an opened bucket: its name.
pub struct Bucket(String);

/// What `cas::new` saw of a key, which `swap` sends back as the condition of its write. It outlives its bucket handle,
/// so it keeps the bucket's name.
pub struct Cas {
    bucket: String,
    path: Path,
    seen: Arc<Seen>,
}

/// A key as the store had it: the value and its ETag, both `None` when there is no such key.
#[derive(Default)]
struct Seen {
    value: Option<Bytes>,
    etag: Option<String>,
}

/// One app's view of the object store. Shared by all of its requests, so the cache outlives a request.
pub(crate) struct Kv {
    store: Arc<dyn ObjectStore>,
    app: String,
    cache: Mutex<Cache>,
}
/// What the store said and when; and the pages of key names, each with the generation object it was listed at.
#[derive(Default)]
struct Cache {
    values: HashMap<Path, (Instant, Arc<Seen>)>,
    pages: HashMap<Path, (Arc<Seen>, KeyResponse)>,
    bytes: usize,
}

impl Cache {
    /// Counts an entry of `n` bytes that replaces one of `old`. Over `CACHE_MAX` it starts again empty: crude, and bounded.
    fn charge(&mut self, n: usize, old: usize) {
        self.bytes = self.bytes - old + n;
        if self.bytes > CACHE_MAX {
            *self = Cache { bytes: n, ..Default::default() };
        }
    }
}

impl Kv {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, app: &str) -> Self {
        Self { store, app: app.into(), cache: Default::default() }
    }

    fn path(&self, bucket: &str, key: &str) -> R<Path> {
        if key.is_empty() || key.len() > KEY_MAX {
            return Err(Error::Other(format!("a key is 1 to {KEY_MAX} bytes"))); // an empty key would be the bucket's prefix
        }
        Ok(Path::from_iter(["kv", &self.app, bucket, key])) // each part is percent-encoded, so `/` stays inside it
    }

    /// Caches what the store said at `at`, unless the cache already has something newer: a write that finished while a
    /// read was in flight must not be overwritten by what that read found.
    fn remember(&self, p: &Path, seen: Arc<Seen>, at: Instant) -> Arc<Seen> {
        let weigh = |s: &Seen| p.as_ref().len() + STRING + s.value.as_ref().map_or(0, Bytes::len);
        let mut c = self.cache.lock().unwrap();
        let old = c.values.get(p).map(|(prev, s)| (*prev, weigh(s)));
        if old.is_none_or(|(prev, _)| prev <= at) {
            c.charge(weigh(&seen), old.map_or(0, |(_, n)| n));
            c.values.insert(p.clone(), (at, seen.clone()));
        }
        seen
    }

    /// The key as the store has it, from the cache when that is younger than `max_age`. Otherwise it is asked with
    /// `If-None-Match`, so an unchanged value costs no transfer.
    async fn read(&self, p: &Path, max_age: Duration) -> R<Arc<Seen>> {
        let (old, at) = (self.cache.lock().unwrap().values.get(p).cloned(), Instant::now());
        if let Some((at, seen)) = &old
            && at.elapsed() < max_age
        {
            return Ok(seen.clone());
        }
        let opts = GetOptions { if_none_match: old.as_ref().and_then(|(_, s)| s.etag.clone()), ..Default::default() };
        let seen = match self.store.get_opts(p, opts).await {
            Ok(mut r) => Arc::new(Seen { etag: r.meta.e_tag.take(), value: Some(r.bytes().await.map_err(other)?) }),
            Err(E::NotModified { .. }) => old.map(|(_, s)| s).unwrap_or_default(),
            Err(E::NotFound { .. }) => Arc::default(),
            Err(e) => return Err(other(e)),
        };
        Ok(self.remember(p, seen, at))
    }

    async fn get(&self, bucket: &str, key: &str) -> R<Option<Bytes>> {
        Ok(self.read(&self.path(bucket, key)?, FRESH).await?.value.clone())
    }

    /// Writes `v` at `p` (`None` deletes) and remembers it, so that we read our own writes at once. `false` when the
    /// condition in `mode` failed. An unconditional write that the store refuses is an error.
    async fn put(&self, p: &Path, v: Option<Bytes>, mode: PutMode) -> R<bool> {
        if v.as_ref().is_some_and(|v| v.len() > VALUE_MAX) {
            return Err(Error::Other(format!("a value is {VALUE_MAX} bytes or less")));
        }
        let conditional = mode != PutMode::Overwrite;
        let done = match &v {
            Some(v) => self.store.put_opts(p, v.clone().into(), mode.into()).await.map(|r| r.e_tag),
            None => self.store.delete(p).await.map(|()| None),
        };
        match done {
            Ok(etag) => {
                self.remember(p, Arc::new(Seen { value: v, etag }), Instant::now());
                Ok(true)
            }
            Err(E::Precondition { .. } | E::AlreadyExists { .. }) if conditional => Ok(false),
            Err(e) => Err(other(e)),
        }
    }

    /// Writes the items in order up to the first failure, then the bucket's generation once, failure or not: a write
    /// that failed may have been applied, and so may the items before it, and a listing must not hide them.
    async fn write(&self, bucket: &str, items: impl IntoIterator<Item = (String, Option<Bytes>)>) -> R<()> {
        let items = items.into_iter().map(|(k, v)| Ok((self.path(bucket, &k)?, v))).collect::<R<Vec<_>>>()?;
        let done = async {
            for (p, v) in items {
                self.put(&p, v, PutMode::Overwrite).await?;
            }
            Ok(())
        }
        .await;
        done.and(self.touch(bucket).await)
    }

    /// Overwrites the bucket's generation with the time in nanoseconds, so that every write changes its body.
    async fn touch(&self, bucket: &str) -> R<()> {
        let now = UNIX_EPOCH.elapsed().map_err(other)?.as_nanos().to_string();
        self.put(&Path::from_iter(["kvgen", &self.app, bucket]), Some(now.into()), PutMode::Overwrite).await.map(drop)
    }

    /// Reads from the store, never the cache alone, so that the ETag is current.
    async fn cas(&self, bucket: &str, key: &str) -> R<Cas> {
        let path = self.path(bucket, key)?;
        Ok(Cas { bucket: bucket.into(), seen: self.read(&path, Duration::ZERO).await?, path })
    }

    /// `None` when the swap won, or else a handle that sees what the winner wrote. Only a swap that creates the key
    /// changes the listing, so only that one writes the generation.
    async fn swap(&self, c: Cas, v: Bytes) -> R<Option<Cas>> {
        let mode = match &c.seen.etag {
            Some(e_tag) => PutMode::Update(UpdateVersion { e_tag: Some(e_tag.clone()), version: None }),
            None => PutMode::Create,
        };
        if self.put(&c.path, Some(v), mode).await? {
            return if c.seen.etag.is_none() { self.touch(&c.bucket).await.map(|_| None) } else { Ok(None) };
        }
        Ok(Some(Cas { seen: self.read(&c.path, Duration::ZERO).await?, ..c }))
    }

    /// One page of keys in order. A cached page is good while the generation object is the one it was listed at, and
    /// reading that costs a GET (none within `FRESH`) where listing costs a LIST.
    async fn list(&self, bucket: &str, cursor: Option<String>) -> R<KeyResponse> {
        let generation = self.read(&Path::from_iter(["kvgen", &self.app, bucket]), FRESH).await?;
        let prefix = Path::from_iter(["kv", &self.app, bucket]);
        let after = cursor.map_or(Ok(prefix.clone()), |k| self.path(bucket, &k))?; // everything is after the prefix itself
        if let Some((g, page)) = self.cache.lock().unwrap().pages.get(&after)
            && g.value == generation.value
        {
            return Ok(page.clone());
        }
        let keys: Vec<String> = (self.store.list_with_offset(Some(&prefix), &after).take(PAGE))
            .map_ok(|m| percent_decode_str(m.location.filename().unwrap_or_default()).decode_utf8_lossy().into_owned())
            .map_err(other)
            .try_collect()
            .await?;
        let page = KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys }; // may end on an empty page
        let weigh = |p: &KeyResponse| after.as_ref().len() + p.keys.iter().map(|k| k.len() + STRING).sum::<usize>();
        let mut c = self.cache.lock().unwrap();
        let old = c.pages.get(&after).map_or(0, |(_, p)| weigh(p));
        c.charge(weigh(&page), old);
        c.pages.insert(after, (generation, page.clone()));
        Ok(page)
    }
}

impl Host {
    fn at(&self, b: &Resource<Bucket>) -> R<(&Kv, &str)> {
        Ok((&self.app.kv, &self.table.get(b).map_err(other)?.0))
    }
}

impl store::Host for Host {
    async fn open(&mut self, name: String) -> R<Resource<Bucket>> {
        let ok = (1..=BUCKET_MAX).contains(&name.len())
            && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
        ok.then_some(Bucket(name)).ok_or(Error::NoSuchStore).and_then(|b| self.table.push(b).map_err(other))
    }
}

impl store::HostBucket for Host {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> R<Option<Vec<u8>>> {
        let (kv, bucket) = self.at(&b)?;
        Ok(kv.get(bucket, &key).await?.map(|v| v.to_vec()))
    }
    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, [(key, Some(value.into()))]).await
    }
    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, [(key, None)]).await
    }
    async fn exists(&mut self, b: Resource<Bucket>, key: String) -> R<bool> {
        let (kv, bucket) = self.at(&b)?;
        Ok(kv.get(bucket, &key).await?.is_some())
    }
    async fn list_keys(&mut self, b: Resource<Bucket>, cursor: Option<String>) -> R<KeyResponse> {
        let (kv, bucket) = self.at(&b)?;
        kv.list(bucket, cursor).await
    }
    async fn drop(&mut self, b: Resource<Bucket>) -> wasmtime::Result<()> {
        Ok(self.table.delete(b).map(drop)?)
    }
}

impl wasi::keyvalue::batch::Host for Host {
    async fn get_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<Vec<(String, Option<Vec<u8>>)>> {
        let (kv, bucket) = self.at(&b)?;
        let (mut out, mut bytes) = (vec![], 0);
        for key in keys {
            let value = kv.get(bucket, &key).await?;
            bytes += value.as_ref().map_or(0, Bytes::len);
            if bytes > BATCH_MAX {
                return Err(Error::Other(format!("get-many returns {BATCH_MAX} bytes or less")));
            }
            out.push((key, value.map(|v| v.to_vec())));
        }
        Ok(out)
    }
    async fn set_many(&mut self, b: Resource<Bucket>, items: Vec<(String, Vec<u8>)>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, items.into_iter().map(|(k, v)| (k, Some(v.into())))).await
    }
    async fn delete_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, keys.into_iter().map(|k| (k, None))).await
    }
}

impl atomics::Host for Host {
    async fn increment(&mut self, b: Resource<Bucket>, key: String, delta: i64) -> R<i64> {
        let (kv, bucket) = self.at(&b)?;
        let mut cas = kv.cas(bucket, &key).await?;
        for _ in 0..RETRIES {
            let now = cas.seen.value.as_deref().map_or(Ok(0), |v| v.try_into().map(i64::from_le_bytes));
            let now = now.map_err(|_| other("not a counter"))?; // 8 bytes, little-endian, as Spin stores it
            let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
            match kv.swap(cas, Bytes::copy_from_slice(&next.to_le_bytes())).await? {
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
        let (kv, bucket) = self.at(&b)?;
        let cas = kv.cas(bucket, &key).await?;
        self.table.push(cas).map_err(other)
    }
    async fn current(&mut self, c: Resource<Cas>) -> R<Option<Vec<u8>>> {
        Ok(self.table.get(&c).map_err(other)?.seen.value.as_deref().map(<[u8]>::to_vec))
    }
    async fn drop(&mut self, c: Resource<Cas>) -> wasmtime::Result<()> {
        Ok(self.table.delete(c).map(drop)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    /// Too big for the guest tests, as a `Uri` of 64 KiB is the most their fixture can pass.
    #[tokio::test]
    async fn value_limit() {
        let kv = Kv::new(Arc::new(InMemory::new()), "app");
        let put = |n| kv.write("b", [("k".into(), Some(Bytes::from(vec![0; n])))]);
        assert!(put(VALUE_MAX).await.is_ok());
        assert!(put(VALUE_MAX + 1).await.is_err());
        assert_eq!(kv.get("b", "k").await.unwrap().unwrap().len(), VALUE_MAX); // the failed write changed nothing
    }

    /// A batch that fails part-way has written the items before the failure, so a listing must not keep hiding them.
    #[tokio::test]
    async fn failed_write_changes_the_listing() {
        let kv = Kv::new(Arc::new(InMemory::new()), "app");
        assert!(kv.list("b", None).await.unwrap().keys.is_empty());
        let items = [("a".into(), Some(Bytes::from("1"))), ("b".into(), Some(vec![0; VALUE_MAX + 1].into()))];
        assert!(kv.write("b", items).await.is_err());
        assert_eq!(kv.list("b", None).await.unwrap().keys, ["a"]);
    }
}
