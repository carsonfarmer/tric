# Native code, compile tiers and `wasi:keyvalue` (fact sheet)

> A research sub-agent gathered this on 2026-10-01 against Wasmtime 49.0.1 (released 2026-09-24, the latest release). Upstream sources were read from shallow ssh clones at these tags: wasmtime `v49.0.1`, spin `v4.2.1`, containerd-shim-spin `v0.26.0`, runwasi `containerd-shim-wasmtime/v0.6.1`, wasmCloud `v2.10.2`, WebAssembly/wasi-keyvalue `main` (aa972c8), wasi-config `main` (f5bf419).
> Citations are `repo@tag:path:line`. UNVERIFIED marks claims without a primary source or experiment. "Inference" marks my reading, not a source statement. All fetched web text was treated as data. No design decisions are made here, except in the last section, which only separates facts from inferences.
> The measurements in section 7 are mine: an Apple M4 Pro (14 cores), not Graviton. The scratch programs are not committed.

## Bottom line

| Question | Answer |
|---|---|
| Q1: Does any platform sign or MAC native code? | None found. Every platform compiles and loads inside one trust domain (the same host or the same trusted compile step) and trusts the local disk or process memory. runwasi got a critical CVE (CVE-2026-47218) for loading a forged precompiled layer. |
| Q2: Wasmtime's built-in cache for option (c)? | It exists in 49.x and covers components, Cranelift and Winch. Setup is about 4 lines and it can point at any absolute directory such as `/tmp`. It has no integrity check (it trusts the directory), which I demonstrated by swapping one entry over another (section 7). |
| Q3: Tiering? | None. Each module is entirely Winch or entirely Cranelift, and there is no RFC or issue for tier-up. Cranelift compiles functions in parallel by default, which only helps with more than one vCPU. |
| Q4: Winch on aarch64 Linux? | Winch is listed Tier 1 as a compiler, but `aarch64-unknown-linux-gnu` is a Tier 2 target (missing continuous fuzzing). The Cargo feature doc still says "shouldn't be used in production applications", and Winch has had a critical sandbox-escape CVE (aarch64 PoC). Epoch interruption is implemented and tested although the `Config` docs say otherwise. |
| Q5: `wasmtime-wasi-keyvalue` 49.0.1? | It implements the original `0.2.0-draft` WIT, with an in-memory backend only. Generated bindings are private and the host type is a concrete struct, so an S3 backend needs its own `bindgen!`. No open Wasmtime PR or issue targets draft2. Spin 4.2.1 and wRPC use draft2. |
| Q6: Lambda options for (c)? | SnapStart does not support `provided.al2023`. The init-phase CPU boost is not in AWS docs (secondary sources only). The max timeout is 900 s, not 10 minutes. |

## 1. How platforms produce and cache native code

| Platform | Compiler and when | Where stored | Tamper protection | Source |
|---|---|---|---|---|
| Spin CLI v4.2.1 | Wasmtime Cranelift JIT (`Component::new`) at `spin up`. The word "winch" does not appear in the Spin repo. | Wasmtime's disk cache, on by default in the default cache dir (`--disable-cache`, `--cache <config>`). | None: trusts the local directory (Wasmtime cache, section 2). | `spin@v4.2.1:crates/trigger/src/cli.rs:35,37,70-87,260-263`; `crates/core/src/lib.rs:50-61` (`Cache::from_file`), `:83` (`epoch_interruption(true)`); `crates/trigger/src/loader.rs:129` (JIT) |
| Spin AOT loader | `Component::deserialize_file` behind Cargo feature `unsafe-aot-compilation`; `unsafe fn enable_loading_aot_compiled_components`. | n/a | The doc says "Precompiled binaries must never be loaded from untrusted sources." Only the shim enables the feature. | `spin@v4.2.1:crates/trigger/src/loader.rs:36-48,55-62` |
| SpinKube (`containerd-shim-spin` v0.26.0 on runwasi) | Cranelift (`spin_core::Config::default().wasmtime_config()`), run once at image pull/first run through runwasi's `Compiler` trait (`cache_key()` is `precompile_compatibility_hash()`). | containerd content store, labelled `runwasi.io/precompiled/<shim>/<cache_key>`. | Trust rests on local containerd metadata; no MAC. AOT loading is enabled only when every layer came from the local precompile cache; precompiled-looking OCI layers are rejected. | `shim-spin@v0.26.0:containerd-shim-spin/src/engine.rs:63-68,113-129,171-185,255-299`; `runwasi@containerd-shim-wasmtime/v0.6.1:crates/containerd-shim-wasm/src/shim/shim.rs:49-67`, `src/containerd/client.rs:45,395-562` |
| `wasmtime serve` / `run` | Cranelift JIT by default. A `.cwasm` is refused unless `--allow-precompiled`. | The Wasmtime disk cache is on by default in the CLI. | `--allow-precompiled` doc: "this option is not safe to pass if the module being passed in is arbitrary user input." | `wasmtime@v49.0.1:src/common.rs:50-58,240-300`; `crates/cli-flags/src/lib.rs:925-933`; `src/commands/serve.rs:562` |
| wasmCloud v2.10.2 (`wash-runtime`) | Cranelift JIT (`Component::new`) at workload load; features `cranelift`, `parallel-compilation`, `pooling-allocator`, no `winch`, no `cache`. | An in-process moka cache of compiled `Component`s keyed by digest (default capacity 100). No on-disk native code. | Not applicable (nothing persisted). | `wasmcloud@v2.10.2:crates/wash-runtime/src/engine/mod.rs:290-300,351-354,698-735,1302,1321-1322` |
| Fastly Compute | Runs customer code "using Wasmtime" (Fastly docs/blog). The CLI compiles source to Wasm before upload. The Wasm to native step, caching and integrity are not publicly documented (UNVERIFIED). Historical: Lucet AOT, instantiation "under 50 microseconds". | Not documented. | Not documented. | <https://www.fastly.com/blog/announcing-lucet-fastly-native-webassembly-compiler-runtime>, <https://www.fastly.com/blog/how-lucet-wasmtime-make-stronger-compiler-together>; `wasmtime@v49.0.1:ADOPTERS.md` lists Fastly as production |
| Fermyon Wasm Functions / Akamai Functions / Fermyon Cloud | "Wasmtime-based runtime", "sub-millisecond startup" (Akamai product page). Compiler, caching and integrity not stated (UNVERIFIED); nothing found for Fermyon Cloud. | Not documented. | Not documented. | <https://www.akamai.com/products/akamai-functions>; `ADOPTERS.md` lists Akamai |
| Cloudflare Workers (contrast) | V8, not Wasmtime. V8 compiles Wasm with Liftoff (baseline, lazily on first call), then TurboFan tier-up; only TurboFan code for modules of 128 kB or more is code-cached. Cloudflare describes a cold start as fetching the source, compiling, running top-level code, then the first invocation. | In-process and V8's code cache; nothing found about Wasm-specific compiled-code storage for Workers. | Not documented. | <https://v8.dev/docs/wasm-compilation-pipeline>, <https://v8.dev/blog/liftoff>, <https://v8.dev/blog/wasm-dynamic-tiering>, <https://v8.dev/blog/wasm-code-caching>, <https://blog.cloudflare.com/eliminating-cold-starts-2-shard-and-conquer/> |

**Real incident: runwasi forged precompiled layer.** GHSA-cc25-vq59-rcjx / CVE-2026-47218 (critical, published 2026-06-17), "Forged Wasmtime precompiled OCI layer bypasses runwasi WASI sandbox". Vulnerable: runwasi `<= v0.6.0`; patched `v0.6.1`. It states: "Precompiled artifacts are native-code cache entries and must only be loaded from a trusted local cache, not from untrusted image content." The fix accepts precompiled artifacts only when they originate from runwasi's local precompile cache, with a regression test (`crates/containerd-shim-wasmtime/src/tests.rs:315-381`). <https://github.com/containerd/runwasi/security/advisories/GHSA-cc25-vq59-rcjx>

**Common pattern (facts, then one inference):**
- Spin CLI, wasmtime CLI, SpinKube and wasmCloud all compile with Cranelift on the machine that runs the code, or at a trusted step on that machine's local store, and none uses Winch.
- Where native code is persisted (Spin CLI, `wasmtime serve`, SpinKube) it sits in a local directory or content store and is trusted because it is local. No platform I could read signs or MACs native code. Wasmtime's own docs recommend a compile-in-control-plane, run-in-data-plane split (section 2).
- Where the origin of a persisted artifact could be attacker-influenced (runwasi OCI layers), the fix was to restrict loading to artifacts produced locally, not to add a MAC.
- Fastly and Akamai are Wasmtime users but their compile and cache internals are undocumented, so the platforms with the strongest claim to "AOT at scale" cannot be cited.
- Inference: (a)/(b) with a MAC would be new ground relative to these platforms; (c) matches wasmCloud and Spin CLI (JIT in the serving process, local cache only).

## 2. Wasmtime's built-in compilation cache (49.0.1)

**What it is (fact).** `wasmtime::Cache` / `CacheConfig` (crate `wasmtime-internal-cache`, documented as internal/unsupported at `crates/cache/src/lib.rs:1-6`), enabled per `Config` with `Config::cache(Option<Cache>)` (`crates/wasmtime/src/config.rs:33-34,~1675-1693`). Off by default for the library, on by default in the CLI (`crates/cli-flags/src/lib.rs:925-933`). Gated on the `cache` Cargo feature, which is in the `wasmtime` crate's default features (`crates/wasmtime/Cargo.toml:131-156`).

**Keying.** SHA-256 over the hashed compile state, base64url as the filename (`crates/cache/src/lib.rs:145-220`). The state is the engine's compile environment (`HashedEngineCompileEnv`, `crates/wasmtime/src/compile/code_builder.rs:846-866`: Wasmtime version, compiler settings, tunables, target), the wasm bytes, DWARF package and unsafe-intrinsics import (`crates/wasmtime/src/compile/runtime.rs:32-43`). Directory: `<cache_dir>/modules/<compiler_name>-<COMPILER_VERSION>[-<exe mtime for git builds>]/<hash>` (`lib.rs:222-252`).

**Components and Winch (fact).** `compile_component` goes through `compile_cached` (`compile/runtime.rs:11-104,137`), and the hit path distinguishes component from module (`:73-81`). Winch works: it sets the `winch_callable` tunable (`crates/wasmtime/src/config.rs:2729-2736`, `crates/environ/src/tunables.rs:129`), so Winch and Cranelift get different keys. I verified both paths run (section 7).

**Compression (fact).** zstd, baseline level 3, written atomically (`lib.rs:264-308,324-333`). A background worker thread, spawned by `Cache::new`, recompresses at level 20 after 0x100 uses, cleans up hourly and writes `.stats` files (`crates/cache/src/worker.rs:1-6`, `config.rs:171-221`). Defaults: soft limits 65,536 files and 512 MiB total (`with_files_total_size_soft_limit` exists), which matches Lambda's default 512 MB `/tmp` size, so the limit would need setting.

**Integrity (fact).** None. The hit path is `fs::read` then `zstd::decode_all` then `load_code_bytes` (`lib.rs:254-262`; `compile/runtime.rs:73-81`); a decode failure is warned and treated as a miss. There is no MAC, signature or content check beyond the filename. `docs/cli-cache.md` makes no security statement. Wasmtime's own docs say, for `Module::deserialize`: "Arbitrary input could, for example, replace valid compiled code with any other valid compiled code, meaning that this can trivially be used to execute arbitrary code otherwise." and "It is the caller's responsibility to provide the guarantee that only previously-serialized bytes are being passed in here." (`crates/wasmtime/src/runtime/module.rs:385-420`; `Component::deserialize*` identical, `runtime/component/component.rs:222-244,259,281`). `docs/examples-pre-compiling-wasm.md` recommends: "Compilation, triggered by the control plane, can happen inside a Wasmtime build that can compile but not run Wasm programs. Execution, in the data plane, can happen inside a Wasmtime build that can run but not compile new Wasm programs."

**Demonstrated (section 7).** I copied component B's cache file over component A's cache filename in a cache directory; a fresh `Engine` then returned B's result for A. So anything that can write the directory can execute native code in the host.

**Pointing it at `/tmp` (inference, built and run as part of the demo):**

```rust
let mut cc = wasmtime::CacheConfig::new();
cc.with_directory("/tmp/wasmtime-cache");          // must be absolute (config.rs:387)
let mut cfg = wasmtime::Config::new();
cfg.cache(Some(wasmtime::Cache::new(cc)?));         // spawns the worker thread
// then Component::new(&engine, wasm_bytes) is cached transparently
```

**Line count (inference).** About 4 lines of configuration and zero extra lines on the load path, versus roughly 40 to 60 lines for a hand-rolled cache (key from `precompile_compatibility_hash` plus the wasm hash, read, zstd, atomic write, fallback to compile). The built-in cache also gets Winch/Cranelift key separation and version isolation for free.

**Caveats (facts unless marked).**
- The cache crate is documented as internal and unsupported (`lib.rs:1-6`), though `wasmtime::Cache` and `CacheConfig` are re-exported publicly.
- The hit path reads the whole file and copies code into a new mapping; it does not mmap the file. Measured hit times are in section 7; `Component::deserialize_file` (mmap) is 0.3 to 4.8 ms for the same artifacts (`state-scaling.md`).
- A separate, experimental, Cranelift-only per-function `CacheStore` exists behind the `incremental-cache` feature (not default; `crates/wasmtime/Cargo.toml:190`; `crates/environ/src/compile/mod.rs:79-88`; `cranelift/codegen/src/incremental_cache.rs:1-8`). It does not apply to Winch. A writable store has the same code-injection exposure (inference).
- Inference: on Lambda the directory only persists inside one execution environment, so a fresh environment always cold-compiles. A bucket-side or shared cache would have to be a different mechanism.

## 3. Tiering, lazy compilation and parallelism

**Tier-up (fact).** There is none. `docs/stability-platform-support.md:34-37`: "Neither Cranelift nor Winch support tiering at this time in the sense of having a WebAssembly module start from a Winch compilation and automatically switch to a Cranelift compilation. Modules are either entirely compiled with Winch or Cranelift." Compilation is synchronous in `Module`/`Component` creation (`crates/wasmtime/src/runtime/module.rs:45-51`). I found no tier-up, lazy-compile or background-recompile RFC or issue (searched `gh api search/issues` in `bytecodealliance/wasmtime` and `bytecodealliance/rfcs`).

**RFC 28 "Baseline Compilation in Wasmtime"** (merged 2023-01-30, bytecodealliance/rfcs PR #28): lines 22-26 call it a first step toward tiering but state it "does not account for tiered compilation". Its estimates (approximate, from Shopify wasm-bench with a Sightglass subset): baseline compilers 15x to 20x faster to compile, generated code 1.1x to 1.5x slower on average.

**Parallel compile (fact).** `Config::parallel_compilation` defaults to true (`config.rs:193,306`, docs at `2216-2219`: "By default parallel compilation is enabled."), implemented with rayon `into_par_iter` per function on the global pool (`crates/wasmtime/src/engine.rs:203-268`; `compile.rs:630-642,678,751,810,1045`). Feature `parallel-compilation` is default (`Cargo.toml:131-156`). `docs/examples-fast-compilation.md` lists the cache, Winch, parallel compilation and pre-compilation as the tips.

**Does it help on Lambda?** Inference plus measurement: Lambda allocates "the equivalent of one vCPU" at 1,769 MB (section 6), so there is no wall-clock gain from threads there; the single-thread column in section 7 is the estimate. At 10,240 MB (6 vCPUs) the 6-thread column applies. What thread count rayon picks inside Lambda's sandbox (it reads `available_parallelism`) is UNVERIFIED; `RAYON_NUM_THREADS` or `rayon::ThreadPoolBuilder::build_global` controls it. Measured scaling plateaus for Python Cranelift at 6 threads (cause not investigated).

## 4. Winch and Cranelift on aarch64 Linux (Wasmtime 49)

### Tiers and policy (quotes from `wasmtime@v49.0.1:docs/`)

- `stability-tiers.md:28-29`, Tier 1 table: "Compiler | Cranelift [^support]" and "Compiler | Winch [^support]". Footnote: "Compiler support is further broken down below into finer-grained target/wasm proposal combinations. Compilers are not required to support the full matrix of all tier 1 targets/proposals."
- `stability-tiers.md:84-93`, Tier 2: "Target | `aarch64-unknown-linux-gnu`| Continuous fuzzing" (the third column is "Missing Tier 1 Requirements").
- `stability-tiers.md:238-261` (aarch64 compiler table): Winch is supported for `component-model`, `simd`, `multi-memory`, `memory64`, `exception-handling`, `custom-page-sizes`; not for `reference-types` (footnote `[^a]` is referenced but never defined in the file), `relaxed-simd`, `threads`, `tail-call`, `function-references`, `gc`, `wide-arithmetic`. Footnote `[^c]` (line 350): "Winch's support for aarch64 is complete for Core Wasm." Cranelift on aarch64 is all supported except `stack-switching`.
- `stability-tiers.md:365-413` on Tier 3: "This baseline level of support notably does not require any degree of testing, fuzzing, or verification." (Winch is Tier 1, so this is not Winch's tier; it is quoted because `wasi-keyvalue` and `wasi-config` are Tier 3.)
- `security-what-is-considered-a-security-vulnerability.md:7-8`: "Bugs must affect [a tier 1 platform or feature] to be considered a security vulnerability." Lines 29-39: runtime denial of service counts; compile-time denial of service does not.
- Inference: read literally, Winch is a Tier 1 compiler, but its aarch64 Linux target is Tier 2, so coverage by the policy is not unambiguous. In practice the project treated an aarch64 Winch bug as a critical advisory (below).
- Stale or conflicting documentation (facts): `docs/stability-platform-support.md:22` still says "Winch supports x86\_64. The aarch64 backend is in development." The `winch` Cargo feature doc says "It is currently in active development and shouldn't be used in production applications." (`crates/wasmtime/Cargo.toml:172-175`), which contradicts the Tier 1 listing. `winch` is not in the `wasmtime` crate's default features.

### Winch status details (facts)

- **Support start:** the Bytecode Alliance blog (Saúl Cabrera, 2025-08-14, <https://bytecodealliance.org/articles/winch-aarch64-support>) says "As of Wasmtime 35, Winch supports AArch64 for Core Wasm proposals, along with additional Wasm proposals like the Component Model and Custom Page Sizes." and "Further testing and development is needed in order to ensure that Winch generated code for AArch64 can correctly handle interrupts e.g., SIGALRM." It makes no fuzzing, performance or production claims.
- **Unsupported-feature gate:** with Winch, `GC`, `FUNCTION_REFERENCES`, `RELAXED_SIMD`, `TAIL_CALL`, `LEGACY_EXCEPTIONS`, `STACK_SWITCHING` (plus `THREADS` on aarch64) are switched off, so guests using them fail validation (`crates/wasmtime/src/config.rs:2473-2491`). SIMD is partial: unimplemented instructions return compile errors (`config.rs:2413-2415`).
- **Epoch interruption:** `Config::epoch_interruption` docs say it is not compatible with Winch (`config.rs:780`), but the code implements it: `winch/codegen/src/codegen/mod.rs:2205-2256` (`maybe_emit_epoch_check`; function entry `:401`, loop headers `winch/codegen/src/visitor.rs:1955`), merged in PR #9737 (2024-12-06), with tests in `tests/disas/winch/x64/epoch/` and `tests/all/epoch_interruption.rs`. So the docs are stale (inference). Other `Config` notes: Winch is incompatible with some options (`config.rs:442,502,2180,3225`) and inlining is Cranelift-only (`:2386-2389`).
- **CI:** there is a native `Test Linux arm64` job (`ci/build-test-matrix.js:151-156`); the spec-test runner includes `Compiler::Winch` for any host that supports it (`tests/wast.rs:27`). Whether aarch64 skips any Winch tests was not checked.
- **Fuzzing:** the random-config generator picks Winch in 1 of 20 configs (`crates/fuzzing/src/generators/config.rs:974-1018`); component-API fuzzing forces Winch back to Cranelift (`crates/fuzzing/src/oracles/component_api.rs:180-182`); the differential target includes Winch (`fuzz/fuzz_targets/differential.rs:41`). OSS-Fuzz config has no `architectures` key, so continuous fuzzing is probably x86-64 only (inference; matches stability-tiers listing aarch64 as lacking continuous fuzzing). Fuzz targets: call_async, compile, component_api, cranelift-fuzzgen, cranelift-icache, differential, exception_ops, gc_ops, instantiate-many, instantiate, misc, oom, wast_tests.
- **Advisories (fact; <https://github.com/bytecodealliance/wasmtime/security/advisories>):**
  - GHSA-xx5w-cvp6-jv83 / CVE-2026-34987 (critical, 2026-04-09): Winch sandbox escape. Affected `>= 25.0.0` up to 36.0.6, 37.0.0 to 42.0.1 and 43.0.0; fixed in 36.0.7, 42.0.2, 43.0.1. The aarch64 PoC works; x86-64 is theoretical. Workaround: upgrade or use Cranelift.
  - Winch CVE-2026-34945 (low), CVE-2026-34946 (medium), CVE-2026-35186 (medium).
  - The four 2026-09-24 advisories fixed in 49.0.1 (GHSA-j2g9-4prp-pf6h, GHSA-c9gc-w9vx-w86p, GHSA-jqpg-j7w6-42pr, GHSA-m63x-6p34-q65x) include fuel accounting; 49.0.0 is vulnerable.
- **Production use:** I found no primary source naming a production Winch user. `ADOPTERS.md` lists production Wasmtime users (Akamai, Fastly, Shopify, others) without naming a compiler. Shopify (2023-07-18) wrote "Shopify is working on Winch to make Wasm compilation faster" (<https://shopify.engineering/contributing-support-for-a-wasm-instruction-to-winch>). Search-engine summaries asserting production Winch use are secondary and unreliable, so none are cited.

### Cranelift on aarch64, for comparison (facts)

- Tier 1 compiler on a Tier 2 target, same as Winch. Full feature matrix except `stack-switching` (`stability-tiers.md:238-261`).
- Epoch interruption is supported (Spin enables it at `spin@v4.2.1:crates/core/src/lib.rs:83`; wasmCloud at `engine/mod.rs:1302`).
- Fuzzing: the fuzz targets listed above exercise Cranelift as the primary compiler (the generator picks Winch in only 1 of 20 configs); the continuous-fuzzing gap for the aarch64 target applies equally.
- Advisory: GHSA-jhxm-h53p-jm7w / CVE-2026-34971 (critical): aarch64 Cranelift miscompile of a 64-bit heap access when memory64 is combined with Spectre mitigation or signals-based traps disabled; affected `>= 32.0.0` through 43.0.0 (non-default settings).
- Production: Wasmtime/Cranelift is widely listed in `ADOPTERS.md`; the per-architecture breakdown is not stated (UNVERIFIED).

## 5. `wasi:keyvalue` and `wasi:config` in Wasmtime 49.0.1

### `wasmtime-wasi-keyvalue` 49.0.1 (crates.io, published 2026-09-24)

- **WIT version:** the original `wasi:keyvalue/imports@0.2.0-draft` (`crates/wasi-keyvalue/wit/world.wit`, deps in `wit/deps/keyvalue/{store,atomic,batch,watch,world}.wit`). Draft shapes: `atomics.increment(bucket, key, delta: u64) -> result<u64, error>` only (no compare-and-swap); `key-response.cursor: option<u64>`; `get-many -> result<list<option<tuple<string, list<u8>>>>, error>`; resource `bucket` (get, set, delete, exists, list-keys), plus `batch` and `watcher`.
- **Backends:** only in-memory. `src/lib.rs:7-9`: "Currently supported storage backends: * In-Memory (empty identifier)". `open("")` clones the in-memory data; any other identifier returns `NoSuchStore` (`lib.rs:163-171`).
- **Are the bindings reusable? No (fact).** The `bindgen!` is inside a private `mod generated` (`lib.rs:67-79`) and is not re-exported. `Bucket` and `Error` are `pub` but `#[doc(hidden)]`. The host type `WasiKeyValue<'a>::new(&WasiKeyValueCtx, &mut ResourceTable)` (`lib.rs:151-161`) is a concrete struct with no backend trait; the `Host` impls are sync (`trappable` errors only; `lib.rs:182-229`); the only public entry is `add_to_linker<T: Send + 'static>(l, f: fn(&mut T) -> WasiKeyValue<'_>)` (`lib.rs:294-303`). The builder only has `in_memory_data` (`lib.rs:106-136`).
- **Inference:** a custom S3 backend cannot reuse `add_to_linker`; it must run its own `bindgen!` against a vendored copy of the WIT, as Spin and wasmCloud do. The `Host` impls for that look like roughly 100 to 150 lines (estimate, not built).
- **Status:** Tier 3, "unstable proposal" (`docs/stability-tiers.md:126-127`). No p3 or async variant in the crate. Used by the CLI only behind non-default features (`src/commands/serve.rs:40-43,62-66,390-393,431-443,510-517`).
- **Open PR or issue for draft2 or newer: none found** (searched `bytecodealliance/wasmtime` for keyvalue and "draft2"). Relevant history:
  - #8983 "Implement wasi-keyvalue" (merged 2024-08); #9062 removed the Redis provider; #9050 moved it to Tier 3.
  - #11187 (open, 2025-07-07) "Persist Wasmtime keyvalue": alexcrichton: "the wasi-keyvalue implementation in Wasmtime has not seen much implementation work beyond the initial inception. The upstream proposal itself is still a draft".
  - #13410 "wasi-keyvalue: add optional redb persistent backend" closed unmerged 2026-05-20. pchickey: "we do not have the capacity to vet a 29kloc dependency, my opinion is that this crate should not land in Wasmtime." He also said that in "the wasip3 era... virtualization will finally work properly for implementations of the keyvalue interface to be made purely as a webassembly component that imports http, sockets or the filesystem." The contributor published standalone `wasmtime-wasi-keyvalue-redb` and `-redis` crates (v0.1.0, 2026-05-30, negligible downloads).
  - #14275 (merged 2026-09-14) added "named imports" for WASI implementations; not keyvalue-specific.
  - PR #13819 (merged 2026-07-08) fixed a `list_keys` cursor clamp and `increment` `checked_add` in the draft crate.

### Upstream `WebAssembly/wasi-keyvalue` (main aa972c8, 2026-08-15)

- **Phase 2.** Champions Dan Chiarlone, David Justice, Jiaxiao Zhou. Portability criteria need two implementations in each of the open-source and proprietary categories. Issue #67 "Champions status": devigned and danbugs say they lack time; ChihweiLHBird offers to help.
- **Tags:** the only tag/release is `v0.2.0-draft`. No tag exists for draft2 (issue #50 "Tag and publish releases"; on 2024-10-22 the publisher held off tagging draft2 pending a change for #47).
- **`wit/world.wit` on main is `package wasi:keyvalue@0.2.0-draft2`** (worlds `imports` and `watch-service`).
- **Draft2 changes (PR #46, 2024-09-23):** `atomics` gains `resource cas { new: static func(bucket, key) -> result<cas, error>; current: func() -> result<option<list<u8>>, error> }`, `variant cas-error { store-error(error), cas-failed(cas) }`, `swap(cas, value) -> result<_, cas-error>`; `increment` takes and returns `s64`; `key-response.cursor` and `list-keys(cursor)` are `option<string>`.
- **PR #55 (merged 2026-08-15):** `get-many` now returns `result<list<tuple<string, option<list<u8>>>>, error>`, one entry per requested key. ChihweiLHBird in that thread: "Spin is actually already on this version."
- **Open issues:** #68 "Asyncify the API" (2026-07-22; should build on WASI 0.3.0, points to wasmCloud's async `cas.wit`); #65 (avoid resources for CAS, fitzgen 2026-02-25); #64 "No config parameters on open"; #52 list-keys cursor as resource; #53/#58/#59/#60 CAS and increment semantics; #47 list-keys portability.
- **p3/async version:** none in the repo; only issue #68 and wasmCloud's own non-WASI package (below).

### Who implements which version (facts)

| Runtime | Version | Backends | Source |
|---|---|---|---|
| Wasmtime crate 49.0.1 | draft (original) | in-memory | above |
| Spin v4.2.1 | `0.2.0-draft2`, vendored in `wit/deps/keyvalue-2024-10-17/` (type-identical to upstream main after #55; only doc comments differ in `batch.wit`; added in commit 470a264ca, 2024-10-31) | sqlite (`key-value-spin`), DynamoDB (`key-value-aws`), Azure, Redis. No S3 backend. `factor-key-value` defines a `Cas` trait (`lib.rs:200`). | `spin@v4.2.1:crates/world/src/lib.rs:16,47-48`, `crates/capabilities/src/lib.rs:77-79`, `crates/key-value-*` |
| wasmCloud v2.10.2 | `wasi:keyvalue/{atomics,batch,store}@0.2.0-draft` (original), via its own bindgen (does not use `wasmtime-wasi-keyvalue`); plus a non-WASI async package `wasmcloud:keyvalue@0.2.0` (`world async-keyvalue`, `async func`, `list-keys(prefix, cursor: option<string>)`, one-shot `cas` interface) for p3 | in_memory, filesystem, NATS, Redis, multiplexed (`src/plugin/wasi_keyvalue/`) | `wasmcloud@v2.10.2:crates/wash-runtime/wit/world.wit:22-26`, `wit/keyvalue/wit/*.wit` |
| bytecodealliance/wrpc | `0.2.0-draft2` (`wrpc-wasi-keyvalue` 0.2.0, 2026-06-15) | mem, redis | `crates/wasi-keyvalue/wit/deps/keyvalue/world.wit` |
| augentic/omnia | draft2 (`omnia-wasi-keyvalue` 0.36.0; a 4-star project, minor data point) | not examined | crate `keyvalue.wit` |

Fact: draft and draft2 are binary-incompatible WIT shapes (u64 versus s64 increment, u64 versus string cursor, CAS present versus absent, `get-many` shape), so a guest built against one does not link against a host that implements the other.

### `wasmtime-wasi-config` 49.0.1

- WIT `wasi:config/imports@0.2.0-rc.1`; `store` has `get(key) -> result<option<string>, error>`, `get-all() -> result<list<tuple<string,string>>, error>`, `error { upstream(string), io(string) }`.
- `src/lib.rs` (153 lines): private bindgen (`imports: { default: trappable }`); public `WasiConfigVariables` (a `HashMap<String,String>` with `new`/`insert`/`FromIterator`), `WasiConfig<'a>::{new, from(&vars)}`, `add_to_linker<T: 'static>(l, f: fn(&mut T) -> WasiConfig<'_>)`. It is a static map, not a pluggable backend trait. Inference: it could be populated from a bucket-held manifest at instantiation time. Tier 3 (`docs/stability-tiers.md:126-127`).
- Upstream `WebAssembly/wasi-config`: Phase 2; tags `v0.2.0-draft` and `v0.2.0-rc.1` (PR #24, 2025-11-04); main is `0.2.0-rc.1`, matching Wasmtime and wasmCloud (`wit/world.wit:9`). The champion was removed 2025-07-10; issues #16, #18, #20, #21 are open; no p3 version. Spin v4.2.1 uses the older `wasi:config/store@0.2.0-draft-2024-09-27` (`crates/world/src/lib.rs:44`), which is not the same version as Wasmtime's crate.

## 6. AWS Lambda options (docs fetched 2026-10-01)

| Topic | Finding | Source |
|---|---|---|
| SnapStart | Supported: Java 11+, Python 3.12+, .NET 8+. "Other managed runtimes (such as `nodejs24.x` and `ruby4.0`) and OS-only runtimes are not supported." `provided.al2023` is OS-only, so not supported. Also not compatible with provisioned concurrency, EFS, or ephemeral storage above 512 MB; only on published versions or aliases. | <https://docs.aws.amazon.com/lambda/latest/dg/snapstart.html> |
| Memory and CPU | 128 to 10,240 MB. "At 1,769 MB, a function has the equivalent of one vCPU." "Up to 6 vCPUs at 10,240 MB" appears in AWS What's New posts (2021-07-28, <https://aws.amazon.com/about-aws/whats-new/2021/07/aws-lambda-supports-10-gb-memory-6-vcpu-cores-bahrain-osaka-hong-kong-regions/>) but not in the current memory or quotas pages. | AWS Lambda configuration-memory and quotas pages |
| Init-phase CPU boost | Not in the AWS docs pages fetched. UNVERIFIED, secondary sources only: van Donkersgoed (2022-04-08, <https://lucvandonkersgoed.com/2022/04/08/lambda-cold-starts-and-bootstrap-code/>) measured init getting unthrottled CPU (128 MB bootstrap 13.6x faster than the same code in the handler; equal at 3,008 MB and 10 GB). Classmethod (2024-01-22, <https://dev.classmethod.jp/articles/lambda-boost-host-cpu-in-init-phase/>) reports the boost covers up to the first 10 s of Init and cites an AWS re:Post statement I did not verify. | secondary |
| Init limit | "The `Init` phase is limited to 10 seconds. If all three tasks do not complete within 10 seconds, Lambda retries the `Init` phase at the time of the first function invocation with the configured function timeout." The limit does not apply to provisioned concurrency, SnapStart or Managed Instances. `/tmp` persists within a warm environment. | <https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html> |
| Timeout | 900 s (15 minutes). The brief said "max 10 minutes"; the current page says 15. | <https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html> |
| Size limits | Zip 50 MB zipped (direct upload) and 250 MB unzipped including layers; 5 layers; container image 10 GB uncompressed; `/tmp` 512 to 10,240 MB; 6 MB synchronous payload. | same quotas page |
| Other | The quotas page also lists "Lambda MicroVMs" (ARM64, 8 h max); I did not investigate it. | same quotas page |

## 7. Measurements (this research)

Setup: scratch program using `wasmtime =49.0.1` with feature `winch`, inputs from the sibling research (`docs/research/state-scaling.md`, "Per-app cold compile"). Apple M4 Pro, 14 cores (10 performance plus 4 efficiency). Thread counts are set with `RAYON_NUM_THREADS` on all 14 cores, which is not the same as a Lambda CPU quota. Graviton is not measured (UNVERIFIED). Each value is the range of 3 in-process `Component::new` runs (compile only; the Winch output was not executed against these guests).

**Compile time, ms (min to max of 3)**

| Component (wasm) | Compiler | 1 thread | 2 threads | 6 threads | 14 threads | Serialized size |
|---|---|---|---|---|---|---|
| Rust medium (1.5 MB) | Cranelift | 418 to 422 | 221 to 229 | 86 to 91 | 57 to 72 | 3.0 MB |
| | Winch | 88 to 97 | 54 to 58 | 25 to 28 | 20 to 24 | 5.1 MB |
| Rust large (2.5 MB) | Cranelift | 766 to 798 | 404 to 406 | 157 to 165 | 100 to 126 | 5.0 MB |
| | Winch | 143 to 148 | 87 to 94 | 40 to 43 | 29 to 31 | 8.9 MB |
| JS (12.8 MB) | Cranelift | 4,744 to 4,778 | 2,436 to 2,519 | 935 to 1,009 | 680 to 735 | 31.1 MB |
| | Winch | 821 to 844 | 507 to 522 | 225 to 258 | 180 to 184 | 56.2 MB |
| Python (37.5 MB) | Cranelift | 5,530 to 5,588 | 2,305 to 2,322 | 1,606 to 1,622 | 1,624 to 1,668 | 33.8 MB |
| | Winch | 866 to 887 | 510 to 543 | 244 to 288 | 198 to 227 | 48.0 MB |

At the same thread count Winch compiled 4.5x (medium) to 6.4x (Python) faster at 1 thread, and produced 1.4x to 1.8x larger native code. The 1-thread Cranelift numbers agree with `state-scaling.md` (424 to 430 ms, 775 to 794 ms, 4,969 to 5,249 ms, 5,602 to 6,050 ms).

**Built-in cache, fresh process, hit path (Cranelift unless noted)**

| Component | Cache file (zstd) | Hit time |
|---|---|---|
| Rust medium | 0.96 MB | 5 to 7 ms |
| JS | 9.2 MB | 46 to 49 ms |
| Python | 10.4 MB | 93 to 95 ms |
| Python, Winch | 10.0 MB | 100 to 109 ms |

A miss costs the compile time plus a synchronous write (first run: medium 70 ms, JS 781 ms, Python 1,778 ms at 14 threads). Cranelift and Winch entries sit under the same `modules/wasmtime-49.0.1/` directory with different hash filenames. Compare `Component::deserialize_file` (mmap), 0.3 to 4.8 ms for the same artifacts.

**Tamper demo (fact, observed):** two tiny WAT components A (`f` returns 1) and B (`f` returns 2), each compiled through the cache so two entries were written. Copying B's entry file over A's entry file, then creating a new `Engine` on the same directory and calling `Component::new` for A's bytes: it was a cache hit and calling `f` returned 2 (B's code). So 49.0.1 performs no authenticity or content check on cache entries.

## Implications for spinit

**Facts that bear on the choice**
- Wasmtime documents `deserialize` of bytes that were not produced by your own compiler as arbitrary code execution, and recommends a compile-in-control-plane, run-in-data-plane split. runwasi shipped, then patched, a critical CVE for exactly this. Wasmtime's built-in cache does not authenticate entries; a writable cache directory is code execution (observed).
- No surveyed platform signs or MACs native code; trust rests on a local disk or on in-process memory.
- Built-in cache: about 4 lines, transparent to `Component::new`, Winch and Cranelift keys separated, components supported, hit costs 5 ms (Rust medium) to about 95 to 109 ms (Python), uses a background worker thread.
- No tier-up exists. Parallel Cranelift compile is on by default and scales with CPU (1 vCPU at 1,769 MB, 6 vCPUs at 10,240 MB per AWS posts).
- Measured on an M4 Pro (not Graviton), single-thread cold compile: Cranelift 0.42 s (medium Rust) to 5.6 s (Python); Winch 0.09 s to 0.89 s. Winch code is 1.4x to 1.8x larger.
- Winch on aarch64 Linux: Tier 1 compiler on a Tier 2 target, no continuous fuzzing for that target, 1-in-20 fuzz-config coverage, one critical sandbox-escape CVE (April 2026) with an aarch64 PoC, a Cargo feature doc advising against production use, and a guest feature gap (no GC, tail-call, function-references, relaxed-simd). Epoch interruption is implemented.
- Cranelift on aarch64 Linux: same target tier, complete feature matrix, one critical CVE under non-default settings.
- `wasmtime-wasi-keyvalue` 49.0.1: draft (not draft2), in-memory only, sync traits, private bindings, no pluggable backend. No open upstream movement to draft2; maintainers have said backends should not land in the Wasmtime tree and that p3-era virtualization is the long-term path. Spin 4.2.1 and wRPC use draft2; wasmCloud's `wasi:keyvalue` uses draft plus its own async package.
- `wasmtime-wasi-config` 49.0.1: `0.2.0-rc.1`, static map, usable as is; Spin uses an older, different draft version.
- Lambda: no SnapStart for `provided.al2023`; 900 s timeout; Init limited to 10 s on on-demand concurrency; the init CPU boost is not documented by AWS.

**Inferences (not source statements)**
- Option (c) with `Config::cache` pointed at `/tmp` needs only a few lines on top of `Component::new`, and the cache is not attacker-reachable provided guests have no filesystem access to the host's `/tmp` and nothing else writes there; a bucket writer cannot influence it. It removes the MAC and any compile-function plumbing from options (a) and (b).
- The cost of (c) is paid on every fresh execution environment for each component it serves: `/tmp` does not outlive the environment. Against the 500 ms cold-start p99 goal, Cranelift at 1 vCPU is out of budget even for the medium Rust component (about 0.42 s on an M4 Pro, before Graviton scaling), whereas Winch at 1 vCPU (0.09 to 0.15 s Rust, about 0.85 s JS and Python) is marginal for Rust only. At higher memory settings, more vCPUs shorten both.
- Choosing Winch trades latency for a weaker assurance level than Cranelift on aarch64 for a runtime that executes untrusted components. The Winch advisory history above is the evidence for that trade-off. A hand-written Winch-first, Cranelift-later scheme is possible with public APIs (separate engines and caches) but is not provided by Wasmtime and was not built or measured.
- For KV, there is nothing in Wasmtime to "lean on" beyond `Linker`, `bindgen!` and `ResourceTable`: an S3-backed `wasi:keyvalue` needs its own bindgen on either draft or draft2. Picking draft2 aligns with Spin and upstream main; picking draft aligns with the Wasmtime crate and wasmCloud's `wasi:keyvalue`. Guests built for one will not instantiate against the other.
- Not measured and worth a spike if (c) is pursued: Winch and Cranelift compile on Graviton at 1 and 6 vCPUs; whether rayon sees the Lambda CPU quota; Winch behaviour running (not only compiling) the JS and Python guests, including epoch interruption on aarch64; and the fraction of a cold start that is a first-request compile under realistic traffic.
