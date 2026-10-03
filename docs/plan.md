# torpor build plan

**Status:** draft for approval, 2026-10-01. This plan only puts the decisions in order. The decisions themselves live in [decisions.md](decisions.md), and that file wins if the two disagree. Items marked **(choice)** are small calls this plan makes that the grilling never settled; they're collected in [Choices to confirm](#choices-to-confirm).

## What v1 is

An operator installs torpor into their own AWS account with one OpenTofu module, then its teams publish and release WASI components with `torpor publish` and `torpor release`.
- One shared Lambda function serves every app, each at its own subdomain of one domain (Q11, Q67).
- All platform state lives in buckets (Q1).
- An idle install costs only its storage.

| Target | Value | From |
|---|---|---|
| Cold start p99 | ≤ 500 ms for components of about 5 MB or less; about 1 s for large JS | Q5, Q47 |
| Warm read p50 | ≤ 30 ms | Q5 |
| Acknowledged write p99 | ≤ 200 ms | Q5 |
| Idle cost | ≤ $0.05/month | Q4 |
| 1M requests + 100k writes | about $1–2/month | Q4 |

**Not in v1:** cron, custom domains (Q66), GCP and Azure installs, `deploy --from oci://`, `deploy --plug`, the Spin adapter, dedicated functions, and the rest of the [deferred table](decisions.md#deferred-work-and-fast-follows).

## Shape of the code

- **Crate and binary (Q18, Q43):** one crate, `torpor-cli`, and one binary, `torpor`.
- **Subcommands:** `serve`, `compile-worker`, `publish`, `release`, `releases`, `secret`, `secrets` and `gc`.
- **Lambda adapter:** Lambda runs `torpor serve` and `torpor compile-worker` behind the Lambda Web Adapter (Q29), so there is no Lambda crate.
- **Libraries:**
  - Wasmtime (the latest release when M1 starts; 49.0.1 in the spike), with `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config`;
  - `object_store` 0.14;
  - tokio and hyper;
  - `zstd`, serde, `toml`, `clap` and `tracing`.

**Line budget:** about 1,390 lines of Rust (1,200 before M2, 1,290 before M3 and its review raised `serve` and `state`; 943 written so far, after four trim passes) plus about 250 of HCL. A module that runs well past its budget is a design problem to raise, not to push through.

| Module | Does | Budget |
|---|---|---|
| `engine` | Wasmtime config, epoch ticker, limits, a fresh store per request with a WASI context that grants nothing, p2/p3 dispatch, loading native code, Cranelift fallback | ~210 (`engine` ~120 and `guest` ~90, merged in the trim pass; 225 after the trim passes, before native code) |
| `outbound` | Allow list plus the resolved-address block, in the HTTP send hook | ~100 (raised in M2 from ~60: the matcher is hand-written, and the connect is our own; 118 after the trim passes) |
| `kv` | `wasi:keyvalue` draft2 over the app's own prefix of the bucket: one object per key, CAS, no cache | ~250 (raised in M2 from ~200; 219 after the trim passes) |
| `config` | `wasi:config` from the manifest, with secrets | 0 (Wasmtime's crate serves it, and `serve` adds secrets in 1 line) |
| `state` | The bucket layout, releases, compare-and-swap updates, defensive reads | ~130 (raised in the M3 review from ~100; 102 after the trim passes) |
| `serve` | hyper server, routing, logs | ~170 (raised in M3 from ~100, which was the router alone, then in the M3 review from ~140 for a pointer recheck per app; 125 after the trim passes) |
| `compile` | The compile worker and compile-request markers | ~70 |
| `cli` | `publish`, `release`, `releases`, `secret`, `secrets`, `gc`, and `main`'s arguments | ~350 (146 after the trim passes, before `gc` and `compile`) |
| `infra/aws` | OpenTofu module | ~250 HCL |

### Bucket layout

Q43 applies: no product name appears in any stored key or field, so a rename never needs a migration.

| Bucket | Key | What | Who writes |
|---|---|---|---|
| app | `apps/<app>/blobs/sha256/<hex>` | Components, in the OCI layout under the app (Q40, Q72) | team (put-if-absent) |
| app | `apps/<app>/releases/<hex>` | Immutable releases: component, config, allow list (Q69) | team (put-if-absent) |
| app | `apps/<app>/current` | The release the app serves, and its secrets, in plain (Q65, Q73) | team (CAS) |
| app | `kv/<app>/<store>/<key>` | KV values, percent-encoded keys (Q42) | serving function |
| app | `compile/<hex>` | Compile requests | team, serving function; compile function deletes |
| native | `<hex>/<compat hash>.zst` | Compiled native code (Q54) | **compile function only** |

**IAM shape:**
- The serving function reads the app bucket and may write only `kv/` and `compile/`. It can only read the native bucket.
- Each team has a role tagged `team`, and one ABAC policy lets it read, write and list only `apps/${team}-*`, so a team owns the apps named `<team>-…`, plus write `compile/` (Q64, Q72). Team names have no hyphens. Nobody but the serving function writes `kv/`.
- The admin, who runs `gc`, gets delete but never put in the native bucket, so `gc` can prune **(choice)**.
- The compile function reads `apps/*/blobs/`, deletes `compile/`, and writes the native bucket.

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
  - An epoch ticker on an OS thread every 10 ms; a guest yields on each tick, and a 10 s deadline drops its store (Q35, Q57 as applied in M1).
  - `StoreLimits` of 256 MiB, plus caps on instance and table counts (Q55).
- **Each request:** a fresh store, and a WASI context that grants nothing: no environment, arguments, preopened files or sockets (Q55).
- **Guest output:** stdout and stderr are captured into the host log, tagged with the app and capped per request **(choice)**.
- **HTTP:** serves both p2 `incoming-handler` and p3 `handler` through `wasmtime-wasi-http` (Q31). The spike's `guest.rs` carries over.
- **Dev mode:** `torpor serve` in an app directory reads `torpor.toml` (name, component, allowed outbound hosts, `[config]`) and runs that one app against the in-memory store. Secrets are never in `torpor.toml`; in dev mode they come from `TORPOR_VAR_<KEY>` environment variables **(choice)**. Since Q79, dev mode publishes and releases the directory into an in-memory install and serves that, at `<name>.localhost`.
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
  - Nothing is cached: every call goes to the bucket (M2 review, replacing Q33's cache).
  - `cas` maps to `If-Match`; `increment` is a CAS loop.
  - `list-keys` is one LIST per page.
  - An app may open any store whose name matches `[a-z0-9-]{1,63}` (64 until the M3 review), scoped to that app, with no manifest field **(choice)**.
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

### M3: State and releases (many apps, many teams, one host)

The M3 review reshaped this milestone; decisions.md Q60–Q79 has the why.

- **`torpor publish [DIR]`:** checks that the component loads, puts it and a release put-if-absent under the app's name, and prints `APP ID` (Q61, Q70, Q72).
- **`torpor release APP ID`:** checks the release, then swaps it into the app's `current` with CAS, and fails if another change landed in between (Q77). Rolling back is releasing an older id, and `torpor releases APP` lists them newest first (Q69).
- **`torpor secret APP NAME` and `secrets APP`:** set a secret from stdin into the app's `current`, or remove it with an empty value, so it takes effect without a release and outlives releases (Q65, Q73, Q76). A secret overrides config. `secrets` lists names only.
- Commands return once their write lands; a change is live everywhere within 5 s (Q74).
- **`torpor serve --store <url>`** (the install mode, which Lambda runs):
  - Routes by the first label of `Host` (Q67), then reads that app's `current`. A name with nothing released is not kept, so a made-up name costs a GET and no memory (Q72).
  - Rereads an app's `current` once it is 5 s old, and reloads the app only if it changed (Q75, Q78). It gives up a recheck after 1 s, serving what it last read.
  - Loads each app once per release, compiling on tokio's blocking pool; a failed load stands until the next recheck (Q62, Q63).
- **Bucket contents are never trusted (Q53):**
  - reads are size-capped and parsed strictly;
  - every blob's and release's hash is checked on read.
- **Logs:** one request line per request in a span tagged with the app, so a log filter turns on one app (Q14, see decisions.md "During M3").

**Done when:**
- In-process tests publish and release two apps, serve them by subdomain, take one offline, roll one back, and list its releases, all on the in-memory store.
- A blob with the wrong hash is refused.
- Of two concurrent changes to one app's `current`, one lands and the other fails, so neither is lost (Q77).
- A hung recheck serves the last read within its timeout.
- Secrets round-trip, an empty value removes one, and `secrets` lists names only.

### M4: Native code (compile once, run everywhere)

This refines the trigger in Q58 **(choice)**; see [Choices to confirm](#choices-to-confirm).

**Open for M4:** since Q72 a blob lives under its app, so a marker must name the app as well as the hash, like `compile/<app>/<hex>`, where `<hex>` is the component's SHA-256, as in `apps/<app>/blobs/sha256/<hex>`.

- **`torpor compile-worker`** receives bucket events through the Lambda Web Adapter's pass-through path (`POST /events`). For each `compile/<hex>`:
  1. It fetches and hash-checks the blob.
  2. If `<hex>/<own compat hash>.zst` already exists, it skips to step 5.
  3. It compiles with Cranelift.
  4. It writes the zstd-compressed result to the native bucket.
  5. It deletes the marker.
- **Serving:**
  - **Loading:** native code comes only from the native bucket. It is decompressed into `/tmp` and loaded with `deserialize_file`, which maps the file.
  - **On a miss:** the host writes `compile/<hex>` once per environment, then compiles locally with Cranelift into `/tmp` (Q58).
- **Publish:** writes `compile/<hex>` and waits until it is deleted. A timeout points at the compile logs. The CLI never reads the native bucket.

**Done when:** tests over two in-memory stores (app and native) cover:
- the normal path;
- a miss, which compiles locally and writes a marker;
- native code planted in the app bucket, which is never loaded;
- a changed compat hash, which recompiles lazily;
- a failed compile, which leaves the marker so the next miss asks again.

### M5: AWS install and cloud validation (needs your explicit approval to apply)

- **`infra/aws`** (latest OpenTofu, Q20) contains:
  - the app and native buckets;
  - the **serving function:** arm64 on `provided.al2023` with the Lambda Web Adapter layer. 1769 MB by default, rejecting anything under 512 MB (Q51). A public Function URL with a reserved-concurrency cap and a budget alert (Q50);
  - the **compile function:** the same binary at 3008 MB with a 120 s timeout **(choice)**, fired by bucket events on `compile/`, and updated before the serving function;
  - **CloudFront in front of the Function URL**, with a wildcard certificate and wildcard DNS for `*.tric.works` in its Route 53 zone (Q71). It passes the viewer's `Host` on in a header, since the Function URL needs its own;
  - the IAM shape above, a log-retention variable (default 7 days) **(choice)**, and tags on everything.
- **Build:** the release build runs in Docker against AL2023's glibc and produces the zip.
- **One cloud session:** apply, run the checks below, and destroy the same day, with a $5 cap.
  - Cold start p99 at n = 100, at 1769 and 1024 MB, for Rust and JS.
  - `deserialize_file` against `deserialize` (the 46–58 ms lead).
  - The state read during start-up.
  - Compile times, and marker-to-native-code latency.
  - Publish-and-release time, end to end.
  - The storage suite against a real S3 bucket (Q12).

**Done when:**
- On a fresh account: apply, publish and release, and `https://<app>.tric.works` works.
- The targets are met, or each gap is written up with options.
- After destroy, nothing tagged is left.

### M6: First release

- **`torpor gc` (Q48):** keeps each app's current release and its last 10 by `releases`, with a 1 h grace period. It prunes unreachable blobs, releases and native code, and keeps only the newest compat hash per component.
- **Docs:**
  - a README quick start;
  - an app-author guide: the contract, consistency, limits, and composition with `wac plug`, including that composed parts share capabilities;
  - an operator guide: install, lazy recompiles on upgrade, adding teams and their roles, the trust model, and costs.
- **Release:**
  - Rename `spinit` to `torpor` across the docs, and run a trademark and domain check (Q43).
  - Create the GitHub repo, with Actions running the Docker tests (the real-bucket suite is run by hand).
  - Publish `torpor-cli`.

**Done when:** a new user can get from nothing to a released app using only the README.

## Risks being watched

| Risk | Where it's settled |
|---|---|
| Deserialize takes 46–58 ms on Lambda | M5 measures `deserialize_file` |
| Large JS components about 1 s cold | M5; dedicated functions are a deferred item |
| The p3 outbound send hook in `wasmtime-wasi-http` may differ from p2's | Checked first in M2 |
| The Lambda Web Adapter's event pass-through for the compile function | Checked first in M4, proven in M5 |

## Choices to confirm

These are small calls this plan makes. Say which, if any, to change.

1. **Merge `spike/latency` into `main`** at the start of M1, so `prototypes/latency/` and its results stay on record (Q13).
2. **One kind of compile marker** (refines Q58):
   - **What:** `compile/<hex>`. Publish writes it, and so does a serving miss (once per environment). The compile worker deletes it when the native code exists, and publish waits for that deletion.
   - **Why:** compared with Q58's create-only `compile/<compat hash>/<hex>` plus a component-upload trigger, this needs one trigger instead of two. The CLI never needs the compat hash or the native bucket, a failed compile retries on the next miss, and republishing an already-uploaded component still works.
   - **Cost:** at most one tiny PUT per cold environment until the native code lands.
3. **The admin gets delete-only on the native bucket** so `gc` can prune it. Deleting live native code only causes a recompile.
4. **KV store names:** any `[a-z0-9-]{1,63}`, scoped to the app, with no manifest field.
5. **CLI settings from the environment:** `TORPOR_STORE` (bucket URL). `TORPOR_RECIPIENT` went with age (Q73).
6. **Dev-mode secrets:** `TORPOR_VAR_<KEY>` environment variables. `torpor.toml` never holds secrets.
7. **Guest stdout/stderr:** captured into the host log, tagged with the app and capped per request.
8. **Defaults:** the compile function at 3008 MB with a 120 s timeout; log retention of 7 days.
9. **"CI" is local Docker until M6** creates the GitHub repo. The real-bucket suite runs in M5's cloud session and by hand after that.
