//! Runs each request in a fresh store, under hard limits: 10 s, 256 MiB, nothing inherited, no outbound HTTP.
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes};
use std::task::{Context, Poll};
use std::{collections::BTreeMap, pin::Pin, sync::Arc, time::Duration, time::Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Sleep, sleep, timeout_at};
use wasmtime::component::{GuestTaskId, ResourceTable};
use wasmtime::{Engine, ResourceLimiter, Store, StoreContextMut};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p2::pipe::MemoryOutputPipe};
use wasmtime_wasi_config::WasiConfigVariables;
use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Request, Response, ShouldAccept};
use wasmtime_wasi_http::handler::{WorkerExpiration, WorkerState, WorkerStatus};
use wasmtime_wasi_http::{Error, RequestOptions, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

const TIMEOUT: Duration = Duration::from_secs(10); // from the start of instantiation to the end of the request
const MEMORY: usize = 256 << 20; // all linear memories of a store together
const LOG_CAP: usize = 64 << 10; // per stream, per request
const MAX_INFLIGHT: usize = 64;
const RESOURCES: usize = 256; // live resources per store; the default is a million

/// What every store of one app shares, so a request costs one reference count, not a copy.
pub(crate) struct Shared {
    name: String,
    pub(crate) config: WasiConfigVariables,
}

/// Per-store state.
pub(crate) struct Host {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: Deny,
    pub(crate) app: Arc<Shared>,
    memory: usize, // linear memory in use
}
impl ResourceLimiter for Host {
    fn memory_growing(&mut self, current: usize, desired: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        let total = self.memory - current + desired;
        let fits = total <= MEMORY;
        if fits {
            self.memory = total;
        }
        Ok(fits)
    }
    fn table_growing(&mut self, _: usize, desired: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= 100_000)
    }
    fn instances(&self) -> usize {
        16
    }
    fn tables(&self) -> usize {
        16
    }
    fn memories(&self) -> usize {
        4
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

/// No outbound HTTP yet; M2's allow list goes in this `send_request`. Never `default_hooks()`: it ignores the socket checks.
struct Deny;
type Fut<T> = Box<dyn Future<Output = Result<T, Error>> + Send>;
impl WasiHttpHooks for Deny {
    fn send_request(&mut self, _: Request, _: Option<RequestOptions>, _: Fut<()>) -> Fut<(Response, Fut<()>)> {
        Box::new(async { Err(Error::HttpRequestDenied) })
    }
}

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
    fn drop(&self, store: Store<Host>, result: wasmtime::Result<()>) {
        let (app, out, err) = (&store.data().app.name, self.out.contents(), self.err.contents());
        if let Err(e) = result {
            tracing::warn!(app = %app, "guest failed: {e}");
        }
        if !out.is_empty() {
            tracing::info!(app = %app, stream = "stdout", "{}", String::from_utf8_lossy(&out).trim_end());
        }
        if !err.is_empty() {
            tracing::warn!(app = %app, stream = "stderr", "{}", String::from_utf8_lossy(&err).trim_end());
        }
    }
}

/// An app that serves HTTP, one fresh instance per request. Clones share the component and the in-flight limit.
#[derive(Clone)]
pub struct App(ProxyHandler<State>);
impl App {
    pub(crate) fn new(name: &str, engine: Engine, pre: ProxyPre<Host>, config: BTreeMap<String, String>) -> Self {
        let app = Arc::new(Shared { name: name.into(), config: config.into_iter().collect() });
        Self(ProxyHandler::new(State { engine, pre, app, permits: Arc::new(Semaphore::new(MAX_INFLIGHT)) }))
    }

    /// Serves one request. A guest that traps, times out or hits a limit before it responds gives an empty 500 and a
    /// log line with the cause; the guest's own output is logged too. Waits while 64 requests are already in flight.
    /// The 10 s deadline also cuts a body that is still streaming, so read it to the end promptly.
    pub async fn handle<B>(&self, req: hyper::Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Error>,
    {
        self.0.handle((), req.map(|b| b.map_err(Into::into).boxed_unsync())).await.unwrap_or_else(|e| {
            tracing::warn!(app = %self.0.state().app.name, "request failed: {e}");
            let mut res = Response::default(); // an empty body
            *res.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            res
        })
    }
}

struct State {
    engine: Engine,
    pre: ProxyPre<Host>,
    app: Arc<Shared>,
    permits: Arc<Semaphore>,
}
impl HandlerState for State {
    type StoreData = Host;
    type WorkerExpiration = Deadline;
    type WorkerState = Worker;
    async fn instantiate(&self) -> wasmtime::Result<Instance<Host, Deadline, Worker>> {
        let _permit = self.permits.clone().acquire_owned().await?;
        let expiration = Deadline(Box::pin(sleep(TIMEOUT)));
        let (out, err) = (MemoryOutputPipe::new(LOG_CAP), MemoryOutputPipe::new(LOG_CAP));
        let mut host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().stdout(out.clone()).stderr(err.clone()).build(), // nothing else is granted
            http: WasiHttpCtx::new(),
            hooks: Deny,
            app: self.app.clone(),
            memory: 0,
        };
        host.table.set_max_capacity(RESOURCES);
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| h);
        // `serve` is concurrent, so yield at every tick instead of trapping, and let `Deadline` end the request.
        store.epoch_deadline_async_yield_and_update(1);
        let proxy = timeout_at(expiration.0.deadline(), self.pre.instantiate_async(&mut store)).await??;
        Ok(Instance { store, proxy, view: Host::http, expiration, state: Worker { out, err, _permit } })
    }
}
