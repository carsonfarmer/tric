# Scoping decisions

Running log from the grilling session. Background research is in [research/](research/), and the original brainstorm is in [initial-thoughts.md](initial-thoughts.md).

## Round 1: requirements (settled 2026-09-30)

| # | Topic | Decision |
|---|---|---|
| Q1 | Audience and trust | Open-source tool that one operator deploys into their own cloud account to run their own, trusted code. Built first as a personal platform. A hosted multi-tenant service is out of scope for v1. Fuel limits are a cost guardrail, not a security boundary. **Trust model replaced by Q53.** |
| Q2 | App compatibility | Support the common Spin subset: `spin.toml`, the HTTP trigger, KV, variables, and outbound HTTP. Full Spin parity is out. The user is open to a WASI-first contract built on WASIp2/p3 (option c). Revisited in Q8. |
| Q3 | Workloads | HTTP request/response only, with cron as a fast follow. Requests take under 10 s and are read-heavy (10:1 or more). v1 ceiling is about 100 reads/s and 10 writes/s per app. No websockets or SSE. |
| Q4 | Cost | Idle costs $0.05/month or less, storage only. Target is about $1/month at 1M requests plus 100k writes. Up to about $1.40 is acceptable if the design needs a second PUT per write. Costs may grow linearly past that, with no step-function costs. |
| Q5 | Latency | Cold start p99 of 500 ms or less. Warm reads p50 of 30 ms or less (stretch; the user pushed this down from 50 ms). Acknowledged writes p99 of 200 ms or less. All three are unmeasured, so they must be validated early. |
| Q6 | Durability | An acknowledged write must survive the loss of an availability zone (S3 Standard, GCS regional, Azure ZRS). Single region. Cloud-agnostic is an explicit goal. |
| Q7 | Clouds | AWS, Azure, or GCP. Owning the Wasmtime host is the main point of the project. The storage interface stays narrow (get, put-if-absent, put-if-match, list). |

**Clarification from the user:** platform (system) state, and probably runtime state, must live in buckets. App state *may* live in buckets if that's feasible, but it isn't required to.

## Guiding principle (added in round 2)

**Write as little custom code as possible.** Lean on existing libraries. Prefer elegant designs that reduce total lines of code, and keep implementations clean and idiomatic. When options are otherwise close, the one with less code wins.

## Round 2: architecture (2026-09-30)

| # | Topic | Decision |
|---|---|---|
| Q4 | Cost (revised) | Up to $2/month at the reference load is acceptable. |
| Q8 | App contract | Leaning toward (b): apps use the standard WASI interfaces (HTTP, key-value, config), plus small adapters so unmodified Spin apps can run. The user would accept (c), WASI only with no Spin adapters, if that cuts much code. **Open:** waiting on research into how big the Spin adapter is. |
| Q9 | App state | **Open.** The user asks whether DynamoDB-style stores would be simpler, whether their existing WAL (`object-log`) fits, and whether something off the shelf is simple, cheap, and fast enough for v1. Research is in progress. |
| Q10 | Deploy propagation | Running instances re-check the live version at most every 5 s, so a deploy is visible everywhere within 5 s. `deploy` waits that long before reporting success. |
| Q11 | Function topology | One shared function serves every app. The host keeps each app inside its own bucket prefix and enforces per-app fuel and memory limits. CloudFront handles custom domains; without it, apps are routed by path prefix. A dedicated-function mode can come later. |
| Q12 | Portability | v1 fully supports one cloud. Storage goes through a library that already covers S3, GCS, and Azure. The host also runs as a plain HTTP server. CI tests storage against all three clouds. Never write the same object more than once a second in normal operation. |
| Q13 | Measure first | Milestone 0 is a latency prototype that also decides which cloud goes first. It lives in an isolated git worktree under `prototypes/`, a folder kept for future experiments. |
| Q14 | Observability | Use each cloud's native log sink, defaulting to warnings and errors, with per-app sampling. OTLP export is optional. Logs never go in the bucket. Request record/replay waits until after v1. |
| Q15 | Secrets | For now, the CLI encrypts secrets with age and stores them in the bucket; the decryption key lives in the function's environment. Variables are stored in the bucket as plaintext. **Open:** whether a cloud-agnostic secrets library exists, and what each option really costs. Research is in progress. |

## Round 3: build, spike and state (2026-10-01)

| # | Topic | Decision |
|---|---|---|
| Q16 | object-log | Judged on merit only. If it proves useful, borrow ideas and small snippets from it. Never depend on the library or copy its code wholesale. |
| Q17 | Repo and worktree | Run `git init` here, commit `docs/` and `.claude/skills/` on `main`, then `git worktree add` a `spike/latency` branch with its code under `prototypes/latency/`. The spike runs in this session. AWS profile: `AdministratorAccess-076950137847`. |
| Q18 | Code shape | One Rust crate and one `spinit` binary, with `serve`, `deploy`, `rollback` and `secrets` subcommands sharing the same manifest and layout types. Local dev and integration tests use MinIO in Docker; unit tests use the in-memory store. Split into more crates only when forced. |
| Q19 | Deploy path | No control-plane API. `deploy` writes straight to the bucket, and cloud IAM is the only auth. Components are content-addressed and put-if-absent, manifests are immutable, and one state object is swapped with a conditional write. **Open:** the user wants to deploy many apps, so how that state object scales needs checking (Q26). |
| Q20 | Install | One OpenTofu module per cloud. Targets the latest OpenTofu only, with no Terraform compatibility. The binary never touches infrastructure. |
| Q21 | Spike scope | Accepted as a spike only. The user dislikes extra response headers, so timing headers such as `Server-Timing` don't carry into the product; prefer logs. Cloud Run and GCS are dropped (Q22). |
| Q22 | Spike access and budget | AWS only; GCP is on hold. $5 cap. Every resource is tagged `spinit-spike`. Ask before every `tofu apply`, and destroy the same day measurements finish. Nothing public: the Function URL uses IAM auth, and logs are kept for one day. |
| Q23 | Cron | One fixed once-a-minute ticker invokes the host, which runs due jobs as synthetic HTTP requests. **Revisit later**; acceptable for now because Spin is only a loose target. |
| Q24 | License | Apache-2.0. |
| Q25 | App KV store | Build the bucket store (one object per key) first. If the spike hits read p50 ≤ 30 ms (with revalidation) and write p99 ≤ 200 ms, ship bucket only; otherwise add DynamoDB as the AWS store. No shared store interface until a second store exists. |
| Q26 | System state store | One small CAS state object plus immutable, parent-linked manifests. It must scale to many apps; whether object-log is used doesn't matter to the user. **Open:** scaling research (see Q19). |
| Q27 | Secrets | One age-encrypted blob per app in the bucket, decrypted once per cold start. The age identity lives in a function environment variable, and the CLI encrypts with the public key only. |
| Q28 | App contract | **Reopened.** The user points out that language componentizers already compile to WASI components, and leans toward WASI only, with Spin as inspiration. Research into what each toolchain emits is in progress. |
| Q29 | Lambda adapter | AWS Lambda Web Adapter. Measure its overhead in the spike; switch to `lambda_http` only if it adds more than ~5 ms warm p50 or ~50 ms cold start. |
| Q30 | Wasmtime version | **Changed:** target the latest official Wasmtime release instead of following Spin's pin. |

**New side questions from the user (research in progress):**
- **OCI registry:** can components be stored in, or served from, an OCI-compliant registry such as ECR instead of (or as well as) the bucket?
- **WASI only:** support only the standard WASI interfaces, and treat Spin as inspiration rather than a compatibility target.

## Round 4: app contract and runtime policy (2026-10-01)

| # | Topic | Decision |
|---|---|---|
| Q31 | App contract (replaces Q8/Q28) | **WASI only.** An app is a component exporting `wasi:http` (p2 `incoming-handler` or p3 `handler`; both are served). It may import outbound `wasi:http`, `wasi:keyvalue`, `wasi:config` and the standard WASI basics, with no filesystem or socket access. spinit has its own minimal manifest and doesn't read `spin.toml`. Spin is inspiration only; an adapter could return later behind a cargo feature if there's demand. Interface versions are decided after the componentizer research. |
| Q32 | Config and secrets in releases | Settled in round 5. |
| Q33 | KV freshness | Each instance caches values with their ETags, write-through on its own writes (read-your-writes). Cached values are served for up to **1 s**, then revalidated with a conditional GET. CAS always goes to the bucket with `If-Match`. Fixed in v1, with no per-app setting. **The consistency model must be clearly documented** for app authors. |
| Q34 | Outbound network | **Per-app allow list in the manifest** (Spin-style). Loopback, link-local and private addresses are **always** blocked, even if listed. Default and syntax settled in round 5 (Q37, Q38). |
| Q35 | Resource limits | Epoch-based per-request timeout plus a memory cap; no fuel. Defaults: 10 s and 256 MiB. **No per-app overrides**: limits are install-level settings that apply to every app equally. |
| Q36 | Spike | Start now, **local first**: build and time everything possible in Docker (host, components, MinIO, Lambda emulation), then run one short cloud session in us-west-2 for the numbers only Lambda can give. Ask before `tofu apply`. OpenTofu runs from its Docker image with short-lived exported credentials. |

## Round 5: policy details, state, trust and native code (2026-10-01)

| # | Topic | Decision |
|---|---|---|
| Q32 | Config and secrets (replaces Q27) | **(a), SOPS-style.** Config and secrets live in the immutable release manifest. Each secret is age-encrypted separately, in place, with the install's public key, so `spinit secrets list` shows names without decrypting. Apps read both through `wasi:config` as one flat key set; a key in both tables is rejected at deploy. `spinit secrets set NAME` encrypts one value and creates a new release that copies everything else. No extra reads at cold start. Rollback restores code, config and secrets together; the documented catch is that it can bring back a rotated secret. The age identity stays in the function environment. |
| Q34 | Outbound network (follow-up) | The "always block" rule applies to **resolved** addresses. The host resolves names itself and checks every address before connecting, including IPv6 equivalents and redirect targets. This is a small filtering resolver in the outbound HTTP client (about 20 lines). |
| Q37 | Outbound default | **Deny all** when an app lists no hosts (Spin's behavior). `*://*:*` allows everything explicitly. |
| Q38 | Allow-list syntax | **A Spin subset:** `scheme://host[:port]`. The host may be `*` or start with `*.`, the port may be `*`, and the port defaults from the scheme. No port ranges, `{{ }}` templates or service chaining. About 20 lines on the `url` crate. |
| Q39 | Components per app | **Exactly one component per app.** It does its own routing: no router in the host and no routes in the manifest. Multi-part apps compose at build time (e.g. `wac`) or deploy as separate apps. **Follow-up:** the user asked what the ecosystem's emerging best practice is. Research is in progress ([research/composition.md](research/composition.md)). |
| Q40 | OCI registries | **Adopted; keep the whole plan on record** (also in [research/oci-registries.md](research/oci-registries.md)). (1) The bucket is the only thing the host reads. Components live at `blobs/sha256/<hex>`, the OCI image-layout path, so copying from a registry is a pure byte copy. (2) Until the fast follow, there is no code: `oras cp --to-oci-layout <ref>` then `aws s3 sync`. (3) Fast follow: `spinit deploy --from oci://<ref>`, about 30 lines with `oci-client`, behind a cargo feature so the Lambda binary doesn't carry it. (4) Later, only if wanted: serve the bucket as a read-only `/v2/` registry, or `spinit export` an OCI manifest generated on the fly. (5) Avoid: registry pulls at run time (2–4 sequential requests plus per-cloud token code), and a registry as the primary store. (6) spinit keeps its own release-manifest format; an OCI manifest would cost an extra cold-start read for its config blob. ECR, GAR, GHCR and Docker Hub all store components; ACR is out because of its ~$5/month fixed fee. |
| Q41 | Path-prefix routing | **(a)** The prefix is stripped, so the app sees `/users/1`. The host adds `X-Forwarded-Prefix: /myapp` to the guest's request only, never to responses. |
| Q42 | KV limits and listing | Keys of 256 bytes or less, stored percent-encoded, one object each. Values of 1 MiB or less, rejected on `set`. `list-keys` passes straight through to the bucket LIST, one page of up to 1,000 keys per call, and the cost ($0.005 per 1,000 calls) is documented loudly. **Accepted for now only:** the user wants a way out of the LIST cost as soon as possible (next round). |
| Q43 | Name | "spinit" is a **working name only**. Keep it out of everything stored (bucket keys, manifest fields, media types), so a rename is a find-and-replace and never a migration. The user dislikes the name; candidates are being vetted ([research/names.md](research/names.md)). |
| Q44 | KV and config interface versions | **Open.** The user leans toward (c), supporting both `wasi:keyvalue@0.2.0-draft` (Wasmtime's) and `0.2.0-draft2` (upstream, Spin; has CAS and a string cursor), and wants to lean on Wasmtime more than Spin. Waiting on research into `wasmtime-wasi-keyvalue`: is it pluggable, and is it moving to a newer draft? Config is settled: `wasi:config@0.2.0-rc.1`, reusing Wasmtime's implementation. |
| Q45 | State object (closes Q19/Q26) | **Accepted.** One small global CAS object `{v, apps: name → manifest hash, domains: host → app, cron}`; routes, config and limits stay in each immutable manifest. About 78 bytes per app (78 KB at 1k apps, 781 KB at 10k). A request finding cached state 5 s old or older first makes one blocking conditional GET. The CLI retries the swap with randomized backoff; a multi-app deploy is one atomic swap; retries are idempotent; writes are paced to 1/s on GCS. About $1.10–1.25/month at the reference load. Revisit (sharding or a change log) at about 10k apps or one deploy per second. |
| Q46 | Precompile at deploy | **Superseded by Q54.** Precompiling is still needed for large components, but the user rejected the trust note: bucket write access must never mean control of the host. |
| Q47 | Cold-start target by size | **(b)** 500 ms p99 for components of about 5 MB or less (Rust, C, Go, TinyGo, QuickJS-based JS). StarlingMonkey JS and Python get a documented target of about 1 s p99 until measured. A dedicated-function mode can follow if needed. |
| Q48 | Retention and gc | A manual `spinit gc` (about 100 lines) keeps everything reachable from the state object through each app's last **10** releases (also the rollback depth) and deletes the rest, skipping anything younger than 1 hour. Bucket lifecycle rules can't be used because they can't see references. **Expected to be automated later.** |
| Q49 | Cron vs the idle cap | **(a)** Cron is opt-in at install (an OpenTofu variable, off by default). The idle cap applies to installs without cron; with it, about $0.05–0.10/month more. Low priority for now. |
| Q50 | Public access in v1 | **(a)** A public Function URL with path-prefix routing (Q41). The module sets a reserved-concurrency cap and a billing alert. Fast follow: custom domains through CloudFront in front of the public URL, with a secret origin header the host checks (about 5 lines). The router reads `domains` from state from day one. CloudFront origin access control is out: it needs an IAM-auth Function URL and a body SHA-256 header that browsers don't send. |
| Q51 | Default function memory | **512 MB, provisional,** as an install-level OpenTofu variable (consistent with Q35). The cloud run's 512 MB numbers confirm or change it. |
| Q52 | Cloud session | **Approved:** one apply and one destroy on 2026-10-01, $5 cap. Applied 22:32 UTC with 14 resources, all tagged `project=spinit-spike`. |
| Q53 | Trust model (replaces the trust part of Q1) | **(a) Untrusted code, trusted operator.** Components may be third-party, AI-written or pulled from a registry, so Wasmtime's sandbox is a security boundary. Someone who steals bucket-write access can deploy sandboxed Wasm and read or change app data, but can't run native code or read secrets. Q19 (deploy writes straight to the bucket) stands. **The host never trusts bucket contents**, which keeps (b), semi-trusted deployers confined to their own apps behind a deploy endpoint, possible later. (c), hostile tenants on a public service, stays out of scope. |
| Q54 | Who produces native code | **Open; the user prefers (c)** if it can be pulled off: no stored native code, compile with Winch at each cold start, cache only in the instance's `/tmp`. (a) is the fallback: a compile function writes zstd artifacts with an HMAC that the host verifies before decompressing. The user asked what Spin does. Decided by the cloud `compile-winch` numbers, the Winch runaway-loop test, Winch's support tier on aarch64, and research ([research/native-code-and-kv.md](research/native-code-and-kv.md)). |
| Q55 | Untrusted-components rules | **Accepted, with a clarification.** (1) Limits are security controls: epoch timeout, memory cap (also covering instance and table counts), outbound calls inside the 10 s deadline. (2) The resolved-IP block (Q34) protects Lambda's runtime API and the host on loopback. (3) Guests inherit nothing from the host process: no environment, arguments, files or sockets. (4) A Wasmtime **store** (the in-memory sandbox for one app's code) never serves two apps. Spectre mitigations and guard pages stay at their defaults. (5) All apps share one function process for density, so a Wasmtime escape would reach every app on the install, the same posture as Fastly Compute. **Clarification:** "instance" means a Wasmtime store or component instance, not a machine or a Lambda environment. Many apps share one process. |

**Superseded entries:** Q1's "trusted code" and "fuel is a cost guardrail, not a security boundary" are replaced by Q53 and Q55. Q27 is replaced by Q32. Q46 is replaced by Q54.

## Deferred work and fast follows

Kept here so nothing agreed in the grilling gets lost.

| Item | From | Notes |
|---|---|---|
| `spinit deploy --from oci://<ref>` | Q40 | About 30 lines with `oci-client`, behind a cargo feature. Until then: `oras cp --to-oci-layout` plus `aws s3 sync`. |
| Read-only `/v2/` registry view of the bucket; `spinit export` | Q40 | Only if wanted. Generate OCI manifests on the fly. |
| Custom domains through CloudFront | Q11, Q50 | Secret origin header checked by the host; certificate in us-east-1. |
| Dedicated-function mode | Q11, Q47, Q55 | For apps needing hard isolation, more memory, or the strict cold-start target for large components. |
| A way out of the `list-keys` LIST cost | Q42 | Next round. |
| Automated gc | Q48 | Today: manual `spinit gc`. |
| Cron design revisit | Q23, Q49 | Once-a-minute ticker, opt-in. |
| Rename | Q43 | Before the first public release. |
| Spin adapter behind a cargo feature | Q31 | Only if there's demand. |
| Composition at deploy time | Q39 | Pending the composition research. |
| Semi-trusted deployers | Q53 (b) | A deploy endpoint in the function as the only trusted writer, plus auth. About 200 lines. |
| State sharding or change log | Q45 | At about 10k apps or one deploy per second. |
