//! The commands that change an install. They write the bucket directly, so its IAM is the only auth: a team's role
//! writes only the apps named `<team>-…`.
use crate::state::{self, BLOBS, RELEASES, Release};
use futures_util::TryStreamExt;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutPayload, memory::InMemory};
use serde::Deserialize;
use std::{cmp::Reverse, collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
use torpor::Engine;
use wasmtime::{Result, error::Context};

const MANIFEST: &str = "torpor.toml";
const COMPILE_POLL: Duration = Duration::from_secs(1);
const COMPILE_WAIT: Duration = Duration::from_secs(180); // longer than the compile function may run

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
        let (engine, kv) = (Engine::new()?, Arc::new(InMemory::new()));
        engine.load(&name, kv, &engine.compile(&wasm)?, BTreeMap::new(), &allowed_outbound_hosts)?;
    }
    let component = state::add(store, &name, BLOBS, wasm.into()).await?;
    let release = serde_json::to_vec(&Release { component, config, allowed_outbound_hosts })?;
    let id = state::add(store, &name, RELEASES, release.into()).await?;
    Ok((name, id))
}

/// Asks the compile function for the native code of `app`'s release `id`, and waits until it has made it.
pub async fn precompile(store: &dyn ObjectStore, app: &str, id: &str) -> Result<()> {
    let marker = state::marker(app, &state::release(store, app, id).await?.component)?.into();
    store.put(&marker, PutPayload::new()).await?;
    let made = async {
        loop {
            match store.head(&marker).await {
                Ok(_) => sleep(COMPILE_POLL).await,
                Err(E::NotFound { .. }) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    };
    let late =
        || format!("{app}'s release {id} has no native code after {COMPILE_WAIT:?}: see the compile function's logs");
    timeout(COMPILE_WAIT, made).await.with_context(late)??;
    Ok(())
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
