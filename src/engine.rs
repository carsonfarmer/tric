//! The Wasmtime engine, and apps on it: a fresh store per request, under hard limits: 10 s, 256 MiB, 32 MiB copied in
//! by one host call, nothing inherited, and outbound HTTP only to the hosts the app allows.
use crate::kv::{Imports, Kv};
use crate::outbound::{Allow, Outbound};
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes};
use object_store::ObjectStore;
use std::task::{Context, Poll};
use std::{collections::BTreeMap, pin::Pin, sync::Arc, thread, time::Duration, time::Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Sleep, sleep, timeout_at};
use tracing::Instrument;
use wasmtime::component::{Component, GuestTaskId, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, ResourceLimiter, Result, Store, StoreContextMut};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p2::pipe::MemoryOutputPipe};
use wasmtime_wasi_config::{WasiConfig, WasiConfigVariables};
use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Response, ShouldAccept};
use wasmtime_wasi_http::handler::{WorkerExpiration, WorkerState, WorkerStatus};
use wasmtime_wasi_http::{Error, WasiHttpCtx, WasiHttpCtxView, WasiHttpView, p2, p3};

/// One epoch tick. A guest yields to the scheduler once per tick, so this bounds how long a runaway delays others.
const TICK: Duration = Duration::from_millis(10);
const TIMEOUT: Duration = Duration::from_secs(10); // from the start of instantiation to the end of the request
const MEMORY: usize = 256 << 20; // all linear memories of a store together
const MEMORIES: usize = 4; // per store, as are the next two
const INSTANCES: usize = 16;
const TABLES: usize = 16;
const TABLE_MAX: usize = 100_000; // elements in one table
const LOG_CAP: usize = 64 << 10; // per stream, per request
const MAX_INFLIGHT: usize = 64;
const RESOURCES: usize = 256; // live resources per store; the default is a million
const HOSTCALL_FUEL: usize = 32 << 20; // bytes one host call may copy out of the guest; the default is 128 MiB

/// Compiles components, and holds the object store that all of their state lives in. Make one per process and load every
/// app from it.
pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<Host>,
    store: Arc<dyn ObjectStore>,
}

impl Engine {
    /// Configures Wasmtime and starts the thread that ticks its epoch until the engine and its apps are dropped.
    pub fn new(store: Arc<dyn ObjectStore>) -> Result<Self> {
        let mut cfg = Config::new();
        cfg.wasm_component_model_async(true).wasm_component_model_async_stackful(true);
        cfg.wasm_component_model_more_async_builtins(true).epoch_interruption(true);
        let engine = wasmtime::Engine::new(&cfg)?;
        // An OS thread, not a tokio task: a task would not run once every worker thread is busy with a guest.
        let weak = engine.weak();
        thread::spawn(move || {
            while weak.upgrade().map(|e| e.increment_epoch()).is_some() {
                thread::sleep(TICK);
            }
        });
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        p2::add_only_http_to_linker_async(&mut linker)?;
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        p3::add_to_linker(&mut linker)?;
        wasmtime_wasi_config::add_to_linker(&mut linker, |h: &mut Host| WasiConfig::from(&h.app.config))?;
        Imports::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |h| h)?; // `wasi:keyvalue`
        Ok(Self { engine, linker, store })
    }

    /// Loads the component `wasm` (binary, or WAT text) as the app `name`, with `config` for its `wasi:config` and its
    /// `wasi:keyvalue` data under the prefix `kv`, like `kv/team/app`. It exports either `wasi:http/handler` (p3) or
    /// `incoming-handler` (p2). Its outbound HTTP goes only to `allowed`, items like `https://api.example.com` or
    /// `https://*.example.com:8443`, and none at all if that is empty.
    pub fn load(
        &self,
        name: &str,
        kv: &str,
        wasm: impl AsRef<[u8]>,
        config: BTreeMap<String, String>,
        allowed: &[String],
    ) -> Result<App> {
        let allow = allowed.iter().map(|a| Allow::parse(a)).collect::<Result<_, _>>().map_err(wasmtime::Error::msg)?;
        let pre = self.linker.instantiate_pre(&Component::new(&self.engine, wasm)?)?;
        let pre = match p3::bindings::ServiceIndices::new(&pre) {
            Ok(_) => ProxyPre::P3(p3::bindings::ServicePre::new(pre)?),
            Err(_) => ProxyPre::P2(p2::bindings::ProxyPre::new(pre)?),
        };
        let (kv, config) = (Kv::new(self.store.clone(), kv), config.into_iter().collect());
        let app = Arc::new(Shared { name: name.into(), config, kv, allow });
        let permits = Arc::new(Semaphore::new(MAX_INFLIGHT));
        Ok(App(ProxyHandler::new(State { engine: self.engine.clone(), pre, app, permits })))
    }
}

/// What every store of one app shares, so a request costs one reference count, not a copy.
pub(crate) struct Shared {
    name: String,
    config: WasiConfigVariables,
    pub(crate) kv: Kv,
    pub(crate) allow: Vec<Allow>,
}

/// Per-store state.
pub(crate) struct Host {
    pub(crate) table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: Outbound,
    pub(crate) app: Arc<Shared>,
    memory: usize, // linear memory in use
}
impl ResourceLimiter for Host {
    fn memory_growing(&mut self, current: usize, desired: usize, _: Option<usize>) -> Result<bool> {
        let total = self.memory - current + desired;
        Ok((total <= MEMORY).then(|| self.memory = total).is_some())
    }
    fn table_growing(&mut self, _: usize, desired: usize, _: Option<usize>) -> Result<bool> {
        Ok(desired <= TABLE_MAX)
    }
    fn instances(&self) -> usize {
        INSTANCES
    }
    fn tables(&self) -> usize {
        TABLES
    }
    fn memories(&self) -> usize {
        MEMORIES
    }
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: &mut self.hooks }
    }
}

/// The boxed futures `WasiHttpHooks` deals in.
pub(crate) type Fut<T> = Box<dyn Future<Output = Result<T, Error>> + Send>;

/// Expires the worker, which drops its store, at `TIMEOUT`, even if the guest is only waiting.
struct Deadline(Pin<Box<Sleep>>);
impl WorkerExpiration for Deadline {
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>, _: WorkerStatus, _: Instant) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

/// What outlives the store: its output pipes and its slot among the in-flight requests.
struct Worker {
    out: MemoryOutputPipe,
    err: MemoryOutputPipe,
    _permit: OwnedSemaphorePermit,
}
type Wait = Pin<Box<dyn Future<Output = ()> + Send + Sync>>;
impl WorkerState for Worker {
    type StoreData = Host;
    type RequestData = ();
    fn should_accept_request(&self, _: usize, _: usize) -> ShouldAccept {
        ShouldAccept::Never // one request per store
    }
    fn on_request_start(&self, _: StoreContextMut<Host>, _: (), _: GuestTaskId) -> Wait {
        Box::pin(std::future::pending()) // `Deadline` enforces the timeout
    }
    /// Runs once the worker is done, also after a trap or a timeout, when `result` is the cause.
    fn drop(&self, store: Store<Host>, result: Result<()>) {
        let (app, out, err) = (&store.data().app.name, self.out.contents(), self.err.contents());
        _ = result.inspect_err(|e| tracing::warn!(app, "guest failed: {e}"));
        if !out.is_empty() {
            tracing::info!(app, stream = "stdout", "{}", String::from_utf8_lossy(&out).trim_end());
        }
        if !err.is_empty() {
            tracing::warn!(app, stream = "stderr", "{}", String::from_utf8_lossy(&err).trim_end());
        }
    }
}

/// An app that serves HTTP, one fresh instance per request. Clones share the component and the in-flight limit.
#[derive(Clone)]
pub struct App(ProxyHandler<State>);
impl App {
    /// Serves one request. A guest that traps, times out or hits a limit before it responds gives an empty 500 and a
    /// log line with the cause; the guest's own output is logged too. Waits while 64 requests are already in flight.
    /// The 10 s deadline also cuts a body that is still streaming, so read it to the end promptly.
    pub async fn handle<B>(&self, req: hyper::Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Error>,
    {
        // A span, so a filter like `warn,[request{app=NAME}]=info` turns on one app's request lines.
        let span = tracing::info_span!("request", app = self.0.state().app.name);
        span.in_scope(|| tracing::info!(method = %req.method(), path = req.uri().path()));
        let res = self.0.handle((), req.map(|b| b.map_err(Into::into).boxed_unsync())).instrument(span.clone()).await;
        res.unwrap_or_else(|e| {
            span.in_scope(|| tracing::warn!("request failed: {e}"));
            hyper::Response::builder().status(500).body(Default::default()).unwrap() // an empty body
        })
    }
}

struct State {
    engine: wasmtime::Engine,
    pre: ProxyPre<Host>,
    app: Arc<Shared>,
    permits: Arc<Semaphore>,
}
impl HandlerState for State {
    type StoreData = Host;
    type WorkerExpiration = Deadline;
    type WorkerState = Worker;
    async fn instantiate(&self) -> Result<Instance<Host, Deadline, Worker>> {
        let _permit = self.permits.clone().acquire_owned().await?;
        let expiration = Deadline(Box::pin(sleep(TIMEOUT)));
        let (out, err) = (MemoryOutputPipe::new(LOG_CAP), MemoryOutputPipe::new(LOG_CAP));
        let mut host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().stdout(out.clone()).stderr(err.clone()).build(), // nothing else is granted
            http: WasiHttpCtx::new(),
            hooks: Outbound(self.app.clone()),
            app: self.app.clone(),
            memory: 0,
        };
        host.table.set_max_capacity(RESOURCES);
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| h);
        store.set_hostcall_fuel(HOSTCALL_FUEL);
        // `serve` is concurrent, so yield at every tick instead of trapping, and let `Deadline` end the request.
        store.epoch_deadline_async_yield_and_update(1);
        let proxy = timeout_at(expiration.0.deadline(), self.pre.instantiate_async(&mut store)).await??;
        Ok(Instance { store, proxy, view: Host::http, expiration, state: Worker { out, err, _permit } })
    }
}
