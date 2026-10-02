// Exercises the four required S3 behaviours through object_store 0.14.2 itself.
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use object_store::{
    aws::AmazonS3Builder, path::Path, Error, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions,
    PutPayload, RetryConfig, UpdateVersion,
};
use std::{sync::Arc, time::Duration};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("set {k}"))
}

fn report(name: &str, ok: bool, detail: impl std::fmt::Display) {
    let d = detail.to_string();
    let d: String = d.chars().take(110).collect();
    println!("{} {name}: {d}", if ok { "PASS" } else { "FAIL" });
}

async fn body(s: &dyn ObjectStore, p: &Path) -> String {
    String::from_utf8(s.get(p).await.unwrap().bytes().await.unwrap().to_vec()).unwrap()
}

#[tokio::main]
async fn main() {
    let s3: Arc<dyn ObjectStore> = Arc::new(
        AmazonS3Builder::new()
            .with_endpoint(env("S3_ENDPOINT"))
            .with_bucket_name(env("S3_BUCKET"))
            .with_access_key_id(env("S3_KEY"))
            .with_secret_access_key(env("S3_SECRET"))
            .with_region("us-east-1")
            .with_allow_http(true)
            .with_retry(RetryConfig {
                max_retries: 2,
                retry_timeout: Duration::from_secs(30),
                ..Default::default()
            })
            .build()
            .unwrap(),
    );
    let s = &*s3;
    let put = |m: PutMode| PutOptions { mode: m, ..Default::default() };
    let pl = |t: &'static str| PutPayload::from(Bytes::from_static(t.as_bytes()));

    // 1. create-only (If-None-Match: *)
    let k = Path::from("rs/create");
    let r1 = s.put_opts(&k, pl("one"), put(PutMode::Create)).await;
    report("1a create new key", r1.is_ok(), format!("{:?}", r1.as_ref().map(|r| &r.e_tag)));
    let r2 = s.put_opts(&k, pl("two"), put(PutMode::Create)).await;
    report("1b create existing -> AlreadyExists", matches!(r2, Err(Error::AlreadyExists { .. })), format!("{:?}", r2.as_ref().map(|_| ()).map_err(|e| e.to_string())));
    report("1c body unchanged after rejected create", body(s, &k).await == "one", "");

    // 2. compare-and-swap (If-Match: <etag>)
    let e1 = r1.unwrap().e_tag;
    let r3 = s.put_opts(&k, pl("three"), put(PutMode::Update(UpdateVersion { e_tag: e1.clone(), version: None }))).await;
    report("2a update with current etag", r3.is_ok(), format!("{:?}", r3.as_ref().map(|r| &r.e_tag).map_err(|e| e.to_string())));
    let e3 = r3.ok().and_then(|r| r.e_tag);
    let r4 = s.put_opts(&k, pl("four"), put(PutMode::Update(UpdateVersion { e_tag: e1.clone(), version: None }))).await;
    report("2b update with stale etag -> Precondition", matches!(r4, Err(Error::Precondition { .. })), format!("{:?}", r4.as_ref().map(|_| ()).map_err(|e| e.to_string())));
    report("2c body unchanged after rejected update", body(s, &k).await == "three", "");
    let missing = Path::from("rs/missing");
    let r5 = s.put_opts(&missing, pl("x"), put(PutMode::Update(UpdateVersion { e_tag: e1.clone(), version: None }))).await;
    report("2d update on missing key -> Precondition", matches!(r5, Err(Error::Precondition { .. })), format!("{:?}", r5.as_ref().map(|_| ()).map_err(|e| e.to_string())));

    // 3. conditional GET (If-None-Match: <etag>) -> 304
    let g1 = s.get_opts(&k, GetOptions { if_none_match: e3.clone(), ..Default::default() }).await;
    report("3a get if-none-match current etag -> NotModified", matches!(g1, Err(Error::NotModified { .. })), format!("{:?}", g1.as_ref().map(|_| ()).map_err(|e| e.to_string())));
    let g2 = s.get_opts(&k, GetOptions { if_none_match: Some("\"bogus\"".into()), ..Default::default() }).await;
    report("3b get if-none-match other etag -> 200", g2.is_ok(), "");

    // 4. ListObjectsV2 + continuation tokens (default page = 1000 keys, so 2500 keys = 3 pages)
    let n = 2500usize;
    futures::stream::iter(0..n)
        .map(|i| {
            let s3 = s3.clone();
            async move { s3.put(&Path::from(format!("rslist/obj-{i:05}")), pl("x")).await }
        })
        .buffer_unordered(64)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let listed: Vec<String> = s.list(Some(&Path::from("rslist"))).map_ok(|m| m.location.to_string()).try_collect().await.unwrap();
    let mut uniq = listed.clone();
    uniq.sort();
    uniq.dedup();
    report("4a list 2500 keys (paginated), none lost/duplicated", listed.len() == n && uniq.len() == n, format!("got {} unique {}", listed.len(), uniq.len()));
    let off: Vec<_> = s.list_with_offset(Some(&Path::from("rslist")), &Path::from("rslist/obj-02000")).try_collect().await.unwrap();
    report("4b list_with_offset (start-after)", off.len() == n - 2001, format!("got {}", off.len()));

    // 5. atomicity under concurrency: 20 rounds x 32 racing writers per round
    let (mut bad_create, mut bad_cas) = (0, 0);
    for round in 0..20 {
        let race = Path::from(format!("rs/race-create-{round}"));
        let res: Vec<_> = futures::future::join_all((0..32).map(|i| {
            let s3 = s3.clone();
            let race = race.clone();
            async move { s3.put_opts(&race, PutPayload::from(format!("w{i}")), PutOptions { mode: PutMode::Create, ..Default::default() }).await }
        })).await;
        let ok = res.iter().filter(|r| r.is_ok()).count();
        let ae = res.iter().filter(|r| matches!(r, Err(Error::AlreadyExists { .. }))).count();
        if !(ok == 1 && ae == 31) { bad_create += 1; println!("  round {round} create: ok={ok} already_exists={ae} other={}", 32 - ok - ae); }
        let cas = Path::from(format!("rs/race-cas-{round}"));
        let e0 = s.put(&cas, pl("base")).await.unwrap().e_tag;
        let res: Vec<_> = futures::future::join_all((0..32).map(|i| {
            let s3 = s3.clone();
            let (cas, e0) = (cas.clone(), e0.clone());
            async move { s3.put_opts(&cas, PutPayload::from(format!("w{i}")), PutOptions { mode: PutMode::Update(UpdateVersion { e_tag: e0, version: None }), ..Default::default() }).await }
        })).await;
        let ok = res.iter().filter(|r| r.is_ok()).count();
        let pf = res.iter().filter(|r| matches!(r, Err(Error::Precondition { .. }))).count();
        if !(ok == 1 && pf == 31) { bad_cas += 1; println!("  round {round} cas: ok={ok} precondition={pf} other={}", 32 - ok - pf); }
    }
    report("5a racing creates (20 rounds x 32) -> exactly 1 winner each", bad_create == 0, format!("{bad_create}/20 rounds wrong"));
    report("5b racing CAS from same etag (20 rounds x 32) -> exactly 1 winner each", bad_cas == 0, format!("{bad_cas}/20 rounds wrong"));
}
