//! torpor: a scale-to-zero host for WASI components.
mod cli;
mod serve;
mod state;

use clap::{Parser, Subcommand};
use object_store::aws::AmazonS3Builder;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use wasmtime::{Error, Result, error::Context};

const NO_STORE: &str = "there is no --store or TORPOR_STORE";

#[derive(Parser)]
struct Args {
    /// The install's bucket, like `s3://NAME`. Credentials, region and endpoint come from the usual `AWS_` variables
    #[arg(long, global = true, env = "TORPOR_STORE")]
    store: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the install in --store, or else the app described by DIR/torpor.toml
    Serve {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// The age identity (`AGE-SECRET-KEY-1…`) that decrypts the install's secrets
        #[arg(long, env = "TORPOR_IDENTITY", hide_env_values = true)]
        identity: Option<String>,
    },
    /// Deploy the app described by each DIR/torpor.toml, all in one change
    Deploy {
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
    },
    /// Move APP back to the release before its current one
    Rollback { app: String },
    #[command(subcommand)]
    Secrets(Secrets),
}

/// An app's secrets
#[derive(Subcommand)]
enum Secrets {
    /// Set APP's secret NAME to stdin, less a trailing newline, in a new release
    Set {
        app: String,
        name: String,
        /// The age recipient (`age1…`) of the install's identity
        #[arg(long, env = "TORPOR_RECIPIENT")]
        recipient: String,
    },
    /// List the names of APP's secrets
    List { app: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
    let Args { store, cmd } = Args::parse();
    let store = store.map(|url| AmazonS3Builder::from_env().with_url(url).build()).transpose()?;
    match cmd {
        Cmd::Serve { dir, listen, identity } => {
            let identity = identity.map(|i| i.parse()).transpose().map_err(Error::msg)?;
            let apps = match store {
                Some(store) => serve::Apps::install(Arc::new(store), identity).await?,
                None => serve::Apps::dir(&dir)?,
            };
            return serve::run(apps, TcpListener::bind(listen).await?).await;
        }
        Cmd::Deploy { dirs } => cli::deploy(&store.context(NO_STORE)?, &dirs).await?,
        Cmd::Rollback { app } => cli::rollback(&store.context(NO_STORE)?, &app).await?,
        Cmd::Secrets(Secrets::Set { app, name, recipient }) => {
            let value = std::io::read_to_string(std::io::stdin())?;
            let value = value.strip_suffix('\n').unwrap_or(&value);
            cli::set_secret(&store.context(NO_STORE)?, &app, &name, value, &recipient).await?
        }
        Cmd::Secrets(Secrets::List { app }) => {
            cli::secrets(&store.context(NO_STORE)?, &app).await?.iter().for_each(|n| println!("{n}"));
            return Ok(());
        }
    }
    tokio::time::sleep(state::FRESH).await; // until every host has the change
    Ok(())
}
