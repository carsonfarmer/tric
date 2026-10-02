# Embedding Wasmtime for torpor M1 (fact sheet)

> Gathered 2026-10-01 by a research sub-agent from the crates.io API, the unpacked `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config` 49.0.1 crates in the Cargo registry, the `wasmtime` v49.0.1 git tag (`src/commands/serve.rs`), and by running a probe host against two Rust guests (p2 and p3, built with spin-sdk 7).
> All runs: one Apple-silicon laptop (macOS), host built with Rust 2024 edition, `cargo build --release` for timings, no Docker. Items marked UNTESTED were read from source only. Line numbers are in the 49.0.1 crates unless a file is named `serve.rs`.
> Harness: the probe lived in the session scratchpad (`embedding-probe/{host,clean,guests}`), not in the repo. The sketches in section 9 are the `clean/` copy, which compiles with no warnings and passed the checks in sections 4 to 7. Nothing was committed.

## Bottom line

**Recommendation: pin `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config` to `=49.0.1` (the latest release today and the spike's version). Serve through `wasmtime_wasi_http::handler::ProxyHandler` with one store per request: a host that does everything M1 needs is about 170 lines (engine 40, guest 92, dev-mode main 40).**

| # | Point | Evidence |
|---|---|---|
| 1 | 49.0.1 (2026-09-24) is the latest on crates.io for all four crates. There are no releases after it, so nothing to adopt. It carries the fix for GHSA-c9gc-w9vx-w86p (host memory exhaustion on an outgoing HTTP body write), a reason to be on it. | Section 1 |
| 2 | One API serves p2 and p3: `ProxyPre::{P2,P3}` plus `ProxyHandler<S>`. The host implements three small traits: `HandlerState` (build a store), `WorkerState` (what outlives the store), `WorkerExpiration` (a deadline). `ShouldAccept::Never` gives one store per request. | Sections 2, 9 |
| 3 | `WasiCtx::builder().build()` grants nothing. Verified in the guest: no env, no args, no files, TCP "Permission denied". `.stdout(pipe.clone()).stderr(pipe.clone())` is the only call needed. | Sections 3, 4 |
| 4 | `MemoryOutputPipe::new(64 KiB)` captures output per request. It survives a trap, and `WorkerState::drop` receives the real error. On overflow the guest gets a write error (host does not trap). | Section 4 |
| 5 | `StoreLimitsBuilder::memory_size` caps **each** linear memory, not the store. With `.memories(4)` the worst case is 4 x 256 MiB. A 12-line `ResourceLimiter` gives a true store-wide cap. | Section 5 |
| 6 | **Flag for the orchestrator (conflicts with Q57).** The epoch trap alone does not bound a guest that is waiting (a `/sleep` guest hung past 20 s), and a runaway pins an executor thread for 10 s (8.7 s delay for a normal request with 14 runaways on 14 cores). A `WorkerExpiration` deadline plus `epoch_deadline_async_yield_and_update(1)` fixes both for p2 and p3 at about 3 more lines. The plain trap is one line (`set_epoch_deadline(100)`). | Section 6 |
| 7 | Do not use `default_hooks()` for outbound HTTP: it connects with `tokio::TcpStream` and bypasses the socket check. A 5-line `Deny` hook returns `HttpRequestDenied` for p2 and p3, and is where M2's allow list goes. | Section 7 |
| 8 | A trap or timeout poisons only that request's store. The next request returned 200 in about 1 ms in every run, p2 and p3. The host must turn the handler's `Err` into a 500 itself, or hyper just closes the connection. | Sections 6, 8 |

## 1. Version

| Item | Finding | Evidence |
|---|---|---|
| Latest release | `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http` 49.0.1, 2026-09-24. `wasmtime-wasi-config` 49.0.1, same day. 49.0.0 was 2026-09-21. The same day also saw 48.0.3 and 36.0.16 backports. | crates.io API `max_version` for each crate, fetched 2026-10-01 |
| Since 49.0.1 | Nothing released. The `main` branch (50.0.0-dev) has `--listenfd` for `wasmtime serve`, removal of the listenfd WASI option, and a borrowed-`Resource` fix. None changes the embedding API used here. Watch list only. | Commits on wasmtime `main` after the v49.0.1 tag (read only) |
| Security | 49.0.1 includes the fix for GHSA-c9gc-w9vx-w86p (host memory exhaustion when a guest writes an outgoing HTTP body). | Advisory list for `wasmtime-wasi-http` |
| Regressions | None seen. All probes ran on 49.0.1, p2 and p3, including trap, timeout and flood cases. | Sections 4 to 7 |
| API at this version | `wasmtime_wasi_http::handler` has `ProxyPre` (line 46), `Proxy` (72), `WorkerStatus` (125), `WorkerExpiration` (145), `WorkerState` (171), `HandlerState` (260), `ProxyHandler::new` (844) and `handle` (886). The module needs `wasmtime-wasi-http`'s `component-model-async` feature. | `wasmtime-wasi-http-49.0.1/src/handler.rs` |
| Outbound hooks | `WasiHttpHooks` (ctx.rs:205-387) replaced the old per-view `send_request`. One hook serves p2 (`p2/http_impl.rs:101`) and p3 (`p3/host/handler.rs:87`). | `wasmtime-wasi-http-49.0.1/src/ctx.rs` |
| Pin style | `=49.0.1` on all four crates. Wasmtime ships breaking changes in every major, and these crates must match each other. | Spike `Cargo.lock` has the same versions |

Dependencies for M1 (probe `Cargo.toml`, 11 lines): `anyhow`, `http`, `http-body-util`, `hyper` (server, http1), `tokio` (rt-multi-thread, macros, net, time, sync), `tracing`, `tracing-subscriber` (json), `wasmtime = "=49.0.1"` (default features), `wasmtime-wasi = { default-features = false, features = ["p2", "p3"] }` (drops p1 and wiggle), `wasmtime-wasi-http = { features = ["component-model-async"] }`, `wasmtime-wasi-config` (M2). Do not disable the default features of `wasmtime`: its `anyhow` feature makes `wasmtime::Error` an `anyhow::Error`, and without it `?` into `anyhow::Result` fails to compile. Release binary with `lto = true`, `strip = true`: 17.6 MB.

## 2. What `wasmtime serve` does, and what torpor needs

`src/commands/serve.rs` at v49.0.1 (1394 lines). The embedder needs the parts marked "keep"; a one-store-per-request host drops the rest.

| Part | `serve.rs` | torpor |
|---|---|---|
| `Host` struct with `WasiCtx`, `ResourceTable`, `WasiHttpCtx`, limits | 51-57, `WasiView` 74, `WasiHttpView` 83 | **Keep** (guest.rs `Host`) |
| `new_store`: `WasiCtxBuilder`, `LogStream` for stdout and stderr, `table.set_max_capacity`, `StoreLimits::default()`, wasi-config vars | 353-450 | **Keep the idea**: pipes instead of `LogStream`, real limits instead of default, no inherit |
| `add_to_linker`: p2 `add_only_http_to_linker_async` and p3 `add_to_linker`, gated by `-Scli` | 452-532 | **Keep** both calls, drop the gating (engine.rs `linker`) |
| `Engine` setup, `epoch_interruption` only if a timeout is set | 533-576 | **Keep**: always on |
| `ServicePre` (p3) tried first, then `ProxyPre` (p2) | 587-589 | **Keep** (engine.rs `load`) |
| `EpochThread`: an OS thread calling `increment_epoch` | 638-647, 953-985 | **Keep** as a 3-line thread |
| `HostWorkerExpiration` (idle and request timeouts) | 759-795 | **Keep** as a 4-line `Deadline` |
| `HostWorkerState`: `should_accept_request` (807), `on_request_start` sleeps `request_timeout` (817-828), `drop` (831) | 796-843 | **Keep** with `Never` and `pending()`; use `drop` for logs |
| `HostHandlerState::instantiate` | 870-899 | **Keep** (guest.rs `App::instantiate`) |
| `setup_epoch_handler`: `epoch_deadline_async_yield_and_update(1)` | 987-1010 | **Keep** (2 lines) |
| `http1::Builder` and `TokioIo` in `handle_client`; `set_nodelay(true)` | 1090, 712 | **Keep** (main.rs) |
| Instance reuse counts (128 p3, 16 concurrent), `Semaphore` for connections | 47-49, 650-660, 677 | **Drop** (one store per request). Keep one `Semaphore` for in-flight requests |
| `GracefulShutdown`, guest profiler and debugger, header injection | 900-946, 1013-1078, 1137 | **Drop** |
| wasi-nn, keyvalue, `-Scli` gating | 452-532 | **Drop** |
| `LogStream` (line-buffered stdout and stderr to the host's stdio) | 1249-1355 | **Drop**: `MemoryOutputPipe` captures per request |

## 3. `WasiCtxBuilder::new()` defaults

All deny. `WasiCtx::builder()` is `WasiCtxBuilder::new()`.

| Capability | Default | Evidence |
|---|---|---|
| stdin, stdout, stderr | `empty()` (reads return EOF, writes discarded) | `wasmtime-wasi-49.0.1/src/ctx.rs:37-66`, setters at 83-98 |
| Environment, args, preopens | none | ctx.rs:37-66 |
| Wall and monotonic clock, random | real host clocks, host RNG | `clocks.rs:55-62`, `random.rs:48-73` |
| TCP, UDP, `ip-name-lookup` | all `false`; socket address check `false` | `sockets/mod.rs:66-166`. The doc comment at ctx.rs:59 says "TCP/UDP are allowed", which is wrong in effect: both are off. |
| Outbound `wasi:http` | **Not part of `WasiCtx`.** Controlled by the `WasiHttpHooks` you pass (section 7) | `wasmtime-wasi-http-49.0.1/src/ctx.rs:98-110` |
| `build()` twice | panics | ctx.rs:442 |

**The minimal call that grants nothing (Q55) is `WasiCtx::builder().build()`; torpor adds only the two capture pipes.** Nothing needs `inherit_*`. Do not call `inherit_env`, `inherit_network`, `inherit_stdio` or `preopened_dir`. Also verified from inside a p2 and a p3 guest:

| Probe | Result |
|---|---|
| env, args, cwd, `read_dir("/")` | `env=[] args=[] cwd=Some("/") read_dir("/")=Err("No such file or directory")` |
| `TcpStream::connect("1.1.1.1:80")` | `Permission denied` |
| Outbound `wasi:http` with the `Deny` hook | `ErrorCode::HttpRequestDenied`, guest sees it and returns normally |

## 4. Capturing stdout and stderr

| Question | Answer | Evidence |
|---|---|---|
| Type | `wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(cap)`. `Clone` (shared `Arc<Mutex<BytesMut>>`), `.contents()` returns `Bytes`. | `p2/pipe.rs:74-147` |
| Works for p3 | It implements `StdoutStream`, which both p2 and p3 consume. | `cli/mem.rs:27`, `p3/cli/host.rs:149-152,250-290` |
| Wire-up | `.stdout(out.clone()).stderr(err.clone())` on the builder, keep the other clones in the `WorkerState`. | guest.rs `instantiate` |
| Read back | In `WorkerState::drop(&self, store, result)`. It runs once per worker, after a trap or timeout too, with `result` carrying the cause. | handler.rs:231, dropper at 448-462 and 742 |
| After a trap (verified) | `result = Err(wasm trap: unreachable)`, stdout `before-trap\n`, stderr `err-before-trap\n`, p2 and p3. | probe `/trap-print` |
| Overflow (verified) | A guest writing 2 MiB captured exactly 64 KiB. No host trap, `result = Ok`. The guest sees an error: p2 `StreamError::Closed` (so Rust `println!` panics), p3 stream closed. The host only traps if a guest ignores the `check_write` budget. | probe `/flood`, `p2/pipe.rs:74-147` |
| Cap | 64 KiB per stream per request in the sketch. At most `MAX_INFLIGHT` x 128 KiB of log memory at once. | guest.rs `LOG_CAP` |

An overflowing guest sees a write error, so a chatty app may panic with a 500. State this in the app docs, or raise the cap.

## 5. Limits

| Limit | Value | Notes |
|---|---|---|
| `memory_size` | 256 MiB | **Per linear memory**, not per store (`limits.rs:230-241`). Worst case is `memories` x 256 MiB. |
| `instances`, `tables`, `memories` | defaults 10,000 each | Measured minimum for a Rust p2 and p3 hello (spin-sdk): instances 3, tables 2, memories 1. Instances 1 or 2 and tables 1 gave a 500. Suggested: `.instances(16).tables(16).memories(4)`. |
| `table_elements` | unlimited by default | Suggested `.table_elements(100_000)`. |
| `trap_on_grow_failure` | `false` by default | `memory.grow` returns -1, the guest handles it (Rust aborts on OOM). Keep the default. |
| `ResourceTable` | default `max_capacity` 1,000,000 | `runtime/component/resource_table.rs:70-74`, setter at 140. No change needed. |
| Hook-up | `store.limiter(\|h\| &mut h.limits);` | One line in `instantiate` |
| End-to-end (spike) | `/grow` reached 254 MiB, then the allocation failed inside the guest. The host was unaffected. | `prototypes/latency/RESULTS.md:293-302` |

UNTESTED: JavaScript (StarlingMonkey) components may need more instances, tables or memories than the Rust minimum, and composed components (M3) will. Treat 16, 16 and 4 as a starting point, and re-measure when JS and composition land.

Store-wide total, if per-memory is not enough (compiled and ran, about 12 lines): implement `ResourceLimiter` on a struct holding the running total.

```rust
struct Total(usize);
impl ResourceLimiter for Total {
    fn memory_growing(&mut self, current: usize, desired: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        let total = self.0 + desired - current;
        Ok(total <= MEMORY && { self.0 = total; true })
    }
    fn table_growing(&mut self, _: usize, desired: usize, _: Option<usize>) -> wasmtime::Result<bool> { Ok(desired <= 100_000) }
    fn instances(&self) -> usize { 16 }
    fn tables(&self) -> usize { 16 }
    fn memories(&self) -> usize { 4 }
}
```

It is called for the initial memory with `current = 0` (`runtime/vm/memory.rs:346`) and on growth (654).

## 6. Epoch interruption and the 10 s deadline

Who sets it: the **host's `HandlerState::instantiate`**. The guest cannot. The handler API has no deadline setting of its own: `WorkerExpiration` (handler.rs:145-163, documented at 197-232) only decides when to drop the store, and the epoch deadline is set on the `Store` before `Instance` is returned. The outer expiration poll is at handler.rs:697-726.

| Mechanism | Code in `instantiate` | Bounds a computing guest | Bounds a waiting guest | Starvation | Verified |
|---|---|---|---|---|---|
| A. Plain trap (Q57) | `store.set_epoch_deadline(100)` with a 100 ms ticker | Yes, 10.27 s (p2), 10.35 s (p3), `wasm trap: interrupt` | **No.** `/sleep` with no `Deadline` hung past a 20 s client timeout. | A runaway pins its executor thread. 14 runaways on 14 cores delayed a normal request by 8.7 s. | Yes, debug host for timing, release for starvation |
| B. Yield plus `Deadline` (what `wasmtime serve` does, 987-1010) | `store.epoch_deadline_async_yield_and_update(1); store.set_epoch_deadline(1);` plus `Deadline` expiration | Yes, 10.01-10.07 s, `result = Err(guest timed out)` | Yes, a 60 s `/sleep` ended at 10.00 s | Yield every tick. Normal request alongside one runaway: 1.6 ms. With 14 runaways: first request 4.3 s then 0.14-0.34 s at a 100 ms tick, 0.05-0.69 s at a 10 ms tick. | Yes, p2 and p3, also in the final clean host (spin 10.007 s p2, 10.015 s p3; sleep 10.003 s, 10.004 s) |
| C. Hybrid: `epoch_deadline_callback` returning `UpdateDeadline::Yield(1)` for 100 ticks, then `Err(Trap::Interrupt)` | about 4 lines | Yes | No (needs `Deadline` too) | Same as B | UNTESTED |

**Recommendation: B.** `Deadline` is needed in both (Q35's 10 s must hold for a guest that only waits), and B adds one line over A to fix the starvation. A is a one-line swap if the orchestrator wants to stay with Q57: replace the two epoch lines with `store.set_epoch_deadline(TIMEOUT.as_millis() as u64 / TICK.as_millis() as u64);` and set `TICK` back to 100 ms.

| Detail | Finding | Evidence |
|---|---|---|
| Ticker | Must be an OS thread (a tokio task is starved by a runaway guest): `std::thread::spawn(move || loop { std::thread::sleep(TICK); engine.increment_epoch(); })`. In B, `TICK` is the yield granularity: use 10 ms. In A it is the deadline granularity: 100 ms is enough. Cost is negligible. | Spike `RESULTS.md` ticker bug; probes |
| `Deadline` | `struct Deadline(Pin<Box<tokio::time::Sleep>>)` with `WorkerExpiration::poll` forwarding to the sleep, created in `instantiate`. Verified: a 60 s sleeping guest is dropped at 10.0 s and `drop` receives `Err(guest timed out)`. | handler.rs:145-163 |
| `on_request_start` | No default. Return `Box::pin(std::future::pending())` when the deadline lives in `Deadline`. Its future can be starved by a busy guest (wasmtime issues #11869 and #11870, cited at handler.rs:200, 435 and 705), so `Deadline` is the more reliable place. | handler.rs:197-232 |
| Trap scope | A trap, a timeout or a panic poisons only that request's store. The next request returned 200 in about 1 ms, p2 and p3, after trap, `/spin` and `/sleep`. | probe runs, clean host |
| Client disconnect | Does **not** cancel the store: a client that disconnected at 2 s still had its guest reaped at 10 s. Fine with the 10 s cap. | probe, `guest timed out` logged about 8 s later |
| Body streaming | A 10 s `Deadline` caps the whole request including a streamed body. UNTESTED with a trickling body. | Not probed |
| What the host sees | p2: `handle()` returns `Err("guest never invoked `response-outparam::set` method")`. p3: `Err(TrapOrPanicError)`, "worker trapped or panicked". The real cause arrives only in `WorkerState::drop`: `Err(wasm trap: unreachable)`, `Err(wasm trap: interrupt)` or `Err(guest timed out)`. Log the cause there and answer the client a 500 from the `handle()` error. | `ExpirationError` and `TrapOrPanicError`, handler.rs:773-830 |
| Config flags | A p2 guest needs none. The p3 Rust guest needs only `wasm_component_model_async(true)` (without it: "failed to parse WebAssembly module"). `wasmtime-wasi`'s `p3` feature already enables `wasmtime/component-model-async`. The sketch also sets `..._async_stackful` and `..._more_async_builtins` (`wasmtime-49.0.1/src/config.rs:1280-1318`) because other toolchains may need them. UNTESTED for JS. | Probe with `PROBE_FLAGS` |

## 7. Outbound `wasi:http`

`WasiHttpCtxView` needs a `hooks: &mut dyn WasiHttpHooks` (ctx.rs:98-110). The obvious choice, `default_hooks()` (ctx.rs:391-406), is not safe for untrusted guests: its `default_send_request` connects with `tokio::net::TcpStream` directly (`default_send_request.rs:66`), bypassing the socket address check in section 3.

M1 uses a `Deny` hook (guest.rs, 5 lines). The signature is at ctx.rs:247-262 and the error type `wasmtime_wasi_http::Error` at `error.rs`; the result is `ErrorCode::HttpRequestDenied` in both p2 and p3. M2's allow list (Q34, Q37, Q38) goes in the same method, calling `default_send_request` for allowed hosts. Optionally, building `wasmtime-wasi-http` with `default-features = false` makes `send_request` a required method (ctx.rs:289) and drops the rustls dependencies: only for M1. M2 needs the default send.

## 8. Dev-mode server loop

hyper 1 with `http1::Builder::serve_connection(TokioIo::new(stream), service_fn(..))` is the smallest loop (main.rs in section 9). Points that cost a debugging session if missed:

| Point | Why | Evidence |
|---|---|---|
| `req.map(\|b\| b.map_err(Into::into).boxed_unsync())` | Turns hyper's `Incoming` into the handler's `Request` (`http::Request<WasiBody>`) | handler.rs `Request` alias |
| `wasmtime_wasi_http::io::TokioIo` | Re-export, so no `hyper-util` dependency | `lib.rs:25` (`pub mod io`; `handler` is at 24) |
| `stream.set_nodelay(true)` | Avoids a Nagle plus delayed-ACK stall of about 40 ms per response | `serve.rs:712` |
| Map `Err` to a 500 inside `service_fn` | If the service returns `Err`, hyper closes the connection and the client sees a reset | probe |
| `anyhow::Error` | `handle()` returns `wasmtime::Error`, which is `anyhow::Error` with default features | Section 1 |
| Semaphore | `handle()` has no backpressure. The permit frees when `handle()` returns the headers, not when the body ends. | handler.rs:886-932 |

## 9. Assembled sketch (compiles on 49.0.1)

`engine.rs` (40 lines, 32 of code):

```rust
//! The process-wide Wasmtime engine, its epoch ticker and the linker.
use crate::guest::Host;
use anyhow::Result;
use std::time::Duration;
use wasmtime::{Config, Engine, component::{Component, Linker}};
use wasmtime_wasi_config::WasiConfig;
use wasmtime_wasi_http::{handler::ProxyPre, p2, p3};

/// One epoch tick. Every running guest yields to the scheduler once per tick, so this bounds how long a runaway guest can delay others.
pub const TICK: Duration = Duration::from_millis(10);

pub fn new() -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model_async(true).wasm_component_model_async_stackful(true).wasm_component_model_more_async_builtins(true);
    cfg.epoch_interruption(true);
    let engine = Engine::new(&cfg)?;
    // An OS thread, not a tokio task: a task would not run once every worker thread is busy with guests.
    let ticker = engine.clone();
    std::thread::spawn(move || loop { std::thread::sleep(TICK); ticker.increment_epoch(); });
    Ok(engine)
}

fn linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut l = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut l)?;
    p2::add_only_http_to_linker_async(&mut l)?;
    wasmtime_wasi::p3::add_to_linker(&mut l)?;
    p3::add_to_linker(&mut l)?;
    wasmtime_wasi_config::add_to_linker(&mut l, |h: &mut Host| WasiConfig::from(&h.config))?; // M2
    Ok(l)
}

/// Links `component` and picks the HTTP world it exports: `wasi:http/handler` (p3) or `incoming-handler` (p2).
pub fn load(engine: &Engine, component: &Component) -> Result<ProxyPre<Host>> {
    let pre = linker(engine)?.instantiate_pre(component)?;
    Ok(match p3::bindings::ServicePre::new(pre.clone()) {
        Ok(p) => ProxyPre::P3(p),
        Err(_) => ProxyPre::P2(p2::bindings::ProxyPre::new(pre)?),
    })
}
```

`guest.rs` (92 lines, 79 of code; `config` is the M2 wasi:config field, a few lines you can omit for M1):

```rust
//! Runs each request in a fresh store, under hard limits: 10 s, 256 MiB, nothing inherited, no outbound HTTP.
use anyhow::Result;
use std::{pin::Pin, sync::Arc, task::{Context, Poll}, time::{Duration, Instant}};
use tokio::{sync::Semaphore, time::Sleep};
use wasmtime::{Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder, component::{GuestTaskId, ResourceTable}};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p2::pipe::MemoryOutputPipe};
use wasmtime_wasi_config::WasiConfigVariables;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};
use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Request, Response, ShouldAccept, WorkerExpiration, WorkerState, WorkerStatus};

const TIMEOUT: Duration = Duration::from_secs(10);
const MEMORY: usize = 256 << 20;
const LOG_CAP: usize = 64 << 10; // per stream, per request
const MAX_INFLIGHT: usize = 64;

/// Per-store state.
pub struct Host { table: ResourceTable, wasi: WasiCtx, http: WasiHttpCtx, hooks: Deny, pub config: WasiConfigVariables, limits: StoreLimits }
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx: &mut self.wasi, table: &mut self.table } }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> { WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: &mut self.hooks } }
}

/// M1 allows no outbound HTTP. M2's allow list goes in this `send_request`.
struct Deny;
impl WasiHttpHooks for Deny {
    fn send_request(&mut self, _: http::Request<WasiBody>, _: Option<RequestOptions>, _: Box<dyn Future<Output = Result<(), Error>> + Send>)
        -> Box<dyn Future<Output = Result<(http::Response<WasiBody>, Box<dyn Future<Output = Result<(), Error>> + Send>), Error>> + Send> {
        Box::new(async { Err(Error::HttpRequestDenied) })
    }
}

/// Expires the worker, which drops its store, `TIMEOUT` after instantiation, even if the guest is only waiting.
struct Deadline(Pin<Box<Sleep>>);
impl WorkerExpiration for Deadline {
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>, _: WorkerStatus, _: Instant) -> Poll<()> { self.0.as_mut().poll(cx) }
}

/// What outlives the store: the app's name and its stdout and stderr pipes.
struct Worker { app: Arc<str>, out: MemoryOutputPipe, err: MemoryOutputPipe }
impl WorkerState for Worker {
    type StoreData = Host;
    type RequestData = ();
    fn should_accept_request(&self, _: usize, _: usize) -> ShouldAccept { ShouldAccept::Never } // one request per store
    fn on_request_start(&self, _: StoreContextMut<Host>, _: (), _: GuestTaskId) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>> {
        Box::pin(std::future::pending()) // `Deadline` enforces the timeout
    }
    /// Runs once the worker is done, also after a trap or a timeout, when `result` is the error.
    fn drop(&self, _: Store<Host>, result: wasmtime::Result<()>) {
        let (out, err) = (self.out.contents(), self.err.contents());
        if let Err(e) = result { tracing::warn!(app = &*self.app, "guest failed: {e:?}"); }
        if !out.is_empty() { tracing::info!(app = &*self.app, stream = "stdout", "{}", String::from_utf8_lossy(&out)); }
        if !err.is_empty() { tracing::info!(app = &*self.app, stream = "stderr", "{}", String::from_utf8_lossy(&err)); }
    }
}

struct App { name: Arc<str>, engine: Engine, pre: ProxyPre<Host>, config: Vec<(String, String)> }
impl HandlerState for App {
    type StoreData = Host;
    type WorkerExpiration = Deadline;
    type WorkerState = Worker;
    async fn instantiate(&self) -> wasmtime::Result<Instance<Host, Deadline, Worker>> {
        let (out, err) = (MemoryOutputPipe::new(LOG_CAP), MemoryOutputPipe::new(LOG_CAP));
        let host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().stdout(out.clone()).stderr(err.clone()).build(), // nothing else: no env, args, files, sockets or stdin
            http: WasiHttpCtx::new(),
            hooks: Deny,
            config: self.config.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).instances(16).tables(16).memories(4).table_elements(100_000).build(),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| &mut h.limits);
        store.epoch_deadline_async_yield_and_update(1); // a guest yields at every tick, so `Deadline` can fire while it computes
        store.set_epoch_deadline(1);
        let proxy = self.pre.instantiate_async(&mut store).await?;
        let expiration = Deadline(Box::pin(tokio::time::sleep(TIMEOUT)));
        Ok(Instance { store, proxy, view: |h| h.http(), expiration, state: Worker { app: self.name.clone(), out, err } })
    }
}

pub struct Guest { handler: ProxyHandler<App>, permits: Semaphore }
impl Guest {
    pub fn new(name: &str, engine: Engine, pre: ProxyPre<Host>, config: Vec<(String, String)>) -> Self {
        Self { handler: ProxyHandler::new(App { name: name.into(), engine, pre, config }), permits: Semaphore::new(MAX_INFLIGHT) }
    }
    pub async fn call(&self, req: Request) -> Result<Response> {
        let _permit = self.permits.acquire().await?;
        Ok(self.handler.handle((), req).await?)
    }
}
```

`main.rs` (40 lines; the torpor `serve` module replaces the argument parsing with `torpor.toml` and adds app routing):

```rust
//! `probe-clean <component.wasm> [addr]`: serves one component over HTTP/1.
mod engine;
mod guest;

use anyhow::{Context, Result};
use http::{Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use std::{convert::Infallible, sync::Arc};
use tokio::net::TcpListener;
use wasmtime::component::Component;
use wasmtime_wasi_http::io::TokioIo;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().init();
    let mut args = std::env::args().skip(1);
    let path = args.next().context("usage: probe-clean <component.wasm> [addr]")?;
    let engine = engine::new()?;
    let pre = engine::load(&engine, &Component::from_file(&engine, &path)?)?;
    let guest = Arc::new(guest::Guest::new(&path, engine, pre, vec![]));
    let listener = TcpListener::bind(args.next().unwrap_or("127.0.0.1:8080".into())).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?; // otherwise Nagle plus delayed ACK stalls a response by ~40 ms
        let guest = guest.clone();
        tokio::spawn(async move {
            let svc = service_fn(|req: hyper::Request<Incoming>| async {
                let res = guest.call(req.map(|b| b.map_err(Into::into).boxed_unsync())).await;
                Ok::<_, Infallible>(res.unwrap_or_else(|e| {
                    tracing::warn!("request failed: {e}");
                    let mut res = Response::new(Empty::new().map_err(|n| match n {}).boxed_unsync());
                    *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    res
                }))
            });
            if let Err(e) = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await { tracing::debug!("connection: {e}"); }
        });
    }
}
```

## 10. What carries over from the spike host

Spike host: `prototypes/latency/host/src/{main,guest,component,bench}.rs`.

| Spike part | M1 |
|---|---|
| `guest.rs` (86 lines): `Host`, `Never` expiration, `Worker`, `App`, `HandlerState`, `Guest { handler, permits }` | **Carries over almost unchanged.** Changes: `Deadline` replaces `Never`; `Deny` replaces `default_hooks()` (unrestricted outbound); real `StoreLimits`; stdout and stderr pipes; yield epochs. |
| `main.rs`: `engine()` (47-63), `linker()` (65-72), ProxyPre detection (92-95), ticker thread (203-206), accept loop (225-233) | **Carries over.** Ticker 100 ms to 10 ms. |
| Bench routes and `bench.rs` | Drop |
| Pooling allocator, Winch, `SPINIT_COMPILER` | Drop (a default Cranelift engine is enough for M1) |
| All `SPINIT_*` environment-variable config | Drop |
| `object_store`, DynamoDB, blake3, hmac, sha2, zstd dependencies | Drop for M1 (M2 and later) |
| `Logged` JSON body wrapper, `rss_kb`, `mac-bench`, `precompile`, `publish` subcommands | Drop |
| p3 instance reuse (128, 16) and `instantiate_us` accounting | Drop (one store per request) |
| `OnceCell` lazy load | Drop (M1 loads at start; lazy load belongs to the cold-start work) |

## 11. Line budget

| Module | Plan budget | Sketch | Notes |
|---|---|---|---|
| `engine` | about 120 | 40 | Includes the linker, ticker and p2/p3 detection |
| `guest` | about 90 | 92 (79 of code) | Includes the wasi:config field (M2) and log lines. About 80 without them |
| `serve` | about 100 | 40 for the loop | `torpor.toml` parsing and app routing not included |
| Total | about 310 | about 170 | |

## 12. Unverified or open

| Item | Status |
|---|---|
| JS (StarlingMonkey) component needs for async flags, instance, table and memory counts | UNTESTED. Rust only |
| `wasi:config` in a real guest (`wasmtime-wasi-config` 49.0.1, `wasi:config@0.2.0-rc.1`, `get` and `get_all` only) | Compiles and links; no guest tested. `WasiConfigVariables` is a `HashMap` wrapper and **not** `Clone`, so rebuild it per store from the manifest. M2 |
| Mechanism C (hybrid callback) | UNTESTED |
| Trickling request or response body under the 10 s `Deadline` | UNTESTED |
| Q57 versus Mechanism B | Decision needed from the orchestrator (section 6) |
| Semaphore permit release at headers, not body end | By design of `handle()`. Bodies stream after the permit frees; the 10 s `Deadline` still bounds them |
