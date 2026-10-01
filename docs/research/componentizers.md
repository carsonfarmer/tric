# Componentizers and WASI-only interfaces (fact sheet)

> A research sub-agent gathered this on 2026-10-01. It built hello-world and config + KV apps with every toolchain below and ran each on plain Wasmtime 49.0.1 (linux aarch64, in Docker), using published tools only.
> The small sources, Dockerfiles and observed WIT are preserved in [componentizers-poc/](componentizers-poc/), excluding build caches and binaries. UNVERIFIED marks claims without a primary source or experiment.
> Build times are wall-clock on one Apple-silicon laptop under Docker. They are indicative. No cloud resources were created.

## Bottom line

**WASI-only is viable today.** Rust, JS/TS, Python, Go, TinyGo and C each produced a component that links and responds on stock `wasmtime serve` 49.0.1 with no Spin code. The only flags are `-S cli`, `-S keyvalue` and `-S config` (the first is needed for any 0.2.x guest). C# is the one toolchain I could not build here.

| # | Point | Evidence |
|---|---|---|
| 1 | Serve **both** HTTP worlds. p2 `incoming-handler@0.2.x` is the only export from JS (StarlingMonkey), TinyGo, C# and `wstd`. p3 `handler@0.3.0` is available from Rust (`wasip3` crate), Python, Go, C and JS (QuickJS backend only). | per-language table |
| 2 | Implement **`wasi:keyvalue@0.2.0-draft` + `wasi:config@0.2.0-rc.1`** first. These are what stock Wasmtime 49 ships, and every experiment linked against them. Developers can then test with `wasmtime serve -S keyvalue -S config` and no spinit. | [Interface versions](#interface-versions) |
| 3 | `0.2.0-draft2` keyvalue (adds CAS; used by Spin 4.2.1) and `0.2.0-draft-2024-09-27` config (Spin) do **not** link on Wasmtime 49. A second `bindgen!` over the same backend is cheap (~90-120 lines) and is the only way to get CAS. | experiments D, E |
| 4 | **No p3 keyvalue exists** upstream. A p3 guest that imports the p2-style sync keyvalue still works on Wasmtime. | [Interface versions](#interface-versions) |
| 5 | No toolchain ships ready-made keyvalue/config bindings. All of them generate from WIT you vendor, so the version is whatever spinit documents. Custom WIT is first-class everywhere except C#. | per-language table |
| 6 | Host cost for WASI-only keyvalue + config on our own store: **~50-60 lines** (store only) to **~90-120** (with atomics and batch), on top of the PoC. | [Host cost](#host-cost-under-wasi-only) |
| 7 | Unused filesystem/socket imports are harmless. They resolve and are denied by a deny-by-default `WasiCtx`: no preopens, no network. | [Imports](#extra-imports-in-the-guests) |

## Per-language table

Latest versions on 2026-10-01. "Wasmtime 49" means `wasmtime serve -S cli -S keyvalue -S config` on plain 49.0.1 with the `wasi:keyvalue@0.2.0-draft` and `wasi:config@0.2.0-rc.1` kv app. Sizes are release builds, hello / config+KV, in bytes.

| Toolchain (version) | p2 export | p3 export | Custom WIT | Ready-made kv/config | Wasmtime 49 | Size hello / kv | Build (cold) |
|---|---|---|---|---|---|---|---|
| Rust `wasm32-wasip2` + `wstd` 0.6.8, `wit-bindgen` 0.62.0 (stable 1.97.1, re-run on 1.99.0) | yes (`#[wstd::http_server]`) | no (wstd HTTP is still p2) | `wit_bindgen::generate!({ world, path, generate_all })` | none; generate from vendored WIT | yes | 237,260 / 244,663 (1.99.0: 240,222 / 247,027) | 6.4 s |
| Rust, `wasip3` 0.9.0 + `http-compat` on `wasm32-wasip2` (stable) | no | yes (`wasip3::http::service::export!`) | same | none | yes | 172,188 / 180,164 (1.99.0: 177,313 / 184,084) | ~3 s |
| Rust `wasm32-wasip3` (nightly 1.101.0 only) + wstd | yes (HTTP still p2) | no | same | none | yes; **runs without `-S cli`** | 259,956 / 267,344 | 2.5 s |
| cargo-component 0.21.1 | p2 | no | `package.metadata.component` | none | not run (stale: crates.io 2025-03-18, last commit 2025-07-14, README says experimental) | n/a | n/a |
| JS/TS jco 1.35.0 + componentize-js (npm 0.23.0; jco pins ^0.22), StarlingMonkey | yes (`addEventListener('fetch')`) | no | `--wit` dir, must be real 0.2.10 WIT | none; `import ... from 'wasi:...'` | yes | 14,365,513 / 14,595,066 | 2.9 s |
| JS/TS jco `--backend qjs` (componentize-qjs 0.4.5) | no | yes (`export const handler = { async handle }`) | minimal world importing `wasi:http/types@0.3.0` | none | yes | 1,699,030 / 1,713,755 | <1 s |
| Python componentize-py 0.25.1 | yes | yes (`componentize_py_async_support`) | `-d wit -w app componentize app -o app.wasm`; bindings module `wit_world` | none | yes | p2 18,424,834 / 18,416,230; p3 21,642,197 / 21,665,551 | ~1.7 s |
| Go componentize-go v0.4.3 (upstream Go 1.27.1) | yes | yes (patched Go fork `go1.27.1-wasi-on-idle` auto-downloaded; build tag `componentizego_async`) | `wit/` dir; `bindings` generates `wasi_*` packages and `export_*` stubs, `build` links | none | yes | p2 2,618,736 / 2,652,085; p3 2,750,491 / 2,784,965 | 2.6 s (p3: 15 s first, for the fork) |
| TinyGo 0.42.0 + go-modules `wit-bindgen-go` v0.7.0 (marked unmaintained upstream) | yes | **no** (wasip1/wasip2 only) | WIT dir; world must include `wasi:cli/imports@0.2.0` | none | yes | 592,354 / 862,348 | 2.7 s |
| C/C++ wasi-sdk 34 + `wit-bindgen` C backend 0.62.0 | yes | yes (callback/state-machine async API; verbose) | `wit-bindgen c` over your world | none | yes; p3 runs **without `-S cli`** | p2 148,074 / 288,146; p3 175,737 / 339,295 | ~0.4 s incl. container |
| C# componentize-dotnet 0.8.0-preview00011 (2026-06-12) | p2 only | no | WIT via MSBuild items | none found | NOT RUN, UNVERIFIED. NuGet lacks a linux-arm64 `ILCompiler.LLVM`, so it is effectively x64-only | n/a | n/a |

Notes:
- **Rust target.** `wasm32-wasip3` is Tier 3. Its prebuilt std is only on nightly; the stable 1.99.0 manifest lists wasip1, wasip1-threads and wasip2 only. The stable p3 path is the `wasip3` crate on `wasm32-wasip2`.
- **JS.** StarlingMonkey is p2 only with embedded 0.2.10 interfaces. Real 0.2.10 WIT is required, so strip `@unstable(feature = network-error-code)` in `sockets/network.wit`. QuickJS (`--backend qjs`) gives p3 at 1.7 MB but has no web globals, `console` or `TextEncoder`. It needs absolute paths. `wasi:http/service@0.3.0` fails there because `wasi:http/client@0.3.0` `send` has an async type mismatch, so use a minimal world.
- **Python.** Output is 18-22 MB because the interpreter is embedded. Hello and kv builds worked at a 1 GB container memory limit (the earlier PoC's OOM kill was on the larger Spin sdk 5.0.0 guest).
- **Go.** `wasip3` needs the patched Go fork. TinyGo has no p3 at all. Upstream Go with `componentize-go` replaces the old `wit-bindgen-go` path.
- **C.** Source is `handler.c` (not `app.c`: `wit-bindgen c` generates `app.c` and overwrites it).
- **App size.** The Rust kv app is 18 lines of code beyond the world.

### Other languages (not built)

| Language | Status |
|---|---|
| MoonBit | `wit-bindgen` has a moonbit backend with async support. UNVERIFIED (not built). |
| Zig | No `wit-bindgen` backend. Possible via `wit-bindgen c` + `zig cc`. UNVERIFIED. |
| Grain | 0.7.2; no component-model support found. |

## Experiment matrix

Runner: [bin/run-case.sh](componentizers-poc/bin/run-case.sh) inside a linux container, with `wasmtime serve -S cli -S keyvalue -S config -S config-var=greeting=hello -S keyvalue-in-memory-data=seed=s1`. The kv case returns `config.greeting=hello kv.seed=<s1> kv.k=<v1>`; all passing runs printed exactly that (Python prints `b's1'`).

| Case | Rust p2 | Rust p3 | JS p2 | JS p3 (qjs) | Py p2 | Py p3 | Go p2 | Go p3 | TinyGo | C p2 | C p3 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A. hello HTTP | pass | pass | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| B. config + kv get/set | pass | pass | pass | pass | pass | pass | pass | pass | pass | pass | pass |

Cases that **do not link** (Rust, same app, different WIT version):

| Case | Error (Wasmtime 49.0.1) |
|---|---|
| D. `wasi:keyvalue/store@0.2.0-draft2` (Spin's version) | component imports instance `wasi:keyvalue/store@0.2.0-draft2`, but ... instance export `bucket` has the wrong type: resource implementation is missing |
| E. `wasi:config/store@0.2.0-draft-2024-09-27` (Spin's version) | component imports instance `wasi:config/store@0.2.0-draft-2024-09-27`, but ... instance export `get` has the wrong type: function implementation is missing |
| F. any 0.2.x-cli guest without `-S cli` | component imports instance `wasi:cli/environment@0.2.x`, but a matching implementation was not found in the linker; instance export `get-environment` has the wrong type; function implementation is missing |
| G. p3 release-candidate guest (e.g. Spin Rust sdk 6, Python sdk 4) | fails to link: Wasmtime 49 implements final `wasi:http@0.3.0` (see [host-and-spin-adapter.md](host-and-spin-adapter.md)) |

Observations:
- **`-S cli` rule.** Components that import only `0.3.0` cli (C p3, Rust wstd on `wasm32-wasip3`) run on vanilla `serve` without `-S cli`. Every guest that imports `0.2.x` cli needs it. A real host calling the full p2 + p3 `add_to_linker` has the same effect: no stubbing for the standard WASI surface.
- **Semver.** 0.2.x guests (0.2.0, 0.2.9, 0.2.10, 0.2.12) all link semver-compatibly on the host's 0.2.x.
- **Wasmtime's kv clones per `open`.** `open("")` clones the preset in-memory data on each open, so a `set` is lost after the bucket drops. It is fine for tests, not for local dev with state.
- **Draft vs draft2 is a toolchain non-issue.** Both compiled in Rust with no code change, only different vendored WIT. The version mismatch only appears at link time.

## Extra imports in the guests

Counted from [observed-wit/](componentizers-poc/observed-wit/) (`wasm-tools component wit`). Hello-world components; kv adds 2 imports.

| Guest | Total imports | `wasi:filesystem` | `wasi:sockets` | Note |
|---|---|---|---|---|
| Rust p2 / p3 | 16 | 0 | 0 | io, clocks, random, cli, http |
| C p2 / p3 | 5 / 3 | 0 | 0 | smallest surface |
| TinyGo | 12 | 2 | 0 | |
| JS p2 / p3 | 18 / 20 | 2 | 0 | |
| Go p2 / p3 | 19 | 2 | 0 | |
| Python p2 / p3 | 27 / 37 | 2 | 7 | sockets imported though unused |

Every one of these resolves on a standard-WASI host, and is denied or empty when `WasiCtx` grants nothing. spinit needs **no stubbing for any of them**, so Q31's "no filesystem or socket access" is enforced by what the host grants, not by rejecting components.

## Interface versions

| Interface | Version | Phase / state | Who implements it |
|---|---|---|---|
| `wasi:keyvalue` | `0.2.0-draft` (tag `v0.2.0-draft`, the only tag) | Phase 2; no CAS; sync | Wasmtime 49.0.1 (in-memory, atomics increment u64, batch); wasmCloud `wash-runtime` plugin (version UNVERIFIED) |
| `wasi:keyvalue` | `0.2.0-draft2` (upstream `main`) | Phase 2; adds a CAS resource and `swap`, s64 increment, string cursor, changed `get-many` shape | Spin 4.2.1; **not** Wasmtime |
| `wasi:keyvalue` | p3 / async | **None.** Champion removed 2025-07-10; last repo commit 2026-08-15 | nobody |
| `wasmcloud:keyvalue` | `0.2.0` (wasmCloud 2.10.1) | wasmCloud's own async interface: async funcs, CAS, expiry | wasmCloud only |
| `wasi:config` | `0.2.0-rc.1` (tag 2025-11-04) | Phase 2; on the `wasi.dev` registry via `wkg` | Wasmtime 49.0.1 |
| `wasi:config` | `0.2.0-draft-2024-09-27` (Spin's vendored `wasi-runtime-config-2024-09-27`) | same WIT content, different version string | Spin |

Hosts seen using other versions are UNVERIFIED: Golem, Cloudflare, Fastly, and the jco keyvalue shim.

**Convergence.** There is none. Wasmtime ships the older draft with `config rc.1`. Spin ships draft2 with the 2024-09-27 config. wasmCloud is moving to its own async interface. All three are Phase 2 with no active p3 work. The toolchains are not a deciding vote because all of them are version-agnostic.

**Two versions are cheap.** Different interface names (`@0.2.0-draft` vs `@0.2.0-draft2`) can live in one linker. The cost is a second `bindgen!` and one more small impl over the same backend trait. It is not a second backend. Third-party reports of a Spin 4.1 draft2 first-insert failure are UNVERIFIED and not reproduced here.

**Recommendation.**
- Ship `wasi:keyvalue@0.2.0-draft` + `wasi:config@0.2.0-rc.1`, served by Wasmtime's own sync model on top of our `KvBackend` (see below).
- Document that the manifest/WIT version is the contract. Interface versions are tied to the host release, not to a toolchain.
- Add `0.2.0-draft2` only if CAS is wanted; it is the only version with `swap`. Then spinit would add its own `bindgen!` for it.
- Do not wait for p3 keyvalue. A p3 app that imports the sync draft works unchanged.

## Host cost under WASI-only

All counts are non-blank, non-comment lines and are ESTIMATES unless marked measured. They sit on top of the existing PoC (`docs/research/host-poc/minihost`), which already has a `KvBackend` trait (get/set/delete/keys).

| Piece | Reuse `wasmtime-wasi-*` crate? | Our lines |
|---|---|---|
| `wasi:config` `rc.1` | **Yes.** `wasmtime-wasi-config` is 69 effective lines (measured). `WasiConfigVariables` (a `HashMap`) and `WasiConfig` + `add_to_linker` are public. Populate the map from our store per request. | ~10-15 |
| `wasi:keyvalue@0.2.0-draft`, store only (open, get, set, delete, exists, list-keys) | **No.** `wasmtime-wasi-keyvalue` 49.0.1 (205 effective lines, measured) has its own `bindgen!` and a private `Bucket.in_memory_data`, so a custom backend needs our own `bindgen!` | ~50-60 |
| + atomics (`increment`) + batch | no | +40-60 |
| + `0.2.0-draft2` second module (CAS, `swap`) | no | +90-120 |
| Shared dispatch (already in the PoC) | n/a | ~145 |
| **WASI-only total (draft only)** | | **~275-310** |
| Spin adapter option (b), for comparison | | ~390 |

(The earlier PoC estimate for WASI-only was ~240-290; this adds the draft kv store and config reuse.)

Notes:
- **Why not reuse the kv crate.** It is in-memory only, clones preset data per `open`, and hides its storage. It is a test aid, not a backend.
- **Sync vs async.** Draft keyvalue is sync, so our async `KvBackend` is called through the PoC's `tokio::task::block_in_place` pattern, or the host uses an async `bindgen!` (`imports: { default: async }`). The PoC v2 section already does the former (~30 lines).

## What developers lose under WASI-only vs Spin compatibility

Under WASI-only they lose the Spin SDK conveniences (the `#[http_component]` macro and router, the `Store` and variables wrappers), the `spin new` templates, and `spin up` / `spin watch` for local iteration. They also lose every non-WASI Spin interface (SQLite, MySQL, Postgres, Redis, MQTT, LLM), because those are `spin:*` interfaces, not WASI. KV and config remain, but as raw `wasi:keyvalue` and `wasi:config` calls with hand-written glue. In practice they use `wit-bindgen`, `wstd`, `componentize-py` or `jco` directly and a router library of their choice. **Spin SDK apps that only use HTTP do still run**: the Rust sdk 5.2 and 7 hellos and the JS hello import only `wasi:*`. The exception is the Python spin-sdk, which imports the whole `spin:up` world, so it needs the PoC's `stub_unknown()` (~20 lines) even for HTTP-only apps. Apps that touch Spin KV or variables need the adapter (see [host-and-spin-adapter.md](host-and-spin-adapter.md)).

## Version table (2026-10-01)

| Tool | Version | | Tool | Version |
|---|---|---|---|---|
| Wasmtime | 49.0.1 (2026-09-24) | | Rust stable / nightly | 1.99.0 / 1.101.0 |
| `wit-bindgen` | 0.62.0 | | `wstd` | 0.6.8 |
| `wasip3` crate | 0.9.0 | | `wasip2` crate | 2.0.1+wasi-0.2.12 |
| `wasi` crate | 0.14.7 | | cargo-component | 0.21.1 |
| jco | 1.35.0 | | componentize-js | 0.23.0 (jco pins ^0.22) |
| componentize-py | 0.25.1 | | componentize-go | v0.4.3 (Go 1.27.1) |
| TinyGo | 0.42.0 | | go-modules `wit-bindgen-go` | v0.7.0 |
| wasi-sdk | 34 | | componentize-dotnet | 0.8.0-preview00011 |
| WASI p2 / p3 | 0.2.12 / 0.3.0 (0.3.1 on 2026-08-11) | | Spin / wasmCloud | 4.2.1 / 2.10.1 |

## Reproduce

Sources: [componentizers-poc/](componentizers-poc/). Standard WASI WIT dependencies (`wasi:http`, `wasi:cli`, `wasi:io`, ...) are **not** copied; fetch them from upstream with `wkg wit fetch` or the tool's own `wit/deps`. Only `wit-common/` (keyvalue draft and draft2, config rc.1) and each app's `wit/world.wit` are included.

| Toolchain | Image | Build |
|---|---|---|
| Rust | [docker/rust.Dockerfile](componentizers-poc/docker/rust.Dockerfile) | `cargo build --release --target wasm32-wasip2` (workspace in `rust/`) |
| Rust nightly | [docker/rust-nightly.Dockerfile](componentizers-poc/docker/rust-nightly.Dockerfile) | `cargo +nightly build --release --target wasm32-wasip3` (needs `rustup target add wasm32-wasip3 --toolchain nightly`) |
| JS | `node:24-bookworm`, `npm i` per [js/package.json](componentizers-poc/js/package.json) | `jco componentize app.js --wit wit -n app -o app.wasm [--backend qjs]` |
| Python | [docker/py.Dockerfile](componentizers-poc/docker/py.Dockerfile) | `componentize-py -d wit -w app componentize app -o app.wasm` |
| Go | [docker/go.Dockerfile](componentizers-poc/docker/go.Dockerfile) | `go mod download go.bytecodealliance.org/pkg && componentize-go -d wit -w app bindings -o . && go mod tidy && componentize-go -d wit -w app build -o app.wasm` (p3: build tag `componentizego_async`, patched Go auto-downloaded) |
| C | [docker/c.Dockerfile](componentizers-poc/docker/c.Dockerfile) | `wit-bindgen c`, then wasi-sdk clang with `--target=wasm32-wasip2` / `-wasip3` |

Then run `bin/run-case.sh <component.wasm> -S cli` in a container that has the Wasmtime 49.0.1 binary, and inspect with `wasm-tools component wit <component.wasm>`.
