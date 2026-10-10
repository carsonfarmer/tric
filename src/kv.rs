//! `wasi:keyvalue@0.2.0-draft2` over names: a store is a name, open for writing to the turn on it, and a snapshot to
//! everything else.
use crate::engine::Host;
use crate::name::{self, Turn};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::sync::Arc;
use wasmtime::component::Resource;

const BATCH_MAX: usize = 16 << 20; // bytes of values in one `get-many` reply
const IN_FLIGHT: usize = 32; // value objects read at once for one batch

wasmtime::component::bindgen!({
    path: "wit/keyvalue",
    world: "imports",
    imports: { default: async },
    with: { "wasi:keyvalue/store.bucket": Bucket, "wasi:keyvalue/atomics.cas": Cas },
});
use wasi::keyvalue::atomics::{self, CasError};
pub use wasi::keyvalue::store::{self, Error, KeyResponse};
type R<T> = Result<T, Error>;

pub fn other(e: impl ToString) -> Error {
    Error::Other(e.to_string())
}

/// An opened store: the request's turn, or a snapshot of a name, which is a turn that has answered.
pub type Bucket = Arc<Turn>;

/// What `cas::new` saw of a key.
pub struct Cas {
    bucket: Bucket,
    key: String,
    current: Option<Bytes>,
}

impl Cas {
    async fn new(bucket: Bucket, key: String) -> R<Self> {
        Ok(Self { current: bucket.get(&key).await?, bucket, key })
    }
}

impl Host {
    fn bucket(&self, b: &Resource<Bucket>) -> R<Bucket> {
        self.table.get(b).cloned().map_err(other)
    }
}

impl store::Host for Host {
    /// The request's turn if it is on `name`, or else the name as it was when the request first opened it.
    async fn open(&mut self, name: String) -> R<Resource<Bucket>> {
        if !name::is_name(&name) {
            return Err(Error::NoSuchStore);
        }
        let ctx = self.ctx.clone();
        let bucket = match &ctx.turn {
            Some(t) if t.name == name => t.clone(),
            _ => ctx.snap(&name).await.map_err(|e| other(format!("{e:#}")))?,
        };
        self.table.push(bucket).map_err(other)
    }
}

impl store::HostBucket for Host {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> R<Option<Vec<u8>>> {
        Ok(self.bucket(&b)?.get(&key).await?.map(Into::into))
    }
    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> R<()> {
        self.bucket(&b)?.write(vec![(key, Some(value.into()))]).await
    }
    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> R<()> {
        self.bucket(&b)?.write(vec![(key, None)]).await
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
        let bucket = &self.bucket(&b)?;
        let mut values =
            stream::iter(keys).map(|key| async move { Ok((bucket.get(&key).await?, key)) }).buffered(IN_FLIGHT);
        let (mut out, mut bytes) = (vec![], 0);
        while let Some((value, key)) = values.try_next().await? {
            bytes += value.as_ref().map_or(0, Bytes::len);
            if bytes > BATCH_MAX {
                return Err(other(format!("get-many returns {BATCH_MAX} bytes or less")));
            }
            out.push((key, value.map(Into::into)));
        }
        Ok(out)
    }
    async fn set_many(&mut self, b: Resource<Bucket>, items: Vec<(String, Vec<u8>)>) -> R<()> {
        self.bucket(&b)?.write(items.into_iter().map(|(k, v)| (k, Some(v.into()))).collect()).await
    }
    async fn delete_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<()> {
        self.bucket(&b)?.write(keys.into_iter().map(|k| (k, None)).collect()).await
    }
}

impl atomics::Host for Host {
    async fn increment(&mut self, b: Resource<Bucket>, key: String, delta: i64) -> R<i64> {
        self.bucket(&b)?.increment(&key, delta).await
    }
    /// Within a turn nothing else writes the name, so a swap fails only when the turn itself wrote the key since.
    async fn swap(&mut self, c: Resource<Cas>, value: Vec<u8>) -> Result<(), CasError> {
        let cas = self.table.delete(c).map_err(|e| CasError::StoreError(other(e)))?;
        if cas.bucket.swap(&cas.key, &cas.current, value.into()).await.map_err(CasError::StoreError)? {
            return Ok(());
        }
        let fresh = Cas::new(cas.bucket, cas.key).await.map_err(CasError::StoreError)?;
        Err(self.table.push(fresh).map_or_else(|e| CasError::StoreError(other(e)), CasError::CasFailed))
    }
}

impl atomics::HostCas for Host {
    async fn new(&mut self, b: Resource<Bucket>, key: String) -> R<Resource<Cas>> {
        let cas = Cas::new(self.bucket(&b)?, key).await?;
        self.table.push(cas).map_err(other)
    }
    async fn current(&mut self, c: Resource<Cas>) -> R<Option<Vec<u8>>> {
        Ok(self.table.get(&c).map_err(other)?.current.as_ref().map(|v| v.to_vec()))
    }
    async fn drop(&mut self, c: Resource<Cas>) -> wasmtime::Result<()> {
        Ok(self.table.delete(c).map(drop)?)
    }
}
