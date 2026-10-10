//! The `/files-p3` route: the ops of `/files`, see files.rs, through `wasi:filesystem` of 0.3, on the preopened `/`,
//! which under `/@name` is the name's tree. What is read, written and listed goes through the streams of the binding,
//! and the rest through the methods of the descriptor. The reply is the same, `{"ok": value}` or `{"err": ..}`, but an
//! error is the Debug of the `error-code`, as `ErrorCode::NoEntry`, where `/files` has the kind of an `io::Error`.
//!
//! Where an op has no `error-code` of its own it is the nearest: text that is not UTF-8 is `illegal-byte-sequence`, an
//! op that is not one is `other`, and no preopen is `no-entry`, as it is to `std`. A `slice` that runs past the end of
//! the file is short, where `read_exact` is an error.
use crate::files::{LATER, OFFSET, fnv, pattern, streamed};
use crate::kv;
use serde_json::{Value, json};
use std::collections::HashMap;
use wasip3::clocks::monotonic_clock;
use wasip3::filesystem::preopens;
use wasip3::filesystem::types::{Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags};
use wasip3::http::types::{ErrorCode as HttpError, Response};
use wasip3::http_compat::http_into_wasi_response;
use wasip3::{wit_bindgen::StreamResult, wit_stream};

/// How much a read asks for, and a `big` writes at once if it is not told.
const CHUNK: usize = 64 << 10;

pub async fn respond(name: Option<&str>, q: &HashMap<String, String>) -> Result<Response, HttpError> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    let root = preopens::get_directories().into_iter().find_map(|(dir, path)| (path == "/").then_some(dir));
    let path = arg("path").to_owned();
    let result = match (root.ok_or(ErrorCode::NoEntry), arg("op")) {
        (Ok(root), "held") => return Ok(streamed(async move { held(root, path).await.unwrap_or_else(failed) })),
        (Ok(root), "late") => return Ok(streamed(async move { late(root, path).await.unwrap_or_else(failed) })),
        (Ok(root), _) => run(&root, q).await,
        (Err(err), _) => Err(err),
    };
    let body = match (result, name.zip(q.get("key"))) {
        (Ok(v), Some((name, key))) => match kv::keep(name, key, arg("value").as_bytes()) {
            Ok(()) => json!({ "ok": v }),
            Err(e) => json!({ "err": e }),
        },
        (Ok(v), None) => json!({ "ok": v }),
        (Err(err), _) => failed(err),
    };
    let status = q.get("status").and_then(|s| s.parse::<u16>().ok()).unwrap_or(200);
    http_into_wasi_response(http::Response::builder().status(status).body(body.to_string()).unwrap())
}

/// `{"err": ..}`: the code, as `ErrorCode::NoEntry`.
fn failed(err: ErrorCode) -> Value {
    json!({ "err": format!("{err:?}") })
}

/// `{"ok": value}` or `{"err": ..}`.
fn outcome<T: Into<Value>>(result: Result<T, ErrorCode>) -> Value {
    result.map_or_else(failed, |v| json!({ "ok": v.into() }))
}

/// `path`, which the guest gives from `/`, as the descriptor of `/` has it: from there.
fn rel(path: &str) -> String {
    match path.trim_start_matches('/') {
        "" => ".".into(),
        path => path.into(),
    }
}

async fn run(root: &Descriptor, q: &HashMap<String, String>) -> Result<Value, ErrorCode> {
    let arg = |k: &str| q.get(k).map_or("", |v| v);
    let num = |k: &str| arg(k).parse::<u64>().unwrap_or(0);
    let (path, to, data) = (arg("path"), arg("to"), arg("data").as_bytes());
    let (read, write, none) = (DescriptorFlags::READ, DescriptorFlags::WRITE, OpenFlags::empty());
    Ok(match arg("op") {
        "write" => json!(put(root, path, [data.to_vec()]).await?),
        "append" => {
            let file = open(root, path, OpenFlags::CREATE, write).await?;
            json!(push(&file, None, [data.to_vec()]).await?)
        }
        "read" => json!(slurp(root, path).await?),
        "ls" => {
            let dir = open(root, path, OpenFlags::DIRECTORY, read).await?;
            let (entries, done) = dir.read_directory();
            let mut names: Vec<_> = entries.collect().await.into_iter().map(|entry| entry.name).collect();
            done.await?;
            names.sort();
            json!({ "count": names.len(), "first": names.first(), "last": names.last() })
        }
        "stat" => {
            let stat = root.stat_at(PathFlags::empty(), rel(path)).await?;
            let kind = match stat.type_ {
                DescriptorType::Directory => "dir",
                DescriptorType::SymbolicLink => "symlink",
                _ => "file",
            };
            json!({ "type": kind, "len": stat.size })
        }
        "mkdir" => json!(root.create_directory_at(rel(path)).await?),
        "mkdirs" => json!(mkdirs(root, path).await?),
        "rm" => json!(root.unlink_file_at(rel(path)).await?),
        "rmdir" => json!(root.remove_directory_at(rel(path)).await?),
        "rmtree" => json!(rmtree(root, path).await?),
        "mv" => json!(root.rename_at(rel(path), root, rel(to)).await?),
        "ln" => json!(root.link_at(PathFlags::empty(), rel(path), root, rel(to)).await?),
        "symlink" => json!(root.symlink_at(to.into(), rel(path)).await?), // what a link says is not a path of `/`
        "readlink" => json!(root.readlink_at(rel(path)).await?),
        "big" => {
            let (n, chunk) = (num("n"), if num("chunk") > 0 { num("chunk") } else { CHUNK as u64 });
            let chunks =
                (0..n).step_by(chunk as usize).map(|start| (start..(start + chunk).min(n)).map(pattern).collect());
            put(root, path, chunks).await?;
            json!(n)
        }
        "sum" => {
            let file = open(root, path, none, read).await?;
            let (mut len, mut hash) = (0, OFFSET);
            pull(&file, 0, |chunk| {
                (len, hash) = (len + chunk.len(), fnv(hash, chunk));
                true
            })
            .await?;
            json!({ "len": len, "fnv": hash.to_string() })
        }
        "patch" => {
            let file = open(root, path, none, write).await?;
            json!(push(&file, Some(num("at")), [data.to_vec()]).await?)
        }
        "slice" => {
            let (file, n) = (open(root, path, none, read).await?, num("n") as usize);
            let mut bytes = Vec::new();
            pull(&file, num("at"), |chunk| {
                bytes.extend_from_slice(&chunk[..chunk.len().min(n - bytes.len())]);
                bytes.len() < n
            })
            .await?;
            json!(bytes)
        }
        "trunc" => {
            let file = open(root, path, none, write).await?;
            json!(file.set_size(num("n")).await?)
        }
        "many" => {
            mkdirs(root, path).await?;
            for i in 0..num("n") {
                put(root, &format!("{path}/f{i:05}"), [i.to_string().into_bytes()]).await?;
            }
            json!(num("n"))
        }
        "incr" => {
            let n = slurp(root, path).await.ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) + 1;
            put(root, path, [n.to_string().into_bytes()]).await?;
            json!(n)
        }
        "orphan" => {
            let file = open(root, path, OpenFlags::CREATE | OpenFlags::TRUNCATE, read | write).await?;
            push(&file, Some(0), [b"abc".to_vec()]).await?;
            root.unlink_file_at(rel(path)).await?;
            let listed = exists(root, path).await?;
            push(&file, None, [b"def".to_vec()]).await?;
            let body = text(&file).await?;
            drop(file);
            json!({ "listed": listed, "text": body, "after": exists(root, path).await? })
        }
        op => return Err(ErrorCode::Other(Some(format!("unknown op {op:?}")))),
    })
}

/// Opens `path`, following a link.
async fn open(
    root: &Descriptor,
    path: &str,
    create: OpenFlags,
    flags: DescriptorFlags,
) -> Result<Descriptor, ErrorCode> {
    root.open_at(PathFlags::SYMLINK_FOLLOW, rel(path), create, flags).await
}

async fn exists(root: &Descriptor, path: &str) -> Result<bool, ErrorCode> {
    match root.stat_at(PathFlags::SYMLINK_FOLLOW, rel(path)).await {
        Ok(_) => Ok(true),
        Err(ErrorCode::NoEntry) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn is_dir(root: &Descriptor, path: &str) -> bool {
    let stat = root.stat_at(PathFlags::SYMLINK_FOLLOW, rel(path)).await;
    stat.is_ok_and(|stat| matches!(stat.type_, DescriptorType::Directory))
}

/// Makes the directory `path`, and those above it that are not.
async fn mkdirs(root: &Descriptor, path: &str) -> Result<(), ErrorCode> {
    let mut at = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        at.push_str(part);
        if !is_dir(root, &at).await {
            root.create_directory_at(at.clone()).await?;
        }
        at.push('/');
    }
    Ok(())
}

/// Removes `path` and all below it, without following links: the files of a directory, then the directory, the deepest
/// first.
async fn rmtree(root: &Descriptor, path: &str) -> Result<(), ErrorCode> {
    let (mut todo, mut dirs) = (vec![rel(path)], vec![]);
    while let Some(dir) = todo.pop() {
        let handle = root.open_at(PathFlags::empty(), dir.clone(), OpenFlags::DIRECTORY, DescriptorFlags::READ).await?;
        let (entries, done) = handle.read_directory();
        let entries = entries.collect().await;
        done.await?;
        for entry in entries {
            let below = format!("{dir}/{}", entry.name);
            match entry.type_ {
                DescriptorType::Directory => todo.push(below),
                _ => root.unlink_file_at(below).await?,
            }
        }
        dirs.push(dir);
    }
    for dir in dirs.into_iter().rev() {
        root.remove_directory_at(dir).await?;
    }
    Ok(())
}

/// Makes or empties the file at `path`, and writes `chunks` to it.
async fn put(root: &Descriptor, path: &str, chunks: impl IntoIterator<Item = Vec<u8>>) -> Result<(), ErrorCode> {
    let file = open(root, path, OpenFlags::CREATE | OpenFlags::TRUNCATE, DescriptorFlags::WRITE).await?;
    push(&file, Some(0), chunks).await
}

/// Writes `chunks` to the stream of a write to `file`, from `at` or, if there is none, at its end, and waits for it to
/// say it is done.
async fn push(file: &Descriptor, at: Option<u64>, chunks: impl IntoIterator<Item = Vec<u8>>) -> Result<(), ErrorCode> {
    let (mut tx, rx) = wit_stream::new();
    let done = match at {
        Some(at) => file.write_via_stream(rx, at),
        None => file.append_via_stream(rx),
    };
    for chunk in chunks.into_iter().filter(|chunk| !chunk.is_empty()) {
        if !tx.write_all(chunk).await.is_empty() {
            break; // the host closed the stream: `done` says why
        }
    }
    drop(tx);
    done.await
}

/// Hands `each` what the stream of a read of `file` from `at` yields, a chunk at a time, until the end or until it
/// says it has enough, and waits for the stream to say how it ended.
async fn pull(file: &Descriptor, at: u64, mut each: impl FnMut(&[u8]) -> bool) -> Result<(), ErrorCode> {
    let (mut rx, done) = file.read_via_stream(at);
    let mut buf = Vec::with_capacity(CHUNK);
    while let (StreamResult::Complete(_), got) = rx.read(buf).await {
        buf = got;
        if !each(&buf) {
            break;
        }
        buf.clear();
    }
    drop(rx);
    done.await
}

/// What `file` holds, as text.
async fn text(file: &Descriptor) -> Result<String, ErrorCode> {
    let mut bytes = Vec::new();
    pull(file, 0, |chunk| {
        bytes.extend_from_slice(chunk);
        true
    })
    .await?;
    String::from_utf8(bytes).map_err(|_| ErrorCode::IllegalByteSequence)
}

/// What the file at `path` holds, as text.
async fn slurp(root: &Descriptor, path: &str) -> Result<String, ErrorCode> {
    text(&open(root, path, OpenFlags::empty(), DescriptorFlags::READ).await?).await
}

/// Unlinks a file that is open, then, after the turn has committed, reads it, and looks for it by name.
async fn held(root: Descriptor, path: String) -> Result<Value, ErrorCode> {
    let file = open(&root, &path, OpenFlags::empty(), DescriptorFlags::READ).await?;
    root.unlink_file_at(rel(&path)).await?;
    monotonic_clock::wait_for(LATER).await;
    Ok(json!({ "read": outcome(text(&file).await), "listed": outcome(exists(&root, &path).await) }))
}

/// Opens a file to write, then, after the turn has answered, writes it, and makes a file, a directory, and removes one.
async fn late(root: Descriptor, path: String) -> Result<Value, ErrorCode> {
    let flags = OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let file = open(&root, &path, flags, DescriptorFlags::WRITE).await?;
    monotonic_clock::wait_for(LATER).await;
    Ok(json!({
        "write": outcome(push(&file, Some(0), [b"late".to_vec()]).await),
        "create": outcome(put(&root, "/late-file", [b"x".to_vec()]).await),
        "mkdir": outcome(root.create_directory_at(rel("/late-dir")).await),
        "remove": outcome(root.unlink_file_at(rel(&path)).await),
    }))
}
