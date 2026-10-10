//! The `/files` route: wasi:filesystem of 0.2, through `std::fs`, on `/`, which under `/@name` is the name's tree. The
//! reply is `{"ok": value}` or `{"err": "<kind>: <message>"}`, with `status=N` as for any route. A file's bytes are
//! `pattern(i)`, so a reader can tell any byte from any other, and a block that lands in the wrong place.
//!
//! `/files?op=OP&path=P&to=T&data=D&n=N&at=A&chunk=C&key=K&value=V`:
//! - `write` stores D at P, and `append` adds it; `read` is P as text; `ls` is `{"count", "first", "last"}` of the
//!   entries of P; `stat` is `{"type": "file" | "dir" | "symlink", "len"}`;
//! - `mkdir` (and `mkdirs`), `rm`, `rmdir`, `rmtree` take P; `mv` renames P to T, `ln` links T to P, `symlink` makes P
//!   a symlink to T, and `readlink` is P's target;
//! - `big` writes N bytes of the pattern at P in chunks of C (64 KiB by default); `sum` is `{"len", "fnv"}`, the length
//!   and FNV-1a of P, which is read in chunks; `patch` writes D at the offset A of P; `slice` is N bytes of P from A;
//!   `trunc` sets the length of P to N;
//! - `many` makes N files in the directory P, named `f00000` and on, each holding its number;
//! - `incr` adds one to the number in P, which is none to begin with, and is the new number;
//! - `orphan` unlinks a file it has open, reads and writes it, and closes it; `held` and `late` are answered before
//!   they are done, and say what they found then: `held` unlinks a file it has open, and reads it, and `late` writes
//!   to a file it has open, and makes others.
//!
//! With `key`, an op also sets that key of the name to `value`, in the turn.
use crate::kv;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use wasip3::http::types::{ErrorCode, Fields, Response};
use wasip3::http_compat::http_into_wasi_response;
use wasip3::{clocks::monotonic_clock, wit_future, wit_stream};

/// How long `held` and `late` wait, which is for the turn to answer, and to commit.
pub(crate) const LATER: u64 = 500_000_000;

pub(crate) fn pattern(i: u64) -> u8 {
    (i % 251) as u8
}

/// FNV-1a of `bytes`, as an accumulator: start from `OFFSET`.
pub(crate) fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) const OFFSET: u64 = 0xcbf29ce484222325;

pub fn respond(name: Option<&str>, q: &HashMap<String, String>) -> Result<Response, ErrorCode> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    let body = match arg("op") {
        "held" => return Ok(streamed(held(arg("path").to_owned()))),
        "late" => return Ok(streamed(late(arg("path").to_owned()))),
        _ => match (run(q), name.zip(q.get("key"))) {
            (Ok(v), Some((name, key))) => match kv::keep(name, key, arg("value").as_bytes()) {
                Ok(()) => json!({ "ok": v }),
                Err(e) => json!({ "err": e }),
            },
            (Ok(v), None) => json!({ "ok": v }),
            (Err(e), _) => json!({ "err": e }),
        },
    };
    let status = q.get("status").and_then(|s| s.parse::<u16>().ok()).unwrap_or(200);
    http_into_wasi_response(http::Response::builder().status(status).body(body.to_string()).unwrap())
}

fn e(err: io::Error) -> String {
    format!("{:?}: {err}", err.kind())
}

/// `{"ok": value}` or `{"err": ..}`.
fn outcome<T: Into<Value>>(result: io::Result<T>) -> Value {
    result.map_or_else(|err| json!({ "err": e(err) }), |v| json!({ "ok": v.into() }))
}

fn run(q: &HashMap<String, String>) -> Result<Value, String> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    let num = |k: &str| arg(k).parse::<u64>().unwrap_or(0);
    let (path, to, data) = (arg("path"), arg("to"), arg("data").as_bytes());
    Ok(match arg("op") {
        "write" => json!(fs::write(path, data).map_err(e)?),
        "append" => {
            let mut file = OpenOptions::new().append(true).create(true).open(path).map_err(e)?;
            json!(file.write_all(data).map_err(e)?)
        }
        "read" => json!(fs::read_to_string(path).map_err(e)?),
        "ls" => {
            let mut names = vec![];
            for entry in fs::read_dir(path).map_err(e)? {
                names.push(entry.map_err(e)?.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            json!({ "count": names.len(), "first": names.first(), "last": names.last() })
        }
        "stat" => {
            let meta = fs::symlink_metadata(path).map_err(e)?;
            let kind = if meta.is_dir() { "dir" } else if meta.is_symlink() { "symlink" } else { "file" };
            json!({ "type": kind, "len": meta.len() })
        }
        "mkdir" => json!(fs::create_dir(path).map_err(e)?),
        "mkdirs" => json!(fs::create_dir_all(path).map_err(e)?),
        "rm" => json!(fs::remove_file(path).map_err(e)?),
        "rmdir" => json!(fs::remove_dir(path).map_err(e)?),
        "rmtree" => json!(fs::remove_dir_all(path).map_err(e)?),
        "mv" => json!(fs::rename(path, to).map_err(e)?),
        "ln" => json!(fs::hard_link(path, to).map_err(e)?),
        #[allow(deprecated)] // `std::os::wasi::fs::symlink_path` is not stable
        "symlink" => json!(fs::soft_link(to, path).map_err(e)?),
        "readlink" => json!(fs::read_link(path).map_err(e)?.display().to_string()),
        "big" => {
            let (n, chunk) = (num("n"), if num("chunk") > 0 { num("chunk") } else { 64 << 10 });
            let mut file = File::create(path).map_err(e)?;
            for start in (0..n).step_by(chunk as usize) {
                let bytes: Vec<u8> = (start..(start + chunk).min(n)).map(pattern).collect();
                file.write_all(&bytes).map_err(e)?;
            }
            json!(n)
        }
        "sum" => {
            let mut file = File::open(path).map_err(e)?;
            let (mut len, mut hash, mut buf) = (0, OFFSET, vec![0; 64 << 10]);
            loop {
                let n = file.read(&mut buf).map_err(e)?;
                if n == 0 {
                    break json!({ "len": len, "fnv": hash.to_string() });
                }
                (len, hash) = (len + n, fnv(hash, &buf[..n]));
            }
        }
        "patch" => {
            let mut file = OpenOptions::new().write(true).open(path).map_err(e)?;
            file.seek(SeekFrom::Start(num("at"))).map_err(e)?;
            json!(file.write_all(data).map_err(e)?)
        }
        "slice" => {
            let mut file = File::open(path).map_err(e)?;
            file.seek(SeekFrom::Start(num("at"))).map_err(e)?;
            let mut buf = vec![0; num("n") as usize];
            file.read_exact(&mut buf).map_err(e)?;
            json!(buf)
        }
        "trunc" => json!(OpenOptions::new().write(true).open(path).and_then(|f| f.set_len(num("n"))).map_err(e)?),
        "many" => {
            fs::create_dir_all(path).map_err(e)?;
            for i in 0..num("n") {
                fs::write(format!("{path}/f{i:05}"), i.to_string()).map_err(e)?;
            }
            json!(num("n"))
        }
        "incr" => {
            let n = fs::read_to_string(path).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) + 1;
            fs::write(path, n.to_string()).map_err(e)?;
            json!(n)
        }
        "orphan" => {
            let mut options = OpenOptions::new();
            let mut file = options.read(true).write(true).create(true).truncate(true).open(path).map_err(e)?;
            file.write_all(b"abc").map_err(e)?;
            fs::remove_file(path).map_err(e)?;
            let (listed, mut text) = (fs::exists(path).map_err(e)?, String::new());
            file.write_all(b"def").map_err(e)?;
            file.seek(SeekFrom::Start(0)).map_err(e)?;
            file.read_to_string(&mut text).map_err(e)?;
            drop(file);
            json!({ "listed": listed, "text": text, "after": fs::exists(path).map_err(e)? })
        }
        op => return Err(format!("unknown op {op:?}")),
    })
}

/// A response whose body is what `body` gives, once it is done: after the handler has answered.
pub(crate) fn streamed(body: impl Future<Output = Value> + 'static) -> Response {
    let (mut tx, rx) = wit_stream::new();
    let (done, trailers) = wit_future::new(|| Ok(None));
    wasip3::wit_bindgen::spawn_local(async move {
        tx.write_all(body.await.to_string().into_bytes()).await;
        drop(tx);
        _ = done.write(Ok(None)).await;
    });
    Response::new(Fields::new(), Some(rx), trailers).0
}

/// Unlinks a file that is open, then, after the turn has committed, reads it, and looks for it by name.
async fn held(path: String) -> Value {
    let opened = File::open(&path).and_then(|file| fs::remove_file(&path).map(|()| file));
    let mut file = match opened {
        Ok(file) => file,
        Err(err) => return json!({ "err": e(err) }),
    };
    monotonic_clock::wait_for(LATER).await;
    let mut text = String::new();
    let read = file.read_to_string(&mut text).map(|_| text);
    json!({ "read": outcome(read), "listed": outcome(fs::exists(&path)) })
}

/// Opens a file to write, then, after the turn has answered, writes it, and makes a file, a directory, and removes one.
async fn late(path: String) -> Value {
    let opened = OpenOptions::new().write(true).create(true).truncate(true).open(&path);
    let mut file = match opened {
        Ok(file) => file,
        Err(err) => return json!({ "err": e(err) }),
    };
    monotonic_clock::wait_for(LATER).await;
    json!({
        "write": outcome(file.write_all(b"late").and_then(|()| file.flush())),
        "create": outcome(fs::write("/late-file", b"x")),
        "mkdir": outcome(fs::create_dir("/late-dir")),
        "remove": outcome(fs::remove_file(&path)),
    })
}
