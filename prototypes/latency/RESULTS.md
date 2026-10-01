# Milestone 0: latency spike

Status (2026-10-01): the local phase is done, including the follow-up Winch and artifact-MAC measurements and a runaway-guest check of the epoch deadline and the memory cap under Winch (components are now treated as untrusted). That check found a host bug, the epoch ticker starved by a runaway guest at one worker, now fixed with an OS thread. The cloud phase ran on 2026-10-01 in us-west-2 (n = 30 cold starts per cell), and its infrastructure is destroyed. The decisions below are taken from the cloud numbers. The local numbers are kept for comparison, and the local sections say what was expected before the cloud run.

## What the spike decides

| Decision | Rule | Status |
|---|---|---|
| Bucket KV or DynamoDB for app state | Ship bucket KV only if the warm read p50 is at most 30 ms with revalidation (conditional GET, 304) and the acknowledged write p99 is at most 200 ms (conditional PUT) | **Bucket KV.** 304 p50 9 to 10 ms, plain GET 23 to 26 ms, write p99 51 to 139 ms at every memory size |
| Web Adapter (LWA) or `lambda_http` | Switch to `lambda_http` if LWA adds more than about 5 ms to the warm p50 or about 50 ms to a cold start | **Keep LWA.** Warm p50 overhead 1.2 to 1.4 ms at 512 and 1769 MB. Cold: the whole first request after an eager load is 40 ms |
| Precompile strategy | Compile in the function, or deserialize a precompiled `.cwasm` | **Precompile.** Cranelift at cold start is 687 / 912 ms (`hello_p3`, 1769 MB) to 6.8 s (128 MB), and 15.5 s for JS. Deserializing a stored artifact works on Graviton |
| Drop stored native code (Winch at cold start) | Only if `compile-winch` meets the 500 ms cold p99 at the memory size in use | **No: keep stored native code.** Winch misses the 500 ms p99 at every size (`hello_p3` 580 ms at 1769 MB, 1176 at 512; JS 3.6 to 11 s), and aarch64 is a Tier 2 target (see "Winch: runaway guests and the memory cap") |
| MAC on stored `.cwasm` (components untrusted) | The check before `deserialize` should cost far less than the fetch | **Yes.** Locally, HMAC-SHA256 with the SHA-2 instructions takes 23 ms for the 33.5 MB JS artifact and about 1 ms for the Rust ones. That is far below the cloud fetch (94 to 211 ms for 1 MB, 165 to 578 ms for JS). Key handling is open |
| Targets | Cold start p99 at most 500 ms. Warm read p50 at most 30 ms inside the function. Acknowledged write p99 at most 200 ms | Reads and writes meet everywhere. Cold start meets only at 1769 MB for Rust (321 / 383 ms); 512 MB is 471 / 619 ms; JS misses everywhere (see Cloud) |

## What is here

All paths are relative to `prototypes/latency/`.

| Path | What |
|---|---|
| `host/` | The host: Wasmtime 49.0.1, plain hyper, `object_store` 0.14, `aws-sdk-dynamodb`. Config by environment variables (documented at the top of `host/src/main.rs`). One JSON log line per request (microseconds), no response headers. `SPINIT_COMPILER=cranelift|winch`. `precompile` and `publish` subcommands (`publish` also uploads a zstd copy of the artifact, `<cwasm key>.zst`; `SPINIT_ZSTD=1` makes the host fetch and decompress it). `/__bench/<op>` routes (spike only, `kb=` sets the object size for the S3 reads), a `mac-bench` subcommand (spike only) and `/__ready` |
| `components/` | Three test components: Rust p3 (`hello_p3`), Rust p2 (`hello_p2`), JavaScript through jco (`hello_js`), and two runaway-guest components, `hello_loop` (p3) and `hello_loop_p2`, with routes `/spin`, `/spin-calls`, `/grow` and `/grow-abort`. All imports are `wasi:*`, checked by `build.sh` with wasm-tools. Sources are copies of the host-research apps with the Spin-specific parts removed |
| `docker/`, `lambda/` | Build image (Amazon Linux 2023, glibc 2.34, the `provided.al2023` runtime's glibc), host build, Lambda zip, LWA + RIE image |
| `compose.yaml` | Project `spinit-spike-latency`: MinIO, DynamoDB Local, host (CPU and memory limited), host behind LWA via the Lambda RIE, oha. Host ports are env-configurable (`SPINIT_HOST_PORT` 18080, `MINIO_HOST_PORT` 19000, `DDB_HOST_PORT` 18000, `LWA_HOST_PORT` 19001) |
| `infra/` | OpenTofu module for us-west-2. Applied once for the cloud run on 2026-10-01 and destroyed after it (14 resources) |
| `bench/local.sh`, `bench/stats.py` | The local measurements and the tables below (`out/local/` holds the raw logs, gitignored) |
| `bench/runaway.py`, `bench/runaway.compose.yaml` | The runaway-guest and memory-cap checks (compose project `spinit-spike-winch`, host port 28080; raw logs in `out/runaway/`, gitignored) |
| `bench/cloud.sh`, `bench/cloud_report.py` | The cloud measurements and their tables (`out/cloud/` holds the raw logs, gitignored) |

Reproduce the local phase (everything runs in Docker; nothing is installed on the host):

```sh
cd prototypes/latency
docker/build-host.sh && components/build.sh && lambda/package.sh   # out/spinit-host, out/hello_*.wasm, out/spinit-host.zip
bench/local.sh all                                                  # or: functional cold bucket zstd winch mac portable warm lwa
python3 bench/stats.py                                              # the tables in this file
docker compose -p spinit-spike-latency down -v                      # teardown (named build-cache volumes spinit-spike-latency-* stay)
```

### Build

| Artifact | Size |
|---|---|
| `out/spinit-host` (aarch64, glibc 2.34 at most, stripped, fat LTO, codegen-units 1) | 26,615,272 bytes (25.4 MiB), with Winch, blake3, hmac and sha2's `asm` feature added |
| `out/spinit-host.zip` (one file, `bootstrap`) | 11,171,589 bytes (10.7 MiB) |

The Lambda limits are 50 MB zipped (direct upload) and 250 MB unzipped including layers, so there is room. The binary runs in `public.ecr.aws/lambda/provided:al2023`, which is where every local measurement ran.

| Component | wasm bytes | sha256 | cwasm bytes |
|---|---|---|---|
| `hello_p3` (Rust, wasi:http 0.3) | 304,869 | `47752d4cb024f8c7b10f23b3b6cb06a4e688be6b7d2fe5b195d67569ed3e5001` | 985,264 |
| `hello_p2` (Rust, wasi:http 0.2) | 263,201 | `88567980bec4cf531a1ade5e90cb9ccf4f13e325975ac61dc6367e4d093dbbf3` | 900,336 |
| `hello_js` (jco, StarlingMonkey) | 12,802,219 | `fb23c54b4110d5c33a54d60f598689b390091bc6b262c6626d234c04b5ed5630` | 33,459,536 |

`.wasm` files and `out/` are gitignored. The three digests above are from the build on 2026-10-01. Rebuild with `components/build.sh` (Docker, writes `out/hello_*.wasm`, prints each sha256). The JS and Rust digests can differ if a toolchain or dependency moves, so check them against `infra/variables.tf` (`component` default is the `hello_p3` digest) when rebuilding.

## Local (Docker on M-series arm64)

**Indicative only.** These numbers say where time goes and what can be ruled out. They are not Lambda numbers, and none of them settles a target.

How it was run: Docker Desktop on an Apple M4 Pro (the Linux VM has 14 CPUs and 7.7 GiB), the host in `public.ecr.aws/lambda/provided:al2023` with `--cpus`/`--memory` approximating Lambda sizes (1.0 CPU and 1769 MB, 0.29 and 512 MB, 0.07 and 128 MB, since Lambda gives one vCPU at 1769 MB and scales linearly). MinIO and DynamoDB Local on the same machine. Load from oha in another container.

What the approximation gets wrong:

- **CFS throttling.** At 0.29 and 0.07 CPU, back-to-back requests hit the kernel's 100 ms quota period, which shows up as p99.9 of 70 to 100+ ms (and p99 of about 95 ms at 0.07). Lambda does not behave like this. That is why the warm table also has paced runs at 50 requests per second (`@50/s`): the CPU idles between requests the way a real function mostly does. Read the paced rows for tail latency, and the unpaced rows for throughput and the median only.
- **Init-phase CPU.** Lambda boosts CPU during init, so process start and eager loading at 128 MB are probably faster in the cloud than the 0.07 CPU rows here.
- **MinIO and DynamoDB Local are on localhost.** Their latencies (about 0.3 ms GET, 1.3 to 3 ms PUT, 0.4 to 0.7 ms DynamoDB) are not S3 or DynamoDB latencies. They prove the code paths and the conditional operations work, nothing more.
- **The Lambda RIE is not the Lambda Runtime API.** The Web Adapter overhead is the adapter plus the emulator, measured as REPORT Duration minus the host's own `total_us`.
- **Local-path component spec.** The local path computes a SHA-256 of the whole wasm for the cache key (22 ms for the 12.8 MB JS component at 1.0 CPU, with sha2's software backend) and reads it from disk. The production `sha256:<hex>` spec skips both, so the "load total" for `hello_js` in the deserialize table overstates it. The bucket table is the production route.
- **CPU compatibility of `.cwasm`.** Artifacts compiled on Apple Silicon use that machine's CPU features. `SPINIT_TARGET=aarch64-unknown-linux-gnu` compiles for a baseline arm64 ISA instead. A baseline artifact deserialized fine in a default engine on the same machine, which does not prove it loads on Graviton. `bench/cloud.sh seed` publishes baseline artifacts, and the first cloud request confirms.
- The Wasmtime epoch ticker and the 256 MiB `StoreLimits` cap are on in every run. Fuel is off. The ticker was a tokio task in every run below and is now an OS thread (see "Winch: runaway guests and the memory cap"); it wakes at the same 100 ms rate, and these tables were not re-measured.

### What the local numbers say

- **Compile in the request path does not fit with Cranelift.** Cranelift for the 304 KB Rust component takes 137 ms at 1.0 CPU, 536 ms at 0.29 and 5.9 s at 0.07. The 12.8 MB JavaScript component takes 5.6 s at 1.0 CPU, 28.7 s at 0.29, and is OOM-killed at 128 MB (RSS about 292 MB while compiling). At 512 MB even the small components miss the 500 ms target on the compile alone. So the artifact has to be precompiled at publish time and fetched, and the compile fallback must be limited to the large-memory case or fail clearly. Winch changes this for the Rust components (the Winch bullets below).
- **Deserialize is cheap.** From a cached `.cwasm`, the Rust components load in about 7 to 13 ms (deserialize 5 to 11 ms) at every CPU profile. From the bucket (MinIO) the whole first request is 10 to 19 ms for the Rust components, with the 98 ms p2 first request at 0.07 a one-off to re-check in the cloud run. The JS component, with a 33.5 MB artifact, takes 27 ms at 1.0 and 0.29 CPU and 419 ms at 0.07 CPU (116 ms to fetch from MinIO, 199 ms to deserialize). The real S3 fetch of those 33 MB is the unknown that decides the JS cold start.
- **zstd copy of the artifact (`SPINIT_ZSTD=1`).** The `hello_js` `.cwasm` is 33,459,536 bytes raw and 10,115,750 bytes at zstd's default level 3 (30%). Level 19 gives 8,012,683 bytes (24%, 21% smaller than level 3) but takes 5.4 s to compress at 1.0 CPU against 69 ms, and decompresses a little slower (33 ms against 29 ms with the CLI at 1.0 CPU). Default level 3 is kept; trying 19 is a one-number change in `publish` (`encode_all(&cwasm[..], 0)`). The Rust artifacts shrink from 985,264 and 900,336 bytes to 270,500 and 244,582. In the host, `hello_js` decompresses in 22 to 24 ms at 1.0 CPU (five runs, in memory, `decompress_us`). At 0.29 CPU it was 23 ms in three of five runs and 62 and 87 ms in the other two, which is CFS throttling: the load costs about 45 ms of CPU against a quota of 29 ms per 100 ms period, and the stall lands on a different step each run (decompress, cache write or deserialize). From MinIO the first request takes 44.5 ms at 1.0 CPU and 123 ms at 0.29 CPU, against 27.5 and 27.1 ms for the raw artifact. Locally zstd costs more than it saves because a localhost transfer is almost free. It pays only if moving the 23 MB it saves takes longer than the roughly 23 ms of decompression, that is, if a single S3 GET moves less than about 1 GB/s. That is the expectation, not a measurement: the cloud run (`precompiled-zstd` against `precompiled`) decides it.
- **Winch (`SPINIT_COMPILER=winch`) works for all three components, with no compile error to record.** Wasmtime 49.0.1 on aarch64 with the component model: `hello_p3` (p3 async), `hello_p2` and `hello_js` compile, instantiate and serve 3000 warm requests each at success rate 1.0. The one failure is `hello_js` at 0.07 CPU and 128 MB, and it is memory, not Winch: compiling it needs about 297 MB of RSS under either compiler, so the container sits at 127.7 of 128 MiB and gives no response in 300 to 600 s (Cranelift in the same cell is OOM-killed). Notes: (1) Wasmtime's config docs say epoch interruption is incompatible with Winch, but the 49.0.1 Winch codegen emits epoch checks and every run here had epoch interruption on. The runaway-loop test is in "Winch: runaway guests and the memory cap (local)" below: epoch interruption works under Winch (`/spin` is interrupted at about 10.3 s, as under Cranelift) and the doc note looks stale. (2) Winch compiles the component trampolines with Cranelift, so the host binary still carries Cranelift and Winch artifacts still contain Cranelift code. (3) By the Winch source, aarch64 Winch does not support threads, GC, function references, relaxed SIMD, tail calls, exceptions or stack switching; none of the three components needs them.
- **Winch compiles 3.3 to 5.2 times faster than Cranelift for the Rust components and 6.5 times faster for JS** (Winch table, same run, so the ratios are fair; the Cranelift column differs by up to 17% from the older Compile table, so read gaps under that as noise). At 1.0 CPU: `hello_p3` 43 ms, `hello_p2` 37 ms, `hello_js` 836 ms. At 0.29 CPU: 133 ms, 88 ms, 4.1 s. At 0.07 CPU: 1.79 s and 1.51 s for the Rust ones. A design with no cache loads in the "without the cache write" column: `hello_p3` 44.7 ms at 1.0 CPU and 134.6 ms at 0.29, `hello_p2` 38.3 and 88.9 ms, `hello_js` 864 ms and 4.2 s. So compiling the Rust components at cold start would stay inside 500 ms at 512 and 1769 MB (about 35 ms and 125 ms more than deserializing a stored artifact, before the wasm fetch and platform init), and would miss at 128 MB (1.8 s, though Lambda's init CPU boost may help). It does not work for JS: 0.9 s at 1.0 CPU and 4.3 s at 0.29, so JS keeps a stored artifact. RSS after compile matches Cranelift (26 MB Rust, about 297 MB JS).
- **Winch code is slower, and its artifacts are bigger.** Warm guest-handle p50/p99 at 1.0 CPU, Winch against Cranelift: `hello_p3` 42 / 180 against 36 / 190 µs, `hello_p2` 102 / 236 against 89 / 220, `hello_js` 557 / 950 against 425 / 736. That is +17%, +15% and +31% at p50. Client p50 is 0.15 ms for both on `hello_p3`, 0.20 against 0.19 on `hello_p2` and 0.63 against 0.51 on `hello_js` (throughput 1501 against 1859 requests per second). These guests do almost no compute, so the gap is mostly per-call overhead; a compute-heavy guest would show a larger gap, which is not measured. Winch `.cwasm` is 1,509,680 bytes for `hello_p3` (+53%), 1,228,120 for `hello_p2` (+36%) and 57,645,296 for `hello_js` (+72%). Winch and Cranelift artifacts are not interchangeable (the engine config is embedded), so the host keeps Winch artifacts under `winch/` in the cache and `publish` refuses Winch.
- **MAC before `deserialize`.** Keyed BLAKE3 (single thread, NEON) against HMAC-SHA256 over buffers the size of the three artifacts (p50 of 9 runs, 150 ms apart; see the MAC table). With sha2's `asm` feature, which uses the aarch64 SHA-2 instructions, HMAC-SHA256 is the faster of the two: 0.28 ms for 0.27 MB, 6.9 ms for 10.1 MB and 23.1 ms for 33.5 MB at 1.0 CPU (about 1.45 GB/s), against 0.35, 10.5 and 29.2 ms for BLAKE3 (about 1.15 GB/s). At 0.29 CPU the two large buffers are within about 10% of the 1.0 CPU figures (21.5 ms against 28.7 ms for the 33.5 MB buffer), because the work just fits inside the 29 ms CFS quota. Without the `asm` feature, which is the `sha2` 0.10 default and what this repo had until now, HMAC-SHA256 runs in software at about 0.42 GB/s: 78.9 ms for 33.5 MB at 1.0 CPU and 155 ms at 0.29 CPU (throttled; 133 to 281 ms across runs), 2.8 to 5.8 times slower than BLAKE3. So the MAC is about 23 ms for the raw JS artifact (the same order as the 23 ms zstd decompress), about 1 ms for the Rust artifacts, and 6.9 ms if it is computed over the 10.1 MB zstd copy before decompressing, which also keeps the decompressor off unauthenticated bytes. Caveat: measured on an Apple M4 Pro under Docker, not Graviton. Graviton exposes the SHA-2 instructions, but its throughput is untested, and the `asm` backend checks for them at run time and falls back to software if they are missing. The host build now enables `asm` for `sha2`; the compile, warm and earlier tables were measured before that change (`sha2` is only on the local-path digest and blob-verify paths).
- **Process init is small.** 12 to 18 ms from process start to listening at 1.0 and 0.29 CPU, 108 to 183 ms at 0.07 CPU (probably less in a Lambda init phase).
- **Warm requests are dominated by nothing in the host.** Host overhead (host total minus guest handle) is about 2 µs unpaced and about 20 µs paced. Host total p50 at 1.0 CPU: `hello_p3` 31 µs, `hello_p2` 85 µs, `hello_js` 365 µs. Paced at 50 requests per second, client p50 is 0.9, 1.3 and 2.6 ms and p99 is 3.7, 4.7 and 7.1 ms, so guest work fits in a couple of milliseconds and leaves more than 25 ms of the 30 ms read budget to storage. At 0.07 CPU (128 MB) `hello_js` saturates (p99 about 103 ms even paced), so JS apps want 512 MB or more, and the Rust ones are fine at 128 MB.
- **p3 against p2.** p3 reuses a worker and shows 0 µs per-request instantiate, p2 instantiates every request (22 µs p50 unpaced, about 240 µs paced with cold caches). Both are negligible next to a storage call.
- **Allocator.** Pooling against the default allocator made no meaningful difference to deserialize, first request or warm latency at 1.0 CPU, so the default (less code) is the provisional choice.
- **Web Adapter.** Adapter plus RIE overhead is 453 µs p50 (744 µs p99) for `hello_p3` and 525 µs p50 (952 µs p99) for `hello_p2`. The INIT REPORT is 26 to 29 ms against about 12 ms for the host alone, so roughly 14 to 17 ms more at init. Both are far below the switch thresholds (5 ms warm, 50 ms cold). Real Lambda decides.
- **One bug found.** With 16 concurrent connections the p3 p99 was about 41 ms, which was Nagle plus delayed ACK (headers and body leave in separate writes). The host now sets `TCP_NODELAY` on accepted sockets: p99 2.5 ms. The earlier JSON is in `out/local-pre-nodelay/`. The adapter-to-host loopback hop is the same shape, so keep an eye on it in the cloud numbers.
- **Functional checks pass.** MinIO (a stand-in for S3): create-if-absent succeeds, then reports AlreadyExists for an existing key; update with the current ETag succeeds; update with a stale ETag fails the precondition; GET with `If-None-Match` of the current ETag returns NotModified. DynamoDB Local: conditional put on the current version succeeds, on a stale version fails with ConditionalCheckFailedException. (S3's ETag is the MD5 of the body, so If-Match tests use distinct payloads.) These behaviours must still be confirmed on real S3. The S3 read bench ops also take an object size (`?kb=80`, `?kb=800`, seeded with `/__bench/seed?kb=...`) to model a state object of that size; locally they run (MinIO p50 0.2 ms at 1 and 80 KB, 0.5 ms at 800 KB, 0.2 ms for every 304), which says nothing about S3.

### Tables (generated by `python3 bench/stats.py`)

#### Compile (Cranelift) at a CPU share, from the wasm blob

| component | Docker --cpus (approx. Lambda memory) | wasm KB | cwasm KB | compile ms | RSS MB after | result |
|---|---|---|---|---|---|---|
| hello_p3 | 1.0 (~1769 MB) | 304 | 985 | 136.7 | 27 | ok |
| hello_p3 | 0.29 (~512 MB) | 304 | 985 | 536.1 | 27 | ok |
| hello_p3 | 0.07 (~128 MB) | 304 | 985 | 5875.8 | 27 | ok |
| hello_p2 | 1.0 (~1769 MB) | 263 | 900 | 117.5 | 24 | ok |
| hello_p2 | 0.29 (~512 MB) | 263 | 900 | 419.8 | 24 | ok |
| hello_p2 | 0.07 (~128 MB) | 263 | 900 | 5308.2 | 24 | ok |
| hello_js | 1.0 (~1769 MB) | 12802 | 33459 | 5568.6 | 292 | ok |
| hello_js | 0.29 (~512 MB) | 12802 | 33459 | 28712.5 | 291 | ok |
| hello_js | 0.07 (~128 MB) | - | - | - | - | FAILED (http 000, oom-killed true) |

#### Deserialize + first request from a cached .cwasm (local path spec, default allocator)

| component | Docker --cpus | host init ms (process start to listening) | deserialize ms | instantiate_pre + ProxyPre ms | 1st instantiate ms | 1st handle ms | load total ms | RSS MB |
|---|---|---|---|---|---|---|---|---|
| hello_p3 | 1.0 (~1769 MB) | 15.1 | 6.0 | 0.2 | 0.6 | 1.8 | 7.2 | 13 |
| hello_p3 | 0.29 (~512 MB) | 13.4 | 8.3 | 0.2 | 0.4 | 1.6 | 9.7 | 12 |
| hello_p3 | 0.07 (~128 MB) | 108.3 | 9.3 | 0.2 | 0.8 | 2.4 | 13.1 | 13 |
| hello_p2 | 1.0 (~1769 MB) | 12.6 | 6.4 | 0.2 | 0.4 | 1.3 | 7.6 | 12 |
| hello_p2 | 0.29 (~512 MB) | 12.1 | 5.3 | 0.2 | 0.4 | 1.4 | 6.5 | 12 |
| hello_p2 | 0.07 (~128 MB) | 176.0 | 10.7 | 0.3 | 0.7 | 2.6 | 12.8 | 12 |
| hello_js | 1.0 (~1769 MB) | 18.1 | 11.3 | 2.2 | 0.7 | 5.7 | 41.8 | 28 |
| hello_js | 0.29 (~512 MB) | 11.6 | 7.1 | 0.3 | 0.6 | 2.1 | 35.3 | 28 |
| hello_js | 0.07 (~128 MB) | 182.9 | 198.7 | 1.1 | 90.8 | 97.8 | 1324.8 | 28 |

#### Allocator: first request after deserialize at --cpus 1.0

| component | allocator | deserialize ms | instantiate_pre ms | 1st instantiate ms | 1st handle ms |
|---|---|---|---|---|---|
| hello_p3 | default | 6.0 | 0.2 | 0.58 | 1.81 |
| hello_p3 | pooling | 9.1 | 0.2 | 0.36 | 1.62 |
| hello_p2 | default | 6.4 | 0.2 | 0.42 | 1.35 |
| hello_p2 | pooling | 13.8 | 0.2 | 0.32 | 1.44 |
| hello_js | default | 11.3 | 2.2 | 0.72 | 5.67 |
| hello_js | pooling | 6.6 | 0.3 | 0.47 | 2.24 |

#### Cold path from the bucket (MinIO on localhost: fetch times are optimistic, not S3 times)

| component | Docker --cpus | route | fetch ms | fetched KB | zstd decompress ms | compile or deserialize ms | write /cache ms | load total ms | whole first request ms | route taken |
|---|---|---|---|---|---|---|---|---|---|---|
| hello_p3 | 1.0 (~1769 MB) | blob + compile | 2.7 | 304 | - | 136.1 | 0.2 | 140.2 | 141.9 | compile |
| hello_p3 | 1.0 (~1769 MB) | precompiled artifact | 3.0 | 985 | - | 5.8 | 0.2 | 9.2 | 10.8 | cwasm |
| hello_p3 | 1.0 (~1769 MB) | precompiled artifact, zstd copy | 2.6 | 270 | 1.2 | 9.7 | 0.1 | 14.1 | 15.6 | cwasm |
| hello_p3 | 0.29 (~512 MB) | precompiled artifact | 2.8 | 985 | - | 9.4 | 0.1 | 12.5 | 14.2 | cwasm |
| hello_p3 | 0.07 (~128 MB) | precompiled artifact | 3.9 | 985 | - | 12.5 | 0.2 | 16.9 | 19.1 | cwasm |
| hello_p2 | 1.0 (~1769 MB) | blob + compile | 2.3 | 263 | - | 109.6 | 0.2 | 113.4 | 114.9 | compile |
| hello_p2 | 1.0 (~1769 MB) | precompiled artifact | 2.8 | 900 | - | 7.0 | 0.1 | 10.3 | 11.8 | cwasm |
| hello_p2 | 0.29 (~512 MB) | precompiled artifact | 2.7 | 900 | - | 5.0 | 0.1 | 8.2 | 9.9 | cwasm |
| hello_p2 | 0.07 (~128 MB) | precompiled artifact | 6.5 | 900 | - | 6.1 | 0.4 | 13.6 | 98.1 | cwasm |
| hello_js | 1.0 (~1769 MB) | blob + compile | 6.3 | 12802 | - | 5053.5 | 6.2 | 5097.9 | 5102.1 | compile |
| hello_js | 1.0 (~1769 MB) | precompiled artifact | 12.1 | 33459 | - | 8.8 | 4.1 | 25.4 | 27.5 | cwasm |
| hello_js | 1.0 (~1769 MB) | precompiled artifact, zstd copy | 5.2 | 10115 | 22.9 | 9.0 | 4.6 | 42.7 | 44.5 | cwasm |
| hello_js | 0.29 (~512 MB) | precompiled artifact | 11.8 | 33459 | - | 8.6 | 3.8 | 24.9 | 27.1 | cwasm |
| hello_js | 0.29 (~512 MB) | precompiled artifact, zstd copy | 5.7 | 10115 | 22.4 | 86.2 | 5.4 | 120.5 | 123.0 | cwasm |
| hello_js | 0.07 (~128 MB) | precompiled artifact | 115.6 | 33459 | - | 199.1 | 5.6 | 321.9 | 418.7 | cwasm |

#### Winch against Cranelift: the blob compiled in the host at a CPU share (empty cache, default allocator)

| component | Docker --cpus | compiler | compile ms | serialize + cache write ms | cwasm KB | RSS MB after | load total ms | load total without the cache write ms | whole first request ms |
|---|---|---|---|---|---|---|---|---|---|
| hello_p3 | 1.0 (~1769 MB) | cranelift | 156.6 | 0.7 | 985 | 27 | 159.6 | 159.0 | 161.3 |
| hello_p3 | 1.0 (~1769 MB) | winch | 43.1 | 1.3 | 1509 | 26 | 45.9 | 44.7 | 47.3 |
| hello_p3 | 0.29 (~512 MB) | cranelift | 548.4 | 0.8 | 985 | 28 | 550.6 | 549.8 | 552.6 |
| hello_p3 | 0.29 (~512 MB) | winch | 133.0 | 1.8 | 1509 | 26 | 136.4 | 134.6 | 138.1 |
| hello_p3 | 0.07 (~128 MB) | cranelift | 6628.6 | 0.6 | 985 | 28 | 6632.6 | 6632.0 | 6719.8 |
| hello_p3 | 0.07 (~128 MB) | winch | 1786.6 | 92.8 | 1509 | 26 | 1883.7 | 1790.8 | 1985.4 |
| hello_p2 | 1.0 (~1769 MB) | cranelift | 121.3 | 0.8 | 900 | 25 | 123.5 | 122.7 | 124.9 |
| hello_p2 | 1.0 (~1769 MB) | winch | 36.9 | 1.1 | 1228 | 25 | 39.4 | 38.3 | 40.7 |
| hello_p2 | 0.29 (~512 MB) | cranelift | 456.4 | 1.0 | 900 | 25 | 458.9 | 457.9 | 460.6 |
| hello_p2 | 0.29 (~512 MB) | winch | 87.5 | 1.3 | 1228 | 25 | 90.2 | 88.9 | 91.6 |
| hello_p2 | 0.07 (~128 MB) | cranelift | 6200.3 | 2.5 | 900 | 25 | 6206.7 | 6204.1 | 6305.6 |
| hello_p2 | 0.07 (~128 MB) | winch | 1514.6 | 93.8 | 1228 | 24 | 1612.3 | 1518.4 | 1615.6 |
| hello_js | 1.0 (~1769 MB) | cranelift | 5420.9 | 15.8 | 33459 | 290 | 5464.1 | 5448.3 | 5468.0 |
| hello_js | 1.0 (~1769 MB) | winch | 835.9 | 31.2 | 57645 | 297 | 894.8 | 863.5 | 898.9 |
| hello_js | 0.29 (~512 MB) | cranelift | 26714.0 | 92.0 | 33459 | 291 | 26840.8 | 26748.8 | 26848.8 |
| hello_js | 0.29 (~512 MB) | winch | 4099.6 | 106.0 | 57645 | 298 | 4281.6 | 4175.6 | 4286.5 |
| hello_js | 0.07 (~128 MB) | cranelift | - | - | - | - | - | - | FAILED (http 000, oom-killed true) |
| hello_js | 0.07 (~128 MB) | winch | - | - | - | - | - | - | FAILED (http 000, oom-killed false) |

#### Winch against Cranelift: warm requests through the guest at --cpus 1.0 (c=1, default allocator, epoch interruption on in both)

| component | compiler | client p50 ms | client p99 ms | req/s | guest handle p50/p99 µs | instantiate p50/p99 µs |
|---|---|---|---|---|---|---|
| hello_p3 | cranelift | 0.15 | 0.36 | 6115 | 36 / 190 | 0 / 0 |
| hello_p3 | winch | 0.15 | 0.35 | 6126 | 42 / 180 | 0 / 0 |
| hello_p2 | cranelift | 0.19 | 0.36 | 4922 | 89 / 220 | 23 / 106 |
| hello_p2 | winch | 0.20 | 0.45 | 4629 | 102 / 236 | 23 / 90 |
| hello_js | cranelift | 0.51 | 0.87 | 1859 | 425 / 736 | 30 / 102 |
| hello_js | winch | 0.63 | 1.09 | 1501 | 557 / 950 | 31 / 87 |

#### MAC over a precompiled artifact: keyed BLAKE3 against HMAC-SHA256 (ms, p50 of 9 runs, with min to max; mac-soft-*.log is the same binary built without the sha2 asm feature)

| buffer MB | --cpus | SHA-256 backend | BLAKE3 keyed | min to max | HMAC-SHA256 | min to max | HMAC / BLAKE3 |
|---|---|---|---|---|---|---|---|
| 0.27 | 1.0 | SHA2 instructions | 0.35 | 0.25 to 0.59 | 0.28 | 0.15 to 0.76 | 0.8x |
| 10.12 | 1.0 | SHA2 instructions | 10.46 | 9.81 to 11.93 | 6.87 | 5.80 to 8.58 | 0.7x |
| 33.46 | 1.0 | SHA2 instructions | 29.24 | 27.19 to 32.62 | 23.09 | 20.18 to 24.26 | 0.8x |
| 0.27 | 0.29 | SHA2 instructions | 0.39 | 0.31 to 0.48 | 0.22 | 0.17 to 0.71 | 0.6x |
| 10.12 | 0.29 | SHA2 instructions | 10.37 | 9.10 to 12.13 | 7.05 | 5.53 to 10.07 | 0.7x |
| 33.46 | 0.29 | SHA2 instructions | 28.72 | 26.80 to 29.45 | 21.52 | 18.68 to 24.61 | 0.7x |
| 0.27 | 1.0 | software | 0.28 | 0.17 to 0.64 | 1.63 | 1.19 to 3.13 | 5.7x |
| 10.12 | 1.0 | software | 10.39 | 9.54 to 11.64 | 33.66 | 30.26 to 34.96 | 3.2x |
| 33.46 | 1.0 | software | 28.24 | 27.16 to 30.12 | 78.87 | 76.25 to 83.77 | 2.8x |
| 0.27 | 0.29 | software | 0.28 | 0.18 to 0.67 | 1.63 | 1.42 to 3.04 | 5.8x |
| 10.12 | 0.29 | software | 10.61 | 9.79 to 11.29 | 43.44 | 29.85 to 101.03 | 4.1x |
| 33.46 | 0.29 | software | 28.32 | 26.97 to 43.38 | 155.45 | 133.12 to 281.03 | 5.5x |

#### Warm requests through the guest (oha from another container; host columns are the host's own per-request log, µs)

| component | --cpus | allocator | conc | client p50 ms | client p99 ms | client p99.9 ms | req/s | host total p50/p99 µs | guest handle p50/p99 µs | instantiate p50/p99 µs | host overhead p50 µs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| hello_p3 | 1.0 | default | 1 | 0.11 | 0.20 | 0.49 | 8683 | 31 / 103 | 29 / 98 | 0 / 0 | 2 |
| hello_p3 | 1.0 | default | 1 @50/s | 0.90 | 3.71 | 10.02 | 50 | 345 / 1557 | 318 / 1531 | 0 / 0 | 21 |
| hello_p3 | 1.0 | default | 16 | 1.38 | 2.54 | 5.73 | 11317 | 197 / 1501 | 108 / 683 | 0 / 0 | 3 |
| hello_p3 | 1.0 | pooling | 1 | 0.13 | 0.24 | 0.36 | 7243 | 33 / 140 | 31 / 135 | 0 / 0 | 2 |
| hello_p3 | 0.29 | default | 1 | 0.14 | 0.29 | 67.16 | 2674 | 37 / 172 | 34 / 167 | 0 / 0 | 2 |
| hello_p3 | 0.29 | default | 1 @50/s | 0.92 | 4.13 | 8.81 | 50 | 348 / 2520 | 322 / 2488 | 0 / 0 | 22 |
| hello_p3 | 0.07 | default | 1 | 0.36 | 94.81 | 99.53 | 215 | 116 / 876 | 108 / 809 | 0 / 0 | 6 |
| hello_p3 | 0.07 | default | 1 @50/s | 0.88 | 4.64 | 72.89 | 50 | 337 / 2354 | 312 / 2330 | 0 / 0 | 20 |
| hello_p2 | 1.0 | default | 1 | 0.18 | 0.27 | 0.51 | 5512 | 85 / 175 | 82 / 171 | 22 / 85 | 2 |
| hello_p2 | 1.0 | default | 1 @50/s | 1.27 | 4.74 | 8.47 | 50 | 710 / 3316 | 686 / 3205 | 242 / 1355 | 23 |
| hello_p2 | 1.0 | default | 16 | 2.25 | 3.12 | 5.74 | 7402 | 77 / 233 | 74 / 231 | 20 / 124 | 2 |
| hello_p2 | 1.0 | pooling | 1 | 0.16 | 0.27 | 0.41 | 6001 | 67 / 163 | 64 / 161 | 13 / 74 | 2 |
| hello_p2 | 0.29 | default | 1 | 0.20 | 0.39 | 69.74 | 1814 | 100 / 237 | 97 / 232 | 25 / 92 | 2 |
| hello_p2 | 0.29 | default | 1 @50/s | 1.27 | 5.10 | 9.07 | 50 | 699 / 3412 | 673 / 3328 | 235 / 1711 | 22 |
| hello_p2 | 0.07 | default | 1 | 0.70 | 98.02 | 102.29 | 107 | 355 / 89667 | 342 / 88859 | 101 / 891 | 9 |
| hello_p2 | 0.07 | default | 1 @50/s | 1.21 | 4.01 | 23.06 | 50 | 656 / 2514 | 631 / 2404 | 232 / 972 | 21 |
| hello_js | 1.0 | default | 1 | 0.43 | 0.63 | 1.24 | 2256 | 365 / 540 | 363 / 536 | 26 / 51 | 2 |
| hello_js | 1.0 | default | 1 @50/s | 2.55 | 7.06 | 11.74 | 50 | 1957 / 6076 | 1936 / 6027 | 250 / 1721 | 18 |
| hello_js | 1.0 | default | 16 | 6.47 | 7.84 | 11.60 | 2580 | 355 / 533 | 354 / 527 | 21 / 40 | 2 |
| hello_js | 1.0 | pooling | 1 | 0.43 | 0.71 | 0.98 | 2241 | 365 / 622 | 364 / 618 | 18 / 38 | 2 |
| hello_js | 0.29 | default | 1 | 0.56 | 68.62 | 78.32 | 552 | 473 / 858 | 471 / 850 | 31 / 116 | 2 |
| hello_js | 0.29 | default | 1 @50/s | 2.57 | 6.58 | 7.71 | 50 | 1982 / 5527 | 1958 / 5503 | 265 / 1853 | 19 |
| hello_js | 0.07 | default | 1 | 2.75 | 106.59 | 193.87 | 29 | 1989 / 104712 | 1963 / 104693 | 163 / 2812 | 14 |
| hello_js | 0.07 | default | 1 @50/s | 2.60 | 103.23 | 106.67 | 32 | 1905 / 102596 | 1889 / 102579 | 154 / 744 | 13 |

Host-only route (`/__ready`, no guest) from the same client: p50 0.09 ms, p99 0.22 ms. Everything above this floor in the client columns is the guest plus the host's own work.

#### Lambda Web Adapter overhead (RIE: invocation event in, HTTP to the host, response out)

| component | warm invocations | REPORT Duration p50/p99 µs | host total p50/p99 µs | adapter + RIE overhead p50/p99 µs | INIT REPORT ms (cold: extension + host init + readiness) | host init ms | 1st invocation Duration ms (includes Cranelift: no cache in this image) |
|---|---|---|---|---|---|---|---|
| hello_p3 | 3000 | 500 / 1010 | 43 / 157 | 453 / 744 | 29.3 | 12.2 | 179.2 |
| hello_p2 | 3000 | 630 / 1360 | 100 / 261 | 525 / 952 | 25.7 | 12.3 | 161.0 |

### Winch: runaway guests and the memory cap (local)

Question: with components treated as untrusted, do the two per-request guards, the 10 s epoch deadline (the CPU-time guard) and the 256 MiB memory cap, hold under Winch as they do under Cranelift? The `Config::epoch_interruption` doc says the option is "not compatible with the Winch compiler", but the Winch codegen in 49.0.1 emits epoch checks. Wasmtime 49.0.1, aarch64 Linux in Docker on an M-series Mac, no cloud. **Fact** marks a measurement or a source line, **inference** marks my reading.

**Test components and harness.** `components/rust-p3-loop` (`hello_loop`, p3, 299,744 bytes) and `components/rust-p2-loop` (`hello_loop_p2`, p2, 262,695 bytes) serve `/` (200 "ok"), `/spin` (a `black_box` loop that never calls the host), `/spin-calls` (the same loop with a cheap WASI call in it), `/grow` (allocates until the allocation fails, then answers 200 "grew N MiB before the allocation failed") and `/grow-abort` (the same, but the failed allocation aborts, which is a trap). `components/build.sh` builds them with the other three and checks that every import is `wasi:*`. `bench/runaway.py` recreates the host container for each scenario, sends the runaway request or requests, probes `/` on the same host every 0.5 s (3 s timeout), reads `docker stats --no-stream` 5 s in and 3 s after the last response, and reads thread names, VmRSS and VmHWM of the host (PID 1) from `/proc`. Host settings are the defaults: epoch tick 100 ms, deadline 100 ticks (10 s), 256 MiB `StoreLimits`. Raw logs are in `out/runaway/` (gitignored). The default `--cpus 1.0` gives the host one tokio worker (`available_parallelism` honours the cgroup quota; counted by thread name in `/proc`, 1 worker and 1 main thread); `--workers N` sets `TOKIO_WORKER_THREADS`.

Reproduce (Docker only; `bench/runaway.py` sets the project `spinit-spike-winch` and its own host ports, 28080 and so on, so it does not touch the `spinit-spike-latency` project):

```sh
cd prototypes/latency
docker/build-host.sh && components/build.sh                  # out/spinit-host, out/hello_*.wasm including hello_loop*.wasm
bench/runaway.py                                              # the four compiler:component combinations, 1.0 CPU, 1 worker; /spin x1 and x4, /spin-calls, /grow
bench/runaway.py --workers 2 cranelift:hello_loop winch:hello_loop
bench/runaway.py --cpus 0.29 --mem 512m --spins 1 cranelift:hello_loop winch:hello_loop
bench/runaway.py --cpus 0.07 --mem 128m --spins 1 cranelift:hello_loop winch:hello_loop
docker compose -p spinit-spike-winch down -v                  # teardown
```

**A host bug came first.** As shipped, the host did not stop a runaway guest at `--cpus 1.0`, under either compiler. **Fact:** the epoch ticker was a tokio task. With one worker, a guest that never yields keeps that worker busy, so the ticker never ran and the epoch never advanced. With `TOKIO_WORKER_THREADS=2` one runaway request was interrupted at 10.2 s, but four concurrent ones occupied both workers and none was interrupted. The ticker is now an OS thread (`host/src/main.rs`, a separate commit), and every row of the first two tables below is from that host. The as-shipped host is in the third table. The older tables in this file were measured with the task ticker; a thread that wakes every 100 ms costs the same, and none was re-measured. **Inference:** the failure is a host design problem, not a Winch one (Cranelift failed identically), and it applies to any embedding that drives `increment_epoch` from the same runtime as the guests.

#### Runaway guest, fixed host (OS-thread ticker), `--cpus 1.0`, 1769 MB, 1 tokio worker (HTTP status, seconds to response)

| compiler | component | `/spin` | `/spin-calls` | 4 concurrent `/spin`, responses at (s) | CPU 3 s after the trap |
|---|---|---|---|---|---|
| cranelift | hello_loop (p3) | 500 at 10.31 | 500 at 10.36 | 10.33, 20.74, 31.13, 41.51 | 0.05 to 0.07% |
| winch | hello_loop (p3) | 500 at 10.31 | 500 at 10.30 | 10.23, 20.52, 30.78, 41.06 | 0.04 to 0.05% |
| cranelift | hello_loop_p2 | 500 at 10.32 | 500 at 10.29 | 10.23, 20.50, 30.83, 41.22 | 0.05% |
| winch | hello_loop_p2 | 500 at 10.21 | 500 at 10.30 | 10.31, 20.66, 31.04, 41.40 | 0.04 to 0.05% |

Every response is `internal error`, and the host logs `worker failed: error while executing at wasm backtrace`, `Caused by: wasm trap: interrupt`. CPU is 99.5 to 100% at the 5 s sample and 0.04 to 0.07% three seconds after the trap. The host answered `/` (200, 2 to 3 ms) right after and kept serving. While a runaway request runs on the single worker, `/` is blocked: of 3 probes in a 10 s spin, 2 timed out at 3 s and the third was served only after the trap; during 4 concurrent spins, 1 of 12 probes was served, after the first trap. The four spins are served one after another, each getting its own 10 s.

With `TOKIO_WORKER_THREADS=2` (p3, both compilers): `/spin` 500 at 10.31 (cranelift) and 10.15 (winch), `/spin-calls` 500 at 10.23 and 10.29, and 20 of 20 probes of `/` were served in 1 to 5 ms while the spin ran. Four concurrent `/spin` finish in two waves (cranelift 10.30, 10.30, 20.65, 20.65 s; winch 10.37, 10.37, 20.68, 20.68 s), and `/` is blocked during them (1 of 6 probes served).

#### Runaway guest, as-shipped host (epoch ticker as a tokio task), `--cpus 1.0`, 1769 MB

| tokio workers | what was sent | Cranelift and Winch, p3 and p2 (8 combinations per row) |
|---|---|---|
| 1 (the default at 1.0 CPU) | `/spin`, `/spin-calls`, 4 concurrent `/spin` | no response in 30 s (client timeout; 45 s in the first run), every `/` probe timed out (9 of 9), CPU still 99 to 100% after the client gave up and 30 s or more later, container alive |
| 2 | one `/spin` or `/spin-calls` | 500 at 10.20 to 10.25 s, `/` served in 1 to 13 ms (20 of 20 probes), CPU 0.1 to 0.3% three seconds later |
| 2 | 4 concurrent `/spin` | no response in 30 s, every `/` probe timed out, CPU about 100% |

#### Smaller CPU shares, fixed host, p3, 1 tokio worker

| `--cpus` / memory | compiler | `/spin` | `/spin-calls` | CPU at 5 s | CPU 3 s after the trap | `/grow` |
|---|---|---|---|---|---|---|
| 0.29 / 512 MB | cranelift | 500 at 10.35 | 500 at 10.30 | 28.8%, 29.2% | 0.07%, 0.03% | 200, 254 MiB in 264 ms, RSS 15 to 268 MiB |
| 0.29 / 512 MB | winch | 500 at 10.33 | 500 at 10.30 | 28.9%, 29.0% | 0.04%, 0.05% | 200, 254 MiB in 245 ms, RSS 15 to 269 MiB |
| 0.07 / 128 MB | cranelift | 500 at 10.48 | 500 at 10.39 | 6.9%, 7.0% | 0.04% | container OOM-killed (exit 137, `OOMKilled=true`), the request got a closed connection after 1.50 s |
| 0.07 / 128 MB | winch | 500 at 10.39 | 500 at 10.29 | 7.0%, 7.0% | 0.06% | container OOM-killed, closed connection after 1.11 s |

**Fact:** the deadline is counted in wall-clock ticks. At 0.07 CPU the guest got about 7% of a core, so roughly 0.7 s of CPU time passed before the 10 s trap. It is a wall-clock guard, not a CPU-time guard. At 128 MB the 256 MiB cap cannot protect anything: the cgroup limit (128 MiB) is hit first and the whole container dies, taking the host with it.

#### Memory cap, fixed host, `--cpus 1.0`, 1769 MB (the as-shipped host gave the same results)

| compiler | component | `/grow` | RSS before, after `/grow`, after `/grow-abort`; VmHWM (MiB) | second `/grow` | `/grow-abort` |
|---|---|---|---|---|---|
| cranelift | hello_loop (p3) | 200, "grew 254 MiB before the allocation failed", 79 ms | 15, 269, 15; 269 | 254 MiB again, 13 ms | 500 at 22 ms |
| winch | hello_loop (p3) | 200, 254 MiB, 70 ms | 15, 269, 15; 269 | 254 MiB, 9 ms | 500 at 21 ms |
| cranelift | hello_loop_p2 | 200, 254 MiB, 86 ms | 14, 14, 15; 269 | 254 MiB, 72 ms | 500 at 71 ms |
| winch | hello_loop_p2 | 200, 254 MiB, 84 ms | 15, 15, 15; 269 | 254 MiB, 74 ms | 500 at 73 ms |

The cap works under both compilers: `memory.grow` fails at 256 MiB, the guest sees a failed allocation (254 MiB of its own), and the host is unaffected (peak RSS 268 to 269 MiB, `OOMKilled=false`, no restart, `/` served in 1 to 3 ms afterwards). In p3 the instance is reused, so the grown linear memory stays resident (RSS 269 MiB after the request, and the second `/grow` is fast because that memory is already mapped); in p2 the instance is dropped and RSS goes back to about 14 MiB. `/grow-abort` (the allocation failure aborts the guest) answers 500 with the host log `wasm trap: wasm unreachable instruction executed`, RSS drops back to 15 MiB, and the host keeps serving.

#### Wasmtime 49.0.1 source (tag `v49.0.1`, commit 46c23a8)

- **Where Winch emits epoch checks (fact).** `winch/codegen/src/codegen/mod.rs:401` in `emit_body` (function entry) and `winch/codegen/src/visitor.rs:1955` in `visit_loop` (every loop header), both calling `maybe_emit_epoch_check` (`codegen/mod.rs:2205` to 2258). It loads the epoch counter and the store's deadline (`emit_load_epoch_deadline_and_counter`, `:2260`), compares them (unsigned less-than, `:2233`) and calls the `new_epoch` builtin (`:2243` to 2247) once the deadline is reached. So every Wasm function entry and every loop header is a check; a call out to the host has none of its own, and `/spin-calls` is caught at its loop header.
- **Fuel under Winch (fact).** The same two sites call `maybe_emit_fuel_check` (`codegen/mod.rs:399`, `visitor.rs:1956`; defined at `codegen/mod.rs:2138`), and per-operator accounting runs from `before_visit_op` (`codegen/mod.rs:493`, `fuel_before_visit_op` at `:2349`, `emit_fuel_increment` at `:2301`). None of this is architecture-specific. A throwaway core-module check (a WAT `loop { br }` and a loop with a call, 500,000,000 fuel or a 1 s epoch deadline, not kept in the repo) on aarch64 gave: epoch only, `Trap::Interrupt` at 1.03 to 1.04 s under both compilers; fuel only, `Trap::OutOfFuel` under both (Cranelift in 0.16 to 0.21 s, Winch in 0.65 to 0.74 s); fuel and epoch together, `OutOfFuel` under both. `Engine::new` accepted both options with `Strategy::Winch`.
- **Config validation (fact).** Nothing rejects `epoch_interruption(true)` with Winch. The only epoch check in `Engine::new` is "epochs currently require 64-bit atomics" (`crates/wasmtime/src/engine.rs:414`). Winch's strategy switch (`crates/wasmtime/src/config.rs:2473` to 2487) only masks Wasm proposals: GC, function references, relaxed SIMD, tail calls, legacy exceptions and stack switching on every target, plus threads on aarch64. The "not compatible with the Winch compiler" note is on `epoch_interruption` (`config.rs:780`, function at `:788`); the `consume_fuel` doc (`config.rs:630` to 643) has no such note. **Inference:** the note is stale, because the codegen emits the checks and every run here trapped. Wasmtime's own epoch tests (`tests/all/epoch_interruption.rs:186`, `epoch_interrupt_infinite_loop`) use plain `#[wasmtime_test]`, whose default strategy list includes Winch (`crates/test-macros/src/wasmtime_test.rs:175` to 182) with no architecture filter; I did not run the upstream suite.
- **Stability tier (fact).** From `docs/stability-tiers.md` at v49.0.1. The Tier 2 table (`:84` to 86; the Description cell is as printed in the file, without a space before its closing bar):

```text
| Category             | Description                | Missing Tier 1 Requirements |
|----------------------|----------------------------|-----------------------------|
| Target               | `aarch64-unknown-linux-gnu`| Continuous fuzzing          |
```

  Winch is listed in the Tier 1 table as a compiler (`:29`) whose support "is further broken down below" by target and proposal (`:78`); the aarch64 matrix (`:238` to 260) marks Winch with a cross for reference-types (footnote `a`: not every table and element-segment case), relaxed SIMD, threads, tail calls, function references, GC and wide arithmetic, and says (`:350`):

```text
[^c]: Winch's support for aarch64 is complete for Core Wasm.
```

  The Tier 1 compiler rule and maintenance promise (`:500` to 502 and `:504` to 506):

```text
* **Compiler**
  * A compiler, like a target, must be continuously fuzzed on at least one
    target to be considered Tier 1 for a particular target.

* **Maintenance**
  * CVEs and security releases will be performed as necessary for any bugs found
    in features and targets.
```

  The Tier 2 definition (`:421` to 476) has no security-release bullet.
- **Security policy (fact).** `docs/security-what-is-considered-a-security-vulnerability.md:7` to 8:

```text
Bugs must affect [a tier 1 platform or feature](./stability-tiers.md) to be
considered a security vulnerability.
```

  The same page says a denial of service while executing Wasm is a vulnerability (its example: a fuel-limited guest in an infinite loop that never yields), and its cheat sheet lists "Uninterruptible infinite loops" and "User-controlled memory exhaustion" as "Yes" at Wasm execution time (`:67` and `:68`). So a failure of the epoch guard or of the memory cap would be in scope on a Tier 1 platform.
- **Inference.** `aarch64-unknown-linux-gnu` is a Tier 2 target (not continuously fuzzed). The tier page does not give "Winch on aarch64" a tier of its own: Winch is a Tier 1 compiler in general, and the Tier 1 compiler rule ties a compiler's tier to fuzzing on a particular target. On a plain reading Winch on aarch64 is therefore a Tier 2 combination, and a bug that exists only on aarch64, in Winch or in Cranelift, does not "affect a tier 1 platform or feature". A bug in Winch's architecture-independent code (the epoch and fuel checks are emitted through the generic macro assembler) would also show on x86_64 and so would count (inference from the source layout, not tested). I did not check whether Winch is fuzzed on any target, how maintainers treat aarch64-only reports in practice, or the security advisory history.

**Verdict.** In Wasmtime 49.0.1 on aarch64, epoch interruption works under Winch exactly as under Cranelift: `/spin` and `/spin-calls` trap with `wasm trap: interrupt` at 10.2 to 10.5 s in both p3 and p2 at every CPU share tried (1.0, 0.29, 0.07), CPU returns to idle within 3 s, the host keeps serving, and the config doc's "not compatible with the Winch compiler" note looks stale (the codegen, my run and upstream's own test list say otherwise). It is a guard only if the ticker cannot be starved, which the host as shipped got wrong (now an OS thread); it counts wall-clock time, not CPU time; and a runaway guest holds its tokio worker until the trap, so at one worker nothing else on that host is served for up to 10 s, and concurrent runaways are interrupted in waves of one worker each (about 10 s per wave). That is likely harmless behind Lambda's one-invocation-at-a-time model (inference; the worker count Lambda reports through `available_parallelism` was not measured) and matters for any host that serves concurrent requests. The 256 MiB cap holds under both compilers at 512 MB and above, and is useless at 128 MB, where the container is OOM-killed first; a cap derived from the function's memory size is a design question for that size. Winch's tier on aarch64: the target `aarch64-unknown-linux-gnu` is Tier 2 (missing Tier 1 requirement: continuous fuzzing), Winch is a Tier 1 compiler in general but the page does not give it a separate aarch64 tier, and on a plain reading Winch on aarch64 is a Tier 2 combination. Wasmtime counts as security vulnerabilities only bugs that affect "a tier 1 platform or feature", so by the documentation an aarch64-only Winch (or Cranelift) bug in front of untrusted components is outside the project's security policy. Whether that is acceptable for Graviton is a decision, not a measurement.

## Cloud (us-west-2, Graviton)

Run on 2026-10-01 from 22:34 to 23:52 UTC, then destroyed: `tofu destroy` removed all 14 resources, and a check afterwards found nothing tagged `spinit-spike` left in the account. To finish inside the SSO session, the cells ran as three parallel streams, one per memory size. Each function still ran one cell at a time. The planned n = 100 cells were dropped, so every cold cell has n = 30 and its p99 is close to the maximum: indicative only. No request failed: every cold, warm and storage call returned 200. The `.cwasm` artifacts were compiled off Lambda (`publish` in the `provided:al2023` image under Docker on the M4 Pro, baseline aarch64 target). They loaded on Graviton in every precompiled cell, with no fallback to compiling. Raw logs are in `out/cloud/` (gitignored). `python3 bench/cloud_report.py out/cloud` regenerates the tables.

Order of operations to rerun (needs AWS credentials in the environment or `AWS_PROFILE`, and `aws` CLI v2, curl 7.75 or newer, python3, docker):

```sh
cd prototypes/latency
docker/build-host.sh && components/build.sh && lambda/package.sh
docker run --rm -v "$PWD":/w -w /w/infra ghcr.io/opentofu/opentofu:latest init
docker run --rm -v "$PWD":/w -w /w/infra -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN \
  ghcr.io/opentofu/opentofu:latest apply                            # or run `tofu` directly in infra/
bench/cloud.sh all                                                  # seed functional cold warm bench report, one at a time if preferred
docker run --rm -v "$PWD":/w -w /w/infra -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN \
  ghcr.io/opentofu/opentofu:latest destroy                          # bucket has force_destroy, so this empties it
```

The header of `bench/cloud.sh` lists the IAM permissions the caller needs. For a defensible cold p99, use more than 30 samples, for example `COLD_N=100 bench/cloud.sh cold 1769 hello_p3 precompiled`. `bench/cloud.sh all` runs the cells one after another, which took longer than an SSO session. Running one stream per memory size in parallel (`cold`, `warm` and `bench` take the memory size) brings it to about 80 minutes.

### What the cloud numbers say

- **Bucket KV meets every target at every memory size.**
  - Revalidation (conditional GET, 304) p50 is 9 to 10 ms.
  - A plain 1 KB GET p50 is 23 to 26 ms.
  - Create-if-absent PUT p99 is 51 to 82 ms, and If-Match update p99 is 59 to 139 ms. The targets were 30 ms and 200 ms.
  - The 80 KB and 800 KB state objects revalidate in 9 to 11 ms. A plain 800 KB GET is 27 to 30 ms (30.3 ms at 128 MB, 0.3 ms over).
  - DynamoDB is about ten times faster: reads 2 to 3 ms, conditional writes about 4 ms. The targets do not need it.
- **Warm overhead is small.** REPORT Duration minus the host's own total is 1.2 to 1.4 ms p50 at 512 and 1769 MB, about 2 ms p99. That covers the LWA proxy hop and Lambda's own accounting, so LWA stays. At 128 MB the Rust p50 is unchanged, but the p99 is 13 to 20 ms. JS pays 9 ms p50. Both are CPU throttling.
- **Cold start meets the 500 ms p99 only at 1769 MB (Rust, precompiled): 321 / 383 ms.** At 512 MB it is 471 / 619 ms, and at 128 MB 1330 / 1646 ms. Three fixed costs come before the guest runs:
  - Init: about 157 ms p50 at every size. The host's own init is about 81 ms of it; the rest is process start, the LWA extension and Lambda's bootstrap.
  - The first bucket fetch of the 1 MB artifact: 94 ms at 1769 MB, 211 ms at 512 and 800 ms at 128.
  - Deserialize: 46 to 58 ms.
- **The init phase gets more CPU than the memory size buys.** With `SPINIT_EAGER=1` (the `precompiled-eager` cells), the host fetches and deserializes the artifact before it reports ready.
  - In init, the same 1 MB fetch takes 89 ms at 128 MB and 90 ms at 512 MB. On the first request it takes 799 and 211 ms.
  - Init + Duration at 128 MB drops from 1330 / 1646 to 522 / 598 ms. At 512 MB it drops from 471 / 619 to 337 / 518 ms.
  - This matches the init-phase CPU boost others have reported. AWS does not document it, so it can change.
  - At low memory, most of the first-request fetch is probably connection setup (DNS, TCP and TLS to S3) at a fraction of a CPU. If so, opening the S3 connection during init would capture much of the gain without loading any app early. Not measured.
- **Deserialize is slower on Lambda than in Docker.** It is 46 to 58 ms p50 for the 1 MB Rust artifacts at every memory size, and 43 to 46 ms in init, against 5 to 11 ms locally. It does not shrink with more CPU, which points at memory mapping or page faults in the Firecracker VM rather than computation. Not investigated. After Init, it is the largest single item in the 1769 MB cold start.
- **Precompile: compiling in the function is too slow.** Cranelift at cold start takes 687 / 912 ms for `hello_p3` at 1769 MB, 1835 / 2149 at 512 and 6.8 s at 128. For JS it takes 15.5 s at 1769 MB, with 452 to 465 MB of memory used.
- **Winch at cold start misses the 500 ms p99 at every size.**
  - `hello_p3`: 439 / 580 ms at 1769 MB and 901 / 1176 at 512.
  - `hello_p2`: 429 / 526 and 845 / 1111.
  - JS: 3.6 s and 11 s.
  - So does Winch's security standing on aarch64 (Tier 2, outside the security policy). Stored native code stays, with an integrity check before `deserialize`.
- **JS cold start misses at every size.**
  - The raw 33.5 MB artifact: 727 / 1473 ms at 1769 MB, 963 / 2565 at 512 and 2.9 / 9.2 s at 128.
  - The 10.1 MB zstd copy helps at 512 MB and above. At 1769 MB it reaches 498 / 1052, with a 165 ms fetch against 449 and 71 ms to decompress. At 128 MB it hurts: decompression takes 1022 ms.
  - Deserialize p50 is 51 to 83 ms, but its p99 is 534 ms at 1769 MB and 1.3 to 1.5 s at 512. That is the same first-touch pattern on a 33 times larger image.
  - Max memory used is 100 to 110 MB for JS and 45 to 48 MB for Rust.
- **128 MB is not usable.** Everything CPU-bound is 3 to 10 times slower than at 512 MB. The local check also showed the container is OOM-killed before a runaway guest reaches its 256 MiB cap.

### Cold starts (REPORT: Init Duration and Duration of the first request, ms; p50 / p99)

| component | mode | MB | n | Init | Duration (1st request) | Init + Duration | max | host load total | deserialize or compile | fetch from bucket | zstd decompress | serialize + cache write (compile modes) | host init | max memory MB | client curl (reference) | non-200 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| hello_js | compile | 1769 | 30 | 157.5 / 245.3 | 15384.4 / 18430.1 | 15541.3 / 18625.3 | 18625.3 | 15359.2 / 18404.0 | 15045.3 / 18020.4 | 212.0 / 264.7 | - | 83.9 / 104.5 | 81.7 / 175.1 | 452 / 465 | 15916.6 / 18923.8 |  |
| hello_js | compile-winch | 1769 | 30 | 159.2 / 197.9 | 3487.3 / 5185.8 | 3647.3 / 5382.2 | 5382.2 | 3471.4 / 5160.1 | 3124.6 / 4762.0 | 190.7 / 259.5 | - | 137.2 / 180.2 | 81.3 / 109.8 | 462 / 465 | 3930.5 / 5980.9 |  |
| hello_js | compile-winch | 512 | 30 | 159.3 / 231.4 | 10916.1 / 16921.7 | 11076.5 / 17153.1 | 17153.1 | 10853.2 / 16858.9 | 10060.2 / 15856.5 | 304.7 / 350.1 | - | 423.7 / 584.6 | 80.6 / 108.3 | 461 / 464 | 11406.4 / 17730.5 |  |
| hello_js | precompiled | 128 | 30 | 157.5 / 196.2 | 2731.7 / 9027.4 | 2889.1 / 9223.6 | 9223.6 | 2446.3 / 8747.5 | 178.5 / 5961.6 | 1710.4 / 3417.6 | - | - | 80.4 / 105.7 | 100 / 101 | 3190.0 / 9550.3 |  |
| hello_js | precompiled | 1769 | 30 | 157.1 / 198.6 | 570.1 / 1276.5 | 726.6 / 1473.0 | 1473.0 | 554.7 / 1258.7 | 51.0 / 563.4 | 449.2 / 734.5 | - | - | 81.8 / 111.6 | 107 / 108 | 1050.6 / 1808.0 |  |
| hello_js | precompiled | 512 | 30 | 156.2 / 194.7 | 807.9 / 2370.1 | 962.9 / 2564.8 | 2564.8 | 760.4 / 2312.7 | 65.3 / 1513.9 | 578.0 / 1140.0 | - | - | 80.4 / 108.3 | 107 / 109 | 1279.2 / 2921.7 |  |
| hello_js | precompiled-zstd | 128 | 30 | 158.1 / 196.4 | 3057.2 / 9273.7 | 3213.0 / 9468.2 | 9468.2 | 2768.8 / 9011.8 | 170.0 / 5802.5 | 1134.1 / 1413.0 | 1022.3 / 1222.2 | - | 82.0 / 106.2 | 100 / 101 | 3524.9 / 9938.3 |  |
| hello_js | precompiled-zstd | 1769 | 30 | 157.0 / 197.1 | 339.9 / 854.8 | 498.1 / 1051.8 | 1051.8 | 325.0 / 837.3 | 51.2 / 534.2 | 164.7 / 395.9 | 71.1 / 85.2 | - | 81.9 / 109.8 | 107 / 110 | 778.5 / 3172.5 |  |
| hello_js | precompiled-zstd | 512 | 30 | 157.0 / 195.3 | 736.1 / 2176.7 | 893.3 / 2372.0 | 2372.0 | 687.4 / 2115.0 | 83.0 / 1332.9 | 285.9 / 383.6 | 216.5 / 298.7 | - | 81.5 / 109.0 | 107 / 109 | 1218.1 / 2837.8 |  |
| hello_p2 | compile | 128 | 30 | 158.0 / 195.9 | 5978.8 / 6859.8 | 6135.6 / 7055.2 | 7055.2 | 5809.6 / 6697.6 | 4939.8 / 5679.2 | 786.2 / 956.3 | - | 21.0 / 91.0 | 81.7 / 105.5 | 58 / 59 | 6473.0 / 7478.4 |  |
| hello_p2 | compile | 1769 | 30 | 157.1 / 194.5 | 483.5 / 562.6 | 641.1 / 756.6 | 756.6 | 472.6 / 550.1 | 379.1 / 441.5 | 87.2 / 104.4 | - | 2.4 / 3.7 | 82.6 / 108.3 | 58 / 60 | 891.3 / 1214.2 |  |
| hello_p2 | compile | 512 | 30 | 156.5 / 196.2 | 1462.2 / 1675.7 | 1618.7 / 1870.7 | 1870.7 | 1421.5 / 1636.1 | 1208.5 / 1386.7 | 206.3 / 242.3 | - | 2.4 / 4.0 | 80.3 / 110.8 | 58 / 63 | 1940.6 / 2593.4 |  |
| hello_p2 | compile-winch | 1769 | 30 | 159.2 / 197.4 | 270.2 / 328.4 | 428.5 / 525.6 | 525.6 | 260.3 / 315.0 | 164.9 / 210.8 | 88.7 / 99.4 | - | 3.0 / 4.9 | 81.0 / 109.5 | 57 / 59 | 733.3 / 1533.2 |  |
| hello_p2 | compile-winch | 512 | 30 | 157.9 / 187.6 | 690.7 / 923.5 | 845.2 / 1111.1 | 1111.1 | 659.3 / 884.6 | 441.6 / 626.1 | 205.9 / 250.3 | - | 3.0 / 4.8 | 79.3 / 102.6 | 57 / 61 | 1160.8 / 1556.2 |  |
| hello_p2 | precompiled | 128 | 30 | 156.8 / 196.5 | 1148.7 / 1442.6 | 1304.1 / 1636.9 | 1636.9 | 907.6 / 1202.9 | 97.5 / 219.0 | 801.9 / 960.8 | - | - | 79.9 / 110.0 | 45 / 45 | 1603.2 / 1986.7 |  |
| hello_p2 | precompiled | 1769 | 30 | 157.8 / 196.3 | 158.8 / 208.7 | 316.4 / 402.8 | 402.8 | 143.1 / 191.3 | 46.1 / 82.5 | 97.3 / 121.6 | - | - | 82.6 / 109.5 | 45 / 47 | 571.6 / 767.5 |  |
| hello_p2 | precompiled | 512 | 30 | 156.2 / 196.1 | 302.8 / 410.7 | 461.3 / 582.5 | 582.5 | 264.2 / 348.8 | 52.3 / 80.0 | 208.7 / 281.0 | - | - | 82.5 / 108.1 | 45 / 47 | 757.3 / 1410.5 |  |
| hello_p3 | compile | 128 | 30 | 157.2 / 196.9 | 6665.1 / 7540.6 | 6823.2 / 7733.8 | 7733.8 | 6469.0 / 7359.3 | 5639.4 / 6379.3 | 783.9 / 937.9 | - | 20.8 / 99.7 | 80.5 / 107.3 | 62 / 64 | 7152.6 / 8276.0 |  |
| hello_p3 | compile | 1769 | 30 | 157.2 / 323.6 | 529.6 / 631.4 | 687.4 / 911.8 | 911.8 | 516.9 / 616.8 | 420.5 / 496.7 | 90.2 / 134.8 | - | 2.0 / 3.2 | 81.3 / 111.4 | 63 / 65 | 973.8 / 1493.8 |  |
| hello_p3 | compile | 512 | 30 | 157.4 / 196.3 | 1678.8 / 1953.0 | 1835.2 / 2149.3 | 2149.3 | 1637.2 / 1896.1 | 1423.3 / 1646.4 | 209.1 / 243.0 | - | 2.1 / 3.4 | 81.4 / 107.3 | 63 / 66 | 2165.1 / 4055.9 |  |
| hello_p3 | compile-winch | 1769 | 30 | 158.4 / 197.2 | 281.5 / 384.2 | 439.4 / 580.2 | 580.2 | 269.1 / 371.4 | 175.0 / 233.7 | 87.7 / 139.5 | - | 3.5 / 5.7 | 80.4 / 104.6 | 59 / 61 | 750.3 / 1053.5 |  |
| hello_p3 | compile-winch | 512 | 30 | 159.1 / 259.5 | 742.5 / 979.3 | 900.7 / 1175.8 | 1175.8 | 701.4 / 938.2 | 478.4 / 670.4 | 206.8 / 253.5 | - | 3.7 / 11.4 | 80.2 / 176.3 | 59 / 66 | 1208.9 / 1532.1 |  |
| hello_p3 | precompiled | 128 | 30 | 157.5 / 196.2 | 1179.2 / 1459.5 | 1330.0 / 1646.1 | 1646.1 | 913.1 / 1205.2 | 89.3 / 219.2 | 799.4 / 964.7 | - | - | 81.1 / 104.1 | 45 / 45 | 1634.9 / 2015.6 |  |
| hello_p3 | precompiled | 1769 | 30 | 157.0 / 197.2 | 163.7 / 201.9 | 320.8 / 382.6 | 382.6 | 146.5 / 185.1 | 47.0 / 83.4 | 94.0 / 112.4 | - | - | 83.4 / 109.0 | 45 / 47 | 607.8 / 738.7 |  |
| hello_p3 | precompiled | 512 | 30 | 157.0 / 204.9 | 313.2 / 460.6 | 471.3 / 618.5 | 618.5 | 271.6 / 400.6 | 57.9 / 73.3 | 211.0 / 332.8 | - | - | 80.9 / 115.1 | 45 / 46 | 784.3 / 1383.2 |  |
| hello_p3 | precompiled-eager | 128 | 30 | 294.8 / 375.9 | 226.5 / 260.6 | 521.8 / 598.1 | 598.1 | 136.8 / 203.3 | 43.5 / 60.9 | 89.0 / 162.4 | - | - | 217.4 / 283.6 | 45 / 45 | 829.1 / 1092.8 |  |
| hello_p3 | precompiled-eager | 512 | 30 | 300.9 / 473.4 | 40.1 / 59.4 | 336.9 / 517.9 | 517.9 | 139.3 / 275.2 | 46.1 / 71.5 | 89.6 / 212.2 | - | - | 222.2 / 385.4 | 45 / 47 | 647.8 / 824.5 |  |
| hello_p3 | precompiled-zstd | 1769 | 30 | 156.7 / 198.2 | 162.4 / 221.5 | 317.9 / 389.5 | 389.5 | 147.9 / 206.4 | 48.9 / 71.6 | 88.9 / 136.8 | 4.6 / 6.2 | - | 82.1 / 109.3 | 45 / 48 | 593.0 / 1403.7 |  |

### Warm requests through the guest (ms; p50 / p99)

| component | MB | REPORT lines | host lines | REPORT Duration | host total | guest handle | instantiate | Duration minus host total (adapter overhead) | client curl (reference) |
|---|---|---|---|---|---|---|---|---|---|
| hello_js | 128 | 300 | 300 | 10.36 / 22.31 | 1.23 / 1.49 | 1.21 / 1.48 | 0.13 / 0.16 | 9.11 / 21.02 | 87.1 / 181.8 |
| hello_js | 1769 | 300 | 300 | 2.76 / 3.97 | 1.36 / 2.23 | 1.35 / 2.21 | 0.19 / 0.27 | 1.39 / 2.25 | 77.5 / 140.5 |
| hello_js | 512 | 300 | 300 | 2.59 / 3.19 | 1.26 / 1.70 | 1.25 / 1.69 | 0.14 / 0.18 | 1.31 / 1.80 | 74.6 / 146.0 |
| hello_p2 | 128 | 300 | 300 | 1.77 / 20.19 | 0.39 / 0.48 | 0.38 / 0.46 | 0.12 / 0.15 | 1.37 / 19.78 | 79.5 / 160.4 |
| hello_p2 | 1769 | 300 | 300 | 1.74 / 2.54 | 0.45 / 0.64 | 0.44 / 0.63 | 0.17 / 0.28 | 1.28 / 1.83 | 75.3 / 139.5 |
| hello_p2 | 512 | 300 | 300 | 1.61 / 2.62 | 0.37 / 0.52 | 0.36 / 0.51 | 0.11 / 0.17 | 1.23 / 2.05 | 72.7 / 155.8 |
| hello_p3 | 128 | 300 | 300 | 1.43 / 13.70 | 0.22 / 0.32 | 0.21 / 0.31 | 0.00 / 0.00 | 1.21 / 13.48 | 75.2 / 171.9 |
| hello_p3 | 1769 | 300 | 300 | 1.43 / 2.27 | 0.22 / 0.43 | 0.21 / 0.40 | 0.00 / 0.00 | 1.20 / 1.95 | 78.5 / 154.0 |
| hello_p3 | 512 | 300 | 300 | 1.46 / 2.16 | 0.23 / 0.35 | 0.21 / 0.33 | 0.00 / 0.00 | 1.23 / 1.88 | 76.9 / 149.9 |

The host and REPORT lines are paired by order; if the two counts differ the overhead column is unreliable. Adapter overhead above ~5 ms warm p50 (or ~50 ms cold) is the trigger for switching to lambda_http.

### Storage operations from inside the function (ms; 1 KB object unless the name says 80kb or 800kb; sequential; the function was warmed with 5 calls first)

| operation | MB | n | first | p50 | p99 | max |
|---|---|---|---|---|---|---|
| ddb-get-eventual | 128 | 200 | 2.2 | 2.4 | 16.2 | 16.5 |
| ddb-get-eventual | 512 | 200 | 2.2 | 2.0 | 2.8 | 5.7 |
| ddb-get-eventual | 1769 | 200 | 2.2 | 1.7 | 3.3 | 4.2 |
| ddb-get-strong | 128 | 200 | 2.8 | 2.7 | 15.0 | 17.4 |
| ddb-get-strong | 512 | 200 | 3.0 | 2.8 | 4.7 | 5.5 |
| ddb-get-strong | 1769 | 200 | 2.7 | 2.5 | 3.5 | 4.3 |
| ddb-put-cond | 128 | 200 | 4.0 | 4.3 | 14.2 | 15.6 |
| ddb-put-cond | 512 | 200 | 4.1 | 4.5 | 6.7 | 7.0 |
| ddb-put-cond | 1769 | 200 | 3.8 | 4.0 | 6.4 | 9.4 |
| s3-get | 128 | 200 | 21.2 | 23.0 | 32.3 | 36.5 |
| s3-get | 512 | 200 | 23.0 | 24.3 | 30.2 | 52.6 |
| s3-get | 1769 | 200 | 20.7 | 25.9 | 68.6 | 227.9 |
| s3-get-304 | 128 | 200 | 33.9 | 9.4 | 33.9 | 41.1 |
| s3-get-304 | 512 | 200 | 10.7 | 9.3 | 59.9 | 226.9 |
| s3-get-304 | 1769 | 200 | 10.7 | 10.3 | 15.2 | 28.2 |
| s3-get-304-800kb | 128 | 200 | 10.6 | 11.2 | 34.7 | 66.7 |
| s3-get-304-800kb | 512 | 200 | 9.2 | 9.8 | 20.6 | 32.0 |
| s3-get-304-800kb | 1769 | 200 | 10.7 | 10.0 | 24.3 | 26.6 |
| s3-get-304-80kb | 128 | 200 | 10.8 | 9.4 | 24.5 | 47.6 |
| s3-get-304-80kb | 512 | 200 | 8.5 | 9.2 | 24.6 | 38.1 |
| s3-get-304-80kb | 1769 | 200 | 10.1 | 9.8 | 14.0 | 24.7 |
| s3-get-800kb | 128 | 200 | 29.6 | 30.3 | 57.8 | 72.7 |
| s3-get-800kb | 512 | 200 | 29.0 | 28.6 | 41.5 | 53.7 |
| s3-get-800kb | 1769 | 200 | 27.7 | 27.4 | 47.2 | 67.7 |
| s3-get-80kb | 128 | 200 | 24.0 | 24.0 | 48.6 | 52.7 |
| s3-get-80kb | 512 | 200 | 36.5 | 26.5 | 42.9 | 73.7 |
| s3-get-80kb | 1769 | 200 | 28.8 | 24.6 | 48.3 | 57.9 |
| s3-put-create | 128 | 200 | 28.0 | 26.1 | 51.2 | 74.4 |
| s3-put-create | 512 | 200 | 25.4 | 24.7 | 82.2 | 163.6 |
| s3-put-create | 1769 | 200 | 28.6 | 25.0 | 56.5 | 91.2 |
| s3-put-update | 128 | 200 | 48.4 | 33.3 | 89.7 | 93.7 |
| s3-put-update | 512 | 200 | 28.9 | 30.0 | 59.2 | 67.3 |
| s3-put-update | 1769 | 200 | 28.4 | 31.3 | 139.4 | 253.2 |

### Decisions

| Decision | Measured | Verdict |
|---|---|---|
| Cold start p99 at most 500 ms (`hello_p3`, precompiled, per memory size) | 128 MB 1330 / 1646. 512 MB 471 / 619 (eager 337 / 518). 1769 MB 321 / 383 (zstd 318 / 389) | Meets at 1769 MB only. At 512 MB the misses are fixed costs: Init, the first fetch and deserialize |
| Bucket KV read p50 at most 30 ms with revalidation | 304: 9.3 to 10.3. Plain GET: 23.0 to 25.9 | Meets at every size: ship bucket KV |
| Bucket KV write p99 at most 200 ms | Create-if-absent 51 to 82. If-Match 59 to 139 | Meets at every size |
| Web Adapter overhead at most 5 ms warm and 50 ms cold | Warm p50 1.2 to 1.4, p99 under 2.3 at 512 and 1769 MB. Cold: the whole first request after an eager load is 40 ms p50 at 512 MB, instantiation and the guest included | Meets: keep LWA |
| Precompiled artifact loads on Graviton (baseline target) | Every precompiled cell loaded with no fallback to compiling. Deserialize 46 to 58 ms (Rust) | Works. Deserialize is 5 to 10 times slower than locally (open issue) |
| JS cold start with a 33 MB artifact, raw against zstd (`precompiled` against `precompiled-zstd`, per memory size) | 128 MB 2889 / 9224 against 3213 / 9468. 512 MB 963 / 2565 against 893 / 2372. 1769 MB 727 / 1473 against 498 / 1052 | zstd at 512 MB and above. JS misses 500 ms everywhere (open issue) |
| State object read p50 at most 30 ms at 80 KB and 800 KB (plain GET and 304 revalidation) | 80 KB: GET 24.0 to 26.5, 304 9.2 to 9.8. 800 KB: GET 27.4 to 30.3, 304 9.8 to 11.2 | Meets, except the plain 800 KB GET at 128 MB (30.3) |
| Allocator (default or pooling) | Not measured in the cloud. Locally there was no difference | Default allocator (less code) |
| Winch compile at cold start, `hello_p3` and `hello_js` at 512 and 1769 MB (`compile-winch` against `precompiled`; `host load total` less `serialize + cache write` is the no-cache cost) | `hello_p3` 512 MB 901 / 1176, 1769 MB 439 / 580. `hello_p2` 845 / 1111, 429 / 526. `hello_js` 11077 / 17153, 3647 / 5382 | Misses everywhere: keep stored native code with a MAC |

## Cost of the cloud run

Prices assumed (arm64, us-west-2):

| Service | Price |
|---|---|
| Lambda | $0.0000133334 per GB-second, $0.20 per million requests. Init time is billed. Function URL requests have no extra charge |
| S3 Standard | $0.005 per 1,000 PUTs, $0.0004 per 1,000 GETs |
| DynamoDB on-demand | $0.625 per million writes, $0.125 per million reads |
| CloudWatch Logs | $0.50 per GB ingested |

| Item | Measured | Cost at list price |
|---|---|---|
| Lambda compute | 3,540 REPORT lines, 1,640 billed GB-seconds (sum of Billed Duration times Memory Size) | about $0.022, inside the free tier's 400,000 GB-seconds a month |
| Lambda requests | 3,540 | under $0.001 |
| S3, DynamoDB, CloudWatch Logs | a few thousand requests each, about 50 MB stored for about 90 minutes, a few MB of logs | under $0.01 each |

Total: under $0.10 against a budget of $5. Cost Explorer lags by about a day and was not checked.

## Open issues and risks

- **n = 30 per cell.** A p99 of 30 samples is close to the maximum. All cells ran within 80 minutes, on one day, in one region. Rerun the cells a decision rests on with n = 100 or more (`COLD_N=100`) before quoting any p99 as final.
- **Cold start below 1769 MB.** At 512 MB, Rust misses the 500 ms p99 by about 120 ms, and all of the miss is fixed costs. Candidates, none measured:
  - Open the S3 connection during init.
  - Load the most-used apps during init (the eager numbers: 518 ms p99 at 512 MB).
  - Find out where deserialize spends its time.
  - Run at 1769 MB. The cost scales with GB-seconds, so a shorter CPU-bound cold start offsets part of the higher memory price.
- **JS cold start.** It misses 500 ms at every size: 498 / 1052 ms at best (zstd, 1769 MB). The 33.5 MB artifact is the cost: fetch plus deserialize, with deserialize p99 spikes of 0.5 to 1.5 s. Options:
  - Open the S3 connection and load the artifact during init.
  - Use a smaller JS engine build.
  - Read the artifact lazily.
  - Put the artifact in the function zip or a layer: no fetch, and the zip limit is 50 MB for direct upload.
  - At 128 MB, neither compiler can compile it at all.
- **Deserialize on Lambda.** 46 to 58 ms for 1 MB Rust artifacts, against 5 to 11 ms locally, and it does not get faster with more CPU. Before the next cloud run, find out what it spends the time on. Also check whether `Component::deserialize_file`, which maps the file, behaves differently from deserializing from bytes.
- **The init-phase CPU boost is undocumented.** The eager and pre-connect gains depend on it, so a design that leans on it needs a fallback if Lambda changes.
- **Winch is out for now.** The cloud numbers miss 500 ms at every size, and Winch costs 15 to 31% on the warm guest-handle median locally. Epoch interruption works under Winch on aarch64 (see "Winch: runaway guests and the memory cap (local)").
  - Security: `aarch64-unknown-linux-gnu` is a Tier 2 target in Wasmtime's stability tiers (missing: continuous fuzzing). Wasmtime's security policy counts only bugs that affect "a tier 1 platform or feature", so on a plain reading an aarch64-only bug is outside it.
  - Cranelift has the same target tier. Winch is the younger compiler.
  - So stored native code needs an integrity check before `deserialize`, for every component.
- **Runaway guests: wall-clock deadline, worker blocking, cap above 128 MB.**
  - The epoch deadline counts wall-clock ticks, not CPU time: about 0.7 s of CPU in the 10 s at 0.07 CPU.
  - A guest that never yields holds its tokio worker until the trap. With one worker, nothing else on that host is served meanwhile. Concurrent runaway requests are interrupted in waves of one worker each, about 10 s per wave.
  - The epoch ticker must stay on an OS thread.
  - Not measured: how many CPUs Lambda reports to `available_parallelism` at each memory size, and whether one invocation at a time makes the blocking moot.
  - The 256 MiB cap is above a 128 MB function, so the container is OOM-killed first. The cap has to follow the function's memory size (a design question).
- **MAC key handling is not part of this spike.** The MAC timings use a fixed test key. Open design questions:
  - Where the key lives, and how it is fetched at init.
  - Whether the check covers the artifact key and Wasmtime version as well as the bytes.
  - Enable sha2's `asm` feature (done in `host/Cargo.toml`), or HMAC-SHA256 costs 3 to 7 times more. Graviton's SHA-2 throughput is untested.
- **Local-path digest cost.** The local-path spec hashes the component; production specs (`sha256:<hex>`) do not. This is not a production concern, but it inflates "load total" in the local deserialize table for the JS component.
- **Infra caveats.**
  - The functions set `lifecycle { ignore_changes = [environment] }`, because `bench/cloud.sh` edits environment variables to force cold starts. A `tofu apply` after a run does not reset them.
  - Function URLs use AWS_IAM, so the calling identity needs `lambda:InvokeFunctionUrl` on the three functions, plus `lambda:InvokeFunction` for newly created URLs.
  - The module grants `s3:ListBucket` so that a missing key returns 404, not 403.
  - The layer version (30) and LWA 1.1.0 were current for this run. Check both before the next apply.
- **Not covered by this spike.** Sustained concurrency, scale-out (many environments cold at once) and S3 request-rate effects. The client curl column is from the Mac that ran the benchmark (about 75 ms of network) and is a reference only.
