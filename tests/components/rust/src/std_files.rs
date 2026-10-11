//! The `/std-files` route: the ops of `/files`, see files.rs, through Rust's `std::fs`, which on `wasm32-wasip3` is
//! wasi-libc's over `wasi:filesystem` of 0.3, on `/`, which under `/@name` is the name's tree. It is the path most Rust
//! apps take, and `/files` is the binding beneath it: the one tests wasi-libc's use of what the other tests directly.
//!
//! The reply is the same, `{"ok": value}` or `{"err": ..}`, but an error is `<kind>: <message>`, the `io::ErrorKind`
//! that `std` makes of the errno that wasi-libc makes of the `error-code`, as `NotFound: No such file or directory`. A
//! `slice` that runs past the end of the file is an error of `read_exact`, where `/files` is short.
use crate::files::{CHUNK, LATER, OFFSET, fnv, pattern, streamed};
use crate::kv;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use wasip3::clocks::monotonic_clock;
use wasip3::http::types::{ErrorCode, Response};
use wasip3::http_compat::http_into_wasi_response;

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
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.is_symlink() {
                "symlink"
            } else {
                "file"
            };
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
            let (n, chunk) = (num("n"), if num("chunk") > 0 { num("chunk") } else { CHUNK as u64 });
            let mut file = File::create(path).map_err(e)?;
            for start in (0..n).step_by(chunk as usize) {
                let bytes: Vec<u8> = (start..(start + chunk).min(n)).map(pattern).collect();
                file.write_all(&bytes).map_err(e)?;
            }
            json!(n)
        }
        "sum" => {
            let mut file = File::open(path).map_err(e)?;
            let (mut len, mut hash, mut buf) = (0, OFFSET, vec![0; CHUNK]);
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
