//! torpor: a scale-to-zero host for WASI components.
mod cli;
mod compile;
mod serve;
mod state;

use clap::{Parser, Subcommand};
use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{ClientOptions, ObjectStore, aws::AmazonS3Builder};
use std::sync::{Arc, OnceLock};
use std::{net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use wasmtime::{Result, error::Context};

const NO_STORE: &str = "there is no --store or TORPOR_STORE";
const NO_NATIVE: &str = "there is no --native or TORPOR_NATIVE";

#[derive(Parser)]
struct Args {
    /// The install's bucket, like `s3://NAME`. Credentials, region and endpoint come from the usual `AWS_` variables
    #[arg(long, global = true, env = "TORPOR_STORE")]
    store: Option<String>,
    /// The install's bucket of native code, if it has one, which hosts load apps from and `publish` waits for. Without
    /// it, a host compiles every app it loads
    #[arg(long, global = true, env = "TORPOR_NATIVE")]
    native: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the install in --store, or else the app described by DIR/torpor.toml, each app at a host like APP.localhost
    Serve {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// The bucket of the apps' KV data, if not --store: one of its own keeps KV's keys from whoever may list --store
        #[arg(long, env = "TORPOR_KV")]
        kv: Option<String>,
    },
    /// Make the native code that the install's markers ask for, as the Lambda Web Adapter posts their events
    CompileWorker {
        #[arg(long, default_value = "127.0.0.1:8080")] // the adapter's default
        listen: SocketAddr,
    },
    /// Upload the app described by DIR/torpor.toml as a release, without serving it, and print `APP ID`. With --native,
    /// then wait until its native code is made
    Publish {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Serve APP from its release ID
    Release { app: String, id: String },
    /// List APP's releases, newest first, each with when it was first published
    Releases { app: String },
    /// Set APP's secret NAME to stdin, less a trailing newline, whichever release it runs. An empty value removes it
    Secret { app: String, name: String },
    /// List the names of APP's secrets
    Secrets { app: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
    let Args { store, native, cmd } = Args::parse();
    let http = OneClient::default();
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
                    Arc::new(serve::Install::new(store, native, kv)?)
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
    }
    Ok(())
}

/// One HTTP client for every bucket, as each client loads and parses the system's root certificates, about 18 ms of a
/// cold start apiece on Lambda. It is made with the first bucket's options, which all come from the `AWS_` variables.
#[derive(Debug, Clone, Default)]
struct OneClient(Arc<OnceLock<HttpClient>>);

impl HttpConnector for OneClient {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        if let Some(client) = self.0.get() {
            return Ok(client.clone());
        }
        let client = ReqwestConnector::default().connect(options)?;
        Ok(self.0.get_or_init(|| client).clone())
    }
}
