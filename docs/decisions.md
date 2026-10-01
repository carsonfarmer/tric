# Scoping decisions

Running log from the grilling session. Background research is in [research/](research/), and the original brainstorm is in [initial-thoughts.md](initial-thoughts.md).

## Round 1: requirements (settled 2026-09-30)

| # | Topic | Decision |
|---|---|---|
| Q1 | Audience and trust | Open-source tool that one operator deploys into their own cloud account to run their own, trusted code. Built first as a personal platform. A hosted multi-tenant service is out of scope for v1. Fuel limits are a cost guardrail, not a security boundary. |
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
| Q32 | Config and secrets in releases | **Open.** The user is torn. Either way, apps read values through `wasi:config`. |
| Q33 | KV freshness | Each instance caches values with their ETags, write-through on its own writes (read-your-writes). Cached values are served for up to **1 s**, then revalidated with a conditional GET. CAS always goes to the bucket with `If-Match`. Fixed in v1, with no per-app setting. **The consistency model must be clearly documented** for app authors. |
| Q34 | Outbound network | **Per-app allow list in the manifest** (Spin-style). Loopback, link-local and private addresses are **always** blocked, even if listed. **Open:** the default when the list is absent, and the pattern syntax. |
| Q35 | Resource limits | Epoch-based per-request timeout plus a memory cap; no fuel. Defaults: 10 s and 256 MiB. **No per-app overrides**: limits are install-level settings that apply to every app equally. |
| Q36 | Spike | Start now, **local first**: build and time everything possible in Docker (host, components, MinIO, Lambda emulation), then run one short cloud session in us-west-2 for the numbers only Lambda can give. Ask before `tofu apply`. OpenTofu runs from its Docker image with short-lived exported credentials. |
