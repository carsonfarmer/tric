//! The commands that change an install. They write the bucket directly, so its IAM is the only auth: a team's role
//! writes only the apps named `<team>-…`.
use crate::state::{self, COMPONENT, Current, Entry, RELEASE};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, memory::InMemory};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
use tric::Engine;
use wasmtime::{Result, ensure, error::Context};

const MANIFEST: &str = "tric.toml";
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

/// Uploads the app in `dir` as a release, without serving it, and returns the app's name and the release. With `check`,
/// it first compiles the app, to refuse now what a host would refuse.
pub async fn publish(store: &dyn ObjectStore, dir: &Path, check: bool) -> Result<(String, Entry)> {
    let (Manifest { name, config, allowed_outbound_hosts, .. }, wasm) = read(dir)?;
    if check {
        let (engine, kv) = (Engine::new()?, Arc::new(InMemory::new()));
        engine.load(&name, kv, &engine.compile(&wasm)?, BTreeMap::new(), &allowed_outbound_hosts)?;
    }
    let e = state::publish(store, &name, wasm.into(), config, allowed_outbound_hosts).await?;
    Ok((name, e))
}

/// Asks the compile function for the native code of `app`'s release `e`, and waits until it has made it.
pub async fn precompile(store: &dyn ObjectStore, app: &str, e: &Entry) -> Result<()> {
    let marker = state::marker(app, e.n)?.into();
    store.put(&marker, PutPayload::new()).await?;
    let made = async {
        while state::exists(store, &marker).await? {
            sleep(COMPILE_POLL).await;
        }
        Ok::<_, wasmtime::Error>(())
    };
    let late = || {
        format!("{app}'s release {} has no native code after {COMPILE_WAIT:?}: see the compile function's logs", e.id)
    };
    timeout(COMPILE_WAIT, made).await.with_context(late)??;
    Ok(())
}

/// Serves `app` from its release `id`, which must be one it keeps, and whole, as a publish that failed leaves it not.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<()> {
    let kept = |c: &Current| {
        let e = c.kept().find(|e| e.id == id).cloned();
        e.with_context(|| format!("{app} keeps no release {id}: see `tric releases {app}`"))
    };
    let e = kept(&state::current(store, app).await?.0)?;
    for name in [RELEASE, COMPONENT] {
        let whole = state::exists(store, &state::file(app, e.n, name)?.into()).await?;
        ensure!(whole, "{app}'s release {id} has no {name}, as its publish failed: publish it again");
    }
    state::update(store, app, |c| {
        ensure!(kept(c)? == e, "{app}'s release {id} was published again while this ran: run it again");
        c.release = Some(e.clone());
        Ok(())
    })
    .await
}

/// The ids of `app`'s releases, newest first, marking the one it serves.
pub async fn releases(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let Current { release: live, mut releases, .. } = state::current(store, app).await?.0;
    releases.extend(live.clone().filter(|l| !releases.contains(l)));
    let mark = |e: Entry| if Some(&e) == live.as_ref() { format!("{} live", e.id) } else { e.id };
    Ok(releases.into_iter().map(mark).collect())
}

/// Sets `app`'s secret `name` to `value`, in whichever release it runs, or removes it if `value` is empty.
pub async fn set_secret(store: &dyn ObjectStore, app: &str, name: &str, value: &str) -> Result<()> {
    state::update(store, app, |c| {
        _ = match value {
            "" => c.secrets.remove(name),
            _ => c.secrets.insert(name.into(), value.into()),
        };
        Ok(())
    })
    .await
}

/// The names of `app`'s secrets.
pub async fn secrets(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    Ok(state::current(store, app).await?.0.secrets.into_keys().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app keeps the release it runs and its 10 latest publishes, and can serve any of them that is whole.
    /// Publishing a kept one again makes it the latest, in the folder it had, and the next change deletes what was
    /// dropped.
    #[tokio::test]
    async fn keeps_its_latest_releases() {
        let store = InMemory::new();
        let publish = async |n: usize| {
            let config = [("n".into(), n.to_string())].into();
            state::publish(&store, "app", "wasm".into(), config, vec![]).await.unwrap()
        };
        let mut all = vec![publish(0).await];
        release(&store, "app", &all[0].id).await.unwrap();
        for n in 1..=state::KEEP + 1 {
            all.push(publish(n).await); // which drops 1 at the last
        }
        let listed = releases(&store, "app").await.unwrap();
        assert_eq!(listed.len(), state::KEEP + 1);
        assert_eq!(listed.last(), Some(&format!("{} live", all[0].id)));
        let e = release(&store, "app", &all[1].id).await.unwrap_err();
        assert!(e.to_string().contains("keeps no release"), "{e}");

        assert_eq!(publish(5).await, all[5]);
        assert_eq!(releases(&store, "app").await.unwrap()[0], all[5].id);
        let there =
            async |e: &Entry, name: &str| store.head(&state::file("app", e.n, name).unwrap().into()).await.is_ok();
        assert!(!there(&all[1], RELEASE).await && there(&all[2], COMPONENT).await); // copied from another kept release
        store.delete(&state::file("app", all[2].n, COMPONENT).unwrap().into()).await.unwrap();
        assert!(release(&store, "app", &all[2].id).await.is_err());
        release(&store, "app", &all[3].id).await.unwrap();
    }
}
