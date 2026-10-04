//! What an install keeps in its bucket. Everything a team writes sits under the app's name, so the store's IAM can give
//! each team the apps named `<team>-…`:
//!
//! - `apps/<app>/blobs/sha256/<hash>`: components, in the OCI image layout, so a registry copy is a byte copy;
//! - `apps/<app>/releases/<hash>`: the app's releases;
//! - `apps/<app>/current`: the release it runs and that release's component, so a host fetches both at once, and its
//!   secrets;
//! - `kv/<app>/<bucket>/<key>`: its `wasi:keyvalue` data, which only hosts write, unless `--kv` gives it a bucket;
//! - `compile/<app>/<hash>`: a marker that asks the compile function for the component's native code.
//!
//! Nothing read back is trusted: every read is capped, parsed strictly, and content-addressed objects are checked
//! against their hash. Writes have the same caps, so nothing is written that a read would refuse.
//!
//! An install may also have a bucket of native code, which only the compile function writes, at
//! `<app>/<hash>/<compat>.zst`. It is under the app, so an app loads only native code made from its own component.
use hyper::body::Bytes;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tric::{NAME_MAX, is_name};
use wasmtime::{Result, bail, ensure, error::Context};

const CURRENT: &str = "current";
const MARKERS: &str = "compile/";
const JSON_MAX: u64 = 64 << 10; // a release id, config and secrets, so a host keeping one per app stays small
pub const BLOBS: Kind = Kind { dir: "blobs/sha256", max: 128 << 20 };
pub const RELEASES: Kind = Kind { dir: "releases", max: JSON_MAX };

/// A kind of content-addressed object: the directory that holds an app's, and the most bytes one may hold.
#[derive(Clone, Copy)]
pub struct Kind {
    pub dir: &'static str,
    max: u64,
}

/// The release an app runs, and its secrets, which outlive releases and override config of the same name.
#[derive(Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub release: Option<Live>,
    pub secrets: BTreeMap<String, String>,
}

/// The id of the release an app runs, and its component's hash.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Live {
    pub id: String,
    pub component: String,
}

/// One immutable release of an app.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String,
    pub config: BTreeMap<String, String>,
    pub allowed_outbound_hosts: Vec<String>,
}

/// `app`, if it is a name a host would serve.
fn checked(app: &str) -> Result<&str> {
    ensure!(is_name(app), "an app name is 1 to {NAME_MAX} of a-z, 0-9 and -, not {app:?}");
    Ok(app)
}

/// `app`'s `object`.
pub fn path(app: &str, object: &str) -> Result<String> {
    Ok(format!("apps/{}/{object}", checked(app)?))
}

pub fn kv(app: &str) -> String {
    format!("kv/{app}")
}

/// Where the markers of `app`'s components are.
pub fn markers(app: &str) -> Result<String> {
    Ok(format!("{MARKERS}{}", checked(app)?))
}

/// The marker that asks for the native code of `app`'s component `hash`.
pub fn marker(app: &str, hash: &str) -> Result<String> {
    Ok(format!("{}/{hash}", markers(app)?))
}

/// The app and the hash that the marker `key` names, if it is one: just what `marker` makes.
pub fn marked(key: &str) -> Option<(&str, &str)> {
    let (app, hash) = key.strip_prefix(MARKERS)?.split_once('/')?;
    (is_name(app) && is_hash(hash)).then_some((app, hash))
}

/// Where the native code of `app`'s component `hash` is in the bucket of native code, for engines of `compat`.
pub fn native(app: &str, hash: &str, compat: &str) -> Result<String> {
    Ok(format!("{}/{hash}/{compat}.zst", checked(app)?))
}

/// The hash of the component whose native code is at `key`, if it is such a key: just what `native` makes.
pub fn compiled(key: &str) -> Option<&str> {
    match *key.split('/').collect::<Vec<_>>() {
        [app, hash, file] if is_name(app) && is_hash(hash) && file.ends_with(".zst") => Some(hash),
        _ => None,
    }
}

fn is_hash(s: &str) -> bool {
    s.len() == 2 * Sha256::output_size() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The object at `path` and its version, or `None` if there is none.
async fn read(store: &dyn ObjectStore, path: &str, max: u64) -> Result<Option<(Bytes, UpdateVersion)>> {
    let mut r = match store.get(&path.into()).await {
        Err(E::NotFound { .. }) => return Ok(None),
        r => r?,
    };
    ensure!(r.meta.size <= max, "{path} is over {max} bytes");
    let version = UpdateVersion { e_tag: r.meta.e_tag.take(), version: r.meta.version.take() };
    Ok(Some((r.bytes().await?, version)))
}

/// Where `app`'s object `hash` of the kind `kind` is.
pub fn object(app: &str, kind: Kind, hash: &str) -> Result<String> {
    Ok(format!("{}/{hash}", path(app, kind.dir)?))
}

/// `app`'s object `hash` of the kind `kind`, refused if its hash is not `hash`.
pub async fn fetch(store: &dyn ObjectStore, app: &str, kind: Kind, hash: &str) -> Result<Bytes> {
    let path = object(app, kind, hash)?;
    let (bytes, _) = read(store, &path, kind.max).await?.with_context(|| format!("there is no {path}"))?;
    ensure!(format!("{:x}", Sha256::digest(&bytes)) == hash, "{path} does not match its hash");
    Ok(bytes)
}

/// Stores `bytes` as an object of `app` of the kind `kind`, and returns its hash. Storing it again, as a publish of what
/// was published before does, makes it new again, so `gc` keeps it.
pub async fn add(store: &dyn ObjectStore, app: &str, kind: Kind, bytes: Bytes) -> Result<String> {
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let path = object(app, kind, &hash)?;
    ensure!(bytes.len() as u64 <= kind.max, "{path} would be over {} bytes", kind.max);
    store.put(&path.into(), bytes.into()).await?;
    Ok(hash)
}

/// `app`'s release `id`.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<Release> {
    Ok(serde_json::from_slice(&fetch(store, app, RELEASES, id).await?)?)
}

/// `app`'s `current` and its version, or the default and `None` if it has none.
pub async fn current(store: &dyn ObjectStore, app: &str) -> Result<(Current, Option<UpdateVersion>)> {
    match read(store, &path(app, CURRENT)?, JSON_MAX).await? {
        Some((bytes, version)) => Ok((serde_json::from_slice(&bytes)?, Some(version))),
        None => Ok(Default::default()),
    }
}

/// Applies `change` to `app`'s `current` with a conditional write, which fails if another change landed in between, so
/// neither is lost.
pub async fn update(store: &dyn ObjectStore, app: &str, change: impl FnOnce(&mut Current)) -> Result<()> {
    let (mut value, version) = current(store, app).await?;
    change(&mut value);
    let json = serde_json::to_vec(&value)?;
    ensure!(json.len() as u64 <= JSON_MAX, "{app}'s {CURRENT} would be over {JSON_MAX} bytes");
    let mode = version.map_or(PutMode::Create, PutMode::Update);
    match store.put_opts(&path(app, CURRENT)?.into(), json.into(), mode.into()).await {
        Ok(_) => Ok(()),
        // a conditional write to a deleted `current` is refused with a 404
        Err(E::Precondition { .. } | E::AlreadyExists { .. } | E::NotFound { .. }) => {
            bail!("{app} changed while this ran: run it again")
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[tokio::test]
    async fn refuses_what_a_host_would_not_serve() {
        let store = InMemory::new();
        for app in ["Acme-blog", "../x", "a/b", ""] {
            assert!(update(&store, app, |_| {}).await.is_err(), "{app}");
            assert!(add(&store, app, BLOBS, "x".into()).await.is_err(), "{app}");
        }
        let big = "x".repeat(JSON_MAX as usize);
        let e = update(&store, "app", |c| _ = c.secrets.insert("big".into(), big)).await.unwrap_err();
        assert!(e.to_string().contains("would be over"), "{e}");
        assert!(current(&store, "app").await.unwrap().0 == Current::default()); // and it still reads
    }

    #[tokio::test]
    async fn adding_again_makes_it_new() {
        let store = InMemory::new();
        let mut times = vec![];
        for _ in 0..2 {
            let key = object("app", BLOBS, &add(&store, "app", BLOBS, "x".into()).await.unwrap()).unwrap();
            times.push(store.head(&key.into()).await.unwrap().last_modified);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(times[1] > times[0]); // so `gc` keeps it
    }

    #[test]
    fn reads_only_the_keys_it_makes() {
        let (hash, upper) = ("a".repeat(64), "A".repeat(64));
        assert_eq!(marked(&marker("app", &hash).unwrap()), Some(("app", &*hash)));
        let keys = [format!("compile/app//{hash}"), format!("compile/App/{hash}"), format!("compile/app/{upper}")];
        for key in keys.into_iter().chain([format!("compile/app/{hash}0"), format!("kv/app/{hash}")]) {
            assert!(marked(&key).is_none(), "{key}");
        }
        assert_eq!(compiled(&native("app", &hash, "0123").unwrap()), Some(&*hash));
        let keys = [format!("app/{hash}/0123"), format!("app/{upper}/0.zst"), format!("apps/app/{hash}/0.zst")];
        let keys = keys.into_iter().chain([format!("app/{hash}/0.zst/x")]);
        for key in keys.chain([marker("app", &hash).unwrap(), object("app", BLOBS, &hash).unwrap()]) {
            assert!(compiled(&key).is_none(), "{key}"); // so a bucket of native code that is the app bucket loses nothing
        }
    }
}
