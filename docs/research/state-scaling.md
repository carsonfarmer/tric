# System state scaling (fact sheet and recommendation)

> Gathered 2026-10-01 by a research sub-agent from AWS, GCP, Azure and Lambda docs (saved and grepped, not summarised by a search engine), the `object_store` 0.14.2 source, Wasmtime 49.0.1 builds measured on an M4 Pro (not Graviton), and small scratch simulations.
> Prices are us-east-1, from [cloud-costs.md](cloud-costs.md). UNVERIFIED marks claims without a primary source or measurement. No cloud resources were created; every latency figure from a real bucket is borrowed from [app-state-and-secrets.md](app-state-and-secrets.md), and every simulation is a model, not a measurement of S3.
> Answers decisions.md Q19 and Q26. Related reading: [host-and-spin-adapter.md](host-and-spin-adapter.md), [comparables.md](comparables.md).

## Bottom line

**Keep one global state object. Slim it, and refresh it lazily.** It scales to roughly 10k apps with no redesign, and the things that break first at many apps are cold-start bytes, not state.

1. **Slim the object.** Store only `{v, apps: name -> manifest hash, domains: host -> app, cron: [...]}`. Routes inside an app, component hash, variables and limits live in the immutable manifest. An entry is about 78 bytes (measured), so 1k apps is 78 KB, 10k apps is 781 KB, and the 1 MB ceiling Vercel puts on a whole Global Config store (Fastly's is 500 entries of 8,000 characters) is reached near 13k apps. Putting routes inline roughly doubles the size (152 B per app) for no benefit.
2. **Refresh with a blocking conditional GET, at most once per 5 s per warm instance, only when a request arrives.** A frozen Lambda cannot poll or receive a push, so the check has to ride on a request. One conditional GET per instance covers every app, which is why one object beats per-app objects. Cost at the reference load is **$0.14 to $0.29 a month**, $0.38 to $0.83 at 10x, **$2 to $4 at 100x**. Checking on every request would cost $0.40, $4 and $40 for the same loads. A completely busy instance can never cost more than $0.21 a month.
3. **Deploy with a compare-and-swap loop (about 16 attempts, full jitter), batch multi-app deploys into one swap, and pace writes to one per second on GCS.** One operator will essentially never collide. In the model, retries start to matter at 50 or more simultaneous deployers.
4. **Precompile at deploy time and store the result.** The state design is not what threatens the 500 ms cold-start budget; compiling is. Compiling a 12.8 MB JS component takes 5 s on one vCPU and a 37.5 MB Python one takes 5.6 to 6 s (a 1.5 MB Rust one takes 0.42 s). Ship zstd-compressed `.cwasm` files keyed by `Engine::precompile_compatibility_hash`, with compile-on-miss as the fallback. Even then, raw JS and Python artifacts miss the budget on transfer alone (about 580 to 615 ms p50 modelled); zstd gets them to about 345 to 370 ms p50, which is borderline at p99.
5. **Add a mark-and-sweep `gc` command** (about 100 lines). Do not use bucket lifecycle expiry; it cannot see references.
6. **Do not build a log (e), sharding (d) or per-app heads (b) now.** Push invalidation (f) is not possible with frozen Lambdas. Revisit (d) or (e) at roughly 10k apps or a sustained deploy rate of one per second.
7. **Keep a dedicated-function mode on the roadmap** (`SPINIT_APP=<name>`, about 40 lines in the router plus an OpenTofu variable). Reasons: noisy neighbours, JS/Python cold starts, and aggregate concurrency above about 500.

Suggested Q26 wording: *Resolved: one slim global CAS state object (name to manifest hash, domains, cron), revalidated lazily at most every 5 s per instance; routes live in immutable manifests; revisit sharding or a log at about 10k apps.*

**Risks that could change this** (details below): at low traffic almost every request pays the check, about 10 ms (a vendor figure, not measured on Lambda), which can push warm p50 over 30 ms when the request also does a bucket read; the once-a-minute cron ticker alone costs $0.04 to $0.06 a month, above the $0.05 idle cap unless the Lambda free tier applies; raw JS/Python cold starts fail the 500 ms budget; new-account Lambda quotas may be lower than the defaults.

## Verified facts

| Claim | Finding | Source |
|---|---|---|
| S3 concurrent conditional writes | The first write to finish wins; later ones get 412. The page documents 409 only for the case where a delete wins the race. | [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) |
| S3 409 on concurrent `If-Match` | Real S3 "occasionally" returns 409 for concurrent `If-Match` writes; `object_store` therefore retries 409 on `PutMode::Update` (`retry_on_conflict`), and `ConditionalRequestConflict` is a documented 409 on the PutObject API. This is not in the user-guide page above. | `object_store-0.14.2/src/aws/mod.rs` line 283 and `client.rs`; [PutObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html) |
| S3 billing of 304, 412, 409 | The error-billing page bills 200 and 4XX responses and lists unbilled 3XX as only 301 and 307; the only unbilled 412 is `RequestIsNotMultiPartContent`. So 412 and 409 are billed; 304 is **inferred** billed (UNVERIFIED). A web-search summary claiming "not charged" is contradicted by this page. | [S3 error billing](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ErrorCodeBilling.html) |
| S3 HEAD and conditional GET price | HEAD and GET are both $0.0004 per 1k; PUT/COPY/POST/LIST are $0.005 per 1k. A conditional GET costs the same as a HEAD and returns the body in the same round trip when the object changed, so it dominates HEAD. | [S3 pricing](https://aws.amazon.com/s3/pricing/) |
| S3 per-prefix rate | At least 3,500 write and 5,500 read requests a second per prefix. A single hot key is not separately limited in the docs. | [S3 performance](https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance.html) |
| S3 consistency | Strong read-after-write on all operations, so a swapped state object is visible to the next GET anywhere. | [S3 consistency](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html) |
| GCS write limit | One write per second to the same object name; above it, throttling errors. | [GCS quotas](https://docs.cloud.google.com/storage/quotas) |
| GCS billing | Generally no charge for operations returning 307, 4xx or 5xx, so a failed CAS (412) is free. 304 is not on that list, so a 304 is **inferred** billed (UNVERIFIED). | [GCS pricing](https://cloud.google.com/storage/pricing) |
| GCS consistency | Strong global consistency for object read-after-write. | [GCS consistency](https://docs.cloud.google.com/storage/docs/consistency) |
| Azure billing | `ClientOtherError` (many 300 to 400 codes, including precondition failures) is billable. Where a 304 is classified is not stated (UNVERIFIED, assume billed). | [Storage Analytics status messages](https://learn.microsoft.com/en-us/rest/api/storageservices/storage-analytics-logged-operations-and-status-messages) |
| Azure per-blob limits | Target of up to 3,000 requests a second for one block blob. No per-blob write cap is documented (absence of evidence, UNVERIFIED), and CAS contention behaviour is untested. | [Blob scalability targets](https://learn.microsoft.com/en-us/azure/storage/blobs/scalability-targets) |
| `object_store` 0.14.2 | `GetOptions` carries `if_none_match`, `if_match`, `if_modified_since`; a match returns `Error::NotModified`. Default `RetryConfig` is 10 retries within 3 minutes. | `object_store-0.14.2/src/lib.rs`, `client/retry.rs` |
| Lambda freeze | The environment freezes once the runtime and all extensions are done and no event is pending. Nothing runs between invocations, so no background poll or push receiver can exist. | [Lambda runtime environment](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html) |
| Lambda `/tmp` | 512 to 10,240 MB, kept across freeze and thaw within one environment, empty in a new one. | [Lambda quotas](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html) |
| Lambda concurrency | Concurrency is requests per second times duration. Default account limit is 1,000 (raisable); the request-rate cap is 10x the concurrency limit. A function can add 500 concurrency or 5,000 requests per second every 10 s, whichever comes first. | [Concurrency](https://docs.aws.amazon.com/lambda/latest/dg/lambda-concurrency.html), [reserved](https://docs.aws.amazon.com/lambda/latest/dg/configuration-concurrency.html) |
| Lambda network and CPU | About 625 Mbps (about 78 MB/s) per environment by default; an opt-in raises it with memory (765 Mbps at 2,048 MB up to 3,000 Mbps at 10,240 MB, non-VPC only). 1,769 MB is one vCPU. | [Quotas](https://docs.aws.amazon.com/lambda/latest/dg/gettingstarted-limits.html), [AWS blog](https://aws.amazon.com/blogs/compute/improving-lambda-function-latency-with-scalable-network-bandwidth/) |

## The global state object

### Model assumptions

- GET p50 26 ms, p99 86 ms; PUT p50 70 ms ([TopicPartition benchmark](https://topicpartition.io/misc/AWS-S3-PUT-latency-benchmark), borrowed). Transfer at 78 MB/s. Conditional GET answered 304 in about 10 ms (vendor figure from [turbopuffer](https://turbopuffer.com/docs/tradeoffs), UNVERIFIED on Lambda).
- Sizes and parse times are measured with `serde_json` on an M4 Pro. Graviton may be about 2x slower (UNVERIFIED), which still leaves parse well under 10 ms at 10k apps.
- "Slim" is the recommended entry (manifest hash only). "Full" is the original idea with routes (two patterns per app) and a cron line for every fifth app inline.

### Size and cold-start cost

Cold GET plus parse = 26 ms + size / 78 MB/s + parse. A changed-state refresh on a warm instance costs the same; an unchanged one is the roughly 10 ms 304.

| Apps | Slim size (gzip) | Full size (gzip) | Parse slim / full | Cold GET+parse slim / full |
|---|---|---|---|---|
| 1 | 0.1 KB | 0.2 KB | under 0.01 ms | 26 ms / 26 ms |
| 10 | 0.8 KB (0.3) | 1.5 KB (0.4) | 0.001 / 0.003 ms | 26 ms / 26 ms |
| 100 | 7.8 KB (2.2) | 15.2 KB (3.0) | 0.01 / 0.03 ms | 26 ms / 26 ms |
| 1,000 | 78 KB (23) | 152 KB (32) | 0.09 / 0.26 ms | 27 ms / 28 ms |
| 10,000 | 781 KB (236) | 1,521 KB (327) | 0.85 / 2.45 ms | **37 ms / 48 ms** |
| 100,000 | 7.8 MB (2.4) | 15.2 MB (3.3) | 10 / 38 ms | 136 ms / 271 ms |

- Building a host-to-app index after parsing costs 0.31 ms at 10k apps and 12 ms at 100k.
- Serialising the full state takes 0.8 ms at 10k and 30 ms at 100k.
- The cold-start tax from state is 26 to 37 ms up to 10k apps. It only becomes a problem at 100k, which is out of scope (and past the ~1 MB single-document ceiling).
- Gzip is possible (storing with `Content-Encoding`) but needs decode code; not worth it below 10k.

### Refresh cost per month

Lazy rule: when a request arrives and the cached state is at least 5 s old, do a blocking conditional GET before serving. For an instance receiving requests at Poisson rate λ per second, checks run at **λ / (1 + 5λ)** per second, never more than 0.2. The share of requests that pay the check is 1 / (1 + 5λ): about 84% at 0.04 requests a second per instance, 34% at 0.39, 5% at 3.9.

Monthly cost = k instances x λ/(1+5λ) x 2,592,000 s x $0.0000004. One fully busy instance (the 0.2 per second ceiling) costs $0.207 a month.

| Load | Avg requests/s | Warm instances k (judgement, UNVERIFIED) | Lazy refresh | Check on every request | Lazy as share of the $0.90 app baseline x scale |
|---|---|---|---|---|---|
| 1x (1M requests, 100k writes) | 0.39 | 1 to 5 | **$0.14 to $0.29** | $0.40 | 16 to 32% |
| 10x | 3.9 | 2 to 5 | **$0.38 to $0.83** | $4.00 | 4 to 9% |
| 100x | 38.6 | 10 to 20 | **$1.97 to $3.75** | $40.00 | 2 to 4% |

- k is chosen as average concurrency (requests per second times 0.1 s) times a burstiness factor of 2 to 5, floored at 1 and rounded. It is a judgement, not a measurement.
- Baseline $0.90 is the cloud-costs.md figure (1M GETs plus 100k PUTs). Refresh is on top of it. At 1x the total is about $1.04 to $1.19, plus the cron ticker ($0.04 to $0.06), so **about $1.1 to $1.25: above the $1 target but inside the $2 limit**. Halving the refresh by relaxing the window to 10 s would break Q10.
- Refresh is a small share from 10x up. Lambda compute dominates there (illustration: 100 ms at 1,769 MB is about $18 a month of compute alone at 10x after the free tier, UNVERIFIED for new accounts).
- Checking on every request would cost about 44% of the 1x baseline and add about 10 ms to every request; it is the wrong design.
- Stale-while-revalidate (serve the old state, refresh in the background) would break the 5 s rule: Lambda freezes right after the response, so the refresh may not run until the next request, which could be minutes later and would then be served stale.

### Idle and cron

- Idle apps cost no requests and no compute. State cost is one 78-byte entry.
- The cron ticker (Q23) runs 43,200 invocations a month. Cost: $0.0086 (invocations) + $0.0173 (one conditional GET each, since 60 s always exceeds the 5 s window) + compute ($0.014 at 50 ms, $0.029 at 100 ms, at 512 MB) = **$0.04 to $0.06 a month**. That exceeds the $0.05 idle cap unless the Lambda free tier covers it (UNVERIFIED for accounts created after July 2025). Options: let the ticker tolerate a 60 s stale state and skip the GET (saves $0.017), or count the ticker as outside the "storage only" idle cap. Needs a decision.
- $0.05 of storage is 2.17 GB, which is the real idle ceiling (see per-app idle cost below).

### Deploy contention

Model (scratch script, not in the repo): D deployers start together; each loops GET (26 ms) then conditional PUT (70 ms), with +-25% jitter. The first PUT to finish wins, the rest get 412, then retry with full-jitter exponential backoff (base 50 ms, cap 2 s). 100 runs each. This models S3's "first to finish wins" and nothing else: no server-side queueing, no 409s, no throttling.

| Simultaneous deployers | Mean attempts | p99 attempts | Wall time p50 | Share needing over 8 attempts | Over 16 attempts |
|---|---|---|---|---|---|
| 1 | 1.0 | 1 | 0.09 s | 0% | 0% |
| 5 | 2.5 | 5 | 0.74 s | 0% | 0% |
| 10 | 3.3 | 6 | 1.6 s | 0% | 0% |
| 25 | 4.7 | 8 | 4.0 s | 0.2% | 0% |
| 50 | 5.9 | 10 | 6.5 s | 8.9% | 0% |
| 100 | 7.7 | 14 | 10.7 s | 39.5% | 0.1% |
| 250 | 12.3 | 24 | 21.4 s | 71.6% | 24.4% |
| 500 | 19.3 | 38 | 37.9 s | 85.2% | 58.0% |

- Sustained commit rate under contention is about 9 to 13 a second on S3 in the model (100 deployers in 10.7 s, 500 in 38 s). A 1.5 MB state (GET 75 ms, PUT 130 ms) lowers it to about 5 to 6 a second; 50 deployers need 10 s and 27% exceed 8 attempts.
- A single operator running `deploy` a hundred times an hour has a collision chance of about 0.3% per deploy (100 ms window). Contention is a CI-fan-out problem, not a normal-use problem.
- **Fixes, cheapest first:** (1) batch: one `deploy` of N apps uploads N manifests (create-only, in parallel) and does one state swap, so D stays 1 and the multi-app deploy is atomic, which sharding and per-app heads cannot offer; (2) 16 attempts with full jitter handles up to about 100 racers; (3) idempotent retry: after a timeout, re-GET and treat "state already holds my manifest hash for this app" as committed (manifest hashes are content-addressed, so this is about 5 lines and the same idea as object-log's committed/conflict/pending outcomes).
- **GCS** limits writes to one a second per object. D simultaneous deployers need at least D seconds, and 429s appear if two PUTs land in one second. Pace it: before a PUT, wait until at least 1 s after the object's `last_modified` (about 5 lines; the GET already returns it).
- **Azure:** no documented per-blob write cap; untested (UNVERIFIED).
- **Deploy rate versus refresh latency:** every deploy changes the ETag, so each instance's next check is a full GET (37 to 48 ms at 10k apps) instead of a 304. At one deploy a minute that touches a few percent of checks; at one a second it makes every check a full GET. That is the sustained rate at which a log or sharding starts to pay.

## Alternatives

### Summary

Lines of code are my estimates on `object_store` (UNVERIFIED, judgement). Cost is refresh at the reference load and at 100x.

| Design | Cold start vs N | Refresh cost (1x / 100x) | Deploy contention | Atomic multi-app | Est. lines | Verdict |
|---|---|---|---|---|---|---|
| (a) slim global CAS object | +26 ms to 1k apps, +37 ms at 10k | $0.14 to $0.29 / $2 to $4 | One hot object; fine for one operator | Yes | about 150 | **Recommended** |
| (b) per-app heads + `routes/<host>` | None | about $0.40 / about $40 on a long tail (see below) | None per app | No | about 250 to 300 | Revisit above 10k apps |
| (c) epoch/index + heads | As (a) | Same as (a) | As (a) | Yes | about 300 | Collapses into (a) |
| (d) sharded state objects | Smaller per shard | Multiplied by shards touched | Divided by shard count | Only within a shard | about 250 to 300 | Revisit above 10k apps with high deploy rate |
| (e) change log + checkpoints | Checkpoint + tail | Same GET count as (a) | Head still one object | Yes | about 300 to 500 | Not now |
| (f) push invalidation | n/a | n/a | n/a | n/a | n/a | **Not possible** with frozen Lambdas |

### (b) Per-app HEAD objects loaded lazily

- Layout: `apps/<name>/head` (manifest hash) plus `routes/<host>` (app name). A request resolves host to app (one GET, cached with negative caching), then loads and revalidates that app's head (another GET).
- **Why it loses on cost:** the 5 s window applies per cached object, so a conditional GET is needed per (instance, app) pair. With a long tail of apps each seen rarely, nearly every request pays a check, which is the $0.40 per million "check on every request" figure, versus one check per instance for the global object, shared across every app that instance serves. Pooling is the whole advantage: $40 versus $2 to $4 at 100x with a long tail of apps; a hot app amortises just as well in either design.
- Wins: cold start does not depend on N; no deploy contention (per-app CAS, and on GCS the one-write-a-second limit applies per app); no size ceiling.
- Costs: the cron ticker still needs a list of cron specs, either a LIST of `apps/` (12.5x a GET; a LIST every minute is $0.22 a month) or a global cron-index object, which reintroduces the object we are avoiding. Multi-app deploys are not atomic.

### (c) Epoch object plus per-app heads, or an index of hashes

- An "epoch only" object (a counter) costs the same conditional GET per refresh as the whole slim object, because a 304 costs the same whatever the object size. It saves only the body bytes on a change, and then forces per-app head GETs. It is strictly worse than (a) below about 10k apps.
- The version where the index holds `app -> manifest hash` and manifests carry routes **is** the slim object. An entry is 78 bytes now. A binary encoding (CBOR with raw 32-byte hashes) would reach about 45 to 50 bytes and move the 1 MB ceiling from about 13k to about 20k apps, at the cost of an extra dependency and unreadable state. Not worth it.

### (d) Sharded state objects by app-name hash

- Divides contention and per-object write rate by the shard count, and keeps each GET small.
- But an instance touching apps in many shards needs one conditional GET per shard per 5 s, multiplying refresh cost by the shards touched (a 16-shard instance at 1x costs up to 16x). Host-based routing needs a host-sharded index as well, so a request is two lookups. Multi-app deploys are atomic only within one shard.
- It only pays above about 10k apps or a deploy rate near one a second. State `v` and a `state/v1/` prefix keep a later move possible.

### (e) Append-only change log with checkpoints (ideas from object-log)

- Design: immutable, create-only `log/<seq>` segments hold app changes; one mutable head object holds the latest sequence and the base checkpoint; checkpoints bound cold-start reads. Refresh is still one conditional GET of the head; on change, fetch only the new segments. A deploy is two PUTs (segment, then head CAS), about 140 ms and a fraction of a cent.
- Borrowed from object-log (read only, not copied): a single mutable head with everything else content-addressed and create-only; the committed/conflict/pending outcome for ambiguous commits; checkpoints. object-log is 7,156 non-test lines; a minimal version here is about 300 to 500.
- It does not remove the single hot head (same GCS limit, same CAS contention), so it does not fix contention. It wins only on bytes: a changed refresh costs O(delta) instead of O(N), which matters above about 1 MB of state or at a high deploy rate.
- Verdict: not for v1. The cost is 2 to 3x the code of (a) for a benefit that appears above about 10k apps.

### (f) Push invalidation via S3 events

- **Refuted for this runtime.** S3 event notifications (SQS, SNS, EventBridge, Lambda) only start a new invocation. A frozen Lambda environment has no open listener, and an invocation is routed to one environment of Lambda's choosing, never to all of them, and the function cannot enumerate its warm environments. So a deploy cannot reach the instances that hold stale state.
- **The one variant that works** is an event-triggered function that calls `UpdateFunctionConfiguration` to change an environment variable, which retires every warm environment so the next request starts fresh. It is AWS-only (breaks the portability goal), turns every deploy into a fleet-wide cold-start storm, and its propagation time was not measured (UNVERIFIED). Rejected.
- Provisioned concurrency and Lambda Managed Instances keep processes running but do not scale to zero, so they break the idle-cost requirement. A long-lived `serve` host (the plain HTTP mode) could subscribe to events, but that is a different deployment.

### (g) Prior art and what is reusable

| System | Mechanism | Lesson for spinit |
|---|---|---|
| [Cloudflare KV](https://developers.cloudflare.com/kv/concepts/how-kv-works/) | Central store plus per-location cache; default `cacheTtl` 60 s; changes can take 60 s or more to show elsewhere. | Cache-with-TTL is the common pattern, but 60 s is too slow for the 5 s rule. |
| [Cloudflare Quicksilver](https://blog.cloudflare.com/quicksilver-v2-evolution-of-a-globally-distributed-key-value-store-part-1/) | Config replicated to a local store on every host, read locally. | Needs a resident daemon per host; no Lambda equivalent. |
| [Fly.io Corrosion](https://github.com/superfly/corrosion) | SQLite per node, CRDT merges, gossip (SWIM) for membership. | Same: needs long-lived peers. Not reusable. |
| [Vercel Global Config](https://vercel.com/docs/edge-config/edge-config-limits) | Small config store, 1 MB per store on every plan, writes propagate within about 10 s globally. | Supports one bounded document as the routing source; also shows that such stores stay small (about 1 MB). |
| [Fastly Config Store](https://docs.fastly.com/products/compute-resource-limits) | 500 entries per store, 8,000 characters per value, 1,000 writes an hour. | Same lesson: a small, bounded config document with a low write rate. |
| [Netlify atomic deploys](https://docs.netlify.com/site-deploys/overview/) | Each deploy is an immutable file set; the site pointer flips only when the set is complete. | Already our model: immutable manifests plus one pointer swap. Rollback is a pointer move. |
| Deno Deploy | Pages were fetched but did not document the config-propagation internals. | UNVERIFIED; no claim made. |

Reusable ideas: bounded single document, immutable releases with a pointer flip, a short cache TTL. Not reusable: anything that needs a persistent process on each node.

## Related scaling questions

### Per-app cold compile and the cold-start budget

Measured on an M4 Pro, Wasmtime 49.0.1. Lambda's 1,769 MB gives one vCPU, so the single-thread columns are the relevant ones; Cranelift compile on Graviton is not measured (UNVERIFIED).

| Component | Wasm size | Compile, 14 threads | Compile, 1 thread | `.cwasm` size | Deserialize | `.cwasm` zstd-3 (decompress) |
|---|---|---|---|---|---|---|
| Rust "medium" | 1.5 MB | 59 to 68 ms | 424 to 430 ms | 3.0 MB | 0.3 to 0.4 ms | 0.95 MB (9 ms) |
| Rust "large" | 2.5 MB | 102 to 114 ms | 775 to 794 ms | 5.0 MB | 0.4 to 0.7 ms | 1.59 MB (13 ms) |
| JS component | 12.8 MB | 593 to 854 ms | 4,969 to 5,249 ms | 31.1 MB | 2.0 ms (mmap), 3.5 ms | 9.18 MB (49 ms) |
| Python (p2 build) | 37.5 MB | 1,772 to 2,181 ms | 5,602 to 6,050 ms | 33.8 MB | 3.1 ms (mmap), 4.8 ms | 10.36 MB (57 ms) |

- JS and Python compile peaked near 570 to 633 MB resident, so compile-on-miss also needs a 1 GB or larger function.
- `Component::deserialize_file` memory-maps the file (needs `unsafe`; acceptable because the operator owns the bucket, Q1). `precompile_compatibility_hash` identifies compatible artifacts; key the object by it and fall back to compile (then write back with put-if-absent) when the host is a different build or config. `Config::target` allows cross-compiling to aarch64 from the deploy machine.
- Zstd shrinks `.cwasm` about 3.1 to 3.4x; it is the lever for JS and Python transfer time.

**Modelled cold start** for the first request to an app on a fresh environment. Assumes a base of 95 ms for runtime init, host start and engine creation (an assumption, to be replaced by the spike), a 30 ms state GET and a 26 ms manifest GET. Sequence: state, then manifest, then component; none can overlap.

| Component | Component GET | Decompress | Deserialize | p50 total | If each GET hits its p99 (upper bound) |
|---|---|---|---|---|---|
| Rust medium, raw cwasm | 65 ms | none | 0.4 ms | about 215 ms | about 395 ms |
| JS, raw cwasm | 424 ms | none | 3 ms | about 580 ms | about 760 ms |
| JS, zstd | 144 ms | 49 ms | 3 ms | about 345 ms | about 525 ms |
| Python, raw cwasm | 459 ms | none | 5 ms | about 615 ms | about 795 ms |
| Python, zstd | 159 ms | 57 ms | 5 ms | about 370 ms | about 550 ms |
| Any, compile on miss, 1 vCPU | n/a | n/a | 0.4 to 6 s of compile | 0.7 s to over 6 s | n/a |

- Rust-sized components meet the budget only with a precompiled artifact. Compiling even the 1.5 MB one takes about 0.43 s on one vCPU.
- JS and Python fail the budget raw and are borderline at p99 with zstd. Streaming decompression while downloading (about 30 lines, not measured) could take about 50 ms off. Otherwise: dedicated mode with more memory (more bandwidth and CPU), or a relaxed budget for those runtimes.
- A warm environment's first touch of another app skips the base and the state GET: about 90 ms for Rust, about 220 ms for zstd JS.
- The p99 column assumes every sequential GET lands at its own p99 simultaneously, so it overstates real p99.

### Memory LRU of compiled components, and `/tmp`

- Lambda gives one request at a time per environment, so only one guest's linear memory (cap 256 MiB, Q35) is live at once. On a 1,769 MB function that leaves about 1.2 GB for cached code: roughly 400 Rust-sized components or about 38 JS ones if all pages were resident (memory-mapped pages count only once touched, so the real count is higher; UNVERIFIED).
- Hit ratio of a per-instance LRU, simulated with Zipf popularity (s = 1.0 is a few popular apps, 0.0 is uniform) over N apps and capacity C apps per instance:

| Zipf s | Apps N | C=8 | C=16 | C=32 | C=64 | C=128 |
|---|---|---|---|---|---|---|
| 1.0 | 100 | 34.6% | 51.1% | 68.8% | 87.8% | n/a |
| 1.0 | 1,000 | 18.0% | 28.1% | 38.8% | 50.1% | 61.8% |
| 1.0 | 10,000 | 11.1% | 17.9% | 25.8% | 33.8% | 42.0% |
| 0.7 | 1,000 | 3.9% | 7.4% | 13.2% | 21.8% | 33.8% |
| 0.0 | 1,000 | 0.8% | 1.6% | 3.2% | 6.4% | 12.8% |
| 0.0 | 10,000 | 0.1% | 0.2% | 0.3% | 0.7% | 1.3% |

- Memory-LRU hit ratios are poor at large N, but a miss is cheap if the file is in `/tmp`: deserialize from disk costs 2 to 5 ms. The cache that matters is `/tmp`: a bucket fetch (91 ms Rust, 220 ms zstd JS) happens only on the first touch of an (environment, app) pair.
- `/tmp` holds raw `.cwasm` (it must be mmap-able): 512 MB fits about 170 Rust-sized apps or 16 JS ones; 10 GB fits about 3,300 or 320. Extra ephemeral storage is billed per GB-second above 512 MB (about $0.03 per million 100 ms requests at 10 GB; price not re-fetched, UNVERIFIED). Use a byte-bounded LRU keyed by file size for both memory and `/tmp`, and evict from disk when over budget.
- Every new environment starts with an empty `/tmp`, so spreading load across many short-lived environments multiplies first-touch fetches. This is the cache-hit-rate cost of one shared function for many apps.
- Not measured: instantiate time and memory for JS and Python, and whether `/tmp` mmap pages are reclaimed under memory pressure (UNVERIFIED).

### Per-app idle cost and storage growth

Per app, storage only, content-addressed so unchanged components are shared across releases.

| App type | Stored (wasm zstd + cwasm zstd) | Cost per month | Apps that fit under the $0.05 idle cap |
|---|---|---|---|
| Rust medium | 0.41 + 0.95 = 1.4 MB | $0.00003 | about 1,600 |
| JS | 3.9 + 9.2 = 13.1 MB | $0.0003 | about 165 |
| Python | 13.2 + 10.4 = 23.5 MB | $0.0005 | about 92 |

- Each retained release adds its changed components, so a retention depth K multiplies these. Retention (how many manifests per app are kept) is an open decision for the user; it sets both rollback depth and storage.
- Idle apps add no request or compute cost, and the state grows by 78 bytes each.
- GC: a `spinit gc` that marks everything reachable from the state object through manifests to components and sweeps the rest, skipping anything younger than a grace period (about 100 lines; a LIST of a few thousand objects costs cents). Bucket lifecycle rules cannot see references and would delete live components. Residual race: a deploy that reuses an old unreferenced component and a sweep running between upload and state swap; acceptable if `gc` is run manually by the single operator, and worth documenting.

### Lambda concurrency and noisy neighbours

- Concurrency = requests per second x duration. At 100x (38.6 requests per second average, up to about 400 per second at a 10x peak) and 100 ms per request, that is about 4 average and about 40 at peak. The default 1,000 account limit is far away. Scale-up is limited to +500 concurrency per function every 10 s; a function that must go from 0 to 1,000 needs about 20 s.
- New accounts may start with a lower quota profile than 1,000 (exact numbers UNVERIFIED); check Service Quotas before relying on it.
- All apps share one function's concurrency. One app that bursts, or that takes 10 s per request, can starve the others; reserved concurrency is per function, not per app. A host cannot fix this by counting, because each environment sees only its own request and shared per-app counters would need a bucket write per request, which the 1-write-a-second rule forbids.
- Memory is also per function: one 1.7 GB JS app sets the memory size, and the billing rate, for every light Rust app.

### Where one shared function stops making sense

| Trigger | Approximate threshold | Response |
|---|---|---|
| State document too large | About 10k apps (781 KB slim; ceiling about 13k) | Shard (d) or log (e) |
| Sustained deploy rate | About 1 a second (every check a full GET; GCS write limit) | Log (e) or shards (d) |
| Aggregate peak concurrency | Over about 500 for one function (burst limit) | Split heavy apps into dedicated functions |
| Heavy runtime (JS/Python, 31 to 34 MB `.cwasm`) | Any, once cold start matters | Dedicated function with more memory |
| Noisy neighbour | Any single app that bursts | Dedicated function (isolated concurrency) |

**Dedicated-function mode:** the same binary with `SPINIT_APP=<name>` makes the router accept only that app (about 40 lines). The deploy flow does not change; the OpenTofu module gets an optional per-app function, which is most of the work. Do not build it before a real trigger, but keep the router written against an optional app filter.

## Open decisions for the user

1. Does the $0.05 idle cap include the cron ticker ($0.04 to $0.06 a month), or is it storage only, as written in Q4?
2. How many releases per app are retained (rollback depth versus storage)?
3. Are JS and Python expected to meet the 500 ms cold-start p99? If so, budget for zstd plus streaming plus a larger dedicated function; otherwise state a longer target for them.
4. Is host-based routing (custom domains through CloudFront) needed in v1? Without it, `domains` is empty and routing is by path prefix.

## UNVERIFIED and not measured

- S3 304 billing (inferred from the 3XX list), GCS 304 billing, and where Azure classifies 304.
- Real S3 behaviour under many concurrent `If-Match` writers (throughput, 409 frequency); the contention table is a model. Azure per-blob CAS contention.
- Conditional-GET latency on Lambda (about 10 ms is a vendor figure); `object_store` conditional GET behaviour on GCS and Azure.
- Graviton compile and parse speed, zstd decompression speed on Lambda, instantiate time and memory for JS and Python, and `/tmp` mmap reclaim behaviour. The Python p3 build was not measured.
- Environment idle retention and provisioned-concurrency freeze behaviour; new-account Lambda quotas; the effect of the 2 GB bandwidth opt-in.
- The `UpdateFunctionConfiguration` propagation time (variant of (f)); the cron ticker's coverage by the Lambda free tier; the ephemeral-storage price.
- Deno Deploy and Netlify internals beyond the pages cited; line-of-code estimates (judgement).

## Method

- Facts: pages saved with `curl` and grepped as stripped text (the S3 pricing, GCS pricing and error-billing pages came back truncated through the fetch tool and were re-read this way). Quotes are paraphrased; each claim above cites its URL.
- Corrections found along the way: a search-summary claim that 304 and 412 are "not charged" is contradicted by the primary S3 page; Lambda scales by 500 concurrency per 10 s, not 1,000; the S3 conditional-writes page documents 409 only for delete races, while the concurrent `If-Match` 409 comes from the `object_store` source comment and the PutObject API error list.
- Measurements: scratch Rust builds of Wasmtime 49.0.1 (compile, serialise, deserialise, with 14 threads and one) on a JS component (12.8 MB) and a Python component (37.5 MB) taken from the earlier host research, plus two generated Rust guests. A scratch `serde_json` benchmark measured state size and parse time for 1 to 100k apps. Two small Python simulations modelled CAS contention and LRU hit ratios. The scripts live in the session scratchpad, not in the repo.
- Refresh formula: with per-instance Poisson rate λ, a blocking check at most every 5 s gives λ / (1 + 5λ) checks per second.
