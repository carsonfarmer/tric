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
use object_store::{Error as E, GetOptions, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::hash::{BuildHasher, RandomState};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::sleep;
use wasmtime::{Result, bail, ensure};

pub const BLOBS: &str = "blobs/sha256";
pub const RELEASES: &str = "releases";
pub const CURRENT: &str = "current";
pub const BLOB_MAX: u64 = 128 << 20;
pub const JSON_MAX: u64 = 8 << 20;
const SWAPS: usize = 32; // attempts at one change before giving up
const PAUSE_MAX: u64 = 1000; // ms, the longest wait after a lost swap
/// How stale a host's view may get, so also how long a change takes to be live everywhere.
pub const FRESH: Duration = Duration::from_secs(5);

/// An app's `current` and its version, which is `None` while there is no `current`, or `None` overall if it is unchanged.
pub type Read = Result<Option<(Current, Option<UpdateVersion>)>>;

/// The release an app runs, and its secrets, which outlive releases and override config of the same name.
#[derive(Default, Serialize, Deserialize)]
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

/// `app`'s `current`, or `None` if its ETag is still `etag`. No `current` reads as the default.
pub async fn current(store: &dyn ObjectStore, app: &str, etag: Option<String>) -> Read {
    let path = path(app, CURRENT);
    match store.get_opts(&path.as_str().into(), GetOptions { if_none_match: etag, ..Default::default() }).await {
        Ok(r) => {
            ensure!(r.meta.size <= JSON_MAX, "{path} is over {JSON_MAX} bytes");
            let version = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
            Ok(Some((serde_json::from_slice(&r.bytes().await?)?, Some(version))))
        }
        Err(E::NotFound { .. }) => Ok(Some(Default::default())),
        Err(E::NotModified { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Applies `change` to `app`'s `current` and swaps the result in with a conditional write. After losing a race it
/// waits up to a second and starts again from the winner's, so `change` must be safe to repeat.
pub async fn update(store: &dyn ObjectStore, app: &str, mut change: impl FnMut(&mut Current)) -> Result<()> {
    let path = path(app, CURRENT);
    for _ in 0..SWAPS {
        let (mut value, version) = current(store, app, None).await?.unwrap_or_default();
        change(&mut value);
        let mode = version.map_or(PutMode::Create, PutMode::Update);
        match store.put_opts(&path.as_str().into(), serde_json::to_vec(&value)?.into(), mode.into()).await {
            Ok(_) => return Ok(()),
            Err(E::Precondition { .. } | E::AlreadyExists { .. }) => {
                sleep(Duration::from_millis(RandomState::new().hash_one(()) % PAUSE_MAX)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("{path} kept changing: gave up after {SWAPS} attempts")
}
