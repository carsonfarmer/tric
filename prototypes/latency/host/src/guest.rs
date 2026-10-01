//! Dispatch of wasi:http requests (p2 `incoming-handler` and p3 `handler`) to one component, built on
//! `wasmtime-wasi-http`'s `ProxyHandler`. Enforces the install-wide limits: a 10 s request timeout and a 256 MiB memory cap.
use anyhow::Result;
use std::{pin::Pin, sync::{Arc, atomic::{AtomicU64, Ordering::Relaxed}}, task::{Context, Poll}, time::{Duration, Instant}};
use tokio::sync::Semaphore;
use wasmtime::{Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder, component::{GuestTaskId, ResourceTable}};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView, default_hooks};
use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Request, Response, ShouldAccept, WorkerExpiration, WorkerState, WorkerStatus};

pub const MEMORY_LIMIT: usize = 256 << 20;
const TIMEOUT: Duration = Duration::from_secs(10);
/// The engine epoch advances once per tick; a request may run for `TIMEOUT / EPOCH_TICK` ticks.
pub const EPOCH_TICK: Duration = Duration::from_millis(100);
const DEADLINE_TICKS: u64 = TIMEOUT.as_millis() as u64 / EPOCH_TICK.as_millis() as u64;
const MAX_INFLIGHT: usize = 64;

/// Per-store state.
pub struct Host { table: ResourceTable, wasi: WasiCtx, http: WasiHttpCtx, limits: StoreLimits }
impl Host {
    fn new() -> Self {
        let limits = StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).build();
        Self { table: ResourceTable::new(), wasi: WasiCtx::builder().build(), http: WasiHttpCtx::new(), limits }
    }
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx: &mut self.wasi, table: &mut self.table } }
}
impl WasiHttpView for Host {
    // Outbound `wasi:http` is unrestricted in the spike; the allow list (Q34) is a product concern.
    fn http(&mut self) -> WasiHttpCtxView<'_> { WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: default_hooks() } }
}

struct App { engine: Engine, pre: ProxyPre<Host> }
struct Worker { p3: bool, instantiate_us: AtomicU64 }
struct Never;
impl WorkerExpiration for Never {
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>, _: WorkerStatus, _: Instant) -> Poll<()> { Poll::Pending }
}
impl WorkerState for Worker {
    type StoreData = Host;
    /// Receives the instantiate time this request paid for (0 when it reused a warm instance).
    type RequestData = Arc<AtomicU64>;
    fn should_accept_request(&self, concurrent: usize, total: usize) -> ShouldAccept {
        let (max_total, max_concurrent) = if self.p3 { (128, 16) } else { (1, 1) }; // p3 instances are reused, p2 ones are not
        if total >= max_total { ShouldAccept::Never } else if concurrent >= max_concurrent { ShouldAccept::No } else { ShouldAccept::Yes }
    }
    fn on_request_start(&self, mut store: StoreContextMut<Host>, paid: Self::RequestData, _: GuestTaskId)
        -> Pin<Box<dyn Future<Output = ()> + Send + Sync>> {
        paid.store(self.instantiate_us.swap(0, Relaxed), Relaxed);
        store.set_epoch_deadline(DEADLINE_TICKS); // traps a guest that computes past the deadline...
        Box::pin(tokio::time::sleep(TIMEOUT))      // ...and this expires one that is merely waiting.
    }
    fn drop(&self, _store: Store<Host>, result: wasmtime::Result<()>) {
        if let Err(e) = result { eprintln!("worker failed: {e:?}"); }
    }
}
impl HandlerState for App {
    type StoreData = Host;
    type WorkerExpiration = Never;
    type WorkerState = Worker;
    async fn instantiate(&self) -> wasmtime::Result<Instance<Host, Never, Worker>> {
        let t = Instant::now();
        let mut store = Store::new(&self.engine, Host::new());
        store.limiter(|h| &mut h.limits);
        store.set_epoch_deadline(DEADLINE_TICKS);
        let proxy = self.pre.instantiate_async(&mut store).await?;
        let state = Worker { p3: matches!(self.pre, ProxyPre::P3(_)), instantiate_us: (t.elapsed().as_micros() as u64).max(1).into() };
        Ok(Instance { store, proxy, view: Host::http, expiration: Never, state })
    }
}

pub struct Guest { handler: ProxyHandler<App>, permits: Semaphore }
impl Guest {
    pub fn new(engine: Engine, pre: ProxyPre<Host>) -> Self {
        Self { handler: ProxyHandler::new(App { engine, pre }), permits: Semaphore::new(MAX_INFLIGHT) }
    }

    /// Handles one request. Also returns the instantiate time (µs) the request paid for, 0 if it reused an instance.
    pub async fn call(&self, req: Request) -> Result<(Response, u64)> {
        let _permit = self.permits.acquire().await?;
        let paid = Arc::new(AtomicU64::new(0));
        let res = self.handler.handle(paid.clone(), req).await?;
        Ok((res, paid.load(Relaxed)))
    }
}
