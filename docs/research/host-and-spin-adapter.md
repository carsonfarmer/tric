# Minimal host and Spin adapter cost (fact sheet)

> A research sub-agent gathered this on 2026-09-30. It built a working proof-of-concept host on `wasmtime =49.0.1` using published crates only.
> The source, observed WIT and end-to-end logs are preserved in [host-poc/](host-poc/), excluding build caches.
> The release binary is 36 MB on macOS. UNVERIFIED marks claims without a primary source or experiment.

## Bottom line

The Spin adapter is a small, isolated add-on. The real cost of option (b) is version skew between the Spin ecosystem and Wasmtime 49, not the adapter code.

| Piece (non-blank, non-comment lines) | (b) WASI + Spin adapter | (c) WASI-only |
|---|---|---|
| Shared dispatch (`main.rs`): handler state, routing, outbound allow-list hook, semaphore, p2/p3 pre-selection, hyper loop | ~145 | ~145 |
| Spin-only parts of `main.rs`: `spin-*` headers (~15), `stub_unknown` (~20), wiring (~10) | ~45 | 0 |
| `spin_adapter.rs` | 154 (~85 without legacy `fermyon:spin` v2 and `MemKv`) | 0 |
| Manifest | 44 (`spin.toml` + `{{var}}` expansion) | ~30 (own, simpler config) |
| Real KV backend binding | included in the adapter | ~60–90 (estimate) |
| **Total** | **~390** | **~240–290** |

(c) saves about 100–150 lines, which is not a substantial reduction.

The adapter is one module with one backend trait (`KvBackend`) and one `add_to_linker`, so it fits behind a cargo feature.

Raw file sizes of the proof of concept (`wc -l`): `main.rs` 208, `manifest.rs` 48, `spin_adapter.rs` 172.

## `define_unknown_imports_as_traps` (Wasmtime 49)

[docs](https://docs.rs/wasmtime/49.0.0/wasmtime/component/struct.Linker.html); source in `crates/wasmtime/src/runtime/component/linker.rs`, around line 357.

**How it stubs:**
- Functions are stubbed with `func_new` / `func_new_concurrent`.
- Resources become `ResourceType::host::<()>()`.
- A Component or Module import makes it bail.

**Caveat, verified empirically:** it re-opens instances that are already defined and fills in missing exports, including type aliases such as `headers` and `trailers`. The filled-in types don't match the real ones, so every guest that imports `wasi:http/types` then fails to instantiate:
- p3 reports "`headers` has the wrong type".
- p2 reports "failed to convert function to given type".

**Workaround:** `stub_unknown()`, about 20 lines. It walks `component.component_type().imports(engine)` and stubs only the instances the host does *not* implement. Verified on the whole-world Python component.

## What real Spin 4 components import

Observed with `wasm-tools component wit`; see [host-poc/observed-wit/](host-poc/observed-wit/).

| Guest | Exports | Imports |
|---|---|---|
| Rust spin-sdk 7.0.0 (p3) hello | `wasi:http/handler@0.3.0` | `wasi:http/types@0.3.0`, io/clocks/cli/random 0.2.9. **No `spin:*` or `fermyon:*`** |
| Rust sdk 7 "sink" | same | adds only what it uses: `wasi:http/client@0.3.0`, `spin:key-value@3.0.0`, `spin:variables@3.0.0` |
| Rust spin-sdk 5.2.0 (p2) | `wasi:http/incoming-handler@0.2.0` | the sink adds **legacy** `fermyon:spin/key-value@2.0.0`, `fermyon:spin/variables@2.0.0` and `outgoing-handler` |
| JS/TS (jco 1.35, `@spinframework/wasi-http-proxy` 2.0.0), 12.8 MB | `wasi:http/incoming-handler@0.2.10` | wasi 0.2.x only; the KV/variables sink adds `fermyon:spin/key-value@2.0.0` and `variables@2.0.0` |
| Python spin-sdk 4.0.0 (p3 rc), 23.5 MB | `wasi:http/handler@0.3.0-rc-2026-03-15` | **The entire `spin:up` world**: every `spin:*@3.x`, every `fermyon:spin/*@2.0.0`, `wasi:config` draft, `wasi:keyvalue` draft2, sockets, filesystem |

**Not verified:**
- Python spin-sdk 5.0.0, the final p3 release: `componentize-py` was OOM-killed.
- Go: not built.

**Compatibility with Wasmtime 49:**
- 0.2.x guests link against 0.2.9 and 0.2.12 hosts, because semver-compatible versions match. Prerelease versions match only exactly.
- Wasmtime 49 implements the final `wasi:http@0.3.0`. Rust sdk 6.0.0 and Python sdk 4.0.0 p3 release-candidate builds therefore **fail to link**. Rust sdk 7 / Python 5 are required for p3.
- Even a WASI-only host must stub `wasi:config` / `wasi:keyvalue` drafts for whole-world Python guests.

## Reusable pieces

- **`wasmtime serve`:** 1,394 lines ([serve.rs](https://github.com/bytecodealliance/wasmtime/blob/v49.0.1/src/commands/serve.rs)), about 400 of them embedder logic. Single component.
- **`wasmtime-wasi-http` 49:** its `handler` module covers p2 **and** p3 (`ProxyPre` P2|P3, `ProxyHandler`, `HandlerState`, worker expiry).
  - There is no built-in backpressure, so the proof of concept uses a semaphore.
  - Outbound allow-listing goes in `WasiHttpHooks::send_request`, and `default_send_request` is public.
- **Linker:** needs the `p2` + `p3` wasi and wasi-http `add_to_linker` calls.
- **Engine flags:** `wasm_component_model_async`, `_async_stackful`, `_more_async_builtins`.
- **`wasmtime-wasi-keyvalue` 49:** in-memory only, the older draft, no CAS.
- **`wasmtime-wasi-config` 49:** implements `0.2.0-rc.1`, not Spin's `draft-2024-09-27`.
- **No adoptable multi-app host exists on crates.io.** wasmCloud's `wash-runtime` is unpublished, pinned to Wasmtime 31/48, and about 101k lines.

## spin.toml

- **Parser:** a 55-line serde struct parses every test app's `spin.toml`. The host's version is 44 lines including `{{ var }}` expansion.
- **No published crate:** `spin-manifest` is unpublished.
- **Locked-app JSON:** a serde subset is feasible, but it's a second format to track.
- **Behaviour the host must replicate:**
  - Route matching: about 12 lines for exact, prefix and `/...` matches. Spin's own crate is 838 lines, and full semantic parity is UNVERIFIED.
  - The seven `spin-*` request headers.
  - `allowed_outbound_hosts` enforcement: about 15 lines, verified end to end.

## Adapter details

- **Bindings:** `bindgen!` with `imports: { default: trappable }`, and `with:` mapping the store resources to a host `Store`.
- **Rough size by part:**

  | Part | Lines |
  |---|---|
  | v3 KV | ~45 |
  | v3 variables | ~8 |
  | legacy v2 (`block_in_place`) | ~30 |
  | helpers | ~30 |
  | trait + `MemKv` | ~20 |
  | wiring | ~15 |

- **Async WIT:** `spin:*@3.0.0` is async, so the host needs `HostWithStore`, `Accessor` and `StreamReader`/`FutureReader` for `get-keys`.
- **End-to-end runs that worked:** Rust sdk 7 and 5.2 (hello and sink: KV, variables, outbound HTTP), Python 3.4.1 (p2), and JS/TS hello, which instantiate and respond.
- **Wasmtime's built-in stubber fails** on every Rust component.

## Lambda adapter

- **`aws-lambda-web-adapter`:** runs the same hyper server unchanged, and the same binary works on Cloud Run. Zero custom code.
- **`lambda_http`:** needs request/response conversion. Not prototyped.
- **Overhead:** no published per-request or cold-start numbers for either (UNVERIFIED). [lambda-perf](https://github.com/maxday/lambda-perf) shows `provided.al2023` hello-world cold starts of about 70 ms, but nothing for a Wasmtime host.
- **Binary size:** 36 MB on macOS, close to Lambda's 50 MB zipped / 250 MB unzipped limits. Not re-verified for Linux. Guest components are additional: JS 12.8 MB, Python 23.5 MB.
