//! tric: a scale-to-zero host for WASI HTTP components, with their state in object storage.
mod cron;
mod dev;
mod engine;
mod kv;
mod manifest;
mod name;
mod outbound;
mod outbox;
mod store;
mod tric;

use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::EnvFilter;
use wasmtime::Result;

#[derive(Parser)]
#[command(version, about)]
enum Cmd {
    /// Serve the app at PATH, at any host, with its state in memory
    Dev {
        /// A component, or a directory with a tric.toml; the app is named after the file, or the directory
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Allow requests to HOST, as `scheme://host[:port]`, besides those tric.toml allows
        #[arg(long, value_name = "HOST")]
        allow: Vec<String>,
        /// Set an environment variable
        #[arg(short, value_name = "NAME=VALUE", value_parser = var)]
        e: Vec<(String, String)>,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
    },
}

fn var(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() && !s.contains('\0') => Ok((k.into(), v.into())),
        _ => Err(format!("{s:?} is not NAME=VALUE")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,tric=info".into());
    match std::env::var_os("AWS_LAMBDA_FUNCTION_NAME") {
        Some(_) => tracing_subscriber::fmt().json().with_env_filter(filter).init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    match Cmd::parse() {
        Cmd::Dev { path, allow, e, listen } => dev::run(&path, &allow, e, listen).await,
    }
}
