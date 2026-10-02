//! The `kv` routes (also built into the Spin fixture): wasi:keyvalue and wasi:config. Every response is `{"ok": value}` or
//! `{"err": "<Debug of the error>"}`, with query parameters percent-decoded.
//!
//! - `/config?key=K` is the value of K or null; without `key` it is an object of every key.
//! - `/kv?op=OP&store=S&key=K&value=V&keys=A,B&cursor=C&delta=N&between=X&n=N`, after `store::open(S)`:
//!   - `open`, `get` (value or null), `set`, `delete`, `exists`, `incr` (the new value, delta defaults to 1);
//!   - `list` is `{"keys": [..], "cursor": C or null}`;
//!   - `cas` is `Cas::new`, `current`, then `swap` to V: `{"seen": current, "swapped": true}`, or `swapped: false` with the
//!     `latest` value from the refreshed handle. `between=X` sets the key to X before the swap, which makes it fail;
//!   - `rmw` adds 1 to the number at K with a CAS retry loop, n times (default 1), and is the count of retries;
//!   - `get-many` is `[[key, value or null], ..]`, and `set-many` stores V under each of `keys`, as `delete-many` deletes them.
use crate::wasi::{config::store as config, keyvalue::{atomics::{self, Cas, CasError}, batch, store}};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::{BTreeMap, HashMap}, fmt::Debug};

pub fn respond(path: &str, query: &str) -> Option<String> {
    if !matches!(path, "/kv" | "/config") {
        return None;
    }
    let q = form_urlencoded::parse(query.as_bytes()).collect();
    Some(match run(path, &q) { Ok(v) => json!({ "ok": v }), Err(e) => json!({ "err": e }) }.to_string())
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

fn run(path: &str, q: &HashMap<Cow<str>, Cow<str>>) -> Result<Value, String> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    if path == "/config" {
        return Ok(match q.get("key") {
            Some(key) => json!(config::get(key).map_err(e)?),
            None => json!(config::get_all().map_err(e)?.into_iter().collect::<BTreeMap<_, _>>()),
        });
    }
    let (key, value) = (arg("key"), arg("value").as_bytes());
    let keys: Vec<String> = arg("keys").split_terminator(',').map(Into::into).collect();
    let text = |v: Option<Vec<u8>>| v.map(|v| String::from_utf8_lossy(&v).into_owned());
    let bucket = store::open(arg("store")).map_err(e)?;
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
