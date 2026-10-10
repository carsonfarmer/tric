//! tric: a scale-to-zero host for WASI HTTP components, with their state in object storage.
mod aws;
mod cron;
mod deploy;
mod dev;
mod engine;
mod kv;
mod lambda;
mod manifest;
mod name;
mod outbound;
mod outbox;
mod retry;
mod route;
mod serve;
mod store;
mod tree;
mod tric;
#[cfg(feature = "ws")]
mod ws;

use clap::{Args, Parser};
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::EnvFilter;
use wasmtime::Result;

#[derive(Parser)]
#[command(version, about)]
enum Cmd {
    /// Serve the app at PATH, at any host, with its state in memory
    Dev {
        #[command(flatten)]
        app: App,
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
    },
    /// Deploy the app at PATH: its component, then its release
    Deploy {
        #[command(flatten)]
        app: App,
        #[arg(long, env = "TRIC_BUCKET")]
        bucket: String,
        /// On AWS, the EventBridge Scheduler group the app's cron goes in
        #[arg(long, env = "TRIC_SCHEDULES", requires_all = ["events", "scheduler_role"])]
        schedules: Option<String>,
        /// On AWS, the router's events function, by its ARN, which the app's schedules invoke
        #[arg(long, env = "TRIC_EVENTS", requires = "schedules")]
        events: Option<String>,
        /// On AWS, the role the app's schedules invoke the router as
        #[arg(long, env = "TRIC_SCHEDULER_ROLE", requires = "schedules")]
        scheduler_role: Option<String>,
    },
    /// Run apps, for the router: each request with the app's tenant id and storage credentials
    Serve {
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// Apps are at `<app>.DOMAIN`, the port included
        #[arg(long, env = "TRIC_DOMAIN")]
        domain: String,
        #[arg(long, env = "TRIC_BUCKET")]
        bucket: String,
        /// The router's outbox: its function's `outbox` alias, by its ARN, or `host:port`
        #[arg(long, env = "TRIC_OUTBOX")]
        outbox: String,
    },
    /// Route requests to serve, each with its app's tenant id and storage credentials; tick cron; relay the outbox
    Route {
        #[arg(long, default_value = "127.0.0.1:3000")]
        listen: SocketAddr,
        /// Where serve hands over delivery events
        #[arg(long, default_value = "127.0.0.1:3001")]
        outbox_listen: SocketAddr,
        /// Apps are at `<app>.DOMAIN`, the port included
        #[arg(long, env = "TRIC_DOMAIN")]
        domain: String,
        #[arg(long, env = "TRIC_BUCKET")]
        bucket: String,
        /// serve: a Lambda function, by its ARN, which runs this router as Lambda's, or `host:port`
        #[arg(long, env = "TRIC_SERVE")]
        serve: String,
        /// The role whose sessions are the apps' credentials; none where STS takes none, as RustFS's
        #[arg(long, env = "TRIC_ROLE")]
        role: Option<String>,
        /// The secret CloudFront sends as `X-Tric-Origin`, without which a request, or a socket's opening, is refused;
        /// a router without one takes only events
        #[arg(long, env = "TRIC_ORIGIN", hide_env_values = true)]
        origin: Option<String>,
        /// On AWS, the EventBridge Scheduler group that a failed delivery's retries are scheduled in
        #[arg(long, env = "TRIC_RETRIES", requires_all = ["retry", "retry_role"])]
        retries: Option<String>,
        /// On AWS, the router's `retry` alias, by its ARN, which the retries invoke
        #[arg(long, env = "TRIC_RETRY", requires = "retries")]
        retry: Option<String>,
        /// On AWS, the role the retries invoke it as
        #[arg(long, env = "TRIC_RETRY_ROLE", requires = "retries")]
        retry_role: Option<String>,
    },
}

#[derive(Args)]
struct App {
    /// A component, or a directory with a tric.toml; the app is named after the file, or the directory
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Allow requests to HOST, as `scheme://host[:port]`, besides those tric.toml allows
    #[arg(long, value_name = "HOST")]
    allow: Vec<String>,
    /// Set an environment variable
    #[arg(short, value_name = "NAME=VALUE", value_parser = var)]
    e: Vec<(String, String)>,
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
        Cmd::Dev { app, listen } => dev::run(&app.path, &app.allow, app.e, listen).await,
        Cmd::Deploy { app, bucket, schedules, events, scheduler_role } => {
            let scheduler = schedules.zip(events.zip(scheduler_role));
            let scheduler = scheduler.map(|(group, (target, role))| deploy::Scheduler { group, target, role });
            deploy::run(&app.path, &app.allow, app.e, &bucket, scheduler).await
        }
        Cmd::Serve { listen, domain, bucket, outbox } => serve::run(listen, domain, bucket, outbox).await,
        Cmd::Route { listen, outbox_listen, domain, bucket, serve, role, origin, retries, retry, retry_role } => {
            let origin = origin.filter(|o| !o.is_empty()); // an empty secret is none
            let retries = retries.zip(retry.zip(retry_role));
            let retries = retries.map(|(group, (target, role))| deploy::Scheduler { group, target, role });
            route::run(listen, outbox_listen, domain, bucket, serve, role, origin, retries).await
        }
    }
}
