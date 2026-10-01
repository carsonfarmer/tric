//! Getting a `Component` by local path or by digest from the bucket, via the cheapest route available:
//! 1. the `.cwasm` cache on local disk (`/tmp` on Lambda), deserialized;
//! 2. a precompiled artifact in the bucket, copied to the cache and deserialized;
//! 3. the wasm blob (`blobs/sha256/<hex>`), compiled with Cranelift and written to the cache.
use anyhow::{Context, Result, ensure};
use object_store::{Error as StoreError, ObjectStore, ObjectStoreExt, path::Path as Key};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Instant};
use wasmtime::{Engine, component::Component};

/// Cache key part: a Wasmtime upgrade must not reuse old artifacts. (A stale file just fails to deserialize and is rebuilt.)
pub const WASMTIME: &str = "49.0.1";

pub struct Source {
    /// A local path, or `sha256:<hex>` for a blob in the bucket.
    pub spec: String,
    pub bucket: Option<Arc<dyn ObjectStore>>,
    pub cache: PathBuf,
    /// Look for a precompiled artifact in the bucket before compiling.
    pub precompiled: bool,
}

/// Bucket key of a component blob (OCI image-layout naming).
pub fn blob_key(hex: &str) -> Key { Key::from(format!("blobs/sha256/{hex}")) }
/// Bucket key of the precompiled artifact for a blob.
pub fn cwasm_key(hex: &str) -> Key { Key::from(format!("cwasm/{}/{WASMTIME}/{hex}", std::env::consts::ARCH)) }

fn us(t: Instant) -> u64 { t.elapsed().as_micros() as u64 }

fn hex(bytes: &[u8]) -> String { Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() }

impl Source {
    fn bucket(&self) -> Result<&Arc<dyn ObjectStore>> { self.bucket.as_ref().context("SPINIT_BUCKET is not set") }

    /// Returns the component and the cold-path phase timings in microseconds.
    pub async fn load(&self, engine: &Engine) -> Result<(Component, Map<String, Value>)> {
        let mut m = Map::new();
        let local = if self.spec.starts_with("sha256:") { None } else {
            let t = Instant::now();
            let bytes = std::fs::read(&self.spec).with_context(|| format!("reading {}", self.spec))?;
            m.insert("fetch_us".into(), us(t).into());
            Some(bytes)
        };
        let digest = match &local {
            Some(bytes) => { let t = Instant::now(); let d = hex(bytes); m.insert("digest_us".into(), us(t).into()); d }
            None => self.spec.trim_start_matches("sha256:").to_string(),
        };
        let cached = self.cache.join(format!("{digest}.{WASMTIME}.cwasm"));

        // 2. precompiled artifact from the bucket
        if !cached.exists() && self.precompiled && self.bucket.is_some() {
            let t = Instant::now();
            match self.bucket()?.get(&cwasm_key(&digest)).await {
                Ok(res) => {
                    let bytes = res.bytes().await?;
                    m.insert("fetch_cwasm_us".into(), us(t).into());
                    self.write_cache(&cached, &bytes, &mut m)?;
                }
                Err(StoreError::NotFound { .. }) => { m.insert("fetch_cwasm_miss_us".into(), us(t).into()); }
                Err(e) => return Err(e.into()),
            }
        }

        // 1. deserialize from the cache
        if cached.exists() {
            let t = Instant::now();
            // SAFETY: the file is written only by this host, from its own compiler output or a bucket we trust.
            if let Ok(c) = unsafe { Component::deserialize_file(engine, &cached) } {
                m.insert("deserialize_us".into(), us(t).into());
                m.insert("cwasm_bytes".into(), std::fs::metadata(&cached)?.len().into());
                m.insert("route".into(), "cwasm".into());
                return Ok((c, m));
            }
            std::fs::remove_file(&cached)?; // stale or corrupt: fall through and rebuild
        }

        // 3. compile the wasm blob
        let bytes = match local {
            Some(bytes) => bytes,
            None => {
                let t = Instant::now();
                let bytes = self.bucket()?.get(&blob_key(&digest)).await?.bytes().await?;
                m.insert("fetch_us".into(), us(t).into());
                let t = Instant::now();
                ensure!(hex(&bytes) == digest, "blob digest mismatch for sha256:{digest}");
                m.insert("digest_us".into(), us(t).into());
                bytes.to_vec()
            }
        };
        let t = Instant::now();
        let component = Component::new(engine, &bytes)?;
        m.insert("compile_us".into(), us(t).into());
        m.insert("wasm_bytes".into(), bytes.len().into());
        let serialized = component.serialize()?;
        m.insert("cwasm_bytes".into(), serialized.len().into());
        self.write_cache(&cached, &serialized, &mut m)?;
        m.insert("route".into(), "compile".into());
        Ok((component, m))
    }

    /// Write-then-rename, so a crash never leaves a half-written artifact.
    fn write_cache(&self, path: &PathBuf, bytes: &[u8], m: &mut Map<String, Value>) -> Result<()> {
        let t = Instant::now();
        std::fs::create_dir_all(&self.cache)?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        m.insert("write_cache_us".into(), us(t).into());
        Ok(())
    }
}

/// `spinit-host precompile <in.wasm> <out.cwasm>`: what a deploy step would upload next to the blob.
pub fn precompile(engine: &Engine, input: &str, output: &str) -> Result<Value> {
    let bytes = std::fs::read(input)?;
    let t = Instant::now();
    let cwasm = engine.precompile_component(&bytes)?;
    let compile_us = us(t);
    std::fs::write(output, &cwasm)?;
    Ok(json!({ "event": "precompile", "digest": hex(&bytes), "compile_us": compile_us, "wasm_bytes": bytes.len(), "cwasm_bytes": cwasm.len() }))
}
