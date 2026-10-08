//! The commands that change an install: `deploy`, `release`, `releases` and `env`. They write the bucket directly, so
//! its IAM is the only auth.
use crate::aws::Aws;
use crate::cron::{self, Cron};
use crate::engine::Engine;
use crate::outbound::Allow;
use crate::state::{self, Current, KEEP, Release, hash};
use bytes::Bytes;
use object_store::ObjectStore;
use object_store::client::HttpClient;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::{fs, path::Path};
use wac_graph::{CompositionGraph, EncodeOptions, types::Package};
use wasmtime::{Error, Result, bail, ensure, error::Context};

const MANIFEST: &str = "tric.toml";

/// An app's `MANIFEST`. Paths in it are relative to its directory.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a misspelt `allowed_outbound_hosts` would silently grant nothing
struct Manifest {
    name: String,
    component: String,
    #[serde(default)]
    allowed_outbound_hosts: Vec<String>, // as in Spin: `scheme://host[:port]`
    /// Components that each export what the next imports, `wasi:http/handler` most likely, outermost first.
    #[serde(default)]
    middleware: Vec<Middleware>,
    #[serde(default)]
    cron: BTreeMap<String, String>, // expression to path
}

/// A middleware component: a file, or a URL with the SHA-256 of what it serves, as Subresource Integrity has one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Middleware {
    path: Option<String>,
    url: Option<String>,
    digest: Option<String>,
}

/// An app, ready to upload.
pub struct Built {
    pub name: String,
    pub wasm: Bytes,
    pub release: Release,
}

/// Reads the app at `path`, a directory with a `MANIFEST` or a component, whose file name less `.wasm` is the app's
/// name; composes it with its middleware; and checks it as a host would.
pub async fn build(path: &Path, http: &HttpClient) -> Result<Built> {
    let (name, wasm, allowed_outbound_hosts, cron) = match path.extension().is_some_and(|e| e == "wasm") {
        true => {
            let name = path.file_stem().and_then(|s| s.to_str()).context("the component's file name is not UTF-8")?;
            (name.to_owned(), fs::read(path).with_context(|| format!("{}", path.display()))?, vec![], BTreeMap::new())
        }
        false => {
            let file = path.join(MANIFEST);
            let text = fs::read_to_string(&file).with_context(|| format!("{}", file.display()))?;
            let m: Manifest = toml::from_str(&text).with_context(|| format!("{}", file.display()))?;
            let mut wasm = fs::read(path.join(&m.component)).with_context(|| m.component.clone())?;
            for mw in m.middleware.iter().rev() {
                wasm = plug(wasm, middleware(path, mw, http).await?)?;
            }
            (m.name, wasm, m.allowed_outbound_hosts, m.cron)
        }
    };
    ensure!(
        state::is_app(&name),
        "{name:?} is no app name: 1 to 63 of a-z, 0-9 and -, starting and ending with a letter or digit"
    );
    for item in &allowed_outbound_hosts {
        Allow::parse(item).map_err(Error::msg)?;
    }
    for (expr, path) in &cron {
        Cron::parse(expr).map_err(Error::msg)?;
        ensure!(path.starts_with('/') && path.parse::<http::uri::PathAndQuery>().is_ok(), "bad cron path {path:?}");
    }
    let engine = Engine::new()?;
    engine.load(&name, &engine.compile(&wasm)?, vec![])?;
    let release = Release { component: hash(&wasm), allowed_outbound_hosts, cron };
    Ok(Built { name, wasm: wasm.into(), release })
}

/// The bytes of `mw`.
async fn middleware(dir: &Path, mw: &Middleware, http: &HttpClient) -> Result<Vec<u8>> {
    match mw {
        Middleware { path: Some(path), url: None, digest: None } => {
            fs::read(dir.join(path)).with_context(|| path.clone())
        }
        Middleware { path: None, url: Some(url), digest: Some(digest) } => {
            let want = digest.strip_prefix("sha256:").context("a digest is `sha256:` and the hash in hex")?;
            let res = http.execute(http::Request::get(url).body(Vec::new().into())?).await?;
            ensure!(res.status().is_success(), "{url} answered {}", res.status());
            let bytes = res.into_body().bytes().await?;
            ensure!(hash(&bytes) == want.to_ascii_lowercase(), "{url} does not match its digest");
            Ok(bytes.into())
        }
        _ => bail!("a middleware is `{{ path }}` or `{{ url, digest }}`"),
    }
}

/// `outer`, its imports that `inner` exports plugged with `inner`'s; what either imports besides, both import.
fn plug(inner: Vec<u8>, outer: Vec<u8>) -> Result<Vec<u8>> {
    let mut graph = CompositionGraph::new();
    let inner = Package::from_bytes("mw:inner", None, inner, graph.types_mut()).map_err(Error::from_anyhow)?;
    let inner = graph.register_package(inner)?;
    let outer = Package::from_bytes("mw:outer", None, outer, graph.types_mut()).map_err(Error::from_anyhow)?;
    let outer = graph.register_package(outer)?;
    wac_graph::plug(&mut graph, vec![inner], outer)?;
    Ok(graph.encode(EncodeOptions::default())?)
}

/// Uploads `built` and makes it the release its app runs, with `env` merged into the app's environment, an empty value
/// removing a variable. Returns the release's id.
pub async fn deploy(
    store: &dyn ObjectStore,
    aws: Option<&Aws>,
    built: Built,
    env: &[(String, String)],
) -> Result<String> {
    let Built { name, wasm, release } = built;
    check_env(env)?;
    let id = state::put_release(store, &name, wasm, &release).await?;
    state::update(store, &name, |c| {
        let mut c = c.unwrap_or_default();
        c.releases.retain(|r| *r != id);
        c.releases.insert(0, id.clone());
        c.releases.truncate(KEEP);
        c.release = id.clone();
        merge(&mut c.env, env);
        Ok(c)
    })
    .await?;
    sync(store, aws, &name, &release).await?;
    Ok(id)
}

/// Makes `app` run its release `id`, which must be one it keeps.
pub async fn release(store: &dyn ObjectStore, aws: Option<&Aws>, app: &str, id: &str) -> Result<()> {
    let kept = |c: &Current| c.releases.iter().any(|r| r == id);
    let c = state::current(store, app).await?.with_context(|| format!("{app} has not been deployed"))?;
    ensure!(kept(&c), "{app} keeps no release {id}: see `tric releases {app}`");
    let release = state::release(store, app, id).await?;
    state::update(store, app, |c| {
        let c = c.with_context(|| format!("{app} has not been deployed"))?;
        ensure!(kept(&c), "{app} dropped its release {id} while this ran");
        Ok(Current { release: id.into(), ..c })
    })
    .await?;
    sync(store, aws, app, &release).await
}

/// `app`'s releases, newest first, marking the one it runs.
pub async fn releases(store: &dyn ObjectStore, app: &str) -> Result<Vec<String>> {
    let c = state::current(store, app).await?.with_context(|| format!("{app} has not been deployed"))?;
    Ok(c.releases.iter().map(|r| if *r == c.release { format!("{r} running") } else { r.clone() }).collect())
}

/// Merges `vars` into `app`'s environment, an empty value removing one, and returns the names it has after.
pub async fn env(store: &dyn ObjectStore, app: &str, vars: &[(String, String)]) -> Result<Vec<String>> {
    check_env(vars)?;
    let c = match vars {
        [] => state::current(store, app).await?,
        _ => Some(
            state::update(store, app, |c| {
                let mut c = c.with_context(|| format!("{app} has not been deployed"))?;
                merge(&mut c.env, vars);
                Ok(c)
            })
            .await?,
        ),
    };
    Ok(c.with_context(|| format!("{app} has not been deployed"))?.env.into_keys().collect())
}

fn check_env(vars: &[(String, String)]) -> Result<()> {
    for (k, v) in vars {
        ensure!(!k.is_empty() && !k.contains(['=', '\0']), "bad variable name {k:?}");
        ensure!(!v.contains('\0'), "{k}'s value has a NUL");
    }
    Ok(())
}

fn merge(env: &mut BTreeMap<String, String>, vars: &[(String, String)]) {
    for (k, v) in vars {
        _ = match v.as_str() {
            "" => env.remove(k),
            _ => env.insert(k.clone(), v.clone()),
        };
    }
}

/// Makes `app`'s cron schedules `release`'s, where the install is on AWS.
async fn sync(store: &dyn ObjectStore, aws: Option<&Aws>, app: &str, release: &Release) -> Result<()> {
    match (aws, state::install(store).await?) {
        (Some(aws), Some(install)) => cron::sync(aws, &install, app, &release.cron).await,
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[tokio::test]
    async fn keeps_its_latest_releases() {
        let store = InMemory::new();
        let deploy = async |n: usize| {
            let wasm = Bytes::from(format!("wasm{n}"));
            let release = Release { component: hash(&wasm), ..Default::default() };
            super::deploy(&store, None, Built { name: "app".into(), wasm, release }, &[]).await.unwrap()
        };
        let mut all = vec![];
        for n in 0..=KEEP {
            all.push(deploy(n).await);
        }
        let listed = releases(&store, "app").await.unwrap();
        assert_eq!(listed.len(), KEEP);
        assert_eq!(listed[0], format!("{} running", all[KEEP]));
        let e = release(&store, None, "app", &all[0]).await.unwrap_err();
        assert!(e.to_string().contains("keeps no release"), "{e}");
        release(&store, None, "app", &all[1]).await.unwrap();
        assert_eq!(releases(&store, "app").await.unwrap()[KEEP - 1], format!("{} running", all[1]));
        assert_eq!(deploy(5).await, all[5]);
        assert_eq!(releases(&store, "app").await.unwrap()[0], format!("{} running", all[5]));
    }

    #[tokio::test]
    async fn env_merges() {
        let store = InMemory::new();
        let kv = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert!(env(&store, "app", &[kv("A", "1")]).await.is_err(), "not deployed");
        let wasm = Bytes::from("wasm");
        let built =
            Built { name: "app".into(), release: Release { component: hash(&wasm), ..Default::default() }, wasm };
        deploy(&store, None, built, &[kv("A", "1"), kv("B", "2")]).await.unwrap();
        assert_eq!(env(&store, "app", &[kv("B", ""), kv("C", "3")]).await.unwrap(), ["A", "C"]);
        assert_eq!(env(&store, "app", &[]).await.unwrap(), ["A", "C"]);
        assert!(env(&store, "app", &[kv("A=B", "1")]).await.is_err());
    }
}
