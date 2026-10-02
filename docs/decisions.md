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

## No backward compatibility (added 2026-10-02)

**Until the first public release, never keep backward compatibility.** There are no users and no outside consumers, so any breaking change is fine and expected. That means no migrations, no compatibility shims, no format or protocol versions, and no hedges written for a later change. Docs describe what the code does today. This dropped a version field from the Q45 state object and the "at most 1 s old" promise from the KV docs.

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
| Q45 | State object (closes Q19/Q26) | **Accepted.** One small global CAS object `{apps: name → manifest hash, domains: host → app, cron}`; routes, config and limits stay in each immutable manifest. About 78 bytes per app (78 KB at 1k apps, 781 KB at 10k). A request finding cached state 5 s old or older first makes one blocking conditional GET. The CLI retries the swap with randomized backoff; a multi-app deploy is one atomic swap; retries are idempotent; writes are paced to 1/s on GCS. About $1.10–1.25/month at the reference load. Revisit (sharding or a change log) at about 10k apps or one deploy per second. |
| Q46 | Precompile at deploy | **Superseded by Q54.** Precompiling is still needed for large components, but the user rejected the trust note: bucket write access must never mean control of the host. |
| Q47 | Cold-start target by size | **(b)** 500 ms p99 for components of about 5 MB or less (Rust, C, Go, TinyGo, QuickJS-based JS). StarlingMonkey JS and Python get a documented target of about 1 s p99 until measured. A dedicated-function mode can follow if needed. |
| Q48 | Retention and gc | A manual `spinit gc` (about 100 lines) keeps everything reachable from the state object through each app's last **10** releases (also the rollback depth) and deletes the rest, skipping anything younger than 1 hour. Bucket lifecycle rules can't be used because they can't see references. **Expected to be automated later.** |
| Q49 | Cron vs the idle cap | **(a)** Cron is opt-in at install (an OpenTofu variable, off by default). The idle cap applies to installs without cron; with it, about $0.05–0.10/month more. Low priority for now. |
| Q50 | Public access in v1 | **(a)** A public Function URL with path-prefix routing (Q41). The module sets a reserved-concurrency cap and a billing alert. Fast follow: custom domains through CloudFront in front of the public URL, with a secret origin header the host checks (about 5 lines). The router reads `domains` from state from day one. CloudFront origin access control is out: it needs an IAM-auth Function URL and a body SHA-256 header that browsers don't send. |
| Q51 | Default function memory | **512 MB, provisional,** as an install-level OpenTofu variable (consistent with Q35). The cloud run's 512 MB numbers confirm or change it. |
| Q52 | Cloud session | **Approved:** one apply and one destroy on 2026-10-01, $5 cap. Applied 22:32 UTC with 14 resources, all tagged `project=spinit-spike`; destroyed 23:52 UTC, and a check afterwards found nothing tagged `spinit-spike` left in the account. Results: [prototypes/latency/RESULTS.md](../prototypes/latency/RESULTS.md) on branch `spike/latency`. |
| Q53 | Trust model (replaces the trust part of Q1) | **(a) Untrusted code, trusted operator.** Components may be third-party, AI-written or pulled from a registry, so Wasmtime's sandbox is a security boundary. Someone who steals bucket-write access can deploy sandboxed Wasm and read or change app data, but can't run native code or read secrets. Q19 (deploy writes straight to the bucket) stands. **The host never trusts bucket contents**, which keeps (b), semi-trusted deployers confined to their own apps behind a deploy endpoint, possible later. (c), hostile tenants on a public service, stays out of scope. |
| Q54 | Who produces native code | **Open; the user prefers (c)** if it can be pulled off: no stored native code, compile with Winch at each cold start, cache only in the instance's `/tmp`. (a) is the fallback: a compile function writes zstd artifacts with an HMAC that the host verifies before decompressing. The user asked what Spin does. Decided by the cloud `compile-winch` numbers, the Winch runaway-loop test, Winch's support tier on aarch64, and research ([research/native-code-and-kv.md](research/native-code-and-kv.md)). |
| Q55 | Untrusted-components rules | **Accepted, with a clarification.** (1) Limits are security controls: epoch timeout, memory cap (also covering instance and table counts), outbound calls inside the 10 s deadline. (2) The resolved-IP block (Q34) protects Lambda's runtime API and the host on loopback. (3) Guests inherit nothing from the host process: no environment, arguments, files or sockets. (4) A Wasmtime **store** (the in-memory sandbox for one app's code) never serves two apps. Spectre mitigations and guard pages stay at their defaults. (5) All apps share one function process for density, so a Wasmtime escape would reach every app on the install, the same posture as Fastly Compute. **Clarification:** "instance" means a Wasmtime store or component instance, not a machine or a Lambda environment. Many apps share one process. |

**Superseded entries:** Q1's "trusted code" and "fuel is a cost guardrail, not a security boundary" are replaced by Q53 and Q55. Q27 is replaced by Q32. Q46 is replaced by Q54. Round 6 replaces round 5's Q42 listing, Q44, Q51 and Q54, and Q40 (1)'s single bucket.

## Round 6: follow-ups after the cloud spike (2026-10-01)

| # | Decision | Answer |
|---|---|---|
| Q39 | Components per app (follow-up) | **Unchanged: one component per app, no composition code in v1.** The ecosystem's practice is to compose into one component and then deploy that: `spin registry push` composes by default, `wasm-tools compose` is deprecated in favour of `wac`, and WASI 0.3 adds a `middleware` world. The docs show `wac plug`. Parts composed into one app share its allow-list, KV and config, because their imports are merged; the docs say so. Fast follow: `deploy --plug` in the CLI only. |
| Q42 | A way out of the LIST cost (replaces Q42's listing) | **(ii) A generation object plus a cached LIST.** Every `set` and `delete` writes the data, then overwrites a tiny per-bucket `gen` object (unconditional, so writers never conflict). Each instance caches LIST pages with the generation's ETag. A `list-keys` within 1 s of the last check is free; after that it makes one conditional GET of `gen` (12.5 times cheaper than a LIST) and re-LISTs only if it changed. About 30 lines. Writes cost 2×, inside the round 2 budget. A list can lag a write by up to 1 s (as Q33), and a crash between the two writes leaves the list stale until the next write. **No DynamoDB KV backend for now.** |
| Q43 | Name | **torpor.** Binary `torpor`, crate `torpor-cli` (the `torpor` crate is an unrelated 23-download crate). No trademark search has been done yet. |
| Q44 | KV and config interface versions | **(b) `wasi:keyvalue@0.2.0-draft2` only.** `wasmtime-wasi-keyvalue` 49.0.1 is draft-only, in-memory, has private bindings and no backend trait, and upstream declined backends in-tree, so we need our own `bindgen!` either way. draft2's `cas` resource maps to `If-Match` and its string cursor is the S3 continuation token; it matches Spin 4.2.1, wRPC and upstream `main`. Add the original draft only if a real guest needs it. Config stays `wasi:config@0.2.0-rc.1` through Wasmtime's crate. |
| Q54 | Who produces native code (replaces round 5's Q54) | **(b) A separate native-code bucket.** A compile function (the same binary, deployed as a second function at higher memory) compiles each component with Cranelift and writes zstd native code to a second bucket that only its role can write. The serving function only reads it; deployers can't touch it. No crypto code: trust comes from IAM, matching Wasmtime's compile-then-run split and runwasi's fix for CVE-2026-47218. (c), Winch at each cold start, is ruled out: it misses 500 ms p99 at every memory size, and Winch is Tier 2 on aarch64. Spin compiles locally with Cranelift and trusts its local cache, which only works on long-lived machines. Changes Q40 (1): the host reads two buckets. |
| Q51 | Default function memory (replaces round 5's Q51) | **1769 MB** (one full vCPU), an install-level OpenTofu variable that rejects anything below 512 MB so the 256 MiB store cap always fits. Rust meets 500 ms p99 at 1769 (383 ms) but not at 512 (619 ms). It costs about 3.5× more per warm request; 1M requests with one KV read is about $0.69 against $0.20, and the free tier covers about 7.7M such requests a month. |
| Q56 | Work during Lambda's start-up phase | **(b)** Read the state object during start-up, which also opens the TLS connection to the bucket while start-up's extra CPU is available. About 3 lines. Loading apps before the first request stays out unless (b) falls short. |
| Q57 | Runaway guests holding a worker | **Keep the 10 s epoch trap for now.** It doesn't matter on Lambda, where each environment handles one request at a time. When Cloud Run (or a concurrent `serve`) becomes a target, switch to yielding on each epoch tick plus a timeout around the request. |
| Q58 | How code gets compiled | (1) A component upload to the app bucket fires the compile function through the bucket's own upload events, wired in OpenTofu, so the CLI has no cloud-specific code. (2) `deploy` waits until the native code appears in the native-code bucket, then swaps state; a timeout points at the compile logs. (3) On a miss, the serving function compiles with Cranelift itself and keeps the result in `/tmp` only, never in a bucket (Spin's model; Cranelift is already in the binary). (4) **Upgrades are lazy (the user's call):** nothing is recompiled up front, so stale apps cost nothing. Native code is keyed by Wasmtime's compatibility hash; on a miss the serving function also writes a create-only marker `compile/<compat hash>/<component hash>` to the app bucket, which fires the same upload event, so only the first miss per app after an upgrade asks for a compile. OpenTofu updates the compile function before the serving function. gc removes markers for old hashes. |
| — | Defaults taken at the end of round 6 | JS keeps Q47's documented target of about 1 s p99 (the cloud run measured 1052 ms). The manifest is `torpor.toml`: app name, component, allowed outbound hosts, config and secrets, nothing else. Cron stays deferred (Q23, Q49). **No local S3 server (replaces Q18's MinIO):** `serve`, unit tests and local integration tests use `object_store`'s in-memory store, which supports both conditional writes. `serve --store <url>` points at any bucket or S3-compatible server when data should outlive the process. S3 behaviour itself (ETag format, 412/304, key encoding, LIST pages) is covered by Q12's CI tests against a real bucket. MinIO is archived (2026-04-25). If a local server is ever wanted, versitygw ≥ 1.8.0 passed every check, including 32-writer races; Garage silently ignores conditional PUTs and S3Mock isn't atomic ([research/local-s3.md](research/local-s3.md)). |

## Build plan approved (2026-10-01)

[plan.md](plan.md) is approved with all nine of its choices. Choice 2 replaces Q58's marker detail: one marker, `compile/<hex>`, written by deploy and by a serving miss, deleted by the compile worker when the native code exists; deploy waits for the deletion. **Primary goal while building: the fewest lines of code and the most elegant design. Performance work comes only after that.** A remote repo may be created under the user's personal GitHub account (`carsonfarmer`) when needed.

## During M1 (2026-10-01)

| # | Topic | Decision |
|---|---|---|
| Q59 | Embeddable runtime (the user's ask) | **The host runtime is a library that can be embedded without the CLI.** One package (`torpor-cli`, refining Q18) holds a library `torpor` (`src/lib.rs`: `engine`, `guest`, later `outbound`, `kv`, `config`) and the `torpor` binary (`src/main.rs`: `serve`, `state`, `compile`, the deploy CLI). Modules declared in `main.rs` are invisible to the library, so the compiler keeps the runtime free of `torpor.toml`, the bucket layout and the CLI. The public API is small: build an engine, load a component into an app, `app.handle(request) -> response`. Limits stay constants (Q35). The library never installs a logging subscriber. Split into a separately published crate only when someone embeds it from crates.io (Q18: split only when forced). |
| Q57 (applied) | Timeout mechanism | `torpor serve` is a concurrent server, which is Q57's own trigger, and Q35's 10 s must also hold for a guest that is only waiting. So M1 yields on every 10 ms epoch tick and drops the store at a 10 s deadline (what `wasmtime serve` does): about one line more than the plain trap ([research/wasmtime-embedding.md](research/wasmtime-embedding.md), section 6). |

## During M2 (2026-10-01)

From the research brief, [research/m2-interfaces.md](research/m2-interfaces.md).

| # | Topic | Decision |
|---|---|---|
| Q38 (applied) | Allow-list matcher | **Hand-written on `http::Uri`, not the `url` crate.** `url` cannot parse `*://*:*`, `http://*:*` or `host:*`, so the wildcard parts would have to be split by hand first, and that split is the whole matcher. About 36 lines with parsing. It matches the request's literal host text, which is stricter than Spin (Spin normalises `127.1` first); the address filter judges the resolved address either way. |
| Q34 (applied) | Address filter and dialling | **Our own connect, not `default_send_request`.** That function resolves the name again inside its connect, so the address checked is not the address dialled (a rebinding gap), and its timeouts default to 600 s. We resolve, reject the request if **any** address is blocked (Spin drops the blocked ones and dials the rest), and dial only the checked list. The blocked set is hand-written with stable `std` (12 lines): `ip_network`'s `is_global` lets through `::127.0.0.1`, NAT64, 6to4 and IPv4 multicast. |
| — | KV cache scope | **Per app per process**, not per store: each request gets a fresh store, so a per-store cache would never hit. Bounded by a byte cap that sits outside the guest's memory cap. |
| — | Spin SDK guests | The SDK's own config import is `wasi:config@0.2.0-draft-2024-09-27`, which does not link on rc.1, and its KV module imports `spin:*`. Spin SDK apps use the SDK for HTTP only and `wit_bindgen::generate!` for KV and config. The app docs say so. |
| Q44 (applied) | KV list cursor | **The last key returned, not an S3 continuation token.** We list with `start-after` (`list_with_offset`), so a cursor stays valid across processes and caches, and the same code works on stores without continuation tokens. |
| — | Optional KV and outbound behaviour | **All kept, over the first budget:** the batch interface (about 18 lines), the page cache (about 8), the cache byte cap (about 6) and the allow-list host check (about 4). Each is real behaviour, not ceremony (2026-10-02). |
| — | Hung DNS lookups | **Accepted for now.** A lookup that hangs holds a blocking-pool thread after the 10 s deadline has already answered the guest. A timeout around the lookup would not free the thread either. |

## M2 review (2026-10-02)

| # | Topic | Decision |
|---|---|---|
| Q33, Q42 | KV cache (replaces Q33's value cache, round 6's Q42 and the cache rows of "During M2") | **No cache.** Every KV call goes to the store: `get` is a GET, a write one PUT, a page of `list-keys` one LIST. This removes the generation object (and with it the breach of Q12's one-write-per-second rule on GCS), the page cache, the byte cap and every staleness case. `kv.rs` drops from 312 to 236 lines, not counting tests. Uncached reads cost more on hot keys: one key read 100 times a second all month is about $104 in GETs. At the reference load, about $0.90 a month. |
| Q12 | Hot keys on GCS | GCS takes about one write a second per object and answers faster writes with 429. `object_store` retries those with backoff for up to 3 minutes. S3 and Azure have no such limit. Documented for app authors; the host does no pacing. |
| — | KV fixes found in review | GCS conditional writes need the object's generation, not its ETag, so a CAS keeps both (`UpdateVersion`). Deleting a missing key is not an error (GCS and Azure answer 404). A swap on a key deleted since `cas::new` loses instead of failing (S3 answers 404 to `If-Match` there). |
| Q9 | App state backend (the user's note) | **Revisit after M2.** Only platform state must live in buckets (the clarification under round 1); app data need not. M2 ships KV over the object store with no cache, and a later round weighs other backends. |
| Q34 | Use Wasmtime's sender? | **Keep our own connect.** `default_send_request` is the only public sender and takes no resolver or connector. It resolves the name inside its own connect, so checking addresses first and then calling it means a second lookup that a DNS answer can change. That would let a request rebind to loopback, and so to the Lambda runtime API. It also builds TLS with `ClientConfig::builder()`, which panics once a second crypto provider is linked; M3 adds aws-lc-rs for S3. A connector hook upstream would let us drop about 40 lines. |
| Q38 | Allow-list matcher (replaces "Hand-written on `http::Uri`" above) | **An item is kept as its origin, `scheme://host:port`, and matched as a whole string, or by prefix and suffix around the one `*` of `*.`.** It saves 2 lines and rejects more malformed items. Dropped: `*` as a scheme, port or whole host (`https://*`, `https://example.com:*`, `*://example.com:*`, `http://*:*`). Only `*://*:*` remains as a catch-all, and it now allows http and https only. Added: an IPv6 host without a port. No library fits: Spin's matcher is not on crates.io, and `urlpattern` brings `regex` and still needs our own parsing. |
| — | User names in request URLs | **Refused with `HttpRequestUriInvalid`**, not `HttpRequestDenied`. RFC 9110 deprecates userinfo in http(s) URLs and says recipients should treat it as an error; fetch throws. The docs point to an `Authorization` header. This costs 3 lines. |
| Q55 | Data copied in by one host call | **32 MiB** (`HOSTCALL_FUEL`), down from Wasmtime's 128 MiB default. Wasmtime charges every string and list it lifts out of a guest against this budget, so it bounds what one call such as `fields.from-list` or `set-many` can make the host allocate. A call over it traps. |
| — | Wasmtime issue | **No new issue.** An earlier claim here that Wasmtime lifts lists without limit was wrong (see the corrected deferred row). A short comment on bytecodealliance/wasmtime#14430 goes up only after the user approves its text. |

## During M3 (2026-10-02)

| # | Topic | Decision |
|---|---|---|
| Q14 (applied) | Per-app request logs | **A span per request tagged with the app, not sampling.** Each request is one `info` line inside `request{app=NAME}`, so `RUST_LOG='warn,[request{app=NAME}]=info'` turns on one app's lines with no code. Sampling would need a rate stored per app and a counter. Turned on, a busy app logs every request. |
| — | One TLS provider | **aws-lc-rs for everything.** `object_store`'s S3 client uses it, so outbound HTTP moved from ring to it too: one provider is linked, and `ClientConfig::builder()` can't panic over two. |
| — | Store URL | `AmazonS3Builder::from_env().with_url(URL)`, so S3 (and S3-compatible stores) only, configured by the usual `AWS_` variables. `parse_url_opts` would cover GCS and Azure too, but needs the `url` crate; it can come with those installs. |
| — | App names | **1 to 64 of `a-z`, `0-9` and `-`**, the KV store-name rule, checked when an app loads. A name is one segment of a KV key and an app's path prefix, so nothing else is safe. |
| — | Release manifests | JSON with unknown fields refused: the component's hash, the parent release, config, secrets and the allow list. Each secret is armored age ciphertext on its own, so `secrets list` reads names without a key. Deploy and `secrets set` read the parent inside the state swap, so a racing `secrets set` is never dropped. |
| Q48 | Rollback | Moves the app to its current release's parent, so a second rollback goes back further and there is no roll-forward (redeploy instead). The depth of 10 is what `gc` will keep; nothing limits it before then. |
| Q53 | Secrets and bucket writers (**needs your call**) | **Q53's "can't read secrets" does not hold.** A bucket writer can deploy a release that lists any app's sealed secrets next to a component that sends them to an allowed host, and the host decrypts them. Holding the claim needs authenticated deploys (Q53 (b)). Proposed: correct Q53 to say a bucket writer can read secrets, and leave the design as is. |
| — | Where the M3 tests live | In the binary (`serve.rs`), since `state`, `cli` and `serve` are binary modules (Q59). They call the router directly, with no sockets. |

## Deferred work and fast follows

Kept here so nothing agreed in the grilling gets lost.

| Item | From | Notes |
|---|---|---|
| `spinit deploy --from oci://<ref>` | Q40 | About 30 lines with `oci-client`, behind a cargo feature. Until then: `oras cp --to-oci-layout` plus `aws s3 sync`. |
| Read-only `/v2/` registry view of the bucket; `spinit export` | Q40 | Only if wanted. Generate OCI manifests on the fly. |
| Custom domains through CloudFront | Q11, Q50 | Secret origin header checked by the host; certificate in us-east-1. |
| Dedicated-function mode | Q11, Q47, Q55 | For apps needing hard isolation, more memory, or the strict cold-start target for large components. |
| ~~Skip the `gen` write on plain updates~~ | Q42 | Moot: there is no generation object since the M2 review. |
| Automated gc | Q48 | Today: manual `spinit gc`. |
| Cron design revisit | Q23, Q49 | Once-a-minute ticker, opt-in. |
| Rename to torpor | Q43 | Find-and-replace `spinit` → `torpor`, plus a trademark and domain check, before the first public release. |
| Spin adapter behind a cargo feature | Q31 | Only if there's demand. |
| `deploy --plug <component>` | Q39 | About 17 lines with `wac-graph`, about 0.82 MB, in the CLI only (not the Lambda binary). |
| Semi-trusted deployers | Q53 (b) | A deploy endpoint in the function as the only trusted writer, plus auth. About 200 lines. |
| State sharding or change log | Q45 | At about 10k apps or one deploy per second. |
| HMAC on native code | Q54 (a) | Defence in depth against a misconfigured bucket policy. About 30 lines plus a shared key. |
| ~~Yield on each epoch tick plus a request timeout~~ | Q57 | Done in M1 (see "During M1"). |
| Load hot apps during start-up | Q56 (c) | Only if reading state during start-up falls short. |
| One load per cold app | M3 | Concurrent first requests to an app may each fetch and compile it. Harmless, and gone once native code (M4) makes a load cheap. |
| Compile off the async thread | M3 | `Engine::load` compiles on the tokio thread that took the request. Performance work, after M4. |
| Remember a release that fails to load | M3 | A release whose blob is bad or whose secrets don't decrypt is fetched again on every request to it, and each request gets a 500. A short negative cache would spare the bucket. |
| A timeout on the state recheck | M3 | The recheck holds the state lock, so a slow GET stalls every request on the host until it returns. A timeout of about 1 s, then serve the last state read. |
| Case-insensitive domains, a `domains` command | Q41, Q50 | `domains` is matched exactly, and nothing sets it yet. Comes with custom domains. |
| Watch Wasmtime for `wasi:keyvalue` draft2 | Q44 | `wasmtime-wasi-keyvalue` implements only the first draft and keeps its bindings private. If it moves to draft2 with public bindings or a backend trait, replace our `bindgen!` with it. |
| Field lists lifted before their size check | Q53, Q55 | Wasmtime charges every lifted string and list against the per-call budget (`HOSTCALL_FUEL`, 32 MiB; GHSA-852m-cvvp-9p4w), so no host call allocates without bound. `fields.from-list` still lifts every field before its 128 KiB check, so a call it refuses can cost up to that budget. Upstream agreed in bytecodealliance/wasmtime#14430 to charge lifting to Store fuel as well. Watch it. |
| ~~Pace generation writes on GCS~~ | Q12, Q42 | Moot: there is no generation object since the M2 review. |
| KV backend round | Q9 | App data need not live in object storage. Weigh other backends (DynamoDB, a serverless Redis, the SQLite family, or pluggable backends as in Spin's runtime config) against KV over the object store with no cache. |
| Outbound connector hook upstream | Q34 | If `wasmtime-wasi-http` gains a connector or resolver hook, or a way to send over a given stream, our connect and TLS code (about 40 lines) can go. |
| Certificate errors as `TlsCertificateError` | Q34 | Today every TLS failure reaches the guest as `TlsProtocolError`. |
| Second short cloud session | Q51, Q56 | `deserialize_file` against `deserialize`, state read during start-up, 1024 MB, n = 100, compile-function timings. Needs its own explicit apply approval. |
