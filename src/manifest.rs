//! An app as its developer hands it over: a component, or a directory whose `tric.toml` names one. The middleware the
//! file lists is plugged in front of the component with `wac-graph`, so tric always runs a single component.
use crate::cron::Cron;
use crate::outbound::{self, Allow};
use crate::store;
use http::header::HOST;
use http_body_util::{BodyExt, Empty, Limited};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::{fs, path::Path};
use wac_graph::{CompositionGraph, EncodeOptions, types::Package};
use wasmtime::{Error, Result, ensure, error::Context, format_err};

const MANIFEST: &str = "tric.toml";
const MIDDLEWARE_MAX: usize = 64 << 20;
/// An app's cron jobs: so deploy finds all its schedules, and a failed deploy's too, in one page of Scheduler's 100.
const CRON_MAX: usize = 50;

/// `MANIFEST`. Paths in it are relative to its directory.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)] // a misspelt `allowed_outbound_hosts` would silently grant nothing
struct Manifest {
    component: String,
    #[serde(default)]
    allowed_outbound_hosts: Vec<String>, // as in Spin: `scheme://host[:port]`
    /// Components that each export what the next imports, `wasi:http/handler` most likely, outermost first.
    #[serde(default)]
    middleware: Vec<Middleware>,
    #[serde(default)]
    cron: BTreeMap<String, String>, // crontab fields to a path
}

/// A middleware component, at an http(s) URL or a path, and the SHA-256 of its bytes, as Subresource Integrity pins
/// a script.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Middleware {
    url: String,
    digest: String,
}

/// An app, checked and with its middleware plugged in.
pub struct App {
    pub name: String,
    pub wasm: Vec<u8>,
    pub allowed_outbound_hosts: Vec<String>,
    pub cron: BTreeMap<String, String>,
}

/// Reads the app at `path`: a component, named after its file, or a directory with a `MANIFEST`, named after the
/// directory. `allow` adds to its allowed hosts.
pub async fn read(path: &Path, allow: &[String]) -> Result<App> {
    let path = fs::canonicalize(path).with_context(|| format!("{}", path.display()))?;
    let (name, wasm, mut m) = match path.is_dir() {
        true => {
            let file = path.join(MANIFEST);
            let text = fs::read_to_string(&file).with_context(|| format!("{}", file.display()))?;
            let m: Manifest = toml::from_str(&text).with_context(|| format!("{}", file.display()))?;
            let mut wasm = fs::read(path.join(&m.component)).with_context(|| m.component.clone())?;
            for mw in m.middleware.iter().rev() {
                wasm = plug(wasm, middleware(&path, mw).await?)?;
            }
            (path.file_name(), wasm, m)
        }
        false => {
            (path.file_stem(), fs::read(&path).with_context(|| format!("{}", path.display()))?, Manifest::default())
        }
    };
    let name = name.and_then(|n| n.to_str()).context("the app's name is not UTF-8")?.to_owned();
    m.allowed_outbound_hosts.extend(allow.iter().cloned());
    for item in &m.allowed_outbound_hosts {
        Allow::parse(item)?;
    }
    ensure!(m.cron.len() <= CRON_MAX, "an app has {CRON_MAX} cron jobs or fewer");
    for (fields, path) in &m.cron {
        Cron::parse(fields)?;
        ensure!(path.starts_with('/') && path.parse::<http::uri::PathAndQuery>().is_ok(), "bad cron path {path:?}");
    }
    Ok(App { name, wasm, allowed_outbound_hosts: m.allowed_outbound_hosts, cron: m.cron })
}

/// The bytes of `mw`, if they match its digest.
async fn middleware(dir: &Path, mw: &Middleware) -> Result<Vec<u8>> {
    let want = mw.digest.strip_prefix("sha256:").context("a digest is `sha256:` and the SHA-256 in hex")?;
    let bytes = match mw.url.split_once("://") {
        Some(("http" | "https", _)) => fetch(&mw.url).await.with_context(|| mw.url.clone())?,
        _ => fs::read(dir.join(&mw.url)).with_context(|| mw.url.clone())?,
    };
    ensure!(store::hash(&bytes) == want.to_ascii_lowercase(), "{} does not match its digest", mw.url);
    Ok(bytes)
}

/// GETs `url` as an app's request goes out: from a public address only.
async fn fetch(url: &str) -> Result<Vec<u8>> {
    let uri: http::Uri = url.parse()?;
    let host = uri.authority().context("no host")?.as_str().to_owned();
    let req = http::Request::get(uri).header(HOST, host).body(Empty::new().map_err(|n| match n {}).boxed_unsync())?;
    let (res, _) = outbound::send(req).await.map_err(|e| format_err!("{e:?}"))?;
    ensure!(res.status().is_success(), "answered {}", res.status());
    let body = Limited::new(res.into_body(), MIDDLEWARE_MAX).collect().await.map_err(|e| format_err!("{e}"))?;
    Ok(body.to_bytes().into())
}

/// `outer`, its imports that `inner` exports plugged with `inner`'s; what either imports besides, both import.
fn plug(inner: Vec<u8>, outer: Vec<u8>) -> Result<Vec<u8>> {
    let mut graph = CompositionGraph::new();
    let inner = Package::from_bytes("mw:inner", None, inner, graph.types_mut()).map_err(Error::from_anyhow)?;
    let inner = graph.register_package(inner)?;
    let outer = Package::from_bytes("mw:outer", None, outer, graph.types_mut()).map_err(Error::from_anyhow)?;
    let outer = graph.register_package(outer)?;
    wac_graph::plug(&mut graph, vec![inner], outer)?;
    Ok(graph.encode(EncodeOptions::default())?)
}
