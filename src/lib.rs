//! The torpor runtime: runs a WASI component per HTTP request under hard limits. It owns no files, config format or
//! subscriber: guest output and errors go through `tracing`, and the embedder decides where they end up.
mod engine;
mod guest;

pub use {engine::Engine, guest::App};
