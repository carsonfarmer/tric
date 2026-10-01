//! minihost: multi-app wasi:http (p2 + p3) host with Spin adapters, built on wasmtime-wasi-http's ProxyHandler.
mod manifest;
mod spin_adapter;

use http_body_util::BodyExt;
use manifest::{HttpTrigger, Manifest, Source, Spec};
use spin_adapter::{KvBackend, MemKv, SpinCtx, SpinHost, SpinView};
use std::{pin::Pin, sync::{Arc, atomic::{AtomicU64, Ordering}}, task::{Context, Poll}, time::Instant};
use tokio::{net::TcpListener, sync::Semaphore};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut, component::{Component, GuestTaskId, Linker, ResourceTable}};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::handler::{HandlerState, Instance, Proxy, ProxyHandler, ProxyPre, ShouldAccept, WorkerExpiration, WorkerState, WorkerStatus};
use wasmtime_wasi_http::{Error as HttpError, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

// ---------------- per-store state ----------------
struct Host { table: ResourceTable, wasi: WasiCtx, http: WasiHttpCtx, hooks: Hooks, spin: SpinCtx }
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx: &mut self.wasi, table: &mut self.table } }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> { WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: &mut self.hooks } }
}
impl SpinHost for Host {
    fn spin(&mut self) -> SpinView<'_> { SpinView { ctx: &mut self.spin, table: &mut self.table } }
}

/// allowed_outbound_hosts enforcement ("https://host[:port]", "*", "*.suffix").
#[derive(Clone)]
struct Hooks(Vec<String>);
impl Hooks {
    fn allows(&self, uri: &http::Uri) -> bool {
        let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else { return false };
        let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });
        self.0.iter().any(|p| {
            let (ps, rest) = p.split_once("://").unwrap_or(("*", p));
            let (ph, pp) = rest.rsplit_once(':').filter(|(_, x)| x.parse::<u16>().is_ok() || *x == "*").unwrap_or((rest, "*"));
            (ps == "*" || ps == scheme) && (pp == "*" || pp == port.to_string())
                && (ph == "*" || ph == host || ph.strip_prefix("*.").is_some_and(|s| host.ends_with(&format!(".{s}"))))
        })
    }
}
impl WasiHttpHooks for Hooks {
    fn send_request(&mut self, req: http::Request<WasiBody>, opts: Option<RequestOptions>, _fut: Box<dyn Future<Output = Result<(), HttpError>> + Send>)
        -> Box<dyn Future<Output = wasmtime_wasi_http::Result<(http::Response<WasiBody>, Box<dyn Future<Output = Result<(), HttpError>> + Send>)>> + Send> {
        let ok = self.allows(req.uri());
        Box::new(async move {
            if !ok { return Err(HttpError::HttpRequestDenied); }
            let (res, io) = wasmtime_wasi_http::default_send_request(req, opts).await?;
            Ok((res.map(BodyExt::boxed_unsync), Box::new(io) as Box<dyn Future<Output = _> + Send>))
        })
    }
}

// ---------------- per-app handler state (instance pooling via ProxyHandler) ----------------
struct App { engine: Engine, pre: ProxyPre<Host>, spin: SpinCtx, hosts: Hooks, env: Vec<(String, String)>, sem: Semaphore, next_id: AtomicU64 }
struct Worker(bool);
struct Never;
impl WorkerExpiration for Never {
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>, _: WorkerStatus, _: Instant) -> Poll<()> { Poll::Pending }
}
impl WorkerState for Worker {
    type StoreData = Host;
    type RequestData = ();
    fn should_accept_request(&self, concurrent: usize, total: usize) -> ShouldAccept {
        let (max_total, max_conc) = if self.0 { (128, 16) } else { (1, 1) }; // p3 reuses instances, p2 does not
        if total >= max_total { ShouldAccept::Never } else if concurrent >= max_conc { ShouldAccept::No } else { ShouldAccept::Yes }
    }
    fn on_request_start(&self, _: StoreContextMut<Host>, _: (), _: GuestTaskId) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>> {
        Box::pin(std::future::pending())
    }
    fn drop(&self, _store: Store<Host>, result: Result<()>) { if let Err(e) = result { eprintln!("worker failed: {e:?}"); } }
}
impl HandlerState for App {
    type StoreData = Host;
    type WorkerExpiration = Never;
    type WorkerState = Worker;
    async fn instantiate(&self) -> Result<Instance<Host, Never, Worker>> {
        let mut b = WasiCtxBuilder::new();
        b.inherit_stdout().inherit_stderr();
        for (k, v) in &self.env { b.env(k, v); }
        let host = Host { table: ResourceTable::new(), wasi: b.build(), http: WasiHttpCtx::new(), hooks: self.hosts.clone(), spin: self.spin.clone() };
        let mut store = Store::new(&self.engine, host);
        let proxy: Proxy = self.pre.instantiate_async(&mut store).await?;
        let _ = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(Instance { store, proxy, view: Host::http, expiration: Never, state: Worker(matches!(self.pre, ProxyPre::P3(_))) })
    }
}

// ---------------- routing + serving ----------------
struct Route { prefix: String, wildcard: bool, id: String, handler: ProxyHandler<App> }
impl Route {
    fn matches(&self, p: &str) -> bool {
        p == self.prefix || (self.wildcard && (self.prefix.is_empty() || p.starts_with(&format!("{}/", self.prefix))))
    }
}

async fn handle(routes: Arc<Vec<Route>>, mut req: hyper::Request<hyper::body::Incoming>, peer: std::net::SocketAddr) -> Result<hyper::Response<WasiBody>> {
    let path = req.uri().path().trim_end_matches('/').to_string();
    let Some(r) = routes.iter().filter(|r| r.matches(&path)).max_by_key(|r| (!r.wildcard, r.prefix.len())) else {
        return Ok(hyper::Response::builder().status(404).body(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync())?);
    };
    let _permit = r.handler.state().sem.acquire().await?;
    // Headers Spin injects (spin-sdk 5 Router and others read these).
    let route = format!("{}{}", r.prefix, if r.wildcard { "/..." } else { "" });
    let host = req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("localhost").to_string();
    for (k, v) in [
        ("spin-full-url", format!("http://{host}{}", req.uri())), ("spin-path-info", path[r.prefix.len().min(path.len())..].to_string()),
        ("spin-matched-route", route.clone()), ("spin-raw-component-route", route.clone()), ("spin-component-route", r.prefix.clone()),
        ("spin-base-path", "/".into()), ("spin-client-addr", peer.to_string()),
    ] {
        req.headers_mut().insert(http::HeaderName::from_static(k), v.parse()?);
    }
    let _ = &r.id;
    r.handler.handle((), req.map(|b| b.map_err(Into::into).boxed_unsync())).await
}

fn engine_of(l: &Linker<Host>) -> Engine { l.engine().clone() }

/// Trap-stub only the imported instances the host does not implement (see report: the built-in API also re-stubs
/// type-alias exports such as `wasi:http/types.headers` of *implemented* instances, which then fail to typecheck).
fn stub_unknown(engine: &Engine, linker: &mut Linker<Host>, component: &Component) -> Result<()> {
    use wasmtime::component::{ResourceType, types::ComponentItem};
    const KNOWN: &[&str] = &["wasi:io/", "wasi:clocks/", "wasi:random/", "wasi:cli/", "wasi:http/", "wasi:filesystem/", "wasi:sockets/",
        "spin:key-value/key-value@", "spin:variables/variables@", "fermyon:spin/key-value@", "fermyon:spin/variables@"];
    let ty = component.component_type();
    for (name, item) in ty.imports(engine) {
        let ComponentItem::ComponentInstance(inst) = item.ty else { continue };
        if KNOWN.iter().any(|k| name.starts_with(k)) { continue; }
        let mut li = linker.instance(name)?;
        for (n, it) in inst.exports(engine) {
            let msg = format!("unknown import `{name}#{n}`");
            match it.ty {
                ComponentItem::ComponentFunc(f) if f.async_() => li.func_new_concurrent(n, move |_, _, _, _| { let m = msg.clone(); Box::pin(async move { wasmtime::bail!("{m}") }) })?,
                ComponentItem::ComponentFunc(_) => li.func_new(n, move |_, _, _, _| wasmtime::bail!("{msg}"))?,
                ComponentItem::Resource(_) => li.resource(n, ResourceType::host::<()>(), |_, _| Ok(()))?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn load(engine: &Engine, linker: &Linker<Host>, kv: Arc<dyn KvBackend>, manifest_path: &str) -> Result<Vec<Route>> {
    let dir = std::path::Path::new(manifest_path).parent().unwrap().to_path_buf();
    let m: Manifest = toml::from_str(&std::fs::read_to_string(manifest_path)?)?;
    let mut routes = vec![];
    for t in m.trigger.get("http").into_iter().flatten() {
        let t: HttpTrigger = t.clone().try_into()?;
        let Some(route) = t.route.as_str() else { continue }; // { private = true }
        let (id, c) = match &t.component { Spec::Ref(id) => (id.clone(), m.component.get(id).ok_or_else(|| wasmtime::format_err!("no component {id}"))?), Spec::Inline(c) => ("inline".into(), &**c) };
        let Some(Source::Local(src)) = &c.source else { wasmtime::bail!("only local sources in this PoC") };
        let component = Component::from_file(engine, dir.join(src))?; // wasip1 modules: see componentize note in the report
        let mut linker = linker.clone();
        match std::env::var("MODE").as_deref() {
            Ok("builtin") => linker.define_unknown_imports_as_traps(&component)?, // wasmtime's API (breaks known instances with alias exports)
            Ok("none") => {}
            _ => stub_unknown(&engine_of(&linker), &mut linker, &component)?, // tolerate whole-world importers (Python/Go)
        }
        let vars = c.variables.iter().map(|(k, v)| Ok((k.clone(), m.expand(v)?))).collect::<Result<_>>()?;
        let instance_pre = linker.instantiate_pre(&component)?;
        let pre = match wasmtime_wasi_http::p3::bindings::ServicePre::new(instance_pre.clone()) {
            Ok(p) => ProxyPre::P3(p),
            Err(_) => ProxyPre::P2(wasmtime_wasi_http::p2::bindings::ProxyPre::new(instance_pre)?),
        };
        let app = App {
            engine: engine.clone(), pre, hosts: Hooks(c.allowed_outbound_hosts.clone()), sem: Semaphore::new(1000), next_id: AtomicU64::new(0),
            spin: SpinCtx { kv: kv.clone(), stores: c.key_value_stores.clone(), vars },
            env: c.environment.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        let wildcard = route.ends_with("/...");
        routes.push(Route { prefix: route.trim_end_matches("/...").trim_end_matches('/').to_string(), wildcard, id, handler: ProxyHandler::new(app) });
    }
    Ok(routes)
}

#[tokio::main]
async fn main() -> Result<()> {
    let manifest = std::env::args().nth(1).expect("usage: minihost spin.toml [addr]");
    let addr = std::env::args().nth(2).unwrap_or("127.0.0.1:8080".into());
    let mut cfg = Config::new();
    cfg.wasm_component_model_async(true).wasm_component_model_async_stackful(true).wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&cfg)?;
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?; // full wasi 0.2 (guests built for wasip2 import cli/environment, exit, ...)
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?; // clocks, random, cli, filesystem, sockets (0.3)
    spin_adapter::add_to_linker(&mut linker)?;
    let routes = Arc::new(load(&engine, &linker, Arc::new(MemKv::default()), &manifest)?);
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("listening on {addr}");
    loop {
        let (stream, peer) = listener.accept().await?;
        let routes = routes.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let routes = routes.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(handle(routes, req, peer).await.unwrap_or_else(|e| {
                        eprintln!("error: {e:?}");
                        hyper::Response::builder().status(500).body(http_body_util::Empty::new().map_err(|e| match e {}).boxed_unsync()).unwrap()
                    }))
                }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(wasmtime_wasi_http::io::TokioIo::new(stream), svc).await { eprintln!("conn: {e:?}"); }
        });
    }
}
