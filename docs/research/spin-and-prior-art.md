# Spin, SpinKube, and prior art (fact sheet)

> A research sub-agent gathered this on 2026-09-30 from GitHub, the Spin docs, release notes, and the projects listed below.
> UNVERIFIED marks claims that weren't confirmed against a primary source. Third-party sources are labelled.

## Spin today
- **Version and runtime:** latest stable is Spin v4.2.1 (2026-09-30), running on Wasmtime 49. Spin moves to a new Wasmtime release roughly every month.
- **Governance:** Spin and SpinKube are CNCF Sandbox projects (accepted 2025-01-21). Akamai acquired Fermyon (announced 2025-12-01) and has committed to keep supporting both. Fermyon Wasm Functions became Akamai Functions, which is closed source.
- **WASIp3 and Spin 4.x:** Spin 4.0 made WASIp3 stable, and the HTTP trigger speaks it natively. WASIp2 components still run. Spin 4.1 added HTTP middleware and `*.spin.internal` service chaining.
- **Rejecting apps:** a host that can't support a feature an app needs rejects the app up front, using `host_requirements` in the locked app.
- **Target environments:** `application.targets` lets an app validate against a named environment, so a custom host can publish its own target.

## What SpinKube adds beyond "run Spin apps"
- **SpinApp CRD:**
  - image, executor, replicas, `enableAutoscaling` (HPA/KEDA)
  - selective `components`, `runtimeConfig` (KV/SQLite/LLM), variables, volumes
  - probes, resources, `invocationLimits`
- **SpinAppExecutor CRD:** creates the Deployment and configures OpenTelemetry (OTel) export.
- **Other pieces:** containerd-shim-spin (active, v0.26.0), runtime-class-manager (pre-1.0), and the `spin kube scaffold` plugin.
- **spin-operator:** last release v0.6.1 (2025-07), maintenance only. Matching SpinKube feature for feature would be custom work.

## Embedding Spin's host
- **Crate architecture:** spin-factors, spin-runtime-factors, spin-trigger, spin-trigger-http. The Spin CLI and containerd-shim-spin are the documented embeddings ([SIP 021](https://github.com/spinframework/spin/blob/main/docs/content/sips/021-spin-factors.md)).
- **Not published to crates.io:** only the guest `spin-sdk` is. Every embedder pins Spin by git tag. The docs say the trigger and embedding APIs are "not stabilized".
- **Adapter entry point:** `HttpServer::handle(req, scheme, client_addr)` is public, so a Lambda adapter could call it without a TCP listener.
- **Reference example:** `examples/spin-timer` shows a custom trigger built on these crates.

## Interfaces apps import (`spin:up@4.1.0`)
- **Imports:**
  - `wasi:http` (0.2.6 and 0.3.x) and `wasi:cli`
  - `spin:key-value@3.0.0` and `wasi:keyvalue@0.2.0-draft2`
  - `spin:sqlite@3.1.0` and `spin:variables`
  - `spin:redis`, `spin:mqtt`, `spin:mysql`, `spin:postgres`
  - `wasi:config` and `wasi:otel`
- **Stable:** HTTP and Redis triggers, outbound HTTP and Redis, variables, Postgres, KV, SQLite.
- **Experimental:** cron, MySQL, MQTT, AI.
- **KV semantics:**
  - `spin:key-value` offers get/set/delete/exists/get-keys only. **No CAS.**
  - `wasi:keyvalue` draft2 adds `atomics` (increment and a CAS resource) and `batch`.
  - Spin promises no consistency model; that's left to the provider.
  - Built-in providers: SQLite file, Redis, Cosmos, DynamoDB. There's no S3 provider.
- **Pluggable seam for an object-store backend:**
  - The `StoreManager` / `Store` / `Cas` traits in `crates/factor-key-value/src/host.rs`. The `Cas` doc comment describes etag + If-Match.
  - The DynamoDB provider implements CAS with a version attribute, a close template for an S3 provider.
- **Triggers:** HTTP and Redis are core. Cron, MQTT, SQS, and command are plugins.

## Packaging (OCI)
- **Layers:**
  - The locked-app JSON uses `application/vnd.fermyon.spin.application.v1+config`, which the code marks as non-final.
  - Components are `application/vnd.wasm.content.layer.v1+wasm` layers.
  - Above 500 layers, Spin falls back to a single tar.gz archive layer.
- **Locked app fields:** `spin_lock_version`, `must_understand`, `host_requirements`.
- **Running without the CLI:** possible in-process with `spin_oci::Client::pull` and `OciLoader::load_app`, but those crates are git-only. The format is simple enough to re-implement.

## Cold start
- **Precompiled components:** Spin's `unsafe-aot-compilation` feature loads `.cwasm` via `Component::deserialize_file`. A `.cwasm` only loads with the exact Wasmtime version and CPU target it was built for.
- **Instantiation:** pooling allocator + copy-on-write memory images bring instantiation down from ~2 ms to ~5 µs (SpiderMonkey module, instantiation only).
- **Wizer:** merged into Wasmtime v39 (`wasmtime wizer`), with component support fixed by v43–v47.
- **Wasmtime release cadence:** monthly majors, and every 12th release is LTS. v46 turned WASIp3 on by default.
- **Lambda numbers (weak):**
  - A naive per-invoke `wasmtime run` (JIT): ~2.2 s first invoke, ~130 ms warm ([third-party](https://dev.classmethod.jp/en/articles/20260731-lambda-wasm/)).
  - A 2022 container PoC: ~1 s.
  - **No tuned, precompiled Wasmtime-on-Lambda benchmark exists. We'd have to measure.**
- **Lambda limits:** init is limited to 10 s, environments are frozen between invokes, and `/tmp` persists between invokes.

## Prior art: state in object storage via conditional writes
- **Per-key CAS throughput** ([third-party benchmark](https://cdouglas.github.io/posts/2026/01/conditional)):

  | Store | CAS ops/s |
  |---|---|
  | S3 Standard | ~15 |
  | S3 Express | ~75 |
  | Azure | 9–16 |
  | GCS | ~1 (documented limit) |

  Its conclusion: object stores are "not (yet!) consensus systems".
- **Leader election with epoch-numbered lock files** ([Morling](https://www.morling.dev/blog/leader-election-with-s3-conditional-writes/)).
- **SlateDB:** embedded LSM on object storage. The manifest is updated with CAS, and a single writer is fenced. v0.17, pre-1.0.
- **Litestream:** v0.5+ uses the LTX format, with conditional-write leases (Go API only) and an experimental writable VFS (~1 s sync, single writer).
- **turbopuffer:** writes one WAL object per write, at p50 165 ms for 500 kB and one WAL entry per second per namespace. Closed source.
- **Delta Lake / Iceberg:** delta-rs still documents a DynamoDB lock for S3, and Iceberg relies on a catalog pointer swap. Not evidence that put-if-absent alone scales.
- **Not relevant to this design:** LiteFS (coordinates through Consul), libSQL bottomless (stalled), mvsqlite (built on FoundationDB), WarpStream (proprietary metadata store), Neon (Paxos quorum on local disk).

## Prior art: "Durable Objects on a bucket"
- **[celld](https://github.com/denoland/celld)** (Deno, Apache-2.0, v0.6, alpha) is the closest match:
  - Each cell is owned through a conditional create/CAS with a fencing epoch.
  - SQLite changes are stored as LTX files under `cells/<cell>/ltx/e<epoch>/`.
  - Responses wait until the write is provably durable.
  - It's qualified on S3, R2, GCS, and Azure.
  - Downsides: it needs long-lived daemons, and it needs ≥2 nodes for fast acks (a single node waits a bucket round-trip on every write).
- **Cloudflare SQLite Durable Objects:** a 5-replica / 3-ack follower quorum plus WAL batches uploaded to object storage.
- **Golem:** Wasm components with oplog replay and snapshots. UNVERIFIED whether it can use a bucket as storage.

## Existing OSS closest to this project
Nothing covers Spin/wasi:http + Lambda + object-store KV together.
- **celld:** the state half, but it runs JS/V8 on daemons.
- **mreferre/spin-wasm-multi-compute** (2022, stale): `spin up` running in a container on Lambda.
- **aws-lambda-web-adapter:** would run an unmodified Spin binary on Lambda, but keeps a TCP server inside.
- **wasmCloud:** embeddable runtime, but with its own capability model, not Spin's.
- **warpline:** a Wasmtime 49 host with tenant KV, 0 stars.

## Biggest feasibility risks (agent's assessment)
1. **Spin's host crates are unstable, unpublished, and change monthly.** We'd either fork Spin or keep chasing it, re-precompiling `.cwasm` on every Wasmtime bump.
2. **Spin 4's execution model fits Lambda poorly, and nobody has measured it.** Spin 4 assumes a long-lived server (WASIp3 async, instance reuse, service chaining). Lambda runs one request per environment, freezes between invokes, and limits init to 10 s.
3. **Object-store CAS only gives per-key atomics at about 15 ops/s on S3.** Lambda scale-out means many concurrent writers, so we'd need single-writer ownership with epoch fencing (as celld does). Lease renewal across Lambda freeze/thaw is likely unreliable (inference, unverified).
