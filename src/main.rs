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
    /// Upload the app described by DIR/torpor.toml as a release, without serving it, and print `APP ID`
    Publish {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Serve APP from its release ID
    Release { app: String, id: String },
    /// List APP's releases, newest first, each with when it was first published
    Releases { app: String },
    #[command(subcommand)]
    Apps(Apps),
    #[command(subcommand)]
    Secrets(Secrets),
}

/// The install's apps, each in a team. Changing them takes the admin's credentials
#[derive(Subcommand)]
enum Apps {
    /// Add APP to TEAM, or move it there, where it starts with nothing published and no KV data
    Add { app: String, team: String },
    /// Take APP out of the install. Its objects stay in the bucket
    Remove { app: String },
}

/// An app's secrets
#[derive(Subcommand)]
enum Secrets {
    /// Set APP's secret NAME to stdin, less a trailing newline, whichever release it runs
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
    let s = || store.as_ref().context(NO_STORE);
    let wait = matches!(cmd, Cmd::Release { .. } | Cmd::Apps(_) | Cmd::Secrets(Secrets::Set { .. }));
    match cmd {
        Cmd::Serve { dir, listen, identity } => {
            let identity = identity.map(|i| i.parse()).transpose().map_err(Error::msg)?;
            let apps = match store {
                Some(store) => serve::Apps::install(Arc::new(store), identity).await?,
                None => serve::Apps::dir(&dir)?,
            };
            serve::run(apps, TcpListener::bind(listen).await?).await?
        }
        Cmd::Publish { dir } => cli::publish(s()?, &dir).await.map(|(app, id)| println!("{app} {id}"))?,
        Cmd::Release { app, id } => cli::release(s()?, &app, &id).await?,
        Cmd::Releases { app } => cli::releases(s()?, &app).await?.iter().for_each(|r| println!("{r}")),
        Cmd::Apps(Apps::Add { app, team }) => cli::assign(s()?, &app, Some(&team)).await?,
        Cmd::Apps(Apps::Remove { app }) => cli::assign(s()?, &app, None).await?,
        Cmd::Secrets(Secrets::Set { app, name, recipient }) => {
            let value = std::io::read_to_string(std::io::stdin())?;
            cli::set_secret(s()?, &app, &name, value.strip_suffix('\n').unwrap_or(&value), &recipient).await?
        }
        Cmd::Secrets(Secrets::List { app }) => cli::secrets(s()?, &app).await?.iter().for_each(|n| println!("{n}")),
    }
    if wait {
        tokio::time::sleep(state::FRESH).await; // until every host has the change
    }
    Ok(())
}
