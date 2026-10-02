//! Runs each request in a fresh store, under hard limits: 10 s, 256 MiB, nothing inherited, no outbound HTTP.
use http_body_util::{BodyExt, Empty};
use hyper::body::{Body, Bytes};
use std::task::{Context, Poll};
use std::{collections::BTreeMap, pin::Pin, sync::Arc, time::Duration, time::Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Sleep, sleep};
use wasmtime::component::{GuestTaskId, ResourceTable};
use wasmtime::{Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p2::pipe::MemoryOutputPipe};
use wasmtime_wasi_config::WasiConfigVariables;
use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Request, Response, ShouldAccept};
use wasmtime_wasi_http::handler::{WorkerExpiration, WorkerState, WorkerStatus};
use wasmtime_wasi_http::{Error, RequestOptions, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

const TIMEOUT: Duration = Duration::from_secs(10);
const MEMORY: usize = 256 << 20; // per linear memory
const LOG_CAP: usize = 64 << 10; // per stream, per request
const MAX_INFLIGHT: usize = 64;

/// Per-store state.
pub struct Host {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: Deny,
    pub config: WasiConfigVariables,
    limits: StoreLimits,
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

/// Expires the worker, which drops its store, `TIMEOUT` after instantiation, even if the guest is only waiting.
pub struct Deadline(Pin<Box<Sleep>>);
impl WorkerExpiration for Deadline {
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>, _: WorkerStatus, _: Instant) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

/// What outlives the store: the app's name, its output pipes, and its slot among the in-flight requests.
pub struct Worker {
    app: Arc<str>,
    out: MemoryOutputPipe,
    err: MemoryOutputPipe,
    _permit: OwnedSemaphorePermit,
}
impl WorkerState for Worker {
    type StoreData = Host;
    type RequestData = ();
    fn should_accept_request(&self, _: usize, _: usize) -> ShouldAccept {
        ShouldAccept::Never // one request per store
    }
    fn on_request_start(
        &self,
        _: StoreContextMut<Host>,
        _: (),
        _: GuestTaskId,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>> {
        Box::pin(std::future::pending()) // `Deadline` enforces the timeout
    }
    /// Runs once the worker is done, also after a trap or a timeout, when `result` is the cause.
    fn drop(&self, _: Store<Host>, result: wasmtime::Result<()>) {
        let (app, out, err) = (&self.app, self.out.contents(), self.err.contents());
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
pub struct App {
    name: Arc<str>,
    handler: ProxyHandler<State>,
}
impl App {
    pub(crate) fn new(name: Arc<str>, engine: Engine, pre: ProxyPre<Host>, config: BTreeMap<String, String>) -> Self {
        let state = State { name: name.clone(), engine, pre, config, permits: Arc::new(Semaphore::new(MAX_INFLIGHT)) };
        Self { name, handler: ProxyHandler::new(state) }
    }

    /// Serves one request. A guest that traps, times out or hits a limit gives a 500, never an `Err`.
    pub async fn handle<B>(&self, req: hyper::Request<B>) -> Response
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Error>,
    {
        self.handler.handle((), req.map(|b| b.map_err(Into::into).boxed_unsync())).await.unwrap_or_else(|e| {
            tracing::warn!(app = %self.name, "request failed: {e}");
            hyper::Response::builder().status(500).body(Empty::new().map_err(|n| match n {}).boxed_unsync()).unwrap()
        })
    }
}

pub struct State {
    name: Arc<str>,
    engine: Engine,
    pre: ProxyPre<Host>,
    config: BTreeMap<String, String>,
    permits: Arc<Semaphore>,
}
impl HandlerState for State {
    type StoreData = Host;
    type WorkerExpiration = Deadline;
    type WorkerState = Worker;
    async fn instantiate(&self) -> wasmtime::Result<Instance<Host, Deadline, Worker>> {
        let _permit = self.permits.clone().acquire_owned().await?;
        let (out, err) = (MemoryOutputPipe::new(LOG_CAP), MemoryOutputPipe::new(LOG_CAP));
        let host = Host {
            table: ResourceTable::new(),
            wasi: WasiCtx::builder().stdout(out.clone()).stderr(err.clone()).build(), // nothing else is granted
            http: WasiHttpCtx::new(),
            hooks: Deny,
            config: self.config.clone().into_iter().collect(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY)
                .instances(16)
                .tables(16)
                .memories(4)
                .table_elements(100_000)
                .build(),
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| &mut h.limits);
        // Q57: `serve` is concurrent, so yield at every tick instead of trapping, and let `Deadline` end the request.
        store.epoch_deadline_async_yield_and_update(1);
        let proxy = self.pre.instantiate_async(&mut store).await?;
        let expiration = Deadline(Box::pin(sleep(TIMEOUT)));
        Ok(Instance {
            store,
            proxy,
            view: Host::http,
            expiration,
            state: Worker { app: self.name.clone(), out, err, _permit },
        })
    }
}
