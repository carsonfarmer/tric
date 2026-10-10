//! The WASI conformance tests of the file system, on the file system as an app has it: the wasi-testsuite's tests for
//! WASI 0.1, in Rust and in C, that name a directory to preopen. Each is a command, which the adapter of the Wasmtime
//! release tric pins makes a component of WASI 0.2. It runs on a name of its own, in a turn that holds the files the
//! test starts with, and the turn is committed after it.
//!
//! The suite and the adapter are fetched, by hash, into the toolchain image (docker/build.Dockerfile), where
//! `TRIC_CONFORMANCE` is the directory they are in. Without it the test is skipped. The host needs an app to be a
//! [`Tric`], so the test needs the fixtures (tests/components/build.sh) too: it is a unit test for the sake of the
//! engine and the host, which an integration test cannot reach.
//!
//! Wasmtime's own runner expects every one of these to pass on Linux, with the directory as `/`, and no environment
//! beyond the test's: so does this, and [`FAILS`] lists those that do not, and why.
use super::{At, Handle, Mode};
use crate::engine::{App, Engine};
use crate::name::{Committed, Turn};
use crate::outbox::Sink;
use crate::store::Store;
use crate::tric::{Ctx, Tric};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

/// The tests that fail, by `language/name`, and why. A test that is here and passes fails the run, so the list does
/// not outlive its reasons.
const FAILS: &[(&str, &str)] = &[];

/// How many tests there are in the pinned suite that name a directory to preopen.
const TESTS: usize = 49;

/// How long a test runs.
const LIMIT: Duration = Duration::from_secs(60);

/// A test's `.json`.
#[derive(Deserialize)]
struct Config {
    /// The directory, next to the test, whose files are the preopened `/`.
    root: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    exit_code: i32,
}

/// What a test left of itself when it failed.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let from = text.char_indices().rev().nth(1999).map_or(0, |(i, _)| i);
    text[from..].trim_end().to_owned()
}

/// Makes the files below `from` the files of `root`.
async fn seed(root: &Handle, from: &Path) {
    let mode = Mode { follow: true, create: true, truncate: true, write: true, ..Default::default() };
    let mut todo = vec![(from.to_owned(), String::new())];
    while let Some((dir, prefix)) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = format!("{prefix}{}", entry.file_name().to_str().unwrap());
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                root.mkdir_at(&path).await.unwrap();
                todo.push((entry.path(), format!("{path}/")));
            } else {
                assert!(kind.is_file(), "{} is neither a file nor a directory", entry.path().display());
                let file = root.open(&path, mode).await.unwrap();
                file.write(At::Offset(0), &std::fs::read(entry.path()).unwrap()).await.unwrap();
            }
        }
    }
}

/// Runs the test `wasm`, which has `config`, and says why it failed if it did.
async fn run(engine: &Engine, app: &Arc<App>, adapter: &Path, wasm: &Path, config: &Config) -> Result<(), String> {
    let adapt = format!("wasi_snapshot_preview1={}", adapter.display());
    let made = Command::new("wasm-tools").args(["component", "new"]).arg(wasm).args(["--adapt", &adapt]).output().await;
    let made = made.map_err(|e| format!("wasm-tools: {e}"))?;
    if !made.status.success() {
        return Err(format!("wasm-tools: {}", tail(&made.stderr)));
    }
    let component = engine.compile(&made.stdout).map_err(|e| format!("compile: {e:#}"))?;

    let tric = Arc::new(Tric {
        app: "a".into(),
        store: Store::memory(),
        code: app.clone(),
        allow: Arc::from([]),
        sink: Arc::new(|_| Box::pin(async { Ok(()) })),
    });
    let turn = Turn::open(&tric.store, "a", "n", false, &Default::default(), Instant::now()).await;
    let turn = turn.map_err(|e| format!("open: {e:#}"))?.ok().ok_or("open: refused")?;
    if let Some(root) = &config.root {
        seed(&Handle::root(&turn), &wasm.with_file_name(root)).await;
    }

    let program = wasm.file_name().unwrap().to_string_lossy().into_owned();
    let args: Vec<String> = std::iter::once(program).chain(config.args.iter().cloned()).collect();
    let env: Vec<(String, String)> = config.env.clone().into_iter().collect();
    let ctx = Ctx::of_command(tric, turn.clone(), "n");
    let ran = timeout(LIMIT, engine.run_command(&component, ctx, &args, &env)).await;
    let (code, out, err) = match ran {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => {
            turn.discard().await;
            return Err(format!("trap: {e:#}"));
        }
        Err(_) => {
            turn.discard().await;
            return Err(format!("ran past {LIMIT:?}"));
        }
    };
    let sink: Sink = Arc::new(|_| Box::pin(async { Ok(()) }));
    match turn.commit("h", &sink).await {
        Ok(Committed::Done(_)) => {}
        Ok(Committed::Conflict) => return Err("its commit lost a race, with no one else there".into()),
        Err(e) => return Err(format!("commit: {e:#}")),
    }
    if code == config.exit_code {
        return Ok(());
    }
    Err(format!("exit {code}, not {}\n  stdout: {}\n  stderr: {}", config.exit_code, tail(&out), tail(&err)))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wasi_testsuite_passes() {
    let Some(dir) = std::env::var_os("TRIC_CONFORMANCE").map(PathBuf::from) else {
        eprintln!("TRIC_CONFORMANCE is not set: the WASI conformance tests are skipped");
        return;
    };
    let engine = Engine::new().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app.wasm");
    let wasm =
        std::fs::read(&fixture).unwrap_or_else(|e| panic!("{}: {e}: build the fixtures first", fixture.display()));
    let app = Arc::new(engine.load(&engine.compile(&wasm).unwrap(), vec![]).unwrap());

    let (mut passed, mut unexpected, mut ran) = (0, vec![], vec![]);
    for language in ["rust", "c"] {
        let suite = dir.join(language).join("testsuite/wasm32-wasip1");
        let mut tests: Vec<PathBuf> = std::fs::read_dir(&suite).unwrap().map(|e| e.unwrap().path()).collect();
        tests.retain(|p| p.extension().is_some_and(|x| x == "wasm"));
        tests.sort();
        for wasm in tests {
            let Ok(json) = std::fs::read_to_string(wasm.with_extension("json")) else { continue };
            let config: Config = serde_json::from_str(&json).unwrap();
            if config.root.is_none() {
                continue;
            }
            let id = format!("{language}/{}", wasm.file_stem().unwrap().to_string_lossy());
            let started = Instant::now();
            let result = run(&engine, &app, &dir.join("adapter.wasm"), &wasm, &config).await;
            eprintln!("{id}: {} in {:?}", if result.is_ok() { "ok" } else { "FAILED" }, started.elapsed());
            let known = FAILS.iter().find(|(name, _)| *name == id);
            match (result, known) {
                (Ok(()), None) => passed += 1,
                (Ok(()), Some((_, why))) => unexpected.push(format!("{id} passes, though it is listed to fail: {why}")),
                (Err(_), Some(_)) => {}
                (Err(why), None) => unexpected.push(format!("{id}: {why}")),
            }
            ran.push(id);
        }
    }
    eprintln!("{passed} of {} passed, {} listed to fail", ran.len(), FAILS.len());
    for (name, _) in FAILS {
        assert!(ran.iter().any(|id| id == name), "{name} is listed to fail, but is not a test");
    }
    assert!(unexpected.is_empty(), "the WASI conformance tests:\n{}", unexpected.join("\n"));
    assert_eq!(ran.len(), TESTS, "the suite is pinned to a commit with {TESTS} tests that need a directory");
}
