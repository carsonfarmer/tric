//! `tric deploy`: an app's component, compiled once to check it, then its release, which is what serve runs.
use crate::engine::Engine;
use crate::manifest;
use crate::store::{self, Store};
use crate::tric::label;
use object_store::PutMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use wasmtime::{Result, ensure};

/// The most a release takes: it holds the app's environment.
pub const RELEASE_MAX: u64 = 1 << 20;

/// `apps/<app>/current`: the component, by its SHA-256, and what it runs with.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String,
    #[serde(default)]
    pub allowed_outbound_hosts: Vec<String>,
    #[serde(default)]
    pub cron: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Deploys the app at `path` to `bucket`, with `env` as its environment and `allow` added to its allowed hosts.
pub async fn run(path: &Path, allow: &[String], env: Vec<(String, String)>, bucket: &str) -> Result<()> {
    let app = manifest::read(path, allow).await?;
    ensure!(label(&app.name), "the app's name, {:?}, is not 1 to 63 of a-z, 0-9 and - (a DNS label)", app.name);
    Engine::new()?.compile(&app.wasm)?;
    let sha = store::hash(&app.wasm);
    let store = Store::s3(store::s3(bucket, None)?);
    store.put(&store::app(&app.name, &["components", &sha]), app.wasm.into(), PutMode::Overwrite).await?;
    let release = Release {
        component: sha.clone(),
        allowed_outbound_hosts: app.allowed_outbound_hosts,
        cron: app.cron,
        env: env.into_iter().collect(),
    };
    let release = serde_json::to_vec(&release)?.into();
    store.put(&store::app(&app.name, &["current"]), release, PutMode::Overwrite).await?;
    println!("{} {sha}", app.name);
    Ok(())
}
