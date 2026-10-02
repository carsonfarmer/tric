//! `torpor serve`: runs the app in a directory over HTTP/1.
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use object_store::memory::InMemory;
use serde::Deserialize;
use std::{collections::BTreeMap, convert::Infallible, env, fs, path::Path, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use torpor::Engine;
use wasmtime::Result;
use wasmtime_wasi_http::io::TokioIo;

const MANIFEST: &str = "torpor.toml";
const VAR_PREFIX: &str = "TORPOR_VAR_";

/// The app's `MANIFEST`.
#[derive(Deserialize)]
struct Manifest {
    name: String,
    component: PathBuf, // relative to the manifest's directory
    #[serde(default)]
    config: BTreeMap<String, String>,
    #[serde(default)]
    allowed_outbound_hosts: Vec<String>, // as in Spin: `scheme://host[:port]`
}

pub async fn run(dir: &Path, listener: TcpListener) -> Result<()> {
    let Manifest { name, component, mut config, allowed_outbound_hosts } =
        toml::from_str(&fs::read_to_string(dir.join(MANIFEST))?)?;
    // Secrets never go in the manifest: `TORPOR_VAR_<KEY>` sets `key` and overrides `[config]`.
    config.extend(env::vars().filter_map(|(k, v)| Some((k.strip_prefix(VAR_PREFIX)?.to_lowercase(), v))));
    let engine = Engine::new(Arc::new(InMemory::new()))?; // until the S3 store is in
    let app = engine.load(&name, fs::read(dir.join(component))?, config, &allowed_outbound_hosts)?;
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?; // otherwise Nagle plus delayed ACK stalls a response by ~40 ms
        let app = app.clone();
        tokio::spawn(async move {
            let svc = service_fn(|req: Request<Incoming>| async { Ok::<_, Infallible>(app.handle(req).await) });
            http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await.ok();
        });
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One request over a real socket checks `torpor.toml` is read and its component loaded.
    #[tokio::test]
    async fn reads_the_manifest() {
        let listener = super::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut conn = tokio::net::TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        tokio::spawn(super::run("tests/app".as_ref(), listener));
        conn.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut res = String::new();
        conn.read_to_string(&mut res).await.unwrap();
        assert!(res.starts_with("HTTP/1.1 200") && res.contains("hello"), "{res}");
    }
}
