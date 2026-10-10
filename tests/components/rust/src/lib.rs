//! The test fixtures: built with `app`, an app whose routes exercise the host; with `guard`, a middleware.
#[cfg(feature = "app")]
mod app;
#[cfg(feature = "app")]
mod chat;
#[cfg(feature = "app")]
mod files;
#[cfg(feature = "app")]
mod files_p3;
#[cfg(feature = "guard")]
mod guard;
#[cfg(feature = "app")]
mod kv;
