//! torpor: a scale-to-zero host for WASI components.
mod serve;

use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
enum Cmd {
    /// Run the app described by DIR/torpor.toml
    Serve {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().json().with_env_filter(filter).init();
    match Cmd::parse() {
        Cmd::Serve { dir, listen } => serve::run(&dir, TcpListener::bind(listen).await?).await,
    }
}
