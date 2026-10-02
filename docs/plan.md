# torpor build plan

**Status:** draft for approval, 2026-10-01. This plan only puts the decisions in order. The decisions themselves live in [decisions.md](decisions.md), and that file wins if the two disagree. Items marked **(choice)** are small calls this plan makes that the grilling never settled; they're collected in [Choices to confirm](#choices-to-confirm).

## What v1 is

An operator installs torpor into their own AWS account with one OpenTofu module, then deploys WASI components with `torpor deploy`.
- One shared Lambda function serves every app (Q11).
- All platform state lives in buckets (Q1).
- An idle install costs only its storage.

| Target | Value | From |
|---|---|---|
| Cold start p99 | ≤ 500 ms for components of about 5 MB or less; about 1 s for large JS | Q5, Q47 |
| Warm read p50 | ≤ 30 ms | Q5 |
| Acknowledged write p99 | ≤ 200 ms | Q5 |
| Idle cost | ≤ $0.05/month | Q4 |
| 1M requests + 100k writes | about $1–2/month | Q4 |

**Not in v1:** cron, custom domains (CloudFront), GCP and Azure installs, `deploy --from oci://`, `deploy --plug`, the Spin adapter, dedicated functions, and the rest of the [deferred table](decisions.md#deferred-work-and-fast-follows).

## Shape of the code

- **Crate and binary (Q18, Q43):** one crate, `torpor-cli`, and one binary, `torpor`.
- **Subcommands:** `serve`, `compile-worker`, `deploy`, `rollback`, `secrets` and `gc`.
- **Lambda adapter:** Lambda runs `torpor serve` and `torpor compile-worker` behind the Lambda Web Adapter (Q29), so there is no Lambda crate.
- **Libraries:**
  - Wasmtime (the latest release when M1 starts; 49.0.1 in the spike), with `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config`;
  - `object_store` 0.14;
  - tokio and hyper;
  - `age`, `zstd`, serde, `toml`, `clap` and `tracing`.

**Line budget:** about 1,200 lines of Rust plus about 250 of HCL. A module that runs well past its budget is a design problem to raise, not to push through.

| Module | Does | Budget |
|---|---|---|
| `engine` | Wasmtime config, epoch ticker, limits, loading native code, Cranelift fallback | ~120 |
| `guest` | A fresh store per request, a WASI context that grants nothing, p2/p3 dispatch | ~90 |
| `outbound` | Allow list plus the resolved-address block, in the HTTP send hook | ~60 |
| `kv` | `wasi:keyvalue` draft2 over the bucket: cache, CAS, generation-cached listing | ~200 |
| `config` | `wasi:config` from the manifest, with secrets decrypted | ~30 |
| `state` | State object and manifests, revalidation, defensive reads | ~100 |
| `serve` | hyper server, routing, logs | ~100 |
| `compile` | The compile worker and compile-request markers | ~70 |
| `cli` | `deploy`, `rollback`, `secrets`, `gc` | ~350 |
| `infra/aws` | OpenTofu module | ~250 HCL |

### Bucket layout

Q43 applies: no product name appears in any stored key or field, so a rename never needs a migration.

| Bucket | Key | What | Who writes |
|---|---|---|---|
| app | `state` | The global state object (Q45) | CLI (CAS) |
| app | `blobs/sha256/<hex>` | Components, at the OCI layout path (Q40) | CLI (put-if-absent) |
| app | `manifests/<hex>` | Immutable, parent-linked release manifests | CLI (put-if-absent) |
| app | `kv/<app>/<store>/<key>` | KV values, percent-encoded keys (Q42) | serving function |
| app | `kvgen/<app>/<store>` | Generation object for cached listing (Q42) | serving function |
| app | `compile/<hex>` | Compile requests | CLI, serving function; compile function deletes |
| native | `<hex>/<compat hash>.zst` | Compiled native code (Q54) | **compile function only** |

**IAM shape:**
- The serving function reads the app bucket and may write only `kv/`, `kvgen/` and `compile/`. It can only read the native bucket.
- Deployers have full access to the app bucket. In the native bucket they get delete but never put, so `gc` can prune **(choice)**.
- The compile function reads `blobs/`, deletes `compile/`, and writes the native bucket.

## Milestones

Each milestone is a branch in its own worktree under `.worktrees/`. It ends with:
- tests passing in Docker;
- a line count against the budget;
- your review.

It is then merged locally into `main`.

### M1: Runtime core (`torpor serve` runs one app locally)

- **Setup:**
  - Scaffold the crate (Apache-2.0, Q24).
  - Add a root `compose.yaml` (the spike's AL2023 build image, plus a `test` service).
  - Merge `spike/latency` into `main` so `prototypes/latency/` stays on record (Q13) **(choice)**.
- **Engine:** Cranelift with the p3 async features.
  - An epoch ticker on an OS thread every 100 ms, with a 10 s deadline that traps (Q35, Q57).
  - `StoreLimits` of 256 MiB, plus caps on instance and table counts (Q55).
- **Each request:** a fresh store, and a WASI context that grants nothing: no environment, arguments, preopened files or sockets (Q55).
- **Guest output:** stdout and stderr are captured into the host log, tagged with the app and capped per request **(choice)**.
- **HTTP:** serves both p2 `incoming-handler` and p3 `handler` through `wasmtime-wasi-http` (Q31). The spike's `guest.rs` carries over.
- **Dev mode:** `torpor serve` in an app directory reads `torpor.toml` (name, component, allowed outbound hosts, `[config]`) and runs that one app against the in-memory store. Secrets are never in `torpor.toml`; in dev mode they come from `TORPOR_VAR_<KEY>` environment variables **(choice)**.
- **Logs:** JSON lines to stdout. Warnings and errors by default (Q14).

**Done when:**
- The spike's Rust p2, Rust p3 and JS components respond.
- The loop component traps at 10 s and the next request still succeeds.
- A memory-hungry guest is refused at 256 MiB.
- A guest can see no host environment or files.
- Everything runs with `docker compose run --rm test`.

### M2: App interfaces (KV, config, outbound HTTP)

- **`wasi:keyvalue@0.2.0-draft2`** through our own `bindgen!` over vendored WIT (Q44):
  - Keys are 256 B or less and percent-encoded; values are 1 MiB or less (Q42).
  - Each instance caches values with their ETags for 1 s and sees its own writes immediately (Q33).
  - `cas` maps to `If-Match`; `increment` is a CAS loop.
  - `list-keys` uses the generation object and cached LIST (Q42).
  - An app may open any store whose name matches `[a-z0-9-]{1,64}`, scoped to that app, with no manifest field **(choice)**.
- **`wasi:config@0.2.0-rc.1`:** `wasmtime-wasi-config` over the manifest's flat key set (Q32).
- **Outbound `wasi:http`:**
  - The allow list uses the Spin subset syntax (Q38), and denies everything when an app lists no hosts (Q37).
  - The host resolves names itself and blocks loopback, link-local and private addresses, including IPv4-mapped IPv6 (Q34).
  - Calls count against the 10 s deadline.
  - Redirects need no special handling, because the guest makes each hop as a new request.
- **Docs:** write the app-author page on the consistency model (Q33) and the LIST cost.

**Done when:**
- KV tests pass on the in-memory store, including racing CAS writers and a list taken within 1 s of a write.
- A Spin SDK guest and a plain `wit-bindgen` guest both use KV and config.
- These outbound targets are all blocked:
  - `127.0.0.1`, `::1`, `::ffff:127.0.0.1`, `169.254.169.254` and `10.0.0.1`;
  - an allowed hostname whose DNS points at a private address.

### M3: State and deploy (many apps, one host)

- **`torpor deploy`:**
  - Builds a release manifest from `torpor.toml` and puts the component and manifest put-if-absent.
  - Swaps the state object with CAS and randomized backoff (Q45). A deploy of several apps is one swap.
  - Waits 5 s (Q10).
  - Carries the previous release's secrets forward.
  - Rejects a key that appears in both config and secrets (Q32).
- **`torpor secrets set NAME` and `list`:** each value is age-encrypted separately to `TORPOR_RECIPIENT`, read from the environment and never from the bucket **(choice)**. `secrets set NAME` reads the value from stdin and makes a new release. `list` shows names only.
- **`torpor rollback <app>`:** moves the app to its parent release. Rollback depth is 10 (Q48).
- **`torpor serve --store <url>`** (the install mode, which Lambda runs):
  - Reads `state` before it starts listening, so the read happens in Lambda's start-up phase (Q56).
  - Rechecks `state` with a conditional GET once it is 5 s old (Q45).
  - Routes by `domains`, then by path prefix, stripping the prefix and adding `X-Forwarded-Prefix` (Q41).
  - Decrypts secrets with the age identity from its environment.
- **Bucket contents are never trusted (Q53):**
  - reads are size-capped and parsed strictly;
  - every blob's hash is checked on read.
- **Logs:** per-app sampling of request log lines (Q14).

**Done when:**
- In-process tests deploy two apps, serve them, and roll one back, all on the in-memory store.
- A blob with the wrong hash is refused.
- Concurrent deploys both land.
- Secrets round-trip, and `secrets list` never decrypts.

### M4: Native code (compile once, run everywhere)

This refines the trigger in Q58 **(choice)**; see [Choices to confirm](#choices-to-confirm).

- **`torpor compile-worker`** receives bucket events through the Lambda Web Adapter's pass-through path (`POST /events`). For each `compile/<hex>`:
  1. It fetches and hash-checks the blob.
  2. If `<hex>/<own compat hash>.zst` already exists, it skips to step 5.
  3. It compiles with Cranelift.
  4. It writes the zstd-compressed result to the native bucket.
  5. It deletes the marker.
- **Serving:**
  - **Loading:** native code comes only from the native bucket. It is decompressed into `/tmp` and loaded with `deserialize_file`, which maps the file.
  - **On a miss:** the host writes `compile/<hex>` once per environment, then compiles locally with Cranelift into `/tmp` (Q58).
- **Deploy:** writes `compile/<hex>` and waits until it is deleted. A timeout points at the compile logs. The CLI never reads the native bucket.

**Done when:** tests over two in-memory stores (app and native) cover:
- the normal path;
- a miss, which compiles locally and writes a marker;
- native code planted in the app bucket, which is never loaded;
- a changed compat hash, which recompiles lazily;
- a failed compile, which leaves the marker so the next miss asks again.

### M5: AWS install and cloud validation (needs your explicit approval to apply)

- **`infra/aws`** (latest OpenTofu, Q20) contains:
  - the app and native buckets;
  - the **serving function:** arm64 on `provided.al2023` with the Lambda Web Adapter layer. 1769 MB by default, rejecting anything under 512 MB (Q51). A public Function URL with a reserved-concurrency cap and a budget alert (Q50). The age identity as a sensitive variable;
  - the **compile function:** the same binary at 3008 MB with a 120 s timeout **(choice)**, fired by bucket events on `compile/`, and updated before the serving function;
  - the IAM shape above, a log-retention variable (default 7 days) **(choice)**, and tags on everything.
- **Build:** the release build runs in Docker against AL2023's glibc and produces the zip.
- **One cloud session:** apply, run the checks below, and destroy the same day, with a $5 cap.
  - Cold start p99 at n = 100, at 1769 and 1024 MB, for Rust and JS.
  - `deserialize_file` against `deserialize` (the 46–58 ms lead).
  - The state read during start-up.
  - Compile times, and marker-to-native-code latency.
  - Deploy time, end to end.
  - The storage suite against a real S3 bucket (Q12).

**Done when:**
- On a fresh account: apply, deploy, and the request works.
- The targets are met, or each gap is written up with options.
- After destroy, nothing tagged is left.

### M6: First release

- **`torpor gc` (Q48):** keeps each app's last 10 releases, with a 1 h grace period. It prunes unreachable blobs, manifests and native code, and keeps only the newest compat hash per component.
- **Docs:**
  - a README quick start;
  - an app-author guide: the contract, consistency, limits, and composition with `wac plug`, including that composed parts share capabilities;
  - an operator guide: install, lazy recompiles on upgrade, the rollback-restores-old-secrets catch, the trust model, and costs.
- **Release:**
  - Rename `spinit` to `torpor` across the docs, and run a trademark and domain check (Q43).
  - Create the GitHub repo, with Actions running the Docker tests (the real-bucket suite is run by hand).
  - Publish `torpor-cli`.

**Done when:** a new user can get from nothing to a deployed app using only the README.

## Risks being watched

| Risk | Where it's settled |
|---|---|
| Deserialize takes 46–58 ms on Lambda | M5 measures `deserialize_file` |
| Large JS components about 1 s cold | M5; dedicated functions are a deferred item |
| The p3 outbound send hook in `wasmtime-wasi-http` may differ from p2's | Checked first in M2 |
| The Lambda Web Adapter's event pass-through for the compile function | Checked first in M4, proven in M5 |
| State object size at about 10k apps | Deferred (Q45) |

## Choices to confirm

These are small calls this plan makes. Say which, if any, to change.

1. **Merge `spike/latency` into `main`** at the start of M1, so `prototypes/latency/` and its results stay on record (Q13).
2. **One kind of compile marker** (refines Q58):
   - **What:** `compile/<hex>`. Deploy writes it, and so does a serving miss (once per environment). The compile worker deletes it when the native code exists, and deploy waits for that deletion.
   - **Why:** compared with Q58's create-only `compile/<compat hash>/<hex>` plus a component-upload trigger, this needs one trigger instead of two. The CLI never needs the compat hash or the native bucket, a failed compile retries on the next miss, and redeploying an already-uploaded component still works.
   - **Cost:** at most one tiny PUT per cold environment until the native code lands.
3. **Deployers get delete-only on the native bucket** so `gc` can prune it. Deleting live native code only causes a recompile.
4. **KV store names:** any `[a-z0-9-]{1,64}`, scoped to the app, with no manifest field.
5. **CLI settings from the environment:** `TORPOR_STORE` (bucket URL) and `TORPOR_RECIPIENT` (age public key, copied from `tofu output`). The recipient is never read from the bucket, because a bucket writer could swap it and read secrets set afterwards.
6. **Dev-mode secrets:** `TORPOR_VAR_<KEY>` environment variables. `torpor.toml` never holds secrets.
7. **Guest stdout/stderr:** captured into the host log, tagged with the app and capped per request.
8. **Defaults:** the compile function at 3008 MB with a 120 s timeout; log retention of 7 days.
9. **"CI" is local Docker until M6** creates the GitHub repo. The real-bucket suite runs in M5's cloud session and by hand after that.
