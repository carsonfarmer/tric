//! `tric deploy`: an app's component, compiled once to check it, then its release, which is what serve runs; on AWS,
//! then its cron, as EventBridge Scheduler's schedules.
use crate::aws::{Aws, query};
use crate::cron::Cron;
use crate::engine::Engine;
use crate::manifest;
use crate::store::{self, Store};
use crate::sweep;
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

/// Makes the app's schedules those of `cron` and its sweep, each an event: `{"app", "path"}` for a cron job and
/// `{"app", "sweep": true}` for the sweep. A schedule is named for its app and for all it is, so one that changes is
/// another: made before the old one is deleted.
async fn schedule(aws: &Aws, app: &str, cron: &BTreeMap<String, String>, s: &Scheduler) -> Result<()> {
    let prefix = &store::hash(app.as_bytes())[..24];
    let want = wanted(app, cron, s)?;
    let path = format!("/schedules?ScheduleGroup={}&NamePrefix={prefix}-&MaxResults=100", query(&s.group));
    let page: Value = serde_json::from_slice(&scheduler(aws, Method::GET, &path, vec![], StatusCode::OK).await?)?;
    ensure!(page["NextToken"].is_null(), "{app} has over 100 schedules, after failed deploys: delete them by hand");
    let have = page["Schedules"].as_array().context("Scheduler listed no `Schedules`")?;
    let have: BTreeSet<&str> = have.iter().filter_map(|s| s["Name"].as_str()).collect();
    let (make, delete) = changes(&have, &want);
    for (name, body) in make {
        scheduler(aws, Method::POST, &format!("/schedules/{name}"), body.to_vec(), StatusCode::CONFLICT).await?;
    }
    for name in delete {
        let path = format!("/schedules/{name}?groupName={}", query(&s.group));
        scheduler(aws, Method::DELETE, &path, vec![], StatusCode::NOT_FOUND).await?;
    }
    Ok(())
}

/// The schedules the app is to have, by name, with the body that makes each: one for each of its cron jobs, and its
/// sweep, which is there whatever its cron is. A name is the app's hash, which the listing of its schedules goes by,
/// and the body's, so no schedule of another app is one of this.
fn wanted(app: &str, cron: &BTreeMap<String, String>, s: &Scheduler) -> Result<BTreeMap<String, Vec<u8>>> {
    let prefix = &store::hash(app.as_bytes())[..24];
    let jobs = cron.iter().map(|(expr, path)| (expr.clone(), json!({ "app": app, "path": path })));
    let sweep = (sweep::schedule(app), json!({ "app": app, "sweep": true }));
    let mut want = BTreeMap::new();
    for (expr, input) in jobs.chain([sweep]) {
        let body = serde_json::to_vec(&json!({
            "GroupName": s.group,
            "ScheduleExpression": Cron::parse(&expr)?.eventbridge(),
            "FlexibleTimeWindow": { "Mode": "OFF" },
            "Target": { "Arn": s.target, "RoleArn": s.role, "Input": input.to_string() },
        }))?;
        want.insert(format!("{prefix}-{}", &store::hash(&body)[..39]), body);
    }
    Ok(want)
}

/// What to make and what to delete, for the schedules that `have` to be `want`: the missing, with their bodies, and the
/// extras. Nothing wanted, as when an app is removed, deletes them all.
fn changes<'a>(
    have: &BTreeSet<&'a str>,
    want: &'a BTreeMap<String, Vec<u8>>,
) -> (Vec<(&'a str, &'a [u8])>, Vec<&'a str>) {
    let make = want.iter().filter(|(name, _)| !have.contains(name.as_str()));
    let delete = have.iter().copied().filter(|name| !want.contains_key(*name));
    (make.map(|(name, body)| (name.as_str(), body.as_slice())).collect(), delete.collect())
}

/// Calls EventBridge Scheduler, for which `done`, besides a success, means it's done already.
pub(crate) async fn scheduler(aws: &Aws, method: Method, path: &str, body: Vec<u8>, done: StatusCode) -> Result<Bytes> {
    let res = aws.send("scheduler", method, path, &[(CONTENT_TYPE.as_str(), "application/json")], body).await?;
    let (status, body) = (res.status(), res.into_body().bytes().await?);
    ensure!(status.is_success() || status == done, "Scheduler answered {status}: {}", String::from_utf8_lossy(&body));
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler() -> Scheduler {
        Scheduler {
            group: "apps".into(),
            target: "arn:aws:lambda:r:1:function:events:cron".into(),
            role: "role".into(),
        }
    }

    fn cron(jobs: &[(&str, &str)]) -> BTreeMap<String, String> {
        jobs.iter().map(|(expr, path)| (expr.to_string(), path.to_string())).collect()
    }

    fn inputs(want: &BTreeMap<String, Vec<u8>>) -> Vec<Value> {
        let input = |body: &Vec<u8>| {
            let body: Value = serde_json::from_slice(body).unwrap();
            serde_json::from_str(body["Target"]["Input"].as_str().unwrap()).unwrap()
        };
        let mut inputs: Vec<Value> = want.values().map(input).collect();
        inputs.sort_by_key(Value::to_string);
        inputs
    }

    #[test]
    fn an_app_with_no_cron_has_its_sweep() {
        let want = wanted("shop", &cron(&[]), &scheduler()).unwrap();
        assert_eq!(inputs(&want), [json!({ "app": "shop", "sweep": true })]);
        let body: Value = serde_json::from_slice(want.values().next().unwrap()).unwrap();
        // Once a day, at a minute that is the app's, as the expression the router runs locally says.
        let at = Cron::parse(&sweep::schedule("shop")).unwrap().eventbridge();
        assert_eq!(body["ScheduleExpression"], at);
        assert_eq!(body["Target"]["Arn"], scheduler().target);
        assert_eq!(body["Target"]["RoleArn"], scheduler().role);
        assert_eq!(body["GroupName"], "apps");
        assert_eq!(body["FlexibleTimeWindow"], json!({ "Mode": "OFF" }));
    }

    #[test]
    fn the_sweep_is_made_with_the_cron_jobs() {
        let jobs = cron(&[("0 8 * * *", "/@digest/run"), ("*/5 * * * *", "/tick")]);
        let want = wanted("shop", &jobs, &scheduler()).unwrap();
        assert_eq!(
            inputs(&want),
            [
                json!({ "app": "shop", "path": "/@digest/run" }),
                json!({ "app": "shop", "path": "/tick" }),
                json!({ "app": "shop", "sweep": true }),
            ]
        );
        // Named by the app's hash, which the listing by prefix finds, and within Scheduler's 64 characters.
        let prefix = format!("{}-", &store::hash(b"shop")[..24]);
        assert!(want.keys().all(|name| name.starts_with(&prefix) && name.len() <= 64));
    }

    #[test]
    fn the_sweep_is_the_same_schedule_each_deploy() {
        let jobs = cron(&[("0 8 * * *", "/run")]);
        let (first, again) = (wanted("shop", &jobs, &scheduler()), wanted("shop", &jobs, &scheduler()));
        assert_eq!(first.unwrap(), again.unwrap());
        let have: BTreeSet<&str> = BTreeSet::new();
        let want = wanted("shop", &jobs, &scheduler()).unwrap();
        let (make, delete) = changes(&have, &want);
        assert_eq!((make.len(), delete.len()), (2, 0));
        let have: BTreeSet<&str> = want.keys().map(String::as_str).collect();
        let (make, delete) = changes(&have, &want);
        assert_eq!((make.len(), delete.len()), (0, 0), "a deploy that changes nothing makes and deletes nothing");
    }

    #[test]
    fn a_cron_job_removed_is_deleted_and_the_sweep_stays() {
        let before = wanted("shop", &cron(&[("0 8 * * *", "/run"), ("0 9 * * *", "/more")]), &scheduler()).unwrap();
        let after = wanted("shop", &cron(&[("0 9 * * *", "/more")]), &scheduler()).unwrap();
        let have: BTreeSet<&str> = before.keys().map(String::as_str).collect();
        let (make, delete) = changes(&have, &after);
        assert!(make.is_empty());
        assert_eq!(delete.len(), 1);
        let gone = &before[delete[0]];
        let gone: Value = serde_json::from_slice(gone).unwrap();
        assert_eq!(gone["Target"]["Input"], json!({ "app": "shop", "path": "/run" }).to_string());
    }

    #[test]
    fn a_removed_app_has_no_schedule_left_not_even_its_sweep() {
        let before = wanted("shop", &cron(&[("0 8 * * *", "/run")]), &scheduler()).unwrap();
        let have: BTreeSet<&str> = before.keys().map(String::as_str).collect();
        let nothing = BTreeMap::new();
        let (make, mut delete) = changes(&have, &nothing);
        assert!(make.is_empty());
        delete.sort();
        assert_eq!(delete, have.iter().copied().collect::<Vec<_>>());
        assert_eq!(delete.len(), 2, "its cron job and its sweep");
    }

    #[test]
    fn apps_share_no_schedule() {
        let (shop, blog) = (wanted("shop", &cron(&[]), &scheduler()), wanted("blog", &cron(&[]), &scheduler()));
        let (shop, blog) = (shop.unwrap(), blog.unwrap());
        assert!(shop.keys().all(|name| !blog.contains_key(name)));
        let prefix = format!("{}-", &store::hash(b"blog")[..24]);
        assert!(shop.keys().all(|name| !name.starts_with(&prefix)));
    }
}
