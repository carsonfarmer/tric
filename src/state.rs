//! What an install keeps in its bucket. Everything a team writes sits under the app's name, so the store's IAM can give
//! each team the apps named `<team>-…`:
//!
//! - `apps/<app>/current`: the release the app runs, the releases it keeps, which it can go back to, and its secrets;
//! - `apps/<app>/releases/<n>/`: the folder of a release it keeps, with the release, its component, and the native code
//!   that the compile function made of the component for engines of `compat`, at `release`, `component` and
//!   `<compat>-<component's hash>.zst`;
//! - `kv/<app>/<bucket>/<key>`: its `wasi:keyvalue` data, which only hosts write, unless `--kv` gives it a bucket;
//! - `compile/<app>/<n>`: a marker that asks the compile function for the native code of folder `n`.
//!
//! A folder is never used again once its release is dropped, so each change deletes the folders that `current` has
//! dropped, and nothing else has to. Nothing read back is trusted but native code: every other read is capped, parsed
//! strictly, and a release and its component are checked against their hashes. Writes have the same caps, so nothing is
//! written that a read would refuse. Native code can't be checked, and loading it runs it, so only the compile function
//! may write it: the store's IAM lets teams write only keys ending in `current`, `release` and `component`, and no one
//! else a key ending in `.zst`.
use futures_util::{StreamExt, TryStreamExt, stream};
use hyper::body::Bytes;
use object_store::{Error as E, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tric::{NAME_MAX, is_name};
use wasmtime::{Result, bail, ensure, error::Context};

const CURRENT: &str = "current";
pub const RELEASE: &str = "release";
pub const COMPONENT: &str = "component";
const MARKERS: &str = "compile/";
const JSON_MAX: u64 = 64 << 10; // a release or `current`, so a host keeping one of each per app stays small
pub const COMPONENT_MAX: u64 = 128 << 20;
pub const KEEP: usize = 10; // the publishes an app keeps besides the release it runs: how far back it can go
const TRIES: usize = 3; // of a change, while others keep landing first

/// The release an app runs, the releases it keeps, and its secrets, which outlive releases and override config of the
/// same name.
#[derive(Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub release: Option<Entry>,
    pub releases: Vec<Entry>, // its last `KEEP` publishes, newest first
    pub next: u64,            // the folder its next new release gets
    pub secrets: BTreeMap<String, String>,
}

impl Current {
    /// The releases it keeps: the one it runs, if any, then the rest.
    pub fn kept(&self) -> impl Iterator<Item = &Entry> {
        self.release.iter().chain(&self.releases)
    }
}

/// A release an app keeps: its id, which is its hash, its component's hash, and its folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub component: String,
    pub n: u64,
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
fn path(app: &str, object: &str) -> Result<String> {
    Ok(format!("apps/{}/{object}", checked(app)?))
}

/// The file `name` in `app`'s folder `n`.
pub fn file(app: &str, n: u64, name: &str) -> Result<String> {
    path(app, &format!("releases/{n}/{name}"))
}

/// Where the native code made of the component with the hash `component`, in `app`'s folder `n`, is for engines of
/// `compat`. It ends in `.zst`, never in what a team may write, which is how the store's IAM keeps teams from writing
/// native code; so the compile function, which hashes what it compiles, vouches that it was made of `component`.
pub fn native(app: &str, n: u64, component: &str, compat: &str) -> Result<String> {
    file(app, n, &format!("{compat}-{component}.zst"))
}

pub fn kv(app: &str) -> String {
    format!("kv/{app}")
}

/// The marker that asks for the native code of `app`'s folder `n`.
pub fn marker(app: &str, n: u64) -> Result<String> {
    Ok(format!("{MARKERS}{}/{n}", checked(app)?))
}

/// The app and the folder that the marker `key` names, if it is one: just what `marker` makes.
pub fn marked(key: &str) -> Option<(&str, u64)> {
    let (app, n) = key.strip_prefix(MARKERS)?.split_once('/')?;
    let n = n.parse().ok()?;
    (marker(app, n).ok()? == key).then_some((app, n))
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The object at `path` and its version, or `None` if there is none.
pub async fn read(store: &dyn ObjectStore, path: &str, max: u64) -> Result<Option<(Bytes, UpdateVersion)>> {
    let mut r = match store.get(&path.into()).await {
        Err(E::NotFound { .. }) => return Ok(None),
        r => r?,
    };
    ensure!(r.meta.size <= max, "{path} is over {max} bytes");
    let version = UpdateVersion { e_tag: r.meta.e_tag.take(), version: r.meta.version.take() };
    Ok(Some((r.bytes().await?, version)))
}

/// Whether there is an object at `path`.
pub async fn exists(store: &dyn ObjectStore, path: &Path) -> Result<bool> {
    match store.head(path).await {
        Err(E::NotFound { .. }) => Ok(false),
        r => Ok(r.map(|_| true)?),
    }
}

/// The object at `path`, refused if its hash is not `hash`.
async fn fetch(store: &dyn ObjectStore, path: &str, max: u64, hash: &str) -> Result<Bytes> {
    let (bytes, _) = read(store, path, max).await?.with_context(|| format!("there is no {path}"))?;
    ensure!(self::hash(&bytes) == hash, "{path} does not match its hash");
    Ok(bytes)
}

/// `app`'s release `e`.
pub async fn release(store: &dyn ObjectStore, app: &str, e: &Entry) -> Result<Release> {
    Ok(serde_json::from_slice(&fetch(store, &file(app, e.n, RELEASE)?, JSON_MAX, &e.id).await?)?)
}

/// The component of `app`'s release `e`.
pub async fn component(store: &dyn ObjectStore, app: &str, e: &Entry) -> Result<Bytes> {
    fetch(store, &file(app, e.n, COMPONENT)?, COMPONENT_MAX, &e.component).await
}

/// Publishes the component `wasm` with `config` and `allowed_outbound_hosts` as a release of `app`, without serving it,
/// and returns its entry. It is recorded before it is written, so no change deletes its folder meanwhile. A release
/// that `app` keeps keeps its folder, which gets what it lacks, and a component that another kept release has is
/// copied.
pub async fn publish(
    store: &dyn ObjectStore,
    app: &str,
    wasm: Bytes,
    config: BTreeMap<String, String>,
    allowed_outbound_hosts: Vec<String>,
) -> Result<Entry> {
    let component = hash(&wasm);
    let json = serde_json::to_vec(&Release { component: component.clone(), config, allowed_outbound_hosts })?;
    ensure!(json.len() as u64 <= JSON_MAX, "{app}'s release would be over {JSON_MAX} bytes");
    ensure!(wasm.len() as u64 <= COMPONENT_MAX, "{app}'s component would be over {COMPONENT_MAX} bytes");
    let id = hash(&json);
    let (e, from) = update(store, app, |c| {
        let e = c.kept().find(|e| e.id == id).cloned();
        let e = e.unwrap_or_else(|| Entry { id: id.clone(), component: component.clone(), n: c.next });
        c.next = c.next.max(e.n.checked_add(1).with_context(|| format!("{app} has used every folder number"))?);
        c.releases.retain(|r| r.n != e.n);
        c.releases.insert(0, e.clone());
        c.releases.truncate(KEEP);
        let from = c.kept().find(|r| r.component == component && r.n != e.n).map(|r| r.n);
        Ok((e, from))
    })
    .await?;
    for (name, bytes) in [(RELEASE, Bytes::from(json)), (COMPONENT, wasm)] {
        let to = file(app, e.n, name)?.into();
        if exists(store, &to).await? {
            continue; // as when it is published again
        }
        let copied = match from {
            Some(from) if name == COMPONENT => store.copy(&file(app, from, name)?.into(), &to).await.is_ok(),
            _ => false,
        };
        if !copied {
            store.put(&to, bytes.into()).await?;
        }
    }
    Ok(e)
}

/// `app`'s `current` and its version, or the default and `None` if it has none.
pub async fn current(store: &dyn ObjectStore, app: &str) -> Result<(Current, Option<UpdateVersion>)> {
    match read(store, &path(app, CURRENT)?, JSON_MAX).await? {
        Some((bytes, version)) => Ok((serde_json::from_slice(&bytes)?, Some(version))),
        None => Ok(Default::default()),
    }
}

/// Applies `change` to `app`'s `current` with a conditional write, which fails if another change landed in between, so
/// neither is lost, and then reads it and tries again, `TRIES` times in all.
///
/// Each try first deletes the folders that the `current` it read dropped: those below its `next` that it does not keep.
/// No later `current` keeps them, so they go whether or not the change lands, and as they go a change after the one
/// that dropped them, a host has mostly moved on from them. What a crash or a slow publish leaves goes the same way.
/// Then it takes `next` past every folder there, so none is used again, even after `current` is deleted, when nothing
/// is below its `next`, or an old one is brought back; and the change after it deletes those it does not keep.
pub async fn update<T>(
    store: &dyn ObjectStore,
    app: &str,
    mut change: impl FnMut(&mut Current) -> Result<T>,
) -> Result<T> {
    let (at, folders) = (path(app, CURRENT)?.into(), path(app, "releases")?.into());
    let folder = |m: &ObjectMeta| m.location.parts().nth(3)?.as_ref().parse::<u64>().ok();
    for _ in 0..TRIES {
        let (mut value, version) = current(store, app).await?;
        let files: Vec<_> = store.list(Some(&folders)).try_collect().await?;
        let past = files.iter().filter_map(folder).max().map_or(0, |n| n.saturating_add(1));
        let dropped = |n: u64| n < value.next && !value.kept().any(|e| e.n == n);
        let gone: Vec<_> =
            files.into_iter().filter(|m| folder(m).is_some_and(dropped)).map(|m| Ok::<_, E>(m.location)).collect();
        store.delete_stream(stream::iter(gone).boxed()).try_collect::<Vec<_>>().await?;
        value.next = value.next.max(past);
        let out = change(&mut value)?;
        let json = serde_json::to_vec(&value)?;
        ensure!(json.len() as u64 <= JSON_MAX, "{app}'s {CURRENT} would be over {JSON_MAX} bytes");
        let mode = version.map_or(PutMode::Create, PutMode::Update);
        match store.put_opts(&at, json.into(), mode.into()).await {
            Ok(_) => return Ok(out),
            // a conditional write to a deleted `current` is refused with a 404
            Err(E::Precondition { .. } | E::AlreadyExists { .. } | E::NotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    bail!("{app} kept changing while this ran: run it again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{PutPayload, memory::InMemory};

    #[tokio::test]
    async fn refuses_what_a_host_would_not_serve() {
        let store = InMemory::new();
        for app in ["Acme-blog", "../x", "a/b", ""] {
            assert!(update(&store, app, |_| Ok(())).await.is_err(), "{app}");
            assert!(publish(&store, app, "x".into(), BTreeMap::new(), vec![]).await.is_err(), "{app}");
        }
        let big = "x".repeat(JSON_MAX as usize);
        let e = update(&store, "app", |c| Ok(_ = c.secrets.insert("big".into(), big.clone()))).await.unwrap_err();
        assert!(e.to_string().contains("would be over"), "{e}");
        assert!(current(&store, "app").await.unwrap().0 == Current::default()); // and it still reads
    }

    #[test]
    fn reads_only_the_markers_it_makes() {
        assert_eq!(marked(&marker("app", 7).unwrap()), Some(("app", 7)));
        for key in [
            "compile/app//7",
            "compile/App/7",
            "compile/app/x",
            "compile/app/7/8",
            "compile/app/-1",
            "compile/app/+7",
            "compile/app/07",
            "kv/app/7",
        ] {
            assert!(marked(key).is_none(), "{key}");
        }
    }

    /// A change deletes the folders that the one before it dropped, and what is written to them later, but not a folder
    /// at or above the `next` it read, as a publish may be writing it, nor anything else, nor anything with no
    /// `current`. Either way it takes `next` past every folder there, and the change after it deletes those it does not
    /// keep.
    #[tokio::test]
    async fn a_change_deletes_what_was_dropped() {
        let store = InMemory::new();
        let entry = |n| Entry { id: String::new(), component: String::new(), n };
        update(&store, "app", |c| Ok((c.release, c.releases, c.next) = (Some(entry(0)), vec![entry(1), entry(2)], 3)))
            .await
            .unwrap();
        for key in ["0/release", "1/component", "2/release", "3/component", "x/release"] {
            store.put(&format!("apps/app/releases/{key}").into(), PutPayload::new()).await.unwrap();
        }
        let left = async || {
            let all: Vec<_> = store.list(Some(&"apps/app/releases".into())).try_collect().await.unwrap();
            let mut all: Vec<_> = all.iter().map(|m| m.location.as_ref()[18..].to_string()).collect();
            all.sort();
            all
        };
        assert_eq!(update(&store, "app", |c| Ok(c.releases.pop().map(|_| c.next))).await.unwrap(), Some(4));
        assert_eq!(left().await.len(), 5); // as a host may still be loading 2
        store.put(&native("app", 2, "c", "0123").unwrap().into(), PutPayload::new()).await.unwrap(); // a late compile's
        update(&store, "app", |_| Ok(())).await.unwrap();
        assert_eq!(left().await, ["0/release", "1/component", "x/release"]);

        store.delete(&"apps/app/current".into()).await.unwrap();
        assert_eq!(update(&store, "app", |c| Ok(c.next)).await.unwrap(), 2);
        assert_eq!(left().await.len(), 3);
        update(&store, "app", |_| Ok(())).await.unwrap();
        assert_eq!(left().await, ["x/release"]);
    }
}
