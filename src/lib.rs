//! The tric runtime: runs a WASI component per HTTP request under hard limits. It owns no files, config format or
//! subscriber: guest output and errors go through `tracing`, and the embedder decides where they end up.
mod engine;
mod kv;
mod outbound;

pub use engine::{App, Engine};
pub use kv::{NAME_MAX, is_name};
