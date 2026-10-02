//! The commands that change an install. They write the bucket directly, so its IAM is the only auth: `assign` takes the
//! admin's credentials, and the rest take those of the app's team.
use crate::state::{self, BLOB_MAX, Current, INDEX, Index, JSON_MAX, Release};
use futures_util::TryStreamExt;
use object_store::{ObjectStore, memory::InMemory};
use serde::Deserialize;
use std::{cmp::Reverse, collections::BTreeMap, fs, path::Path, sync::Arc};
use torpor::{Engine, NAME_MAX, is_name};
use wasmtime::{Error, Result, ensure, error::Context};

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

/// Puts `app` in `team`, or with no team, takes it out of the install. Its objects stay where they were, so an app that
/// moves to another team starts there with nothing published and no KV data.
pub async fn assign(store: &dyn ObjectStore, app: &str, team: Option<&str>) -> Result<()> {
    for name in [Some(app), team].into_iter().flatten() {
        ensure!(is_name(name), "a name is 1 to {NAME_MAX} of a-z, 0-9 and -, not {name:?}");
    }
    state::update(store, INDEX, |i: &mut Index| {
        _ = match team {
            Some(t) => i.insert(app.into(), t.into()),
            None => i.remove(app),
        }
    })
    .await
}

/// Uploads the app in `dir` as a release, without serving it, and returns the app's name and the release's id.
pub async fn publish(store: &dyn ObjectStore, dir: &Path) -> Result<(String, String)> {
    let (Manifest { name, config, allowed_outbound_hosts, .. }, wasm) = read(dir)?;
    let team = team(store, &name).await?;
    let engine = Engine::new(Arc::new(InMemory::new()))?;
    engine.load(&name, "", &wasm, BTreeMap::new(), &allowed_outbound_hosts)?; // refuse now what a host would refuse
    let component = state::add(store, &state::blobs(&team), wasm.into(), BLOB_MAX).await?;
    let release = serde_json::to_vec(&Release { component, config, allowed_outbound_hosts })?;
    let id = state::add(store, &state::releases(&team, &name), release.into(), JSON_MAX).await?;
    Ok((name, id))
}

/// Serves `app` from its release `id`.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<()> {
    let team = team(store, app).await?;
    state::release(store, &state::releases(&team, app), id).await?; // it is there, and sound
    state::update(store, &state::current(&team, app), |c: &mut Current| c.release = Some(id.into())).await
}

/// `app`'s releases, newest first, each as its id and when it was first published.
pub async fn releases(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let dir = state::releases(&team(store, app).await?, app);
    let mut all: Vec<_> = store.list(Some(&dir.into())).try_collect().await?;
    all.sort_by_key(|m| Reverse(m.last_modified));
    Ok(all.iter().map(|m| format!("{} {}", m.location.filename().unwrap_or_default(), m.last_modified)).collect())
}

/// Sets `app`'s secret `name` to `value`, encrypted to `recipient` (`age1…`), in whichever release it runs.
pub async fn set_secret(store: &dyn ObjectStore, app: &str, name: &str, value: &str, recipient: &str) -> Result<()> {
    let recipient: age::x25519::Recipient = recipient.parse().map_err(Error::msg)?;
    let sealed = age::encrypt_and_armor(&recipient, value.as_bytes())?;
    let current = state::current(&team(store, app).await?, app);
    state::update(store, &current, |c: &mut Current| _ = c.secrets.insert(name.into(), sealed.clone())).await
}

/// The names of `app`'s secrets. Nothing is decrypted.
pub async fn secrets(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let current: Option<(Current, _)> =
        state::read(store, &state::current(&team(store, app).await?, app), None).await?;
    Ok(current.map(|(c, _)| c.secrets.into_keys().collect()).unwrap_or_default())
}

async fn team(store: &dyn ObjectStore, app: &str) -> Result<String> {
    let index: Option<(Index, _)> = state::read(store, INDEX, None).await?;
    index.and_then(|(mut i, _)| i.remove(app)).with_context(|| format!("there is no app {app}"))
}
