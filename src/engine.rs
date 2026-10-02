//! The Wasmtime engine, its epoch ticker, and loading components into it.
use crate::guest::{App, Host};
use std::{collections::BTreeMap, thread, time::Duration};
use wasmtime::{
    Config, Result,
    component::{Component, Linker},
};
use wasmtime_wasi_config::WasiConfig;
use wasmtime_wasi_http::{handler::ProxyPre, p2, p3};

/// One epoch tick. A guest yields to the scheduler once per tick, so this bounds how long a runaway delays others.
const TICK: Duration = Duration::from_millis(10);

/// Compiles components. Make one per process and load every app from it.
pub struct Engine {
    engine: wasmtime::Engine,
    linker: Linker<Host>,
}

impl Engine {
    /// Configures Wasmtime and starts the thread that ticks its epoch until the engine and its apps are dropped.
    pub fn new() -> Result<Self> {
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
        Ok(Self { engine, linker })
    }

    /// Loads the component `wasm` (binary, or WAT text) as the app `name`, with `config` for its `wasi:config`. It
    /// exports either `wasi:http/handler` (p3) or `incoming-handler` (p2).
    pub fn load(&self, name: &str, wasm: impl AsRef<[u8]>, config: BTreeMap<String, String>) -> Result<App> {
        let pre = self.linker.instantiate_pre(&Component::new(&self.engine, wasm)?)?;
        let pre = match p3::bindings::ServiceIndices::new(&pre) {
            Ok(_) => ProxyPre::P3(p3::bindings::ServicePre::new(pre)?),
            Err(_) => ProxyPre::P2(p2::bindings::ProxyPre::new(pre)?),
        };
        Ok(App::new(name, self.engine.clone(), pre, config))
    }
}
