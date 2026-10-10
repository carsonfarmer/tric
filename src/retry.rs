//! Retries on Lambda. A delivery event that serve handed to the router, and that cannot be delivered, waits in the
//! bucket as `outbox/<app>/<commit>`, which only the router writes; and a one-time EventBridge Scheduler schedule, in a
//! group of its own, has the router's `retry` alias try it again, as `outbox::backoff` says, until it is delivered or
//! `outbox::WINDOW` is over. The schedule carries a reference to the event, not the event: Scheduler takes 256 KB, and
//! an event may be 1 MB. Each try goes to the app's serve, which delivers the event only if its commit is pending in
//! the head with the event's digest, so what waits in the bucket can send no more than the commit it is of.
use crate::deploy::{self, Scheduler};
use crate::outbox::{self, EVENT_MAX, WINDOW};
use crate::route::{Route, body};
use crate::store;
use crate::tric::{Response, label, status};
use bytes::Bytes;
use chrono::{DateTime, Datelike, Timelike};
use http::{Method, StatusCode};
use hyper::body::Incoming;
use object_store::{PutMode, path::Path};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use wasmtime::{Result, error::Context};

/// A delivery that waits for its next try, as its schedule carries it: the event of `app`'s commit `commit`, which has
/// failed `tries` times, and is lost if a next try would come after `deadline`, in Unix seconds.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Waiting {
    app: String,
    commit: String,
    tries: u32,
    deadline: u64,
}

impl Waiting {
    /// The first failure of `app`'s event of `commit`, now.
    pub(crate) fn new(app: String, commit: String) -> Self {
        Self { app, commit, tries: 1, deadline: unix() + WINDOW.as_secs() }
    }

    /// Whether `app` is a label and `commit` 128 bits in hex, as the router makes them: the only keys it writes.
    pub(crate) fn valid(&self) -> bool {
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        label(&self.app) && self.commit.len() == 32 && self.commit.bytes().all(hex)
    }

    fn key(&self) -> Path {
        Path::from_iter(["outbox", &self.app, &self.commit])
    }
}

fn unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The schedule that has `w` tried again at `at`, in Unix seconds, with its name. The name is for the app, the commit
/// and the try, so one app's cannot be another's, and making it twice makes it once.
fn schedule(retries: &Scheduler, w: &Waiting, at: u64) -> Result<(String, Vec<u8>)> {
    let t = DateTime::from_timestamp(at as i64, 0).context("a time out of range")?;
    let at =
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second());
    let name = format!("{}-{}", &store::hash(format!("{}/{}", w.app, w.commit).as_bytes())[..32], w.tries);
    let body = json!({
        "GroupName": retries.group,
        "ScheduleExpression": format!("at({at})"),
        "FlexibleTimeWindow": { "Mode": "OFF" },
        "ActionAfterCompletion": "DELETE",
        "Target": { "Arn": retries.target, "RoleArn": retries.role, "Input": serde_json::to_string(w)? },
    });
    Ok((name, serde_json::to_vec(&body)?))
}

impl Route {
    /// Delivers the event `bytes`, which serve handed over on Lambda, for the first time. If that fails, keeps the
    /// event and schedules a try again. Gives the messages the event published.
    pub(crate) async fn first(&self, retries: &Scheduler, w: Waiting, bytes: Bytes) -> Result<Vec<(String, String)>> {
        match self.deliver(&w.app, bytes.clone()).await {
            Ok(published) => Ok(published),
            Err(after) => {
                self.store.put(&w.key(), bytes, PutMode::Overwrite).await?;
                self.wait(retries, w, after).await.map(|()| vec![])
            }
        }
    }

    /// Takes the retry that Scheduler sends, answering as `answer` does.
    pub(crate) async fn retry(&self, req: hyper::Request<Incoming>) -> Response {
        let Some(retries) = &self.retries else { return status(StatusCode::FORBIDDEN) };
        let w = match body::<Waiting>(req).await {
            Ok((_, w)) if w.valid() => w,
            Ok(_) => return status(StatusCode::BAD_REQUEST),
            Err(code) => return status(code),
        };
        let app = w.app.clone();
        self.answer(&app, self.again(retries, w).await).await
    }

    /// Delivers the waiting event of `w` again. Gives the messages it published.
    async fn again(&self, retries: &Scheduler, mut w: Waiting) -> Result<Vec<(String, String)>> {
        // None: an earlier try of this schedule delivered it, or it was kept past the lifecycle's days.
        let Some((bytes, _)) = self.store.get(&w.key(), None, EVENT_MAX as u64).await? else { return Ok(vec![]) };
        w.tries += 1;
        match self.deliver(&w.app, bytes).await {
            Ok(published) => {
                self.store.delete(&w.key()).await?;
                Ok(published)
            }
            Err(after) => self.wait(retries, w, after).await.map(|()| vec![]),
        }
    }

    /// Schedules the next try of `w`, which has failed `w.tries` times, `after` being the `Retry-After` of the last. If
    /// that would be after its deadline, the event is lost instead.
    async fn wait(&self, retries: &Scheduler, w: Waiting, after: Option<std::time::Duration>) -> Result<()> {
        let at = unix() + outbox::backoff(w.tries, after).as_secs();
        if at > w.deadline {
            tracing::warn!(
                app = w.app,
                commit = w.commit,
                "outbox: an event failed every try for {WINDOW:?}, so it is lost"
            );
            return self.store.delete(&w.key()).await;
        }
        let (name, body) = schedule(retries, &w, at)?;
        deploy::scheduler(&self.aws, Method::POST, &format!("/schedules/{name}"), body, StatusCode::CONFLICT).await?;
        Ok(())
    }

    /// The answer to an invocation, by how `done` says it went: 204, once the messages it published are sent; else 503,
    /// which fails the invocation. Then Lambda invokes the alias again, twice, and after that drops the event, as it
    /// does when the router fails in any other way.
    pub(crate) async fn answer(&self, app: &str, done: Result<Vec<(String, String)>>) -> Response {
        match done {
            Ok(_published) => {
                #[cfg(feature = "ws")]
                crate::ws::Hub::Aws(self).publish(app, _published).await;
                status(StatusCode::NO_CONTENT)
            }
            Err(e) => {
                tracing::warn!(app, "outbox: {e:#}");
                status(StatusCode::SERVICE_UNAVAILABLE)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(app: &str, commit: &str) -> Waiting {
        Waiting { app: app.into(), commit: commit.into(), tries: 3, deadline: 0 }
    }

    #[test]
    fn keys_are_what_the_router_makes() {
        let commit = store::random();
        assert!(waiting("app", &commit).valid());
        assert_eq!(waiting("app", &commit).key().to_string(), format!("outbox/app/{commit}"));
        for bad in ["", "..", "../app", &commit.to_uppercase(), &commit[1..], &format!("{commit}0"), &"%2e".repeat(11)]
        {
            assert!(!waiting("app", bad).valid(), "{bad}");
        }
        for bad in ["", "a", "App", "a/b", "..", "-a", &"a".repeat(64)] {
            assert!(!waiting(bad, &commit).valid(), "{bad}");
        }
    }

    #[test]
    fn a_schedule_is_one_time_and_carries_a_reference() {
        let retries = Scheduler { group: "g".into(), target: "arn:alias".into(), role: "arn:role".into() };
        let w = waiting("app", &store::random());
        let (name, body) = schedule(&retries, &w, 1_700_000_000).unwrap();
        assert!(name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)), "{name}");
        assert_ne!(name, schedule(&retries, &waiting("other", &w.commit), 0).unwrap().0, "per app");
        assert_ne!(
            name,
            schedule(&retries, &Waiting { tries: 4, ..waiting("app", &w.commit) }, 0).unwrap().0,
            "per try"
        );
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["ScheduleExpression"], "at(2023-11-14T22:13:20)");
        assert_eq!(body["ActionAfterCompletion"], "DELETE");
        assert_eq!(body["FlexibleTimeWindow"]["Mode"], "OFF");
        let target = &body["Target"];
        assert_eq!(
            (&body["GroupName"], &target["Arn"], &target["RoleArn"]),
            (&json!("g"), &json!("arn:alias"), &json!("arn:role"))
        );
        let input = body["Target"]["Input"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Waiting>(input).unwrap().commit, w.commit);
        assert!(input.len() < 256, "a reference, not an event");
    }
}
