//! The AWS APIs tric calls besides what object_store does, signed with the environment's credentials: STS, which mints
//! an app's storage credentials, Lambda, Scheduler, and S3's ListObjectVersions, as object_store lists no versions.
//! Each service is at `AWS_ENDPOINT_URL_<SERVICE>`, or `AWS_ENDPOINT_URL`, or AWS's own endpoint.
use crate::store::Shared;
use http::Method;
use http::header::CONTENT_TYPE;
use object_store::ClientOptions;
use object_store::aws::{AmazonS3, AwsAuthorizer, AwsCredential, AwsCredentialProvider};
use object_store::client::{HttpClient, HttpConnector, HttpRequest, HttpResponse};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use wasmtime::{Result, ensure, error::Context};

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

/// One version of an object, or a delete marker, as ListObjectVersions lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub key: String,
    pub id: String,
    /// Whether it is the object as it is now. A delete marker that is says that the object is deleted.
    pub latest: bool,
    /// Whether it is a delete marker, which holds nothing.
    pub marker: bool,
}

/// A page of versions, in the order of their keys, and where the next page begins, if there is one: the key and the
/// version id to list after.
#[derive(Debug, Default)]
pub struct Versions {
    pub versions: Vec<Version>,
    pub next: Option<(String, String)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListVersionsResult {
    #[serde(default)]
    is_truncated: bool,
    next_key_marker: Option<String>,
    next_version_id_marker: Option<String>,
    #[serde(default)]
    version: Vec<Listed>,
    #[serde(default)]
    delete_marker: Vec<Listed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    key: String,
    version_id: String,
    is_latest: bool,
}

impl Versions {
    /// A page as S3 answers it. Within a key, S3 lists versions and delete markers together, the newest first, and
    /// RustFS lists the markers first; either way each is read for what it is, and the keys are in order.
    fn parse(xml: &[u8]) -> Result<Self> {
        let page: ListVersionsResult = quick_xml::de::from_reader(xml)?;
        let of = |marker| move |l: Listed| Version { key: l.key, id: l.version_id, latest: l.is_latest, marker };
        let mut versions: Vec<_> = page.version.into_iter().map(of(false)).collect();
        versions.extend(page.delete_marker.into_iter().map(of(true)));
        let next = match page.is_truncated {
            true => Some((
                page.next_key_marker.context("a truncated list of versions has no `NextKeyMarker`")?,
                page.next_version_id_marker.context("a truncated list of versions has no `NextVersionIdMarker`")?,
            )),
            false => None,
        };
        Ok(Self { versions, next })
    }
}

/// `s` as a URL query or form value: all but RFC 3986's unreserved characters percent-encoded.
pub fn query(s: &str) -> String {
    const RESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
    utf8_percent_encode(s, RESERVED).to_string()
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

    /// Sends `service` a signed request at `path`, of its endpoint unless it is a URL, with `headers`, and returns its
    /// response as it comes. There is no time limit: a response may stream for as long as the function that answers it
    /// runs.
    pub async fn send(
        &self,
        service: &str,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<HttpResponse> {
        let cred = self.creds.get_credential().await?;
        let base = if path.starts_with('/') { self.endpoint(service) } else { String::new() };
        let uri = format!("{}{path}", base.trim_end_matches('/'));
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

    /// A page of up to 1,000 versions of the objects in `bucket` whose keys begin with `prefix`, after the key and
    /// version id `from` if given: S3's ListObjectVersions, at the endpoint and in the path style that object_store's
    /// requests use.
    pub async fn versions(&self, bucket: &str, prefix: &str, from: Option<(String, String)>) -> Result<Versions> {
        let mut path = format!("/{bucket}?versions=&prefix={}&max-keys=1000", query(prefix));
        if let Some((key, id)) = from {
            path += &format!("&key-marker={}&version-id-marker={}", query(&key), query(&id));
        }
        let res = self.send("s3", Method::GET, &path, &[], vec![]).await?;
        let (status, body) = (res.status(), res.into_body().bytes().await?);
        ensure!(status.is_success(), "ListObjectVersions answered {status}: {}", String::from_utf8_lossy(&body));
        Versions::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(key: &str, id: &str, latest: bool, marker: bool) -> Version {
        Version { key: key.into(), id: id.into(), latest, marker }
    }

    #[test]
    fn a_page_of_versions_is_read_whatever_the_order_of_its_elements() {
        // As S3 lists them: by key, and within a key the newest first, versions and delete markers interleaved.
        let s3 = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>tric</Name><Prefix>apps/a/names/n</Prefix><KeyMarker></KeyMarker><VersionIdMarker></VersionIdMarker>
              <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
              <DeleteMarker><Key>apps/a/names/n</Key><VersionId>v3</VersionId><IsLatest>true</IsLatest>
                <LastModified>2026-10-10T10:00:00.000Z</LastModified></DeleteMarker>
              <Version><Key>apps/a/names/n</Key><VersionId>v2</VersionId><IsLatest>false</IsLatest>
                <LastModified>2026-10-10T09:00:00.000Z</LastModified><ETag>"e2"</ETag><Size>2</Size>
                <StorageClass>STANDARD</StorageClass></Version>
              <DeleteMarker><Key>apps/a/names/n</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest>
                <LastModified>2026-10-10T08:00:00.000Z</LastModified></DeleteMarker>
              <Version><Key>apps/a/names/n</Key><VersionId>v0</VersionId><IsLatest>false</IsLatest>
                <LastModified>2026-10-10T07:00:00.000Z</LastModified><ETag>"e0"</ETag><Size>2</Size></Version>
              <Version><Key>apps/a/names/n2</Key><VersionId>w0</VersionId><IsLatest>true</IsLatest>
                <LastModified>2026-10-10T07:00:00.000Z</LastModified><ETag>"f0"</ETag><Size>2</Size></Version>
            </ListVersionsResult>"#;
        // As RustFS lists them: the delete markers first.
        let rustfs = r#"<ListVersionsResult><IsTruncated>false</IsTruncated>
              <DeleteMarker><Key>apps/a/names/n</Key><VersionId>v3</VersionId><IsLatest>true</IsLatest></DeleteMarker>
              <DeleteMarker><Key>apps/a/names/n</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest></DeleteMarker>
              <Version><Key>apps/a/names/n</Key><VersionId>v2</VersionId><IsLatest>false</IsLatest></Version>
              <Version><Key>apps/a/names/n</Key><VersionId>v0</VersionId><IsLatest>false</IsLatest></Version>
              <Version><Key>apps/a/names/n2</Key><VersionId>w0</VersionId><IsLatest>true</IsLatest></Version>
            </ListVersionsResult>"#;
        for xml in [s3, rustfs] {
            let mut page = Versions::parse(xml.as_bytes()).unwrap();
            page.versions.sort_by(|a, b| (&a.key, &a.id).cmp(&(&b.key, &b.id)));
            assert_eq!(
                page.versions,
                [
                    version("apps/a/names/n", "v0", false, false),
                    version("apps/a/names/n", "v1", false, true),
                    version("apps/a/names/n", "v2", false, false),
                    version("apps/a/names/n", "v3", true, true),
                    version("apps/a/names/n2", "w0", true, false),
                ]
            );
            assert_eq!(page.next, None);
        }
    }

    #[test]
    fn a_truncated_page_says_where_the_next_begins() {
        let xml = r#"<ListVersionsResult><IsTruncated>true</IsTruncated>
              <NextKeyMarker>apps/a/names/n</NextKeyMarker><NextVersionIdMarker>v2</NextVersionIdMarker>
              <Version><Key>apps/a/names/n</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest></Version>
            </ListVersionsResult>"#;
        let page = Versions::parse(xml.as_bytes()).unwrap();
        assert_eq!(page.versions, [version("apps/a/names/n", "v2", true, false)]);
        assert_eq!(page.next, Some(("apps/a/names/n".into(), "v2".into())));
        // One with no marker to go on from is an error, not the end.
        let xml = r#"<ListVersionsResult><IsTruncated>true</IsTruncated></ListVersionsResult>"#;
        assert!(Versions::parse(xml.as_bytes()).is_err());
        // Nothing under the prefix.
        let page = Versions::parse(b"<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>");
        assert!(page.unwrap().versions.is_empty());
    }
}
