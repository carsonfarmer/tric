# Milestone 0: latency spike

Status (2026-10-01): the local phase is done. The cloud phase is built and validated but has not been run: it needs AWS credentials, and this phase used none. Every number below the "Local" heading is indicative only, and no decision in the table below is taken yet.

## What the spike decides

| Decision | Rule | Status |
|---|---|---|
| Bucket KV or DynamoDB for app state | Ship bucket KV only if the warm read p50 is at most 30 ms with revalidation (conditional GET, 304) and the acknowledged write p99 is at most 200 ms (conditional PUT) | Needs the cloud run |
| Web Adapter (LWA) or `lambda_http` | Switch to `lambda_http` if LWA adds more than about 5 ms to the warm p50 or about 50 ms to a cold start | Local: far below both (see below). Needs the cloud run |
| Precompile strategy | Compile in the function, or deserialize a precompiled `.cwasm` | Local says precompile is mandatory below 1769 MB. Needs the cloud run to confirm |
| Targets | Cold start p99 at most 500 ms. Warm read p50 at most 30 ms inside the function. Acknowledged write p99 at most 200 ms | Cloud run |

## What is here

All paths are relative to `prototypes/latency/`.

| Path | What |
|---|---|
| `host/` | The host: Wasmtime 49.0.1, plain hyper, `object_store` 0.14, `aws-sdk-dynamodb`. Config by environment variables (documented at the top of `host/src/main.rs`). One JSON log line per request (microseconds), no response headers. `precompile` and `publish` subcommands. `/__bench/<op>` routes (spike only) and `/__ready` |
| `components/` | Three test components: Rust p3 (`hello_p3`), Rust p2 (`hello_p2`), JavaScript through jco (`hello_js`). All imports are `wasi:*`, checked by `build.sh` with wasm-tools. Sources are copies of the host-research apps with the Spin-specific parts removed |
| `docker/`, `lambda/` | Build image (Amazon Linux 2023, glibc 2.34, the `provided.al2023` runtime's glibc), host build, Lambda zip, LWA + RIE image |
| `compose.yaml` | Project `spinit-spike-latency`: MinIO, DynamoDB Local, host (CPU and memory limited), host behind LWA via the Lambda RIE, oha. Host ports are env-configurable (`SPINIT_HOST_PORT` 18080, `MINIO_HOST_PORT` 19000, `DDB_HOST_PORT` 18000, `LWA_HOST_PORT` 19001) |
| `infra/` | OpenTofu module for us-west-2. Validated with `tofu init -backend=false` and `tofu validate` only. Never planned or applied |
| `bench/local.sh`, `bench/stats.py` | The local measurements and the tables below (`out/local/` holds the raw logs, gitignored) |
| `bench/cloud.sh`, `bench/cloud_report.py` | The cloud measurements and their tables. Written, not run |

Reproduce the local phase (everything runs in Docker; nothing is installed on the host):

```sh
cd prototypes/latency
docker/build-host.sh && components/build.sh && lambda/package.sh   # out/spinit-host, out/hello_*.wasm, out/spinit-host.zip
bench/local.sh all                                                  # or: functional cold bucket portable warm lwa
python3 bench/stats.py                                              # the tables in this file
docker compose -p spinit-spike-latency down -v                      # teardown (named build-cache volumes spinit-spike-latency-* stay)
```

### Build

| Artifact | Size |
|---|---|
| `out/spinit-host` (aarch64, glibc 2.34 at most, stripped, fat LTO, codegen-units 1) | 25,501,160 bytes (24.3 MiB) |
| `out/spinit-host.zip` (one file, `bootstrap`) | 10,738,148 bytes (10.2 MiB) |

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
- **Local-path component spec.** The local path computes a SHA-256 of the whole wasm for the cache key (22 ms for the 12.8 MB JS component at 1.0 CPU) and reads it from disk. The production `sha256:<hex>` spec skips both, so the "load total" for `hello_js` in the deserialize table overstates it. The bucket table is the production route.
- **CPU compatibility of `.cwasm`.** Artifacts compiled on Apple Silicon use that machine's CPU features. `SPINIT_TARGET=aarch64-unknown-linux-gnu` compiles for a baseline arm64 ISA instead. A baseline artifact deserialized fine in a default engine on the same machine, which does not prove it loads on Graviton. `bench/cloud.sh seed` publishes baseline artifacts, and the first cloud request confirms.
- The Wasmtime epoch ticker and the 256 MiB `StoreLimits` cap are on in every run. Fuel is off.

### What the local numbers say

- **Compile in the request path does not fit.** Cranelift for the 304 KB Rust component takes 137 ms at 1.0 CPU, 536 ms at 0.29 and 5.9 s at 0.07. The 12.8 MB JavaScript component takes 5.6 s at 1.0 CPU, 28.7 s at 0.29, and is OOM-killed at 128 MB (RSS about 292 MB while compiling). At 512 MB even the small components miss the 500 ms target on the compile alone. So the artifact has to be precompiled at publish time and fetched, and the compile fallback must be limited to the large-memory case or fail clearly.
- **Deserialize is cheap.** From a cached `.cwasm`, the Rust components load in about 7 to 13 ms (deserialize 5 to 11 ms) at every CPU profile. From the bucket (MinIO) the whole first request is 10 to 19 ms for the Rust components, with the 98 ms p2 first request at 0.07 a one-off to re-check in the cloud run. The JS component, with a 33.5 MB artifact, takes 27 ms at 1.0 and 0.29 CPU and 419 ms at 0.07 CPU (116 ms to fetch from MinIO, 199 ms to deserialize). The real S3 fetch of those 33 MB is the unknown that decides the JS cold start.
- **Process init is small.** 12 to 18 ms from process start to listening at 1.0 and 0.29 CPU, 108 to 183 ms at 0.07 CPU (probably less in a Lambda init phase).
- **Warm requests are dominated by nothing in the host.** Host overhead (host total minus guest handle) is about 2 µs unpaced and about 20 µs paced. Host total p50 at 1.0 CPU: `hello_p3` 31 µs, `hello_p2` 85 µs, `hello_js` 365 µs. Paced at 50 requests per second, client p50 is 0.9, 1.3 and 2.6 ms and p99 is 3.7, 4.7 and 7.1 ms, so guest work fits in a couple of milliseconds and leaves more than 25 ms of the 30 ms read budget to storage. At 0.07 CPU (128 MB) `hello_js` saturates (p99 about 103 ms even paced), so JS apps want 512 MB or more, and the Rust ones are fine at 128 MB.
- **p3 against p2.** p3 reuses a worker and shows 0 µs per-request instantiate, p2 instantiates every request (22 µs p50 unpaced, about 240 µs paced with cold caches). Both are negligible next to a storage call.
- **Allocator.** Pooling against the default allocator made no meaningful difference to deserialize, first request or warm latency at 1.0 CPU, so the default (less code) is the provisional choice.
- **Web Adapter.** Adapter plus RIE overhead is 453 µs p50 (744 µs p99) for `hello_p3` and 525 µs p50 (952 µs p99) for `hello_p2`. The INIT REPORT is 26 to 29 ms against about 12 ms for the host alone, so roughly 14 to 17 ms more at init. Both are far below the switch thresholds (5 ms warm, 50 ms cold). Real Lambda decides.
- **One bug found.** With 16 concurrent connections the p3 p99 was about 41 ms, which was Nagle plus delayed ACK (headers and body leave in separate writes). The host now sets `TCP_NODELAY` on accepted sockets: p99 2.5 ms. The earlier JSON is in `out/local-pre-nodelay/`. The adapter-to-host loopback hop is the same shape, so keep an eye on it in the cloud numbers.
- **Functional checks pass.** MinIO (a stand-in for S3): create-if-absent succeeds, then reports AlreadyExists for an existing key; update with the current ETag succeeds; update with a stale ETag fails the precondition; GET with `If-None-Match` of the current ETag returns NotModified. DynamoDB Local: conditional put on the current version succeeds, on a stale version fails with ConditionalCheckFailedException. (S3's ETag is the MD5 of the body, so If-Match tests use distinct payloads.) These behaviours must still be confirmed on real S3.

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

| component | Docker --cpus | route | fetch ms | compile or deserialize ms | write /cache ms | load total ms | whole first request ms | route taken |
|---|---|---|---|---|---|---|---|---|
| hello_p3 | 1.0 (~1769 MB) | blob + compile | 2.7 | 136.1 | 0.2 | 140.2 | 141.9 | compile |
| hello_p3 | 1.0 (~1769 MB) | precompiled artifact | 3.0 | 5.8 | 0.2 | 9.2 | 10.8 | cwasm |
| hello_p3 | 0.29 (~512 MB) | precompiled artifact | 2.8 | 9.4 | 0.1 | 12.5 | 14.2 | cwasm |
| hello_p3 | 0.07 (~128 MB) | precompiled artifact | 3.9 | 12.5 | 0.2 | 16.9 | 19.1 | cwasm |
| hello_p2 | 1.0 (~1769 MB) | blob + compile | 2.3 | 109.6 | 0.2 | 113.4 | 114.9 | compile |
| hello_p2 | 1.0 (~1769 MB) | precompiled artifact | 2.8 | 7.0 | 0.1 | 10.3 | 11.8 | cwasm |
| hello_p2 | 0.29 (~512 MB) | precompiled artifact | 2.7 | 5.0 | 0.1 | 8.2 | 9.9 | cwasm |
| hello_p2 | 0.07 (~128 MB) | precompiled artifact | 6.5 | 6.1 | 0.4 | 13.6 | 98.1 | cwasm |
| hello_js | 1.0 (~1769 MB) | blob + compile | 6.3 | 5053.5 | 6.2 | 5097.9 | 5102.1 | compile |
| hello_js | 1.0 (~1769 MB) | precompiled artifact | 12.1 | 8.8 | 4.1 | 25.4 | 27.5 | cwasm |
| hello_js | 0.29 (~512 MB) | precompiled artifact | 11.8 | 8.6 | 3.8 | 24.9 | 27.1 | cwasm |
| hello_js | 0.07 (~128 MB) | precompiled artifact | 115.6 | 199.1 | 5.6 | 321.9 | 418.7 | cwasm |

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

## Cloud (us-west-2)

Not run yet. Order of operations (needs AWS credentials in the environment or `AWS_PROFILE`, and `aws` CLI v2, curl 7.75 or newer, python3, docker):

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

`bench/cloud.sh` header lists the IAM permissions the caller needs. For a defensible cold p99 use more than 30 samples, for example `COLD_N=100 bench/cloud.sh cold 1769 hello_p3 precompiled`. `bench/cloud_report.py` prints these tables and a meets-or-misses line against each target.

### Cold starts (REPORT: Init Duration plus Duration of the first request, ms; p50 / p99)

| component | mode | MB | n | Init | Duration (1st request) | Init + Duration | max | host load total | fetch from bucket | deserialize or compile |
|---|---|---|---|---|---|---|---|---|---|---|
| hello_p3 | precompiled | 128 | | | | | | | | |
| hello_p3 | precompiled | 512 | | | | | | | | |
| hello_p3 | precompiled | 1769 | | | | | | | | |
| hello_p3 | precompiled-eager | 128 / 512 / 1769 | | | | | | | | |
| hello_p3 | compile | 128 / 512 / 1769 | | | | | | | | |
| hello_p2 | precompiled | 128 / 512 / 1769 | | | | | | | | |
| hello_js | precompiled | 128 / 512 / 1769 | | | | | | | | |
| hello_js | compile | 1769 | | | | | | | | |

### Warm requests through the guest (ms; p50 / p99)

| component | MB | REPORT Duration | host total | guest handle | adapter overhead (Duration minus host total) |
|---|---|---|---|---|---|
| hello_p3 | 128 / 512 / 1769 | | | | |
| hello_p2 | 128 / 512 / 1769 | | | | |
| hello_js | 128 / 512 / 1769 | | | | |

### Storage operations from inside the function (ms, 1 KB object, sequential)

| operation | MB | n | first | p50 | p99 | max |
|---|---|---|---|---|---|---|
| S3 GET | 128 / 512 / 1769 | | | | | |
| S3 conditional GET (304) | 128 / 512 / 1769 | | | | | |
| S3 PUT create-if-absent | 128 / 512 / 1769 | | | | | |
| S3 PUT If-Match update | 128 / 512 / 1769 | | | | | |
| DynamoDB GetItem eventual | 128 / 512 / 1769 | | | | | |
| DynamoDB GetItem strong | 128 / 512 / 1769 | | | | | |
| DynamoDB conditional PutItem | 128 / 512 / 1769 | | | | | |

### Decisions (filled after the run)

| Decision | Measured | Verdict |
|---|---|---|
| Cold start p99 at most 500 ms (`hello_p3`, precompiled, per memory size) | | |
| Bucket KV read p50 at most 30 ms with revalidation | | |
| Bucket KV write p99 at most 200 ms | | |
| Web Adapter overhead at most 5 ms warm and 50 ms cold | | |
| Precompiled artifact loads on Graviton (baseline target) | | |
| JS cold start with a 33 MB artifact | | |
| Allocator (default or pooling) | | |

## Cost of the cloud run

Prices assumed (arm64, us-west-2): Lambda $0.0000133334 per GB-second and $0.20 per million requests, S3 Standard $0.005 per 1,000 PUTs and $0.0004 per 1,000 GETs, DynamoDB on-demand $0.625 per million writes and $0.125 per million reads, CloudWatch Logs $0.50 per GB ingested. Function URL requests have no extra charge. Init time is billed.

| Item | Volume with the defaults | Cost |
|---|---|---|
| Lambda cold starts | 570 starts (6 cells of 30 per memory size, plus 30 JS compiles), mostly 0.1 to 2 s at 128 to 1769 MB; the 30 JS compile starts are about 6 s at 1769 MB each | about $0.01 to $0.02 |
| Lambda warm requests | about 3,000, a few ms each | under $0.01 |
| Storage operation runs | 7 operations x 3 sizes x 200 samples, plus warm-ups | under $0.01 |
| S3 requests and storage | a few thousand requests, about 50 MB stored for a day | under $0.02 |
| DynamoDB | a few thousand requests | under $0.01 |
| CloudWatch Logs | a few MB ingested, 1-day retention | under $0.01 |
| Cold-start config updates | free | $0 |

Expected total about $0.05, pessimistic under $0.50, against a budget of $5. Doubling the sample counts or running 100 cold starts per cell still stays under $1. Left running, the module costs next to nothing (one S3 object set, an on-demand table with no traffic), and `destroy` removes it.

## Open issues and risks

- **Real S3 and Graviton are untested.** Every latency here is local; the storage numbers especially say nothing about S3. The decisions all wait on the cloud run.
- **JS cold start.** The 33.5 MB artifact is the largest cold-path cost (fetch plus deserialize), and at 128 MB the compile fallback cannot run at all. Options to evaluate if the cloud numbers are bad: compress the artifact (zstd), range-read lazily, keep JS apps at 512 MB or more, or cache in the layer or zip (the zip limit is 50 MB direct upload).
- **`.cwasm` portability.** The baseline-target artifact is only verified on the same Apple Silicon machine. A Graviton deserialize error would surface as a failed first request; the host then falls back to compiling, which at small sizes would blow the 500 ms target. Artifact keys include the architecture and Wasmtime version, so a mismatch cannot silently reuse a wrong artifact across versions.
- **Local-path digest cost.** The local-path spec hashes the component; production specs (`sha256:<hex>`) do not. Not a production concern, but it inflates the "load total" in the deserialize table for the JS component.
- **p2 one-off.** One p2 first request at 0.07 CPU took 98 ms against 10 to 14 ms for the load itself, never reproduced; watch for it in cold p99.
- **Tail latency at 128 MB.** At 0.07 CPU, paced runs still show p99.9 up to 73 ms for p3, and unpaced runs show the throttling artifact. Whether Lambda at 128 MB has comparable jitter is a cloud question.
- **Adapter measured through the RIE only.** If the cloud REPORT Duration minus host total exceeds the thresholds, switch to `lambda_http`: only the HTTP front end changes, the Wasmtime core and loader stay.
- **Infra caveats.** `lifecycle { ignore_changes = [environment] }` on the functions, because `bench/cloud.sh` edits environment variables to force cold starts; `tofu apply` after a run will not reset them. Function URLs use AWS_IAM, so the calling identity needs `lambda:InvokeFunctionUrl` (and `lambda:InvokeFunction` for newly created URLs) on the three functions. The module sets `s3:ListBucket` so a missing key returns 404, not 403.
- **Not covered by this spike:** the layer version (30) and LWA 1.1.0 were current when written; check before applying. Sustained concurrency, scale-out behaviour (many environments cold at once) and S3 request-rate effects are out of scope.
