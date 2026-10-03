//! The commands that change an install. They write the bucket directly, so its IAM is the only auth: a team's role
//! writes only the apps named `<team>-…`.
use crate::state::{self, BLOBS, RELEASES, Release};
use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory};
use serde::Deserialize;
use std::{cmp::Reverse, collections::BTreeMap, fs, path::Path, sync::Arc};
use torpor::Engine;
use wasmtime::Result;

const MANIFEST: &str = "torpor.toml";

/// An app's `MANIFEST`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a misspelt `allowed_outbound_hosts` would silently grant no access
pub struct Manifest {
    pub name: String,
    component: String, // relative to the manifest's directory
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default)]
    pub allowed_outbound_hosts: Vec<String>, // as in Spin: `scheme://host[:port]`
}

/// The manifest in `dir`, and its component.
pub fn read(dir: &Path) -> Result<(Manifest, Vec<u8>)> {
    let m: Manifest = toml::from_str(&fs::read_to_string(dir.join(MANIFEST))?)?;
    let wasm = fs::read(dir.join(&m.component))?;
    Ok((m, wasm))
}

/// Uploads the app in `dir` as a release, without serving it, and returns the app's name and the release's id. With
/// `check`, it first compiles the app, to refuse now what a host would refuse.
pub async fn publish(store: &dyn ObjectStore, dir: &Path, check: bool) -> Result<(String, String)> {
    let (Manifest { name, config, allowed_outbound_hosts, .. }, wasm) = read(dir)?;
    if check {
        Engine::new()?.load(&name, Arc::new(InMemory::new()), &wasm, BTreeMap::new(), &allowed_outbound_hosts)?;
    }
    let component = state::add(store, &name, BLOBS, wasm.into()).await?;
    let release = serde_json::to_vec(&Release { component, config, allowed_outbound_hosts })?;
    let id = state::add(store, &name, RELEASES, release.into()).await?;
    Ok((name, id))
}

/// Serves `app` from its release `id`.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<()> {
    let r = state::release(store, app, id).await?; // it is there, and sound
    store.head(&state::object(app, BLOBS, &r.component)?.into()).await?; // as is its component
    state::update(store, app, |c| c.release = Some(id.into())).await
}

/// `app`'s releases, newest first, each as its id and when it was first published.
pub async fn releases(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let mut all: Vec<_> = store.list(Some(&state::path(app, RELEASES.dir)?.into())).try_collect().await?;
    all.sort_by_key(|m| Reverse(m.last_modified));
    Ok(all.iter().map(|m| format!("{} {}", m.location.filename().unwrap_or_default(), m.last_modified)).collect())
}

/// Sets `app`'s secret `name` to `value`, in whichever release it runs, or removes it if `value` is empty.
pub async fn set_secret(store: &dyn ObjectStore, app: &str, name: &str, value: &str) -> Result<()> {
    state::update(store, app, |c| {
        _ = match value {
            "" => c.secrets.remove(name),
            _ => c.secrets.insert(name.into(), value.into()),
        }
    })
    .await
}

/// The names of `app`'s secrets.
pub async fn secrets(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    Ok(state::current(store, app).await?.0.secrets.into_keys().collect())
}
