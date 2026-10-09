//! The AWS APIs tric calls besides S3, signed with the environment's credentials: STS, which mints an app's storage
//! credentials, and Lambda. Each service is at `AWS_ENDPOINT_URL_<SERVICE>`, or `AWS_ENDPOINT_URL`, or AWS's own endpoint.
use crate::store::Shared;
use http::Method;
use http::header::CONTENT_TYPE;
use object_store::ClientOptions;
use object_store::aws::{AmazonS3, AwsAuthorizer, AwsCredential, AwsCredentialProvider};
use object_store::client::{HttpClient, HttpConnector, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use wasmtime::{Result, ensure};

/// How long minted credentials last: an hour is the most a role assumed by a role may have.
pub const LIFETIME: u64 = 3600;

pub struct Aws {
    creds: AwsCredentialProvider,
    http: HttpClient,
    pub region: String,
}

/// Credentials as STS gives them, and as `credential_process` (the AWS CLI's) takes them.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Credentials {
    #[serde(default = "one")]
    version: u8,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    expiration: String,
}

fn one() -> u8 {
    1
}

impl From<Credentials> for AwsCredential {
    fn from(c: Credentials) -> Self {
        Self { key_id: c.access_key_id, secret_key: c.secret_access_key, token: Some(c.session_token) }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssumeRoleResponse {
    assume_role_result: AssumeRoleResult,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssumeRoleResult {
    credentials: Credentials,
}

/// `s` as a URL query or form value.
pub fn query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

impl Aws {
    /// With the credentials `s3` has from the environment.
    pub fn new(s3: &AmazonS3) -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let region = var("AWS_REGION").or_else(|| var("AWS_DEFAULT_REGION")).unwrap_or_else(|| "us-east-1".into());
        let http = ClientOptions::new().with_allow_http(var("AWS_ALLOW_HTTP").is_some_and(|v| v == "true"));
        let http = http.with_timeout_disabled();
        Ok(Self { creds: s3.credentials().clone(), http: Shared.connect(&http)?, region })
    }

    fn endpoint(&self, service: &str) -> String {
        let var = |k: String| std::env::var(k).ok().filter(|v| !v.is_empty());
        var(format!("AWS_ENDPOINT_URL_{}", service.to_ascii_uppercase()))
            .or_else(|| var("AWS_ENDPOINT_URL".into()))
            .unwrap_or_else(|| format!("https://{service}.{}.amazonaws.com", self.region))
    }

    /// Sends `service` a signed request, with `headers`, and returns its response as it comes. There is no time limit:
    /// a response may stream for as long as the function that answers it runs.
    pub async fn send(
        &self,
        service: &str,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<HttpResponse> {
        let cred = self.creds.get_credential().await?;
        let uri = format!("{}{path}", self.endpoint(service).trim_end_matches('/'));
        let req = headers.iter().fold(http::Request::builder().method(method).uri(uri), |r, (k, v)| r.header(*k, *v));
        let mut req: HttpRequest = req.body(body.into())?;
        AwsAuthorizer::new(&cred, service, &self.region).try_authorize(&mut req, None)?;
        Ok(self.http.execute(req).await?)
    }

    /// Credentials of the session `session` of `role` (or, where STS has no roles, of the caller), with `policy` as
    /// the session policy, which is all they may do.
    pub async fn assume(&self, role: Option<&str>, session: &str, policy: &str) -> Result<Credentials> {
        let mut form = format!(
            "Action=AssumeRole&Version=2011-06-15&RoleSessionName={}&Policy={}&DurationSeconds={LIFETIME}",
            query(session),
            query(policy)
        );
        if let Some(role) = role {
            form += &format!("&RoleArn={}", query(role));
        }
        let headers = [(CONTENT_TYPE.as_str(), "application/x-www-form-urlencoded")];
        let res = self.send("sts", Method::POST, "/", &headers, form.into_bytes()).await?;
        let (status, body) = (res.status(), res.into_body().bytes().await?);
        ensure!(status.is_success(), "AssumeRole answered {status}: {}", String::from_utf8_lossy(&body));
        let res: AssumeRoleResponse = quick_xml::de::from_reader(&body[..])?;
        Ok(res.assume_role_result.credentials)
    }
}
