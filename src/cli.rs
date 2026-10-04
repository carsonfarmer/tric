//! The commands that change an install. They write the bucket directly, so its IAM is the only auth: a team's role
//! writes only the apps named `<team>-…`.
use crate::state::{self, BLOBS, Current, Live, RELEASES, Release};
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{Error as E, ObjectMeta, ObjectStore, ObjectStoreExt, PutPayload, memory::InMemory};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};
use std::{cmp::Reverse, fs, path::Path, sync::Arc};
use tokio::time::{sleep, timeout};
use tric::{Engine, is_name};
use wasmtime::{Result, ensure, error::Context};

const MANIFEST: &str = "tric.toml";
const COMPILE_POLL: Duration = Duration::from_secs(1);
const COMPILE_WAIT: Duration = Duration::from_secs(180); // longer than the compile function may run
const KEEP: usize = 10; // the releases `gc` keeps of each app besides the one it runs: how far back a rollback can go
pub const GRACE: Duration = Duration::from_secs(60 * 60); // how old what `gc` deletes must be: longer than any publish takes

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
    let component = state::release(store, app, id).await?.component; // it is there, and sound
    store.head(&state::object(app, BLOBS, &component)?.into()).await?; // as is its component
    state::update(store, app, |c| c.release = Some(Live { id: id.into(), component })).await
}

/// `app`'s releases, newest first, each as its id and when it was last published.
pub async fn releases(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let all = newest(store, &state::path(app, RELEASES.dir)?).await?;
    Ok(all.iter().map(|m| format!("{} {}", m.location.filename().unwrap_or_default(), m.last_modified)).collect())
}

/// What is under `prefix` in `bucket`, newest first.
async fn newest(bucket: &dyn ObjectStore, prefix: &str) -> Result<Vec<ObjectMeta>> {
    let mut all: Vec<_> = bucket.list(Some(&prefix.into())).try_collect().await?;
    all.sort_by_key(|m| Reverse(m.last_modified));
    Ok(all)
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

/// Deletes what no app needs, and prints each key it deletes. Of each app, that is its releases but the one it runs and
/// its `KEEP` newest, the components no kept release uses, with their markers and native code, and a kept component's
/// native code but the newest, which is for the newest engine. Nothing younger than `grace` goes, as a publish may still
/// be writing it, and KV data never does. An app it cannot read all that it needs of, or that changes meanwhile, it
/// leaves as it is.
pub async fn gc(store: &dyn ObjectStore, native: Option<&dyn ObjectStore>, grace: Duration) -> Result<()> {
    let mut apps = BTreeSet::new(); // what has anything in either bucket, so a deleted app's native code goes too
    let roots = [(store, Some("apps")), (store, Some("compile"))].into_iter().chain(native.map(|n| (n, None)));
    for (bucket, prefix) in roots {
        let listed = bucket.list_with_delimiter(prefix.map(Into::into).as_ref()).await?;
        let names = listed.common_prefixes.iter().filter_map(|p| p.filename());
        apps.extend(names.filter(|a| is_name(a)).map(String::from));
    }
    let mut failed = 0;
    for app in &apps {
        if let Err(e) = sweep(store, native, app, grace).await {
            tracing::warn!(app, "left as it is: {e:#}");
            failed += 1;
        }
    }
    ensure!(failed == 0, "{failed} of {} apps were left as they are", apps.len());
    Ok(())
}

/// Deletes what `app` no longer needs, as `gc` says, once it has read all that it does, unless its `current` changed
/// meanwhile. A rollback to a release it is deleting, between that check and the deletes, breaks the app until that
/// release is published again.
async fn sweep(store: &dyn ObjectStore, native: Option<&dyn ObjectStore>, app: &str, grace: Duration) -> Result<()> {
    let old = |m: &ObjectMeta| SystemTime::from(m.last_modified).elapsed().is_ok_and(|age| age >= grace);
    let (Current { release: live, .. }, version) = state::current(store, app).await?;
    let releases = newest(store, &state::path(app, RELEASES.dir)?).await?;
    let mut used: BTreeSet<_> = live.iter().map(|l| l.component.clone()).collect();
    let mut gone = vec![];
    for (i, m) in releases.into_iter().enumerate() {
        let id = m.location.filename().unwrap_or_default();
        if i < KEEP || !old(&m) || live.as_ref().is_some_and(|l| l.id == id) {
            used.insert(state::release(store, app, id).await?.component);
        } else {
            gone.push(m.location);
        }
    }
    for dir in [state::path(app, BLOBS.dir)?, state::markers(app)?] {
        let all = newest(store, &dir).await?;
        let unused = |m: &ObjectMeta| old(m) && !used.contains(m.location.filename().unwrap_or_default());
        gone.extend(all.into_iter().filter(unused).map(|m| m.location));
    }
    ensure!(state::current(store, app).await?.1 == version, "{app} changed while gc ran: run it again");
    delete(store, gone).await?;
    let Some(native) = native else { return Ok(()) };
    let (mut seen, mut code) = (BTreeSet::new(), vec![]); // the kept components whose newest code is seen
    for m in newest(native, app).await? {
        let Some(hash) = state::compiled(m.location.as_ref()) else { continue }; // what the compile function made
        if !(used.contains(hash) && seen.insert(hash.to_owned())) && old(&m) {
            code.push(m.location);
        }
    }
    delete(native, code).await
}

/// Deletes `keys` from `bucket`, in a request per 1,000 on S3, and prints each.
async fn delete(bucket: &dyn ObjectStore, keys: Vec<object_store::path::Path>) -> Result<()> {
    let mut deleted = bucket.delete_stream(stream::iter(keys).map(Ok).boxed());
    while let Some(key) = deleted.try_next().await? {
        println!("{key}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release of `app` with the component `component`, told apart from others by `n`.
    async fn add(store: &InMemory, app: &str, component: &str, n: usize) -> String {
        let config = [("n".into(), n.to_string())].into();
        let r = Release { component: component.into(), config, allowed_outbound_hosts: vec![] };
        state::add(store, app, RELEASES, serde_json::to_vec(&r).unwrap().into()).await.unwrap()
    }

    async fn there(bucket: &InMemory, key: &str) -> bool {
        bucket.head(&key.into()).await.is_ok()
    }

    /// `gc` keeps an app's current release and its 10 newest, their components, and each one's newest native code, and
    /// deletes the rest, with the markers and native code of an app that is gone. Nothing young goes, nor anything of an
    /// app with a release it keeps that it cannot read.
    #[tokio::test]
    async fn gc_keeps_what_is_used() {
        let (store, native) = (InMemory::new(), InMemory::new());
        let mut hashes = vec![];
        for blob in ["a", "b", "c"] {
            hashes.push(state::add(&store, "app", BLOBS, blob.into()).await.unwrap());
        }
        let [a, b, c] = &hashes[..] else { unreachable!() };
        let (x, y) = (add(&store, "app", b, 0).await, add(&store, "app", c, 0).await);
        sleep(Duration::from_millis(2)).await; // so they are older than the rest
        for n in 1..=KEEP {
            add(&store, "app", a, n).await;
        }
        release(&store, "app", &x).await.unwrap();
        let code = [("app", c, "1"), ("app", a, "1"), ("app", a, "2"), ("gone", a, "1")];
        let code = code.map(|(app, hash, compat)| state::native(app, hash, compat).unwrap());
        let markers = [state::marker("app", c).unwrap(), state::marker("gone", a).unwrap()];
        for (bucket, key) in code.iter().map(|k| (&native, k)).chain(markers.iter().map(|k| (&store, k))) {
            bucket.put(&key.as_str().into(), PutPayload::new()).await.unwrap();
            sleep(Duration::from_millis(2)).await; // so the native code for engine 2 is the newest
        }

        let count = async || store.list(None).count().await + native.list(None).count().await;
        let all = count().await;
        gc(&store, Some(&native), GRACE).await.unwrap();
        assert_eq!(count().await, all);

        let bad = format!("apps/bad/releases/{}", "0".repeat(64)); // a release that does not match its hash
        let kv = format!("kv/app/{c}"); // KV data, were it in this bucket
        for key in [&bad, &kv] {
            store.put(&key.as_str().into(), "x".into()).await.unwrap();
        }
        gc(&store, None, Duration::ZERO).await.unwrap_err();
        assert_eq!(native.list(None).count().await, code.len());
        let e = gc(&store, Some(&native), Duration::ZERO).await.unwrap_err();
        assert!(e.to_string().contains("1 of 3 apps"), "{e}");
        assert!(there(&store, &bad).await && there(&store, &kv).await);
        let kept = releases(&store, "app").await.unwrap();
        assert!(
            kept.len() == KEEP + 1 && kept.iter().any(|r| r.starts_with(&x)) && !kept.iter().any(|r| r.starts_with(&y))
        );
        for (hash, kept) in [(a, true), (b, true), (c, false)] {
            assert_eq!(there(&store, &state::object("app", BLOBS, hash).unwrap()).await, kept);
        }
        for (key, kept) in code.iter().zip([false, false, true, false]).chain(markers.iter().zip([false, false])) {
            assert_eq!(there(if key.starts_with("compile/") { &store } else { &native }, key).await, kept, "{key}");
        }
    }
}
