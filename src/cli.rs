//! `deploy`, `rollback` and `secrets`. They write the bucket directly, so its access control is the only auth.
use crate::state::{self, BLOB_MAX, BLOBS, RELEASE_MAX, RELEASES, Release, State};
use object_store::{ObjectStore, memory::InMemory};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc};
use torpor::Engine;
use wasmtime::{Error, Result, bail, ensure};

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

/// Deploys the app in each of `dirs` with one state change. Each new release keeps its app's secrets.
pub async fn deploy(store: &dyn ObjectStore, dirs: &[impl AsRef<Path>]) -> Result<()> {
    let engine = Engine::new(Arc::new(InMemory::new()))?;
    let mut apps = Vec::new();
    for dir in dirs {
        let (Manifest { name, config, allowed_outbound_hosts, .. }, wasm) = read(dir.as_ref())?;
        engine.load(&name, &wasm, BTreeMap::new(), &allowed_outbound_hosts)?; // refuse now what a host would refuse
        let component = state::add(store, BLOBS, wasm.into(), BLOB_MAX).await?;
        apps.push((name, Release { component, config, allowed_outbound_hosts, ..Default::default() }));
    }
    state::update(store, async |s| {
        for (name, r) in &mut apps {
            r.parent = s.apps.get(name).cloned();
            r.secrets = match &r.parent {
                Some(p) => state::release(store, p).await?.secrets,
                None => BTreeMap::new(),
            };
            if let Some(k) = r.config.keys().find(|k| r.secrets.contains_key(*k)) {
                bail!("{name}: `{k}` is both config and a secret");
            }
            s.apps.insert(name.clone(), state::add(store, RELEASES, serde_json::to_vec(r)?.into(), RELEASE_MAX).await?);
        }
        Ok(())
    })
    .await
}

/// Moves `app` back to the release its current one replaced.
pub async fn rollback(store: &dyn ObjectStore, app: &str) -> Result<()> {
    state::update(store, async |s| {
        let Some(parent) = current(store, s, app).await?.1.parent else { bail!("{app} has no earlier release") };
        state::release(store, &parent).await?; // it is still there, and sound
        s.apps.insert(app.into(), parent);
        Ok(())
    })
    .await
}

/// Makes a release of `app` with its secret `name` set to `value`, encrypted to `recipient` (`age1…`).
pub async fn set_secret(store: &dyn ObjectStore, app: &str, name: &str, value: &str, recipient: &str) -> Result<()> {
    let recipient: age::x25519::Recipient = recipient.parse().map_err(Error::msg)?;
    let sealed = age::encrypt_and_armor(&recipient, value.as_bytes())?;
    state::update(store, async |s| {
        let (hash, mut r) = current(store, s, app).await?;
        ensure!(!r.config.contains_key(name), "{app}: `{name}` is config");
        r.secrets.insert(name.into(), sealed.clone());
        r.parent = Some(hash);
        s.apps.insert(app.into(), state::add(store, RELEASES, serde_json::to_vec(&r)?.into(), RELEASE_MAX).await?);
        Ok(())
    })
    .await
}

/// The names of `app`'s secrets. Nothing is decrypted.
pub async fn secrets(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let s = state::read(store, None).await?.map(|(s, _)| s).unwrap_or_default();
    Ok(current(store, &s, app).await?.1.secrets.into_keys().collect())
}

/// `app`'s current release and its hash.
async fn current(store: &dyn ObjectStore, s: &State, app: &str) -> Result<(String, Release)> {
    let Some(hash) = s.apps.get(app) else { bail!("there is no app {app}") };
    Ok((hash.clone(), state::release(store, hash).await?))
}
