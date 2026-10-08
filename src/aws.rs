//! The AWS APIs tric calls besides S3, signed with the environment's credentials: STS, which mints an app's storage
//! credentials. Each service is at `AWS_ENDPOINT_URL_<SERVICE>`, or `AWS_ENDPOINT_URL`, or AWS's own endpoint.
use crate::store::Shared;
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Method, StatusCode};
use object_store::ClientOptions;
use object_store::aws::{AmazonS3, AwsAuthorizer, AwsCredential, AwsCredentialProvider};
use object_store::client::{HttpClient, HttpConnector, HttpRequest};
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
        Ok(Self { creds: s3.credentials().clone(), http: Shared.connect(&http)?, region })
    }

    fn endpoint(&self, service: &str) -> String {
        let var = |k: String| std::env::var(k).ok().filter(|v| !v.is_empty());
        var(format!("AWS_ENDPOINT_URL_{}", service.to_ascii_uppercase()))
            .or_else(|| var("AWS_ENDPOINT_URL".into()))
            .unwrap_or_else(|| format!("https://{service}.{}.amazonaws.com", self.region))
    }

    /// Calls `service`, signed, and returns the response's status and body.
    pub async fn call(
        &self,
        service: &str,
        method: Method,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<(StatusCode, Bytes)> {
        let cred = self.creds.get_credential().await?;
        let uri = format!("{}{path}", self.endpoint(service).trim_end_matches('/'));
        let req = http::Request::builder().method(method).uri(uri).header(CONTENT_TYPE, content_type);
        let mut req: HttpRequest = req.body(body.into())?;
        AwsAuthorizer::new(&cred, service, &self.region).try_authorize(&mut req, None)?;
        let res = self.http.execute(req).await?;
        let status = res.status();
        Ok((status, res.into_body().bytes().await?))
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
        let form = form.into_bytes();
        let (status, body) = self.call("sts", Method::POST, "/", "application/x-www-form-urlencoded", form).await?;
        ensure!(status.is_success(), "AssumeRole answered {status}: {}", String::from_utf8_lossy(&body));
        let res: AssumeRoleResponse = quick_xml::de::from_reader(&body[..])?;
        Ok(res.assume_role_result.credentials)
    }
}
