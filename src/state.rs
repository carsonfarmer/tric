//! The bucket: what each app runs, its releases and components, and the native code hosts make. Names' heads and large
//! values are in `name`.
//!
//! - `apps/<app>/current`: the release the app runs, its last `KEEP` releases, and its environment;
//! - `apps/<app>/releases/<id>`: a release, whose id is its hash;
//! - `apps/<app>/components/<sha256>`: a component, as composed;
//! - `native/<compat>/<sha256>`: native code a host made of a component, for hosts of the same build;
//! - `install`: on AWS, what the install is;
//! - `failed/<commit>`: outbox events that ran out of tries.
//!
//! Every read is capped and parsed strictly, and a release and a component are checked against their hashes; writes
//! have the same caps, so nothing is written that a read would refuse. Native code can't be checked, and loading it runs
//! it, so it is trusted as the bucket is: whoever may write the bucket may run code in it.
use bytes::Bytes;
use object_store::{Error as E, ObjectStore, ObjectStoreExt, PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use wasmtime::{Result, bail, ensure, error::Context};

const JSON_MAX: u64 = 64 << 10; // `current`, a release, or `install`
const COMPONENT_MAX: u64 = 128 << 20;
pub const NATIVE_MAX: u64 = 1 << 30;
pub const KEEP: usize = 10; // the releases an app keeps: how far back it can go
const TRIES: usize = 3; // of a change, while others keep landing first

/// Whether `s` is an app name: a DNS label of `a-z0-9-` (RFC 1123), as it is a host's first label.
pub fn is_app(s: &str) -> bool {
    let alnum = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    s.len() <= 63 && alnum(s.as_bytes().first()) && alnum(s.as_bytes().last()) && s.bytes().all(ok)
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 128 random bits, as hex.
pub fn random() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn path(app: &str, parts: &[&str]) -> Path {
    Path::from_iter(["apps", app].into_iter().chain(parts.iter().copied()))
}

pub fn native(compat: &str, component: &str) -> Path {
    Path::from_iter(["native", compat, component])
}

/// What an app runs, and keeps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub release: String,
    pub releases: Vec<String>, // newest first, the one it runs among them
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// One immutable release of an app.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String, // its hash
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_outbound_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cron: BTreeMap<String, String>, // expression to path
}

/// On AWS, what the install is, so `deploy` can keep each app's cron in step: the function schedules invoke, the role
/// they invoke it with, and the Scheduler group they are in.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub function: String,
    pub role: String,
    pub group: String,
}

/// The object at `path` and its version, or `None` if there is none.
pub async fn read(store: &dyn ObjectStore, path: &Path, max: u64) -> Result<Option<(Bytes, UpdateVersion)>> {
    let mut r = match store.get(path).await {
        Err(E::NotFound { .. }) => return Ok(None),
        r => r?,
    };
    ensure!(r.meta.size <= max, "{path} is over {max} bytes");
    let version = UpdateVersion { e_tag: r.meta.e_tag.take(), version: r.meta.version.take() };
    Ok(Some((r.bytes().await?, version)))
}

/// Whether there is an object at `path`.
async fn exists(store: &dyn ObjectStore, path: &Path) -> Result<bool> {
    match store.head(path).await {
        Err(E::NotFound { .. }) => Ok(false),
        r => Ok(r.map(|_| true)?),
    }
}

/// The object at `path`, refused if its hash is not `hash`.
async fn fetch(store: &dyn ObjectStore, path: &Path, max: u64, hash: &str) -> Result<Bytes> {
    let (bytes, _) = read(store, path, max).await?.with_context(|| format!("there is no {path}"))?;
    ensure!(self::hash(&bytes) == hash, "{path} does not match its hash");
    Ok(bytes)
}

/// The JSON object at `path` and its version, or `None` if there is none.
pub async fn json<T: DeserializeOwned>(
    store: &dyn ObjectStore,
    path: &Path,
    max: u64,
) -> Result<Option<(T, UpdateVersion)>> {
    let Some((bytes, version)) = read(store, path, max).await? else { return Ok(None) };
    Ok(Some((serde_json::from_slice(&bytes).with_context(|| format!("{path}"))?, version)))
}

/// Writes `value` as JSON over the object at `path` with `base` as its version, or where there is none if `base` is
/// `None`, if that is still the object: its new version, or `None` if another write landed first.
pub async fn put(
    store: &dyn ObjectStore,
    path: &Path,
    value: &impl Serialize,
    max: u64,
    base: Option<UpdateVersion>,
) -> Result<Option<UpdateVersion>> {
    let json = serde_json::to_vec(value)?;
    ensure!(json.len() as u64 <= max, "{path} would be over {max} bytes");
    let mode = base.map_or(PutMode::Create, PutMode::Update);
    match store.put_opts(path, json.into(), mode.into()).await {
        Ok(r) => Ok(Some(UpdateVersion { e_tag: r.e_tag, version: r.version })),
        Err(E::Precondition { .. } | E::AlreadyExists { .. } | E::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The apps that have been deployed, or at least uploaded to.
pub async fn apps(store: &dyn ObjectStore) -> Result<Vec<String>> {
    let listed = store.list_with_delimiter(Some(&"apps".into())).await?;
    Ok(listed.common_prefixes.iter().filter_map(|p| p.filename().map(str::to_owned)).collect())
}

/// What `app` runs, if it has been deployed.
pub async fn current(store: &dyn ObjectStore, app: &str) -> Result<Option<Current>> {
    Ok(json(store, &path(app, &["current"]), JSON_MAX).await?.map(|(c, _)| c))
}

/// `app`'s release `id`.
pub async fn release(store: &dyn ObjectStore, app: &str, id: &str) -> Result<Release> {
    let bytes = fetch(store, &path(app, &["releases", id]), JSON_MAX, id).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// `app`'s component with the hash `hash`.
pub async fn component(store: &dyn ObjectStore, app: &str, hash: &str) -> Result<Bytes> {
    fetch(store, &path(app, &["components", hash]), COMPONENT_MAX, hash).await
}

/// The install's details, if it is on AWS.
pub async fn install(store: &dyn ObjectStore) -> Result<Option<Install>> {
    Ok(json(store, &"install".into(), JSON_MAX).await?.map(|(i, _)| i))
}

/// Uploads `wasm` as `release`'s component, and `release` as a release of `app`, each unless it is there, and returns
/// the release's id. Neither is ever deleted, so one written is there for good.
pub async fn put_release(store: &dyn ObjectStore, app: &str, wasm: Bytes, release: &Release) -> Result<String> {
    ensure!(is_app(app), "an app name is 1 to 63 of a-z, 0-9 and -, starting and ending with a letter or digit");
    ensure!(release.component == hash(&wasm), "a release names its component's hash");
    let json = serde_json::to_vec(release)?;
    ensure!(json.len() as u64 <= JSON_MAX, "{app}'s release would be over {JSON_MAX} bytes");
    ensure!(wasm.len() as u64 <= COMPONENT_MAX, "{app}'s component would be over {COMPONENT_MAX} bytes");
    let id = hash(&json);
    for (at, bytes) in
        [(path(app, &["components", &release.component]), wasm), (path(app, &["releases", &id]), json.into())]
    {
        if !exists(store, &at).await? {
            store.put(&at, bytes.into()).await?;
        }
    }
    Ok(id)
}

/// Applies `change` to `app`'s `current`, or to `None` if it has none, with a conditional write, which fails if another
/// change landed in between, so neither is lost; then reads it and tries again, `TRIES` times in all.
pub async fn update(
    store: &dyn ObjectStore,
    app: &str,
    mut change: impl FnMut(Option<Current>) -> Result<Current>,
) -> Result<Current> {
    ensure!(is_app(app), "{app:?} is not an app name");
    let at = path(app, &["current"]);
    for _ in 0..TRIES {
        let (old, version) = json(store, &at, JSON_MAX).await?.unzip();
        let new = change(old)?;
        if put(store, &at, &new, JSON_MAX, version).await?.is_some() {
            return Ok(new);
        }
    }
    bail!("{app} kept changing while this ran: run it again")
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[test]
    fn app_names() {
        for ok in ["a", "a-b", "0", "abc123", &"a".repeat(63)] {
            assert!(is_app(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a.b", "a_b", "a/b", &"a".repeat(64)] {
            assert!(!is_app(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn releases_are_checked() {
        let store = InMemory::new();
        let wasm = Bytes::from("wasm");
        let r = Release { component: hash(&wasm), ..Default::default() };
        let id = put_release(&store, "app", wasm.clone(), &r).await.unwrap();
        assert_eq!(put_release(&store, "app", wasm.clone(), &r).await.unwrap(), id); // the same release
        assert_eq!(release(&store, "app", &id).await.unwrap(), r);
        assert_eq!(component(&store, "app", &r.component).await.unwrap(), wasm);
        store.put(&path("app", &["components", &r.component]), "other".into()).await.unwrap();
        let e = component(&store, "app", &r.component).await.unwrap_err();
        assert!(e.to_string().contains("does not match its hash"), "{e}");
        assert!(put_release(&store, "App", wasm.clone(), &r).await.is_err());
        assert!(put_release(&store, "app", "x".into(), &r).await.is_err());
    }

    #[tokio::test]
    async fn changes_all_land() {
        use object_store::throttle::{ThrottleConfig, ThrottledStore};
        let ms = std::time::Duration::from_millis(10);
        let config = ThrottleConfig { wait_get_per_call: ms, wait_put_per_call: ms, ..Default::default() };
        let store = ThrottledStore::new(InMemory::new(), config);
        update(&store, "app", |_| Ok(Current::default())).await.unwrap();
        let set = |k: &'static str| {
            update(&store, "app", move |c| {
                let mut c = c.unwrap();
                c.env.insert(k.into(), "x".into());
                Ok(c)
            })
        };
        let (a, b, c) = tokio::join!(set("a"), set("b"), set("c"));
        a.and(b).and(c).unwrap();
        assert_eq!(current(&store, "app").await.unwrap().unwrap().env.len(), 3);
        let big = "x".repeat(JSON_MAX as usize);
        let e = update(&store, "app", |_| Ok(Current { release: big.clone(), ..Default::default() })).await;
        assert!(e.unwrap_err().to_string().contains("would be over"));
    }
}
