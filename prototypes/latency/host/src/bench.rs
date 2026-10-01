//! Spike-only `/__bench/<op>?n=<N>` routes: time raw storage operations from inside the function, with no guest involved.
//! Each call runs the operation `n` times in sequence and returns the per-call latencies in microseconds.
use anyhow::{Result, bail};
use aws_sdk_dynamodb::{Client, types::AttributeValue as Av};
use bytes::Bytes;
use object_store::{Error as StoreError, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion, path::Path as Key};
use serde_json::{Value, json};
use std::{future::Future, sync::Arc, time::Instant};
use tokio::sync::OnceCell;

pub struct Bench { bucket: Option<Arc<dyn ObjectStore>>, table: Option<String>, ddb: OnceCell<Client> }

/// Awaits `f`, returning its output and the elapsed microseconds.
async fn timed<T>(f: impl Future<Output = T>) -> (T, u64) {
    let t = Instant::now();
    let v = f.await;
    (v, t.elapsed().as_micros() as u64)
}

/// A 1 KB payload that differs per `i` (identical content would give identical ETags and make If-Match tests meaningless).
fn body(i: usize) -> Bytes { let mut b = format!("{i:016}").into_bytes(); b.resize(1024, b'x'); Bytes::from(b) }

impl Bench {
    pub fn new(bucket: Option<Arc<dyn ObjectStore>>, table: Option<String>) -> Self { Self { bucket, table, ddb: OnceCell::new() } }

    fn s3(&self) -> Result<&Arc<dyn ObjectStore>> { self.bucket.as_ref().ok_or_else(|| anyhow::anyhow!("SPINIT_BUCKET is not set")) }

    /// The SDK client (and its credential chain) is built on first use, so it costs nothing on routes that don't need it.
    async fn ddb(&self) -> Result<(&Client, &str)> {
        let Some(table) = &self.table else { bail!("SPINIT_TABLE is not set") };
        let client = self.ddb.get_or_init(|| async { Client::new(&aws_config::load_from_env().await) }).await;
        Ok((client, table))
    }

    /// Returns `{op, n, setup_us, samples_us}`. `setup_us` is untimed preparation (seeding, fetching an ETag or version).
    pub async fn run(&self, op: &str, n: usize) -> Result<Value> {
        let kv = Key::from("bench/kv-1kb");
        let mut samples = Vec::with_capacity(n);
        let mut setup = 0;
        match op {
            "seed" => {
                self.s3()?.put(&kv, PutPayload::from(body(0))).await?;
                self.put_item(0, None).await?;
            }
            // Correctness of the conditional operations the KV design relies on (run against MinIO and DynamoDB Local before trusting them).
            "check" => return self.check().await,
            // DynamoDB Local only: in AWS the table is created by OpenTofu.
            "ddb-create-table" => {
                use aws_sdk_dynamodb::types::{AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType};
                let (client, table) = self.ddb().await?;
                let key = KeySchemaElement::builder().attribute_name("pk").key_type(KeyType::Hash).build()?;
                let attr = AttributeDefinition::builder().attribute_name("pk").attribute_type(ScalarAttributeType::S).build()?;
                client.create_table().table_name(table).key_schema(key).attribute_definitions(attr).billing_mode(BillingMode::PayPerRequest).send().await?;
            }
            "s3-get" => for _ in 0..n {
                let (r, us) = timed(async { anyhow::Ok(self.s3()?.get(&kv).await?.bytes().await?) }).await;
                r?;
                samples.push(us);
            },
            "s3-get-304" => {
                let (etag, us) = timed(async { anyhow::Ok(self.s3()?.head(&kv).await?.e_tag) }).await;
                setup = us;
                let etag = etag?;
                for _ in 0..n {
                    let opts = GetOptions::new().with_if_none_match(etag.clone());
                    let (r, us) = timed(self.s3()?.get_opts(&kv, opts)).await;
                    match r { Err(StoreError::NotModified { .. }) => samples.push(us), Err(e) => return Err(e.into()), Ok(_) => bail!("expected 304") }
                }
            }
            "s3-put-create" => {
                let run = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
                for i in 0..n {
                    let key = Key::from(format!("bench/create/{run}-{i}"));
                    let (r, us) = timed(self.s3()?.put_opts(&key, PutPayload::from(body(i)), PutMode::Create.into())).await;
                    r?;
                    samples.push(us);
                }
            }
            "s3-put-update" => {
                let key = Key::from("bench/cas");
                let (first, us) = timed(self.s3()?.put(&key, PutPayload::from(body(0)))).await;
                setup = us;
                let mut version = UpdateVersion::from(first?);
                for i in 0..n {
                    let (r, us) = timed(self.s3()?.put_opts(&key, PutPayload::from(body(i + 1)), PutMode::Update(version.clone()).into())).await;
                    version = r?.into();
                    samples.push(us);
                }
            }
            "ddb-get-eventual" | "ddb-get-strong" => {
                let (client, table) = self.ddb().await?;
                for _ in 0..n {
                    let get = client.get_item().table_name(table).key("pk", Av::S("bench".into())).consistent_read(op == "ddb-get-strong");
                    let (r, us) = timed(get.send()).await;
                    if r?.item.is_none() { bail!("item missing: run /__bench/seed first") }
                    samples.push(us);
                }
            }
            "ddb-put-cond" => {
                let (_, us) = timed(self.put_item(0, None)).await;
                setup = us;
                for i in 0..n as u64 {
                    let (r, us) = timed(self.put_item(i + 1, Some(i))).await;
                    r?;
                    samples.push(us);
                }
            }
            _ => bail!("unknown bench op {op}"),
        }
        Ok(json!({ "op": op, "n": n, "setup_us": setup, "samples_us": samples }))
    }

    async fn check(&self) -> Result<Value> {
        fn kind<T>(r: Result<T, StoreError>) -> &'static str {
            match r { Ok(_) => "ok", Err(StoreError::AlreadyExists { .. }) => "AlreadyExists", Err(StoreError::Precondition { .. }) => "Precondition",
                Err(StoreError::NotModified { .. }) => "NotModified", Err(StoreError::NotFound { .. }) => "NotFound", Err(_) => "other-error" }
        }
        let s3 = self.s3()?;
        let key = Key::from(format!("bench/check-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
        let put = |i: usize, mode: PutMode| s3.put_opts(&key, PutPayload::from(body(i)), mode.into());
        let first = put(1, PutMode::Create).await;
        let again = put(2, PutMode::Create).await;
        let v1 = UpdateVersion::from(first.as_ref().map_err(|_| anyhow::anyhow!("create failed"))?.clone());
        let matching = put(3, PutMode::Update(v1.clone())).await;
        let stale = put(4, PutMode::Update(v1.clone())).await;
        let etag = s3.head(&key).await?.e_tag;
        let not_modified = s3.get_opts(&key, GetOptions::new().with_if_none_match(etag)).await.map(|_| ());
        let modified = s3.get_opts(&key, GetOptions::new().with_if_none_match(v1.e_tag.clone())).await.map(|_| ());
        let mut out = json!({ "s3": {
            "create_fresh": kind(first), "create_existing": kind(again), "update_current_etag": kind(matching),
            "update_stale_etag": kind(stale), "get_if_none_match_current": kind(not_modified), "get_if_none_match_old": kind(modified) } });
        if self.table.is_some() {
            let conflict = |r: Result<()>| match r { Ok(()) => "ok".to_string(), Err(e) => format!("{}", e.root_cause().to_string().lines().next().unwrap_or("")) };
            self.put_item(0, None).await?;
            out["ddb"] = json!({ "update_current_version": conflict(self.put_item(1, Some(0)).await), "update_stale_version": conflict(self.put_item(2, Some(0)).await) });
        }
        Ok(json!({ "op": "check", "n": 0, "setup_us": 0, "samples_us": [], "result": out }))
    }

    /// Writes the item at `version`; with `expect`, only if the stored version is still that one.
    async fn put_item(&self, version: u64, expect: Option<u64>) -> Result<()> {
        let (client, table) = self.ddb().await?;
        let mut put = client.put_item().table_name(table).item("pk", Av::S("bench".into()))
            .item("version", Av::N(version.to_string())).item("data", Av::S("x".repeat(1024)));
        if let Some(v) = expect {
            put = put.condition_expression("version = :v").expression_attribute_values(":v", Av::N(v.to_string()));
        }
        put.send().await?;
        Ok(())
    }
}
