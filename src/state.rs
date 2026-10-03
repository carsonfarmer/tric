//! What an install keeps in its bucket. Everything a team writes sits under the app's name, so the store's IAM can give
//! each team the apps named `<team>-…`:
//!
//! - `apps/<app>/blobs/sha256/<hash>`: components, in the OCI image layout, so a registry copy is a byte copy;
//! - `apps/<app>/releases/<hash>`: the app's releases;
//! - `apps/<app>/current`: the release it runs, and its secrets;
//! - `kv/<app>/<bucket>/<key>`: its `wasi:keyvalue` data, which only hosts write.
//!
//! Nothing read back is trusted: every read is capped, parsed strictly, and content-addressed objects are checked
//! against their hash.
use hyper::body::Bytes;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
use wasmtime::{Result, bail, ensure};

pub const BLOBS: &str = "blobs/sha256";
pub const RELEASES: &str = "releases";
pub const CURRENT: &str = "current";
pub const BLOB_MAX: u64 = 128 << 20;
pub const JSON_MAX: u64 = 8 << 20;
/// How stale a host's view may get, so also how long a change takes to be live everywhere.
pub const FRESH: Duration = Duration::from_secs(5);

/// The release an app runs, and its secrets, which outlive releases and override config of the same name.
#[derive(Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub release: Option<String>,
    pub secrets: BTreeMap<String, String>,
}

/// One immutable release of an app.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String,
    pub config: BTreeMap<String, String>,
    pub allowed_outbound_hosts: Vec<String>,
}

/// `app`'s `object`, one of the three above.
pub fn path(app: &str, object: &str) -> String {
    format!("apps/{app}/{object}")
}

pub fn kv(app: &str) -> String {
    format!("kv/{app}")
}

/// The object `<dir>/<hash>`, refused if it is over `max` bytes or its hash is not `hash`.
pub async fn fetch(store: &dyn ObjectStore, dir: &str, hash: &str, max: u64) -> Result<Bytes> {
    let r = store.get(&format!("{dir}/{hash}").into()).await?;
    ensure!(r.meta.size <= max, "{dir}/{hash} is over {max} bytes");
    let bytes = r.bytes().await?;
    ensure!(format!("{:x}", Sha256::digest(&bytes)) == hash, "{dir}/{hash} does not match its hash");
    Ok(bytes)
}

/// Stores `bytes` at `<dir>/<hash>` unless they are there already, and returns the hash. Like `fetch`, it refuses more
/// than `max` bytes, so nothing is written that a host would refuse to read.
pub async fn add(store: &dyn ObjectStore, dir: &str, bytes: Bytes, max: u64) -> Result<String> {
    ensure!(bytes.len() as u64 <= max, "a {dir} object is over {max} bytes");
    let hash = format!("{:x}", Sha256::digest(&bytes));
    match store.put_opts(&format!("{dir}/{hash}").into(), bytes.into(), PutMode::Create.into()).await {
        Ok(_) | Err(E::AlreadyExists { .. }) => Ok(hash),
        Err(e) => Err(e.into()),
    }
}

/// `app`'s release `id`.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<Release> {
    Ok(serde_json::from_slice(&fetch(store, &path(app, RELEASES), id, JSON_MAX).await?)?)
}

/// `app`'s `current` and its version, or the default and `None` if it has none.
pub async fn current(store: &dyn ObjectStore, app: &str) -> Result<(Current, Option<UpdateVersion>)> {
    let path = path(app, CURRENT);
    match store.get(&path.as_str().into()).await {
        Ok(mut r) => {
            ensure!(r.meta.size <= JSON_MAX, "{path} is over {JSON_MAX} bytes");
            let version = UpdateVersion { e_tag: r.meta.e_tag.take(), version: r.meta.version.take() };
            Ok((serde_json::from_slice(&r.bytes().await?)?, Some(version)))
        }
        Err(E::NotFound { .. }) => Ok(Default::default()),
        Err(e) => Err(e.into()),
    }
}

/// Applies `change` to `app`'s `current` with a conditional write, which fails if another change landed in between, so
/// neither is lost.
pub async fn update(store: &dyn ObjectStore, app: &str, change: impl FnOnce(&mut Current)) -> Result<()> {
    let (mut value, version) = current(store, app).await?;
    change(&mut value);
    let mode = version.map_or(PutMode::Create, PutMode::Update);
    match store.put_opts(&path(app, CURRENT).into(), serde_json::to_vec(&value)?.into(), mode.into()).await {
        Ok(_) => Ok(()),
        Err(E::Precondition { .. } | E::AlreadyExists { .. }) => bail!("{app} changed while this ran: run it again"),
        Err(e) => Err(e.into()),
    }
}
