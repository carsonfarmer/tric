//! The two AWS APIs tric calls besides S3, with the bucket's credentials: Lambda's `Invoke`, which the outbox enqueues
//! with, and EventBridge Scheduler, which runs cron.
use bytes::Bytes;
use http::{Method, StatusCode, header::CONTENT_TYPE};
use object_store::aws::{AmazonS3, AwsAuthorizer, AwsCredentialProvider};
use object_store::client::{HttpClient, HttpRequest};
use serde::{Deserialize, Serialize};
use wasmtime::{Result, ensure};

pub struct Aws {
    creds: AwsCredentialProvider,
    http: HttpClient,
    region: String,
}

/// A schedule as `CreateSchedule` takes it.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Schedule {
    pub schedule_expression: String,
    pub schedule_expression_timezone: &'static str,
    pub flexible_time_window: Window,
    pub group_name: String,
    pub target: Target,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Window {
    pub mode: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Target {
    pub arn: String,
    pub role_arn: String,
    pub input: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    schedules: Vec<Named>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Named {
    name: String,
}

/// `s` as a URL query value.
fn query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

impl Aws {
    /// With the credentials of `s3`, in `region`, over `http`.
    pub fn new(s3: &AmazonS3, http: HttpClient, region: String) -> Self {
        Self { creds: s3.credentials().clone(), http, region }
    }

    /// Calls `service`'s JSON API, signed, and returns the response's status and body.
    async fn call(
        &self,
        service: &str,
        method: Method,
        path: &str,
        body: Vec<u8>,
        header: Option<(&str, &str)>,
    ) -> Result<(StatusCode, Bytes)> {
        let cred = self.creds.get_credential().await?;
        let uri = format!("https://{service}.{}.amazonaws.com{path}", self.region);
        let mut req = http::Request::builder().method(method).uri(uri).header(CONTENT_TYPE, "application/json");
        if let Some((name, value)) = header {
            req = req.header(name, value);
        }
        let mut req: HttpRequest = req.body(body.into())?;
        AwsAuthorizer::new(&cred, service, &self.region).try_authorize(&mut req, None)?;
        let res = self.http.execute(req).await?;
        let status = res.status();
        Ok((status, res.into_body().bytes().await?))
    }

    /// Invokes `function` asynchronously with `payload`: Lambda queues the event, and answers once it has.
    pub async fn invoke(&self, function: &str, payload: Vec<u8>) -> Result<()> {
        let path = format!("/2015-03-31/functions/{}/invocations", query(function));
        let event = Some(("x-amz-invocation-type", "Event"));
        let (status, body) = self.call("lambda", Method::POST, &path, payload, event).await?;
        ensure!(status == StatusCode::ACCEPTED, "Invoke answered {status}: {}", String::from_utf8_lossy(&body));
        Ok(())
    }

    /// The names of the schedules in `group` that start with `prefix`.
    pub async fn schedules(&self, group: &str, prefix: &str) -> Result<Vec<String>> {
        let (mut names, mut next) = (vec![], None::<String>);
        loop {
            let mut path = format!("/schedules?NamePrefix={}&ScheduleGroup={}", query(prefix), query(group));
            if let Some(token) = &next {
                path += &format!("&NextToken={}", query(token));
            }
            let (status, body) = self.call("scheduler", Method::GET, &path, vec![], None).await?;
            ensure!(status.is_success(), "ListSchedules answered {status}: {}", String::from_utf8_lossy(&body));
            let listed: Listed = serde_json::from_slice(&body)?;
            names.extend(listed.schedules.into_iter().map(|s| s.name));
            next = listed.next_token;
            if next.is_none() {
                return Ok(names);
            }
        }
    }

    pub async fn create(&self, name: &str, schedule: &Schedule) -> Result<()> {
        let path = format!("/schedules/{}", query(name));
        let (status, body) = self.call("scheduler", Method::POST, &path, serde_json::to_vec(schedule)?, None).await?;
        ensure!(status.is_success(), "CreateSchedule answered {status}: {}", String::from_utf8_lossy(&body));
        Ok(())
    }

    pub async fn delete(&self, group: &str, name: &str) -> Result<()> {
        let path = format!("/schedules/{}?groupName={}", query(name), query(group));
        let (status, body) = self.call("scheduler", Method::DELETE, &path, vec![], None).await?;
        ensure!(
            status.is_success() || status == StatusCode::NOT_FOUND,
            "DeleteSchedule answered {status}: {}",
            String::from_utf8_lossy(&body)
        );
        Ok(())
    }
}
