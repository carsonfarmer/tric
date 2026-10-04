# tric build plan

**Status:** draft for approval, 2026-10-01. This plan only puts the decisions in order. The decisions themselves live in [decisions.md](decisions.md), and that file wins if the two disagree. Items marked **(choice)** are small calls this plan makes that the grilling never settled; they're collected in [Choices to confirm](#choices-to-confirm).

## What v1 is

An operator installs tric into their own AWS account with one OpenTofu module, then its teams publish and release WASI components with `tric publish` and `tric release`.
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

- **Crate and binary (Q18, Q43):** one crate, `tric`, and one binary, `tric`.
- **Subcommands:** `serve`, `compile-worker`, `publish`, `release`, `releases`, `secret`, `secrets` and `gc`.
- **Lambda adapter:** Lambda runs `tric serve` and `tric compile-worker` behind the Lambda Web Adapter (Q29), so there is no Lambda crate.
- **Libraries:**
  - Wasmtime (the latest release when M1 starts; 49.0.1 in the spike), with `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config`;
  - `object_store` 0.14;
  - tokio and hyper;
  - `zstd`, serde, `toml`, `clap` and `tracing`.

**Line budget:** about 1,390 lines of Rust (1,200 before M2, 1,290 before M3 and its review raised `serve` and `state`; 1,325 through M5, after four trim passes and four correctness passes; 1,455 with M6's `gc`, which the trim pass after M6 is to bring back under) plus about 250 of HCL (384 written: see M5). A module that runs well past its budget is a design problem to raise, not to push through.

| Module | Does | Budget |
|---|---|---|
| `engine` | Wasmtime config, epoch ticker, limits, a fresh store per request with a WASI context that grants nothing, p2/p3 dispatch, loading native code, Cranelift fallback | ~210 (`engine` ~120 and `guest` ~90, merged in the trim pass; 225 after the trim passes, 256 after M4, which moved loading native code to `compile`) |
| `outbound` | Allow list plus the resolved-address block, in the HTTP send hook | ~100 (raised in M2 from ~60: the matcher is hand-written, and the connect is our own; 118 after the trim passes) |
| `kv` | `wasi:keyvalue` draft2 over the app's own prefix of the bucket: one object per key, CAS, no cache | ~250 (raised in M2 from ~200; 230 after the trim and review passes, 263 after M5 made batches concurrent and `increment` back off, 270 after the KV rethink) |
| `config` | `wasi:config` from the manifest, with secrets | 0 (Wasmtime's crate serves it, and `serve` adds secrets in 1 line) |
| `state` | The bucket layout, releases, compare-and-swap updates, defensive reads | ~130 (raised in the M3 review from ~100; 123 after the trim and review passes, 159 after the state rethink, 174 with `gc`'s strict reads of keys) |
| `serve` | hyper server, routing, logs | ~170 (raised in M3 from ~100, which was the router alone, then in the M3 review from ~140 for a pointer recheck per app; 144 after the trim and review passes, 156 after M5's routing by `X-Forwarded-Host` and load timings, 161 with M5's KV bucket) |
| `compile` | The compile worker and compile-request markers, and a host's load of native code with its fallback compile | ~150 (raised in M4 from ~70, as it took the host's side from `engine`; 147 after the review pass, 150 with M5's load timing) |
| `cli` | `publish`, `release`, `releases`, `secret`, `secrets`, `gc`, and `main`'s arguments | ~350 (146 after the trim and review passes, before `gc` and `compile`; 219 after M5, `cli` 96 and `main` 123, which builds the buckets on one HTTP client; 313 with `gc`, `cli` 175 and `main` 138) |
| `infra/aws` | OpenTofu module | ~250 HCL (384 after M5's trim (Q91), from 418 as first written. The budget predates CloudFront, its certificate and DNS (Q71, about 75 lines), the teams' roles (about 35) and the budget alert (about 15); the variables, with their docs and checks, are another 60) |

### Bucket layout

Q43 applies: no product name appears in any stored key or field, so a rename never needs a migration.

| Bucket | Key | What | Who writes |
|---|---|---|---|
| app | `apps/<app>/blobs/sha256/<hex>` | Components, in the OCI layout under the app (Q40, Q72) | team (put-if-absent) |
| app | `apps/<app>/releases/<hex>` | Immutable releases: component, config, allow list (Q69) | team (put-if-absent) |
| app | `apps/<app>/current` | The release the app serves, and its secrets, in plain (Q65, Q73) | team (CAS) |
| kv | `kv/<app>/<store>/<key>` | KV values, percent-encoded keys (Q42), in a bucket of their own (`TRIC_KV`, Q88); without one, in the app bucket | serving function |
| app | `compile/<app>/<hex>` | Compile requests | team, serving function; compile function deletes |
| native | `<app>/<hex>/<compat hash>.zst` | Compiled native code, under the app that owns the component (Q54) | **compile function only** |

**IAM shape:**
- The serving function reads the app bucket and may write only its `compile/`, reads and writes the KV bucket, and can only read the native bucket. It lists all three, so a miss is a 404 rather than a 403.
- Each team has a role tagged `team`, and one ABAC policy lets it read and write only `apps/${team}-*`, so a team owns the apps named `<team>-…`, plus read and write `compile/${team}-*`, which `publish` waits on. It lists the whole app bucket: without the list, S3 answers a missing key (a new app's `current`, a deleted marker) with a 403, and a list can't be held to the team's prefixes (Q64, Q72, Q88). Team names have no hyphens. Nobody but the serving function reaches the KV bucket.
- The operator runs `gc` with the credentials that installed tric. A role of its own, with delete but no put in the native bucket, waits for automated gc.
- The compile function reads `apps/*/blobs/`, deletes `compile/`, and reads, lists and writes the native bucket.

## Milestones

Each milestone is a branch in its own worktree under `.worktrees/`. It ends with:
- tests passing in Docker;
- a line count against the budget;
- your review.

It is then merged locally into `main`.

### M1: Runtime core (`tric serve` runs one app locally)

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
- **Dev mode:** `tric serve` in an app directory reads `tric.toml` (name, component, allowed outbound hosts, `[config]`) and runs that one app against the in-memory store. Secrets are never in `tric.toml`; in dev mode they come from `TRIC_VAR_<KEY>` environment variables **(choice)**. Since Q79, dev mode publishes and releases the directory into an in-memory install and serves that, at `<name>.localhost`.
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

- **`tric publish [DIR]`:** checks that the component loads, puts it and a release put-if-absent under the app's name, and prints `APP ID` (Q61, Q70, Q72).
- **`tric release APP ID`:** checks the release, then swaps it into the app's `current` with CAS, and fails if another change landed in between (Q77). Rolling back is releasing an older id, and `tric releases APP` lists them newest first (Q69).
- **`tric secret APP NAME` and `secrets APP`:** set a secret from stdin into the app's `current`, or remove it with an empty value, so it takes effect without a release and outlives releases (Q65, Q73, Q76). A secret overrides config. `secrets` lists names only.
- Commands return once their write lands; a change is live everywhere within 5 s (Q74).
- **`tric serve --store <url>`** (the install mode, which Lambda runs):
  - Routes by the first label of `Host` (Q67), then reads that app's `current`. A name is kept only once it has had a release, so a made-up name costs a GET and no memory (Q72, Q84).
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

This refines the trigger in Q58 **(choice)**; see [Choices to confirm](#choices-to-confirm). decisions.md "M4" has the choices made while building it.

Everything here is on only with `--native` (`TRIC_NATIVE`), the native bucket. A marker is `compile/<app>/<hex>`, where `<hex>` is the component's SHA-256, as in `apps/<app>/blobs/sha256/<hex>`, and native code is `<app>/<hex>/<compat>.zst`.

- **`tric compile-worker`** receives bucket events through the Lambda Web Adapter's pass-through path (`POST /events`). For each marker:
  1. If `<app>/<hex>/<own compat hash>.zst` already exists, it skips to step 4.
  2. It fetches and hash-checks the blob, and compiles it with Cranelift.
  3. It writes the zstd-compressed result to the native bucket.
  4. It deletes the marker.
- **Serving:**
  - **Loading:** native code comes only from the native bucket. It is decompressed into `/tmp` and loaded with `deserialize_file`, which maps the file.
  - **On a miss:** the host compiles locally with Cranelift, in memory, and once that succeeds writes the marker, giving up the write after 1 s (Q58). It loads each release once, so that is once per environment. Native code that is there but fails to load is compiled around the same way, with a warning, and stays until it is deleted.
- **Publish:** prints `APP ID`, then writes the marker and waits until it is deleted. A timeout points at the compile logs. The CLI never reads the native bucket.

**Done when:** tests over two in-memory stores (app and native) cover:
- the normal path;
- a miss, which compiles locally and writes a marker;
- native code planted in the app bucket, which is never loaded;
- a changed compat hash, which recompiles lazily;
- a failed compile, which leaves the marker so the next miss asks again.

All of these are covered, plus another app's native code and corrupt native code, neither ever loaded, and the worker skipping native code that is already there.

### M5: AWS install and cloud validation (needs your explicit approval to apply)

- **`infra/aws`** (latest OpenTofu, Q20) contains:
  - the app, native and KV buckets;
  - the **serving function:** arm64 on `provided.al2023` with the Lambda Web Adapter layer. 1769 MB by default, rejecting anything under 512 MB (Q51). A public Function URL with a reserved-concurrency cap and a budget alert (Q50);
  - the **compile function:** the same binary at 3008 MB with a 120 s timeout **(choice)**, and updated before the serving function. It is fired by `s3:ObjectCreated:*` events on the prefix `compile/`, with `AWS_LWA_ERROR_STATUS_CODES` covering 500 so a failed marker counts as a failed invocation and is retried. A small reserved concurrency queues a burst of markers, so the later ones find the native code and skip;
  - for the serving function, ephemeral storage that fits the native code of every app a host loads (512 MB by default), and the adapter's readiness check over TCP, as an HTTP one costs a bucket read;
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

**Applied (2026-10-03); destroy waits for the user's try of it (Q92).** The release build (`docker compose run --rm release` makes `dist/tric.zip`, 11 MB: aarch64, glibc 2.34 at most) runs as a fresh install at `*.tric.works`. The choices are in decisions.md, "M5 preparation" and "M5 session"; the results:
- **End to end:** as team `t1`, five apps are published and released (Rust p2 and p3, JS, KV, and a probe), each at `https://<app>.tric.works`. Every `publish` saw its marker deleted, so the adapter's event pass-through works.
- **The targets**, at 1769 MB. Cold starts are 100 sequential ones per app, each in a new environment, as Lambda's Init + Duration:

  | Target | Measured | |
  |---|---|---|
  | Cold p99 ≤ 500 ms, Rust (`hello-p3`) | 515 ms (p50 367, p90 422) | Missed by the slowest of 100 |
  | Cold p99 about 1 s, JS (`hello-js`) | 1,048 ms (p50 546, p90 965) | Met, as Q47 set it |
  | Warm read p50 ≤ 30 ms (a KV `get`, the whole request) | 27.5 ms | Met |
  | Acknowledged write p99 ≤ 200 ms (a KV `set`) | 61 ms | Met |
  | Idle ≤ $0.05/month | Storage only: nothing runs while idle | Met (the zone's $0.50 is outside the module) |

- **End to end**, from a client near Vancouver (CloudFront's YVR edge), to the first byte:
  - Warm: `hello-p3` 55 ms (31 straight to the Function URL), and a KV `get` 81 (57).
  - Cold, `hello-p3`: 610 ms at p50 (p90 777), against Lambda's own 383. Lambda's numbers leave out about 170 ms of making the environment and routing the Function URL, and CloudFront's hop to the origin adds about 50 more. That is 25 paired cold starts, each sent through CloudFront and straight to the Function URL at once.
- **At 1024 MB:** Rust 406 / 482 / 661 ms (p50 / p90 / p99), JS 637 / 1,324 / 1,725. Init is 155 ms at both sizes, and the rest scales with the CPU, so 1769 MB stays the default (Q51).
- **The Rust gap.** At p50, a cold start is about 75 ms before `main`, 54 ms for the HTTP client, 25–30 ms for the engine; then, in the request, 88 ms for the first `current` read (on a new TLS connection), and a 107 ms load, of which the native code takes 28 ms to fetch, 6 to decompress and 41 to deserialize. Options:
  1. **Done:** one HTTP client for every bucket. A cold start's p50 fell from 416 to 367 ms and its p99 from 536 to 515.
  2. **Measured, not done:** opening the buckets' connections during start-up moves the handshake rather than saving it (+11 ms at p50).
  3. **Deferred:** loading hot apps during start-up (Q56 (c)), which, as the first request waits for Init to end, would mostly save a warm environment's first request to another hot app (decisions.md, "Deferred"); and a profile of deserialize on Lambda, 41 ms here against the spike's 5–11 locally.
- **The JS tail is the environment.** About one environment in seven starts slowly (Init about 185 ms rather than 155), and in those JS's deserialize took 388–534 ms rather than about 50. In memory (`deserialize`) it was no faster, so `deserialize_file` stays. At p50, JS's native code takes 111 ms to fetch and 94 to decompress, one after the other; overlapping the two is a deferred option, as are dedicated functions.
- **The compile function** (3008 MB): `hello-p3` compiles in 385 ms, `hello-p2` in 284, and JS in 9.4 s using 411 MB, well inside its 120 s. `publish` took 2.8 s end to end with the compile function cold.
- **The storage suite** passes against S3 (decisions.md, "M5 session").

**The cloud session, in order** (each step needs the AWS profile; `apply` needs your approval):
1. **Checks before applying:** `aws lambda get-account-settings` shows unreserved concurrency of at least 122 (20 + 2 reserved, plus the 100 Lambda keeps), or apply with `-var 'concurrency={serve=-1,compile=-1}'`. The `tric.works` zone is public in this account, and no other distribution holds `*.tric.works`. Layer `LambdaAdapterLayerArm64:30` exists in us-west-2. `aws sso login` on the host, as the `tofu` service mounts `~/.aws`.
2. **Apply** with `-var domain=tric.works -var 'budget={emails=[…]}' -var 'teams=["t1"]' -var 'logs={filter="info"}' -var force_destroy=true`, so destroy can empty the buckets and the load times are logged. CloudFront takes about 5 minutes.
3. **The team role's 404s first**, as role `t1`: a HEAD of a missing `apps/t1-x/current` and of a missing `compile/t1-x/<hex>` must be 404, and a list with no prefix (and one of `apps/t2-`) must be refused. If either fails, KV moves to a bucket of its own (Q88).
4. **End to end:** as `t1`, publish and release a fixture as `t1-hello`; `https://t1-hello.tric.works` serves it; `publish` saw the marker deleted (the compile function ran); a request straight to the Function URL with a made-up `X-Forwarded-Host` reaches only that app; a viewer's own `X-Forwarded-Host` is replaced.
5. **The measurements above**, from the info logs with Logs Insights: `loaded` (`ms`, a whole load) and `loaded its native code` (`ms`, the deserialize), plus Lambda's `Init Duration`. Then `memory=1024` in `serve`, and again.
6. **Destroy once the user has tried the live install** (Q92), as `infra/aws/README.md`'s "Spin down" says, then list what is tagged `tric` (`aws resourcegroupstaggingapi get-resources`) in both regions; Lambda's own log groups, if a function logged after its group was deleted, are the likely leftover.

### M6: First release

- **Done: `tric gc` (Q48)** keeps each app's current release and its last 10 by `releases`, with a 1 h grace period. It prunes unreachable blobs, releases, markers and native code, and keeps only the newest compat hash per component.
- **Done: docs:**
  - a README quick start;
  - an app-author guide: the contract, consistency, limits, and composition with `wac plug`, including that composed parts share capabilities;
  - an operator guide: install, lazy recompiles on upgrade, adding teams and their roles, the trust model, and costs.
- **Done: release:**
  - **Done:** the project is renamed to `tric` (Q98), crate and binary both, across the code and docs, after a trademark check (Q43).
  - **Done:** the GitHub repo, [carsonfarmer/tric](https://github.com/carsonfarmer/tric), public (Q99), with Actions running the Docker gate (the real-bucket suite is run by hand).
  - **Done:** a workflow that publishes the CLI to crates.io from a tag, ready but not turned on: the CLI is not published yet.

**Done when:** a new user can get from nothing to a released app using only the README.

**Then:** a whole-codebase trim pass, as aggressive as the earlier ones, before anything new.

## Risks being watched

| Risk | Where it's settled |
|---|---|
| Deserialize takes 46–58 ms on Lambda | Settled in M5: 41 ms at p50 for `hello-p3`, no faster in memory, so `deserialize_file` stays. The time is the first touch of each page; a second deserialize takes about 1 ms |
| Large JS components about 1 s cold | M5 measured 1,048 ms p99 at 1769 MB and 1,725 at 1024; its tail is the slower environments. Dedicated functions are a deferred item |
| The p3 outbound send hook in `wasmtime-wasi-http` may differ from p2's | Checked first in M2 |
| The Lambda Web Adapter's event pass-through for the compile function | Built to its documented contract in M4 (the raw event, posted to `/events`); proven in M5, where every `publish` saw its marker deleted |
| The compile function compiles untrusted components while it can write native code that every host runs | Wasmtime's compiler is the boundary: a component that exploits Cranelift there could write native code for any app. Per-team compile functions would contain it (deferred) |

## Choices to confirm

These are small calls this plan makes. Say which, if any, to change.

1. **Merge `spike/latency` into `main`** at the start of M1, so `prototypes/latency/` and its results stay on record (Q13).
2. **One kind of compile marker** (refines Q58):
   - **What:** `compile/<app>/<hex>`. Publish writes it, and so does a serving miss (once per environment). The compile worker deletes it when the native code exists, and publish waits for that deletion.
   - **Why:** compared with Q58's create-only `compile/<compat hash>/<hex>` plus a component-upload trigger, this needs one trigger instead of two. The CLI never needs the compat hash or the native bucket, a failed compile retries on the next miss, and republishing an already-uploaded component still works.
   - **Cost:** at most one tiny PUT per cold environment until the native code lands.
3. ~~**The admin gets delete-only on the native bucket**~~ **The operator runs `gc` with their own credentials** (M6) until gc is automated. Deleting live native code only causes a recompile.
4. **KV store names:** any `[a-z0-9-]{1,63}`, scoped to the app, with no manifest field.
5. **CLI settings from the environment:** `TRIC_STORE` (bucket URL). `TRIC_RECIPIENT` went with age (Q73).
6. **Dev-mode secrets:** `TRIC_VAR_<KEY>` environment variables. `tric.toml` never holds secrets.
7. **Guest stdout/stderr:** captured into the host log, tagged with the app and capped per request.
8. **Defaults:** the compile function at 3008 MB with a 120 s timeout; log retention of 7 days.
9. **"CI" is local Docker until M6** creates the GitHub repo. The real-bucket suite runs in M5's cloud session and by hand after that.
