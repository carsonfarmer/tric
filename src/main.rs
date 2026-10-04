//! tric: a scale-to-zero host for WASI components.
mod cli;
mod compile;
mod serve;
mod state;

use clap::{Parser, Subcommand};
use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{Certificate, ClientOptions, ObjectStore, aws::AmazonS3Builder};
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use wasmtime::{Result, error::Context};
use webpki_root_certs::TLS_SERVER_ROOT_CERTS;

const NO_STORE: &str = "there is no --store or TRIC_STORE";
const NO_NATIVE: &str = "there is no --native or TRIC_NATIVE";

#[derive(Parser)]
struct Args {
    /// The install's bucket, like `s3://NAME`. Credentials, region and endpoint come from the usual `AWS_` variables
    #[arg(long, global = true, env = "TRIC_STORE")]
    store: Option<String>,
    /// The install's bucket of native code, if it has one, which hosts load apps from and `publish` waits for. Without
    /// it, a host compiles every app it loads
    #[arg(long, global = true, env = "TRIC_NATIVE")]
    native: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the install in --store, or else the app described by DIR/tric.toml, each app at a host like APP.localhost
    Serve {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// The bucket of the apps' KV data, if not --store: one of its own keeps KV's keys from whoever may list --store
        #[arg(long, env = "TRIC_KV")]
        kv: Option<String>,
    },
    /// Make the native code that the install's markers ask for, as the Lambda Web Adapter posts their events
    CompileWorker {
        #[arg(long, default_value = "127.0.0.1:8080")] // the adapter's default
        listen: SocketAddr,
    },
    /// Upload the app described by DIR/tric.toml as a release, without serving it, and print `APP ID`. With --native,
    /// then wait until its native code is made
    Publish {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Serve APP from its release ID
    Release { app: String, id: String },
    /// List APP's releases, newest first, each with when it was last published
    Releases { app: String },
    /// Set APP's secret NAME to stdin, less a trailing newline, whichever release it runs. An empty value removes it
    Secret { app: String, name: String },
    /// List the names of APP's secrets
    Secrets { app: String },
    /// Delete, and print, what no app needs: of each, its releases but the one it runs and its 10 newest, and what only
    /// those used. Anything under an hour old stays, as a publish may still be writing it
    Gc,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
    let Args { store, native, cmd } = Args::parse();
    let http = SharedClients::default();
    let bucket = |url| {
        let s = AmazonS3Builder::from_env().with_url(url).with_http_connector(http.clone()).build();
        s.map(|s| Arc::new(s) as Arc<dyn ObjectStore>)
    };
    let (store, native) = (store.map(bucket).transpose()?, native.map(bucket).transpose()?);
    let s = || store.as_deref().context(NO_STORE);
    match cmd {
        Cmd::Serve { dir, listen, kv } => {
            let install = match store {
                Some(store) => {
                    let kv = kv.map(bucket).transpose()?.unwrap_or_else(|| store.clone());
                    serve::Install::new(store, native, kv)?
                }
                None => serve::Install::dev(&dir).await?,
            };
            serve::run(TcpListener::bind(listen).await?, move |req| install.clone().handle(req)).await?
        }
        Cmd::CompileWorker { listen } => {
            let worker = Arc::new(compile::Worker::new(store.context(NO_STORE)?, native.context(NO_NATIVE)?)?);
            serve::run(TcpListener::bind(listen).await?, move |req| worker.clone().handle(req)).await?
        }
        Cmd::Publish { dir } => {
            let (app, id) = cli::publish(s()?, &dir, true).await?;
            println!("{app} {id}"); // first, as the release stands even if the wait fails
            if native.is_some() {
                cli::precompile(s()?, &app, &id).await?;
            }
        }
        Cmd::Release { app, id } => cli::release(s()?, &app, &id).await?,
        Cmd::Releases { app } => cli::releases(s()?, &app).await?.iter().for_each(|r| println!("{r}")),
        Cmd::Secret { app, name } => {
            let value = std::io::read_to_string(std::io::stdin())?;
            cli::set_secret(s()?, &app, &name, value.strip_suffix('\n').unwrap_or(&value)).await?
        }
        Cmd::Secrets { app } => cli::secrets(s()?, &app).await?.iter().for_each(|n| println!("{n}")),
        Cmd::Gc => cli::gc(s()?, native.as_deref(), cli::GRACE).await?,
    }
    Ok(())
}

/// One HTTP client for each set of options, so the buckets, all at one S3 endpoint, share its connections, and the
/// roots are loaded once. Every bucket's options come from the `AWS_` variables and so are the same, though off Lambda
/// a credential provider connects with its own. `ClientOptions` has no `Eq`, so its `Debug` is the key: that has every
/// field but a certificate added in code, and the roots are added after.
#[derive(Debug, Clone, Default)]
struct SharedClients(Arc<Mutex<HashMap<String, HttpClient>>>);

impl HttpConnector for SharedClients {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let key = format!("{options:?}");
        let mut clients = self.0.lock().unwrap();
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        // Mozilla's roots, as `outbound` trusts, not the system's: those took ~30 ms more of a cold start on Lambda.
        let mut tls = options.clone().with_no_system_certificates(true);
        for der in TLS_SERVER_ROOT_CERTS {
            tls = tls.with_root_certificate(Certificate::from_der(der)?);
        }
        let client = ReqwestConnector::default().connect(&tls)?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}
