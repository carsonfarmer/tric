//! `tric dev`: one app in one process, at any host, with its state in memory, cron ticking in-process, and background
//! requests delivered by an in-process relay.
use crate::cron::{self, Cron};
use crate::engine::Engine;
use crate::manifest;
use crate::outbound::Allow;
use crate::outbox::{self, Sink};
use crate::store::Store;
use crate::tric::{Response, Tric, forward, forwarded, status};
use bytes::Bytes;
use futures_util::{FutureExt, future::BoxFuture};
use http::header::HOST;
use http::{StatusCode, uri::Authority};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{server::conn::http1, service::service_fn};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio::net::TcpListener;
use wasmtime::Result;
use wasmtime_wasi_http::io::TokioIo;

/// Serves the app at `path` on `listen`, with `env` as its environment and `allow` added to its allowed hosts.
pub async fn run(path: &Path, allow: &[String], env: Vec<(String, String)>, listen: SocketAddr) -> Result<()> {
    let app = manifest::read(path, allow).await?;
    let engine = Engine::new()?;
    let code = Arc::new(engine.load(&engine.compile(&app.wasm)?, env)?);
    let allow = app.allowed_outbound_hosts.iter().map(|a| Allow::parse(a)).collect::<Result<_>>()?;
    let listener = TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let jobs = app.cron.iter().map(|(f, path)| Ok((Cron::parse(f)?, format!("http://{addr}{path}"))));
    let jobs = jobs.collect::<Result<_>>()?;
    let tric = Arc::new_cyclic(|tric: &Weak<Tric>| {
        let tric = tric.clone();
        let sink: Sink = Arc::new(move |event: Bytes| -> BoxFuture<'static, Result<()>> {
            if let Some(tric) = tric.upgrade() {
                tokio::spawn(outbox::relay(move || {
                    let (tric, event) = (tric.clone(), event.clone());
                    async move { outbox::deliver(&tric, &event).await }
                }));
            }
            std::future::ready(Ok(())).boxed()
        });
        Tric { app: app.name.clone(), store: Store::memory(), code, allow, sink }
    });
    tokio::spawn(cron::tick(tric.clone(), jobs));
    eprintln!("serving {} at http://{addr}", app.name);
    loop {
        let (tcp, peer) = listener.accept().await?;
        _ = tcp.set_nodelay(true); // otherwise Nagle and delayed ACKs hold a response up by ~40 ms
        let tric = tric.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(tric.clone(), peer, req).map(Ok::<_, Infallible>));
            http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).await.ok();
        });
    }
}

/// Runs a request from `peer`, at the host its `Host` names, its URI made absolute and `Forwarded` set from the socket.
async fn handle(tric: Arc<Tric>, peer: SocketAddr, req: hyper::Request<Incoming>) -> Response {
    let (mut parts, body) = req.into_parts();
    let host = parts.headers.get(HOST).and_then(|h| h.to_str().ok()).and_then(|h| h.parse::<Authority>().ok());
    let Some(host) = host.filter(|h| !h.as_str().contains('@')).map(|h| h.to_string()) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let pq = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let (Ok(uri), Some(from)) = (format!("http://{host}{pq}").parse(), forwarded(peer.ip(), &host, "http")) else {
        return status(StatusCode::BAD_REQUEST);
    };
    parts.uri = uri;
    forward(&mut parts.headers, from);
    let body = body.map_err(wasmtime_wasi_http::Error::from).boxed_unsync();
    tric.run(http::Request::from_parts(parts, body), &host).await
}
