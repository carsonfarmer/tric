//! What an install keeps in its bucket: components and releases, both stored under their SHA-256, and one state object
//! naming each app's current release. Nothing read back is trusted: every read is capped, parsed strictly, and
//! content-addressed objects are checked against their hash.
use hyper::body::Bytes;
use object_store::{Error as E, GetOptions, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::hash::{BuildHasher, RandomState};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::sleep;
use wasmtime::{Result, bail, ensure};

pub const BLOBS: &str = "blobs/sha256"; // the OCI image-layout path, so a registry copy is a byte copy
pub const RELEASES: &str = "manifests";
const STATE: &str = "state";
pub const BLOB_MAX: u64 = 128 << 20;
pub const RELEASE_MAX: u64 = 1 << 20;
const STATE_MAX: u64 = 8 << 20; // about 100k apps
const SWAPS: usize = 32; // attempts at one state change before giving up
const PAUSE_MAX: u64 = 1000; // ms, the longest wait after a lost swap
/// How stale a host's state may get, so also how long a change takes to be live everywhere.
pub const FRESH: Duration = Duration::from_secs(5);

/// Each app's current release, and the app that serves each domain.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub apps: BTreeMap<String, String>,
    pub domains: BTreeMap<String, String>,
}

/// One immutable release of an app.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String,
    pub parent: Option<String>, // the release this one replaced
    pub config: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, String>, // each value age-encrypted on its own, so names show without decrypting
    pub allowed_outbound_hosts: Vec<String>,
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

pub async fn release(store: &dyn ObjectStore, hash: &str) -> Result<Release> {
    Ok(serde_json::from_slice(&fetch(store, RELEASES, hash, RELEASE_MAX).await?)?)
}

/// The state and its version, or `None` before the first deploy or while the state's ETag is still `etag`.
pub async fn read(store: &dyn ObjectStore, etag: Option<String>) -> Result<Option<(State, UpdateVersion)>> {
    match store.get_opts(&STATE.into(), GetOptions { if_none_match: etag, ..Default::default() }).await {
        Ok(r) => {
            ensure!(r.meta.size <= STATE_MAX, "the state is over {STATE_MAX} bytes");
            let version = UpdateVersion { e_tag: r.meta.e_tag.clone(), version: r.meta.version.clone() };
            Ok(Some((serde_json::from_slice(&r.bytes().await?)?, version)))
        }
        Err(E::NotFound { .. } | E::NotModified { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Applies `change` to the current state and swaps the result in with a conditional write. After losing a race it waits
/// up to a second and starts again from the winner's state, so `change` must be safe to repeat.
pub async fn update(store: &dyn ObjectStore, mut change: impl AsyncFnMut(&mut State) -> Result<()>) -> Result<()> {
    for _ in 0..SWAPS {
        let first = (State::default(), PutMode::Create);
        let (mut state, mode) = read(store, None).await?.map_or(first, |(s, v)| (s, PutMode::Update(v)));
        change(&mut state).await?;
        match store.put_opts(&STATE.into(), serde_json::to_vec(&state)?.into(), mode.into()).await {
            Ok(_) => return Ok(()),
            Err(E::Precondition { .. } | E::AlreadyExists { .. }) => {
                sleep(Duration::from_millis(RandomState::new().hash_one(()) % PAUSE_MAX)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("the state kept changing: gave up after {SWAPS} attempts")
}
