//! spinit latency-spike host: one Wasmtime component behind plain hyper, plus `/__bench/` routes that time storage calls.
//!
//! Config is by environment (a Lambda function has no CLI):
//!   SPINIT_ADDR         listen address (default 127.0.0.1:8080)
//!   SPINIT_COMPONENT    local path to a .wasm, or `sha256:<hex>` for `blobs/sha256/<hex>` in the bucket
//!   SPINIT_CACHE_DIR    where `.cwasm` files are cached (default /tmp/spinit-cache)
//!   SPINIT_ALLOCATOR    `default` | `pooling`
//!   SPINIT_PRECOMPILED  `1`: look for a precompiled artifact in the bucket before compiling
//!   SPINIT_EAGER        `1`: load the component before listening (Lambda init phase) instead of on the first request
//!   SPINIT_TARGET       compile for this target triple with baseline ISA flags (portable `.cwasm`), e.g. aarch64-unknown-linux-gnu
//!   SPINIT_BUCKET       bucket for blobs and the S3 bench routes; endpoint and credentials come from AWS_* variables
//!   SPINIT_TABLE        DynamoDB table for the DynamoDB bench routes
//! Every request, and every cold-path phase, is one JSON line on stdout.
//! Subcommand: `spinit-host precompile <in.wasm> <out.cwasm>`.
mod bench;
mod component;
mod guest;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use object_store::{ObjectStore, aws::AmazonS3Builder};
use serde_json::{Map, Value, json};
use std::{pin::Pin, sync::{Arc, atomic::{AtomicU64, Ordering::Relaxed}}, task::{Context as Cx, Poll}, time::Instant};
use tokio::{net::TcpListener, sync::OnceCell};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, component::Linker};
use wasmtime_wasi_http::{WasiBody, handler::ProxyPre, io::TokioIo};

use {bench::Bench, component::Source, guest::Guest};

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|v| !v.is_empty()) }
fn us(t: Instant) -> u64 { t.elapsed().as_micros() as u64 }
fn emit(v: Value) { println!("{v}"); }

/// Resident set size in KiB (Linux only), to see how close a 128 MB function is to its limit.
fn rss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().trim_end_matches("kB").trim().parse().ok()).unwrap_or(0)
}

fn engine(pooling: bool, target: Option<&str>) -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model_async(true).wasm_component_model_async_stackful(true).wasm_component_model_more_async_builtins(true);
    cfg.epoch_interruption(true);
    if let Some(t) = target { cfg.target(t)?; }
    if pooling {
        // Sized for the host's 64 in-flight requests; memory is address space only until a guest touches it.
        let n = 64;
        let mut p = PoolingAllocationConfig::new();
        p.total_component_instances(n).total_core_instances(n * 4).total_memories(n * 4).total_tables(n * 4).total_stacks(n)
            .max_memory_size(guest::MEMORY_LIMIT).max_memories_per_component(4).max_tables_per_component(4)
            .max_core_instances_per_component(8).max_component_instance_size(1 << 20).max_core_instance_size(1 << 20);
        cfg.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
    }
    Ok(Engine::new(&cfg)?)
}

fn linker(engine: &Engine) -> Result<Linker<guest::Host>> {
    let mut l = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut l)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut l)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut l)?;
    wasmtime_wasi::p3::add_to_linker(&mut l)?;
    Ok(l)
}

fn bucket() -> Result<Option<Arc<dyn ObjectStore>>> {
    let Some(name) = env("SPINIT_BUCKET") else { return Ok(None) };
    // Region, endpoint (MinIO), credentials and `allow_http` come from AWS_* variables.
    Ok(Some(Arc::new(AmazonS3Builder::from_env().with_bucket_name(name).build()?)))
}

struct Server { engine: Engine, linker: Linker<guest::Host>, source: Source, guest: OnceCell<Guest>, bench: Bench, seq: AtomicU64 }

impl Server {
    /// The guest, loading the component on first use. Emits the cold-path breakdown when it does.
    async fn guest(&self) -> Result<&Guest> {
        self.guest.get_or_try_init(|| async {
            let t0 = Instant::now();
            let (component, mut m) = self.source.load(&self.engine).await?;
            let t = Instant::now();
            let pre = self.linker.instantiate_pre(&component)?;
            m.insert("instantiate_pre_us".into(), us(t).into());
            let t = Instant::now();
            let (proxy, world) = match wasmtime_wasi_http::p3::bindings::ServicePre::new(pre.clone()) {
                Ok(p) => (ProxyPre::P3(p), "p3"),
                Err(_) => (ProxyPre::P2(wasmtime_wasi_http::p2::bindings::ProxyPre::new(pre)?), "p2"),
            };
            m.insert("proxy_pre_us".into(), us(t).into());
            m.insert("world".into(), world.into());
            m.insert("load_total_us".into(), us(t0).into());
            m.insert("rss_kb".into(), rss_kb().into());
            m.insert("event".into(), "load".into());
            emit(Value::Object(m));
            Ok(Guest::new(self.engine.clone(), proxy))
        }).await
    }

    async fn handle(&self, req: hyper::Request<hyper::body::Incoming>) -> hyper::Response<WasiBody> {
        let t0 = Instant::now();
        let mut log = Map::new();
        log.insert("event".into(), "req".into());
        log.insert("id".into(), self.seq.fetch_add(1, Relaxed).into());
        log.insert("method".into(), req.method().as_str().into());
        log.insert("path".into(), req.uri().path().into());
        let query = req.uri().query().unwrap_or("").to_string();
        let result = match req.uri().path() {
            "/__ready" => Ok(reply(200, "ok")),
            p if p.starts_with("/__bench/") => {
                let n = query.split('&').find_map(|kv| kv.strip_prefix("n=")?.parse().ok()).unwrap_or(20usize).min(1000);
                let op = p.trim_start_matches("/__bench/").to_string();
                log.insert("kind".into(), "bench".into());
                match self.bench.run(&op, n).await {
                    Ok(v) => { emit(json!({ "event": "bench", "op": op, "n": n, "setup_us": v["setup_us"], "samples_us": v["samples_us"] })); Ok(reply(200, &v.to_string())) }
                    Err(e) => Err(e),
                }
            }
            _ => self.call_guest(req, &mut log).await,
        };
        let mut res = result.unwrap_or_else(|e| { eprintln!("error: {e:?}"); reply(500, "internal error") });
        log.insert("status".into(), res.status().as_u16().into());
        log.insert("ttfb_us".into(), us(t0).into());
        // The line is written when hyper drops the body, i.e. once the last byte has been sent.
        let body = std::mem::replace(res.body_mut(), reply(200, "").into_body());
        *res.body_mut() = Logged { inner: body, t0, log: Some(log) }.boxed_unsync();
        res
    }

    async fn call_guest(&self, req: hyper::Request<hyper::body::Incoming>, log: &mut Map<String, Value>) -> Result<hyper::Response<WasiBody>> {
        log.insert("kind".into(), "guest".into());
        let t = Instant::now();
        let guest = self.guest().await?;
        log.insert("guest_us".into(), us(t).into()); // ~0 once loaded; the whole cold path on the request that loads
        let t = Instant::now();
        let (res, instantiate_us) = guest.call(req.map(|b| b.map_err(Into::into).boxed_unsync())).await?;
        log.insert("instantiate_us".into(), instantiate_us.into()); // 0 when a warm instance served the request
        log.insert("handle_us".into(), us(t).into());
        Ok(res)
    }
}

fn reply(status: u16, body: &str) -> hyper::Response<WasiBody> {
    let body = Full::new(Bytes::from(body.to_string())).map_err(|e| match e {}).boxed_unsync();
    hyper::Response::builder().status(status).body(body).expect("static response")
}

/// Passes the body through and logs the request line when it is dropped.
struct Logged { inner: WasiBody, t0: Instant, log: Option<Map<String, Value>> }
impl hyper::body::Body for Logged {
    type Data = Bytes;
    type Error = wasmtime_wasi_http::Error;
    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool { self.inner.is_end_stream() }
    fn size_hint(&self) -> hyper::body::SizeHint { self.inner.size_hint() }
}
impl Drop for Logged {
    fn drop(&mut self) {
        if let Some(mut log) = self.log.take() {
            log.insert("total_us".into(), us(self.t0).into());
            emit(Value::Object(log));
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let start = Instant::now();
    let pooling = env("SPINIT_ALLOCATOR").as_deref() == Some("pooling");
    let target = env("SPINIT_TARGET");

    if std::env::args().nth(1).as_deref() == Some("precompile") {
        let (input, output) = (std::env::args().nth(2).context("usage: precompile <in.wasm> <out.cwasm>")?, std::env::args().nth(3).context("missing <out.cwasm>")?);
        emit(component::precompile(&engine(pooling, target.as_deref())?, &input, &output)?);
        return Ok(());
    }

    let engine = engine(pooling, target.as_deref())?;
    let engine_us = us(start);
    let t = Instant::now();
    let linker = linker(&engine)?;
    let linker_us = us(t);
    // The epoch ticker. One sleep per tick (not an interval), so a frozen Lambda environment waking up never replays missed ticks.
    let ticker = engine.clone();
    tokio::spawn(async move { loop { tokio::time::sleep(guest::EPOCH_TICK).await; ticker.increment_epoch(); } });

    let bucket = bucket()?;
    let table = env("SPINIT_TABLE");
    let source = Source {
        spec: env("SPINIT_COMPONENT").context("SPINIT_COMPONENT is not set")?,
        bucket: bucket.clone(),
        cache: env("SPINIT_CACHE_DIR").unwrap_or("/tmp/spinit-cache".into()).into(),
        precompiled: env("SPINIT_PRECOMPILED").as_deref() == Some("1"),
    };
    let server = Arc::new(Server { engine, linker, source, guest: OnceCell::new(), bench: Bench::new(bucket, table), seq: AtomicU64::new(0) });
    let eager = env("SPINIT_EAGER").as_deref() == Some("1");
    if eager { server.guest().await?; }

    let addr = env("SPINIT_ADDR").unwrap_or("127.0.0.1:8080".into());
    let listener = TcpListener::bind(&addr).await?;
    emit(json!({ "event": "init", "addr": addr, "allocator": if pooling { "pooling" } else { "default" }, "eager": eager,
        "target": target, "wasmtime": component::WASMTIME, "engine_us": engine_us, "linker_us": linker_us, "init_total_us": us(start), "rss_kb": rss_kb() }));
    loop {
        let (stream, _) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(|req| { let server = server.clone(); async move { Ok::<_, std::convert::Infallible>(server.handle(req).await) } });
            if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await { eprintln!("connection: {e:?}"); }
        });
    }
}
