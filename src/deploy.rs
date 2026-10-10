//! `tric deploy`: an app's component, compiled once to check it, then its release, which is what serve runs; on AWS,
//! then its cron, as EventBridge Scheduler's schedules.
use crate::aws::{Aws, query};
use crate::cron::Cron;
use crate::engine::Engine;
use crate::manifest;
use crate::store::{self, Store};
use crate::tric::label;
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Method, StatusCode};
use object_store::PutMode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use wasmtime::{Result, ensure, error::Context};

/// The most a release takes: it holds the app's environment.
pub const RELEASE_MAX: u64 = 1 << 20;

/// `apps/<app>/current`: the component, by its SHA-256, and what it runs with.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub component: String,
    pub allowed_outbound_hosts: Vec<String>,
    pub cron: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
}

/// On AWS, where schedules are: a schedule group, the router's events function or alias, by its ARN, which they invoke,
/// and the role they do so as. Apps' cron has one, and the router's retries another.
pub struct Scheduler {
    pub group: String,
    pub target: String,
    pub role: String,
}

/// Deploys the app at `path` to `bucket`, with `env` as its environment and `allow` added to its allowed hosts.
pub async fn run(
    path: &Path,
    allow: &[String],
    env: Vec<(String, String)>,
    bucket: &str,
    scheduler: Option<Scheduler>,
) -> Result<()> {
    let app = manifest::read(path, allow).await?;
    ensure!(label(&app.name), "the app's name, {:?}, is not 2 to 63 of a-z, 0-9 and - (a DNS label)", app.name);
    Engine::new()?.compile(&app.wasm)?;
    let sha = store::hash(&app.wasm);
    let s3 = store::s3(bucket, None)?;
    let aws = Aws::new(&s3)?;
    let store = Store::s3(s3);
    store.put(&store::app(&app.name, &["components", &sha]), app.wasm.into(), PutMode::Overwrite).await?;
    let release = Release {
        component: sha.clone(),
        allowed_outbound_hosts: app.allowed_outbound_hosts,
        cron: app.cron,
        env: env.into_iter().collect(),
    };
    store.put(&store::app(&app.name, &["current"]), serde_json::to_vec(&release)?.into(), PutMode::Overwrite).await?;
    if let Some(scheduler) = scheduler {
        schedule(&aws, &app.name, &release.cron, &scheduler).await?;
    }
    println!("{} {sha}", app.name);
    Ok(())
}

/// Makes the app's schedules those of `cron`, each a `{"app", "path"}` event. A schedule is named for its app and for
/// all it is, so one that changes is another: made before the old one is deleted.
async fn schedule(aws: &Aws, app: &str, cron: &BTreeMap<String, String>, s: &Scheduler) -> Result<()> {
    let prefix = &store::hash(app.as_bytes())[..24];
    let mut want = BTreeMap::new();
    for (expr, path) in cron {
        let body = serde_json::to_vec(&json!({
            "GroupName": s.group,
            "ScheduleExpression": Cron::parse(expr)?.eventbridge(),
            "FlexibleTimeWindow": { "Mode": "OFF" },
            "Target": { "Arn": s.target, "RoleArn": s.role, "Input": json!({ "app": app, "path": path }).to_string() },
        }))?;
        want.insert(format!("{prefix}-{}", &store::hash(&body)[..39]), body);
    }
    let path = format!("/schedules?ScheduleGroup={}&NamePrefix={prefix}-&MaxResults=100", query(&s.group));
    let page: Value = serde_json::from_slice(&scheduler(aws, Method::GET, &path, vec![], StatusCode::OK).await?)?;
    ensure!(page["NextToken"].is_null(), "{app} has over 100 schedules, after failed deploys: delete them by hand");
    let have = page["Schedules"].as_array().context("Scheduler listed no `Schedules`")?;
    let have: BTreeSet<&str> = have.iter().filter_map(|s| s["Name"].as_str()).collect();
    for (name, body) in want.iter().filter(|(name, _)| !have.contains(name.as_str())) {
        scheduler(aws, Method::POST, &format!("/schedules/{name}"), body.clone(), StatusCode::CONFLICT).await?;
    }
    for name in have.iter().filter(|name| !want.contains_key(**name)) {
        let path = format!("/schedules/{name}?groupName={}", query(&s.group));
        scheduler(aws, Method::DELETE, &path, vec![], StatusCode::NOT_FOUND).await?;
    }
    Ok(())
}

/// Calls EventBridge Scheduler, for which `done`, besides a success, means it's done already.
pub(crate) async fn scheduler(aws: &Aws, method: Method, path: &str, body: Vec<u8>, done: StatusCode) -> Result<Bytes> {
    let res = aws.send("scheduler", method, path, &[(CONTENT_TYPE.as_str(), "application/json")], body).await?;
    let (status, body) = (res.status(), res.into_body().bytes().await?);
    ensure!(status.is_success() || status == done, "Scheduler answered {status}: {}", String::from_utf8_lossy(&body));
    Ok(body)
}
