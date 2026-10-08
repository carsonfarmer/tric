//! The `/kv` route: wasi:keyvalue. The reply is `{"ok": value}` or `{"err": "<Debug of the error>"}`.
//!
//! `/kv?op=OP&key=K&value=V&keys=A,B&cursor=C&delta=N&between=X&n=N`, after `store::open` of the store:
//! - `open`, `get` (value or null), `set`, `delete`, `exists`, `incr` (the new value, delta defaults to 1);
//! - `list` is `{"keys": [..], "cursor": C or null}`;
//! - `cas` is `Cas::new`, `current`, then `swap` to V: `{"seen": current, "swapped": true}`, or `swapped: false` with the
//!   `latest` value from the refreshed handle. `between=X` sets the key to X before the swap, which makes it fail;
//! - `rmw` adds 1 to the number at K with a CAS retry loop, n times (default 1), and is the count of retries;
//! - `get-many` is `[[key, value or null], ..]`, and `set-many` stores V under each of `keys`, as `delete-many` deletes them.
use serde_json::{Value, json};
use std::{collections::HashMap, fmt::Debug};
use wasi::keyvalue::{atomics::{self, Cas, CasError}, batch, store};

wit_bindgen::generate!({
    inline: "package tric:fixture; world kv { include wasi:keyvalue/imports@0.2.0-draft2; }",
    path: ["../../../wit/keyvalue"],
    generate_all,
});

pub fn respond(store: &str, q: &HashMap<String, String>) -> Value {
    match run(store, q) {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": e }),
    }
}

/// Sets `key` of `store` to `value`.
pub fn keep(store: &str, key: &str, value: &[u8]) -> Result<(), String> {
    store::open(store).map_err(e)?.set(key, value).map_err(e)
}

/// Adds `delta` to the number at `key` of `store`, and returns it.
pub fn incr(store: &str, key: &str, delta: i64) -> Result<i64, String> {
    atomics::increment(&store::open(store).map_err(e)?, key, delta).map_err(e)
}

fn e(err: impl Debug) -> String {
    format!("{err:?}")
}

/// `None` when the swap went through, else the refreshed handle.
fn swap(cas: Cas, value: &[u8]) -> Result<Option<Cas>, String> {
    match atomics::swap(cas, value) {
        Ok(()) => Ok(None),
        Err(CasError::CasFailed(cas)) => Ok(Some(cas)),
        Err(CasError::StoreError(err)) => Err(e(err)),
    }
}

fn run(store: &str, q: &HashMap<String, String>) -> Result<Value, String> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    let (key, value) = (arg("key"), arg("value").as_bytes());
    let keys: Vec<String> = arg("keys").split_terminator(',').map(Into::into).collect();
    let text = |v: Option<Vec<u8>>| v.map(|v| String::from_utf8_lossy(&v).into_owned());
    let bucket = store::open(store).map_err(e)?;
    let b = &bucket;
    Ok(match arg("op") {
        "open" => Value::Null,
        "get" => json!(text(b.get(key).map_err(e)?)),
        "set" => json!(b.set(key, value).map_err(e)?),
        "delete" => json!(b.delete(key).map_err(e)?),
        "exists" => json!(b.exists(key).map_err(e)?),
        "list" => {
            let r = b.list_keys(q.get("cursor").map(|c| &**c)).map_err(e)?;
            json!({ "keys": r.keys, "cursor": r.cursor })
        }
        "incr" => json!(atomics::increment(b, key, arg("delta").parse().unwrap_or(1)).map_err(e)?),
        "cas" => {
            let cas = Cas::new(b, key).map_err(e)?;
            let seen = text(cas.current().map_err(e)?);
            if let Some(x) = q.get("between") {
                b.set(key, x.as_bytes()).map_err(e)?;
            }
            match swap(cas, value)? {
                None => json!({ "seen": seen, "swapped": true }),
                Some(c) => json!({ "seen": seen, "swapped": false, "latest": text(c.current().map_err(e)?) }),
            }
        }
        "rmw" => {
            let mut retries = 0;
            for _ in 0..arg("n").parse().unwrap_or(1) {
                let mut cas = Cas::new(b, key).map_err(e)?;
                loop {
                    let n = text(cas.current().map_err(e)?).map_or(0, |s| s.parse::<i64>().unwrap_or(0));
                    match swap(cas, (n + 1).to_string().as_bytes())? {
                        None => break,
                        Some(c) => (cas, retries) = (c, retries + 1),
                    }
                }
            }
            json!(retries)
        }
        "get-many" => json!(batch::get_many(b, &keys).map_err(e)?.into_iter().map(|(k, v)| (k, text(v))).collect::<Vec<_>>()),
        "set-many" => json!(batch::set_many(b, &keys.iter().map(|k| (k.clone(), value.to_vec())).collect::<Vec<_>>()).map_err(e)?),
        "delete-many" => json!(batch::delete_many(b, &keys).map_err(e)?),
        op => return Err(format!("unknown op {op:?}")),
    })
}
