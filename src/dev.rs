//! `tric dev`: one app in one process, at any host, with its state in memory, cron ticking in-process, and background
//! requests delivered by an in-process relay.
use crate::cron::{self, Cron};
use crate::engine::Engine;
use crate::manifest;
use crate::outbound::Allow;
use crate::outbox::{self, Sink};
use crate::store::Store;
use crate::tric::{self, Response, Tric, forward, forwarded, status};
use bytes::Bytes;
use futures_util::{FutureExt, future::BoxFuture};
use http::header::HOST;
use http::{StatusCode, uri::Authority};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Weak};
use tokio::net::TcpListener;
use wasmtime::Result;

/// Serves the app at `path` on `listen`, with `env` as its environment and `allow` added to its allowed hosts.
pub async fn run(path: &Path, allow: &[String], env: Vec<(String, String)>, listen: SocketAddr) -> Result<()> {
    #[cfg(feature = "ws")]
    crate::ws::init();
    let app = manifest::read(path, allow).await?;
    let engine = Engine::new()?;
    let code = Arc::new(engine.load(&engine.compile(&app.wasm)?, env)?);
    let allow = app.allowed_outbound_hosts.iter().map(|a| Allow::parse(a)).collect::<Result<_>>()?;
    let listener = TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let jobs = app.cron.iter().map(|(f, path)| Ok((Cron::parse(f)?, format!("http://{addr}{path}"))));
    let jobs: Vec<_> = jobs.collect::<Result<_>>()?;
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
    let ticker = tric.clone();
    tokio::spawn(cron::tick(move |t| {
        for (_, url) in jobs.iter().filter(|(cron, _)| cron.matches(t)) {
            tokio::spawn(cron::fire(ticker.clone(), url.clone()));
        }
    }));
    eprintln!("serving {} at http://{addr}", app.name);
    tric::listen(listener, move |peer, req| handle(tric.clone(), peer, req)).await
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
    #[cfg(feature = "ws")]
    if crate::ws::wants(&parts) {
        return crate::ws::open(tric, host, parts).await;
    }
    let body = body.map_err(wasmtime_wasi_http::Error::from).boxed_unsync();
    tric.run(http::Request::from_parts(parts, body), &host).await
}
