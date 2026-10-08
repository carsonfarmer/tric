//! tric: a scale-to-zero host for WASI components, with their state in object storage.
mod aws;
mod cron;
mod deploy;
mod engine;
mod kv;
mod name;
mod outbound;
mod outbox;
mod serve;
mod state;

use aws::Aws;
use clap::{Parser, Subcommand};
use object_store::aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey};
use object_store::client::{HttpClient, HttpConnector, ReqwestConnector};
use object_store::{Certificate, ClientOptions, ObjectStore, memory::InMemory};
use serve::Tric;
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use tracing_subscriber::EnvFilter;
use wasmtime::{Result, error::Context};
use webpki_root_certs::TLS_SERVER_ROOT_CERTS;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The install's bucket, like `s3://NAME`. Credentials, region and endpoint come from the usual `AWS_` variables
    #[arg(long, global = true, env = "TRIC_STORE")]
    store: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the app at PATH, at any host, with its state in memory unless there is a --store
    Dev {
        /// A directory with a tric.toml, or a component, whose file name less `.wasm` is the app's name
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Set an environment variable
        #[arg(short, value_name = "NAME=VALUE", value_parser = var)]
        e: Vec<(String, String)>,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
    },
    /// Serve every app of the install, each at APP.DOMAIN
    Serve {
        #[arg(long, env = "TRIC_LISTEN", default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        #[arg(long, env = "TRIC_DOMAIN", default_value = "localhost")]
        domain: String,
    },
    /// Upload the app at PATH and make it the release its app runs, then print the release's id
    Deploy {
        /// A directory with a tric.toml, or a component, whose file name less `.wasm` is the app's name
        path: PathBuf,
        /// Set an environment variable; an empty value removes it
        #[arg(short, value_name = "NAME=VALUE", value_parser = var)]
        e: Vec<(String, String)>,
    },
    /// Run APP's release ID, one of those it keeps
    Release { app: String, id: String },
    /// List APP's releases, newest first, marking the one it runs
    Releases { app: String },
    /// Set APP's environment variables, an empty value removing one, whichever release it runs; then list their names
    Env {
        app: String,
        #[arg(value_name = "NAME=VALUE", value_parser = var)]
        vars: Vec<(String, String)>,
    },
}

fn var(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.into(), v.into())).ok_or_else(|| format!("{s:?} is not NAME=VALUE"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let lambda = std::env::var("AWS_LAMBDA_FUNCTION_NAME").ok();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,tric=info".into());
    match lambda {
        Some(_) => tracing_subscriber::fmt().json().with_env_filter(filter).init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    let Args { store, cmd } = Args::parse();
    let http = SharedClients::default();
    let client = http.connect(&ClientOptions::new())?;
    let (store, aws) = match store {
        Some(url) => {
            let (s3, aws) = bucket(&url, &http, client.clone())?;
            (Some(s3 as Arc<dyn ObjectStore>), Some(aws))
        }
        None => (None, None),
    };
    let s = || store.as_ref().context("there is no --store or TRIC_STORE");
    match cmd {
        Cmd::Dev { path, e, listen } => {
            let store = store.clone().unwrap_or_else(|| Arc::new(InMemory::new()));
            let built = deploy::build(&path, &client).await?;
            let app = built.name.clone();
            deploy::deploy(&*store, None, built, &e).await?;
            Tric::new(store, "localhost", Some(app), None)?.serve(listen).await?
        }
        Cmd::Serve { listen, domain } => Tric::new(s()?.clone(), &domain, None, aws.zip(lambda))?.serve(listen).await?,
        Cmd::Deploy { path, e } => {
            let built = deploy::build(&path, &client).await?;
            println!("{}", deploy::deploy(s()?, aws.as_ref(), built, &e).await?);
        }
        Cmd::Release { app, id } => deploy::release(s()?, aws.as_ref(), &app, &id).await?,
        Cmd::Releases { app } => deploy::releases(s()?, &app).await?.iter().for_each(|r| println!("{r}")),
        Cmd::Env { app, vars } => deploy::env(s()?, &app, &vars).await?.iter().for_each(|n| println!("{n}")),
    }
    Ok(())
}

/// The bucket at `url`, and the other AWS APIs with its credentials, in its region.
fn bucket(url: &str, http: &SharedClients, client: HttpClient) -> Result<(Arc<AmazonS3>, Aws)> {
    let builder = AmazonS3Builder::from_env().with_url(url).with_http_connector(http.clone());
    let region = builder.get_config_value(&AmazonS3ConfigKey::Region).unwrap_or_else(|| "us-east-1".into());
    let s3 = builder.build()?;
    let aws = Aws::new(&s3, client, region);
    Ok((Arc::new(s3), aws))
}

/// One HTTP client for each set of options, so everything at one endpoint shares its connections, and the roots are
/// loaded once. `ClientOptions` has no `Eq`, so its `Debug` is the key: that has every field but a certificate added in
/// code, and the roots are added after.
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
