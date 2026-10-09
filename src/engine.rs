//! The Wasmtime engine, and apps on it: one `wasi:http/handler@0.3` component each, a fresh instance per request, under
//! hard limits, with nothing granted but what tric provides.
use crate::kv::Imports;
use crate::tric::{Ctx, Outbound, Request, Response};
use sha2::{Digest, Sha256};
use std::future::poll_fn;
use std::hash::{Hash, Hasher};
use std::{sync::Arc, thread, time::Duration};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tracing::Instrument;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Result, Store, StoreLimits, StoreLimitsBuilder, bail};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p2::pipe::MemoryOutputPipe};
use wasmtime_wasi_http::p3::bindings::{Service, ServicePre};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView, p3};

/// One epoch tick. A guest yields once per tick, so this bounds how long a runaway holds up the others.
const TICK: Duration = Duration::from_millis(10);
/// From instantiation to the response's head.
pub const ANSWER: Duration = Duration::from_secs(10);
/// From instantiation to the end of everything the instance does, its response's body included.
const TOTAL: Duration = Duration::from_secs(300);
const MEMORY: usize = 256 << 20; // one linear memory
const MEMORIES: usize = 4; // per store, as are the next two
const INSTANCES: usize = 16;
const TABLES: usize = 16;
const TABLE_MAX: usize = 100_000; // elements in one table
const LOG_MAX: usize = 64 << 10; // of stdout, and of stderr, per request
const IN_FLIGHT: usize = 64; // requests per app
const RESOURCES: usize = 256; // live resources per store; the default is a million
const HOSTCALL_FUEL: usize = 32 << 20; // bytes one host call may copy out of the guest; the default is 128 MiB

/// Compiles components, and loads them as apps. One per process.
pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<Host>,
}

impl Engine {
    /// Configures Wasmtime and starts the thread that ticks its epoch for as long as the engine lives.
    pub fn new() -> Result<Self> {
        let mut cfg = Config::new();
        // The architecture's baseline, not this CPU's features, so every host of one build loads any other's native
        // code.
        cfg.target(&target_lexicon::HOST.to_string())?;
        // Callback-only async: no stackful tasks, threads, extra builtins or error contexts.
        cfg.wasm_component_model_async(true).epoch_interruption(true);
        cfg.wasm_component_model_more_async_builtins(false).wasm_component_model_async_stackful(false);
        cfg.wasm_component_model_threading(false).wasm_component_model_error_context(false);
        let engine = wasmtime::Engine::new(&cfg)?;
        // An OS thread, not a task: a task would not run while every worker thread is busy with a guest.
        let weak = engine.weak();
        thread::spawn(move || {
            while weak.upgrade().map(|e| e.increment_epoch()).is_some() {
                thread::sleep(TICK);
            }
        });
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?; // as Rust's standard library on wasm32-wasip2 imports it
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        p3::add_to_linker(&mut linker)?;
        Imports::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |h| h)?; // `wasi:keyvalue`
        Ok(Self { engine, linker })
    }

    pub fn compile(&self, wasm: &[u8]) -> Result<Component> {
        Component::new(&self.engine, wasm)
    }

    /// Which engines' native code this one loads, in hex: those of the same Wasmtime, configuration and target. It is a
    /// SHA-256, as Wasmtime's own cache keys native code: `DefaultHasher` may change from one Rust release to the next.
    pub fn compat(&self) -> String {
        struct Sha(Sha256);
        impl Hasher for Sha {
            fn write(&mut self, bytes: &[u8]) {
                self.0.update(bytes);
            }
            fn finish(&self) -> u64 {
                unreachable!("read with `finalize`")
            }
        }
        let mut sha = Sha(Sha256::new());
        self.engine.precompile_compatibility_hash().hash(&mut sha);
        format!("{:x}", sha.0.finalize())
    }

    /// The component whose native code `bytes` is.
    ///
    /// # Safety
    ///
    /// Loading native code runs it, so `bytes` must be what an engine of the same `compat` made.
    pub unsafe fn deserialize(&self, bytes: &[u8]) -> Result<Component> {
        unsafe { Component::deserialize(&self.engine, bytes) }
    }

    /// Loads `component` as an app, with `env` as its environment.
    pub fn load(&self, component: &Component, env: Vec<(String, String)>) -> Result<App> {
        let pre = ServicePre::new(self.linker.instantiate_pre(component)?)?;
        let (env, permits) = (env.into(), Arc::new(Semaphore::new(IN_FLIGHT)));
        Ok(App { engine: self.engine.clone(), pre, env, permits })
    }
}

/// An app: a component ready to instantiate, its environment and its slots for requests in flight.
pub struct App {
    engine: wasmtime::Engine,
    pre: ServicePre<Host>,
    env: Arc<[(String, String)]>,
    permits: Arc<Semaphore>,
}

/// Per-store state.
pub struct Host {
    pub table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: Outbound,
    pub ctx: Arc<Ctx>,
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

type Answer = Result<Response>;

impl App {
    /// Runs one request in a fresh instance, with `ctx` for its state and its outbound requests, and returns the
    /// response once its head is there, with a handle that ends the instance, body and all. Waits while 64 requests of
    /// the app are in flight. An instance that has not answered 10 s after it was instantiated is ended; one that
    /// answered runs on for up to 300 s, as its body streams.
    pub async fn call(&self, req: Request, ctx: Arc<Ctx>) -> Result<(Response, AbortHandle)> {
        let permit = self.permits.clone().acquire_owned().await?;
        let (out, err) = (MemoryOutputPipe::new(LOG_MAX), MemoryOutputPipe::new(LOG_MAX));
        let wasi = WasiCtx::builder().stdout(out.clone()).stderr(err.clone()).envs(&self.env).build();
        let hooks = Outbound(ctx.clone());
        let limits =
            StoreLimitsBuilder::new().memory_size(MEMORY).memories(MEMORIES).instances(INSTANCES).tables(TABLES);
        let limits = limits.table_elements(TABLE_MAX).build();
        let mut host = Host { table: ResourceTable::new(), wasi, http: WasiHttpCtx::new(), hooks, ctx, limits };
        host.table.set_max_capacity(RESOURCES);
        let mut store = Store::new(&self.engine, host);
        store.limiter(|h| &mut h.limits);
        store.set_hostcall_fuel(HOSTCALL_FUEL);
        store.epoch_deadline_async_yield_and_update(1); // yield at every tick; the deadlines below end a runaway
        let (pre, (tx, rx)) = (self.pre.clone(), oneshot::channel::<Answer>());
        let span = tracing::Span::current();
        let task = tokio::spawn(
            async move {
                let _permit = permit;
                let run = async {
                    let guest = pre.instantiate_async(&mut store).await?;
                    run(&mut store, guest, req, tx).await
                };
                match timeout(TOTAL, run).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("guest failed: {e:#}"),
                    Err(_) => tracing::warn!("guest ran past {TOTAL:?}"),
                }
                for (stream, pipe) in [("stdout", out), ("stderr", err)] {
                    let text = pipe.contents();
                    if !text.is_empty() {
                        tracing::info!(stream, "{}", String::from_utf8_lossy(&text).trim_end());
                    }
                }
            }
            .instrument(span),
        );
        match timeout(ANSWER, rx).await {
            Ok(Ok(Ok(res))) => Ok((res, task.abort_handle())),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => bail!("the guest failed before it answered"),
            Err(_) => {
                task.abort();
                bail!("the guest did not answer within {ANSWER:?}")
            }
        }
    }
}

/// Calls the guest's handler, sends its response, or the error that stood in for one, on `tx`, and runs the store
/// until the guest has nothing left to do.
async fn run(store: &mut Store<Host>, guest: Service, req: Request, tx: oneshot::Sender<Answer>) -> Result<()> {
    let (req, io) = p3::Request::from_http(store.data_mut().http().hooks, req);
    let req = store.data_mut().table.push(req)?;
    let call = guest.wasi_http_handler().func_handle().start_call_concurrent(&mut *store, (req,))?;
    store
        .run_concurrent(async move |accessor| {
            let answered = async {
                let res = guest.wasi_http_handler().func_handle().finish_call_concurrent(accessor, call).await?.0;
                let res = res.map_err(|e| wasmtime::format_err!("the guest answered {e:?}"))?;
                accessor.with(|mut store| {
                    store.get().ctx.answered();
                    let res = store.get().table.delete(res)?;
                    res.into_http(&mut store, io)
                })
            };
            match answered.await {
                Ok(res) => _ = tx.send(Ok(res)),
                Err(e) => {
                    _ = tx.send(Err(wasmtime::format_err!("{e:#}")));
                    return Err(e);
                }
            }
            poll_fn(|cx| accessor.poll_no_interesting_tasks(cx)).await;
            Ok(())
        })
        .await?
}
