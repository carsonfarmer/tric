//! The test fixtures: built with `app`, an app whose routes exercise the host; with `guard`, a middleware.
#[cfg(feature = "app")]
mod app;
#[cfg(feature = "guard")]
mod guard;
#[cfg(feature = "app")]
mod kv;
