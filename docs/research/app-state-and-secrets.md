# App KV state and secrets (fact sheet)

> A research sub-agent gathered this on 2026-09-30 from official pricing pages, the AWS Price List and Azure Retail APIs, docs, crates.io, and its own scratch builds (rustc 1.97.1).
> Prices are us-east-1, or eastus for Azure. UNVERIFIED marks claims without a primary source.
> The object-log repo was only read, never modified.

## The agent's recommendations

1. **App KV v1:** use DynamoDB behind a small `KvBackend` trait, with object-per-key on `object_store` (If-Match) and an in-process cache as the portable bucket backend.
2. **System state:** object-log is a strong fit. Defer KV-on-object-log and SlateDB.
3. **Secrets:** store one age-encrypted blob per app in the bucket, and keep the age identity in a Lambda environment variable. It costs about $0. SSM standard parameters are the $0 AWS-native alternative.

Two assumptions in the research brief were wrong:
- **OpenDAL backends:** OpenDAL 0.59.3 has **no** DynamoDB, Firestore or Cosmos service.
- **`wasmtime-wasi-keyvalue` 49:** it is hard-wired to an in-memory HashMap and has no CAS, because it targets the older draft (increment only).

## Managed KV

| Store | Price | Free tier | Cost at 1M reads + 100k writes |
|---|---|---|---|
| DynamoDB on-demand | $0.625/M WRU, $0.125/M RRU ([price list](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonDynamoDB/current/us-east-1/index.json)) | Still offered: 25 WCU, 25 RCU, 25 GB ([AWS](https://aws.amazon.com/dynamodb/pricing/on-demand/)) | **$0.13** eventually consistent; $0.19 strongly consistent |
| DynamoDB provisioned | $0.00065/WCU-h, $0.00013/RCU-h beyond free | as above | **$0**. 25 RCU is about 50 eventually-consistent reads/s; [burst](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/burst-adaptive-capacity.html) keeps up to 300 s of unused capacity, best-effort |
| Firestore / Datastore | $0.03/100k reads, $0.09/100k writes ([Firestore](https://cloud.google.com/firestore/pricing), [Datastore](https://cloud.google.com/datastore/pricing)) | 50k reads and 20k writes per day | **$0**; $0.39 without the free quota |
| Cosmos DB | Serverless $0.25/M RU | 1,000 RU/s and 25 GB for life, not on serverless ([docs](https://learn.microsoft.com/en-us/azure/cosmos-db/free-tier)) | **About $0.53** serverless; $0 on the free tier |
| Azure Table Standard | $0.00036/10k ops; $0.045/GB (LRS) | none | **About $0.04** |

- **DynamoDB:** replicates across three AZs and has native conditional writes and atomic counters ([doc](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/WorkingWithItems.html)). AWS states "single-digit ms" server side; latency measured from Lambda is UNVERIFIED.
- **Cosmos:** read p50 about 4 ms and write p50 about 5 ms, both p99 under 10 ms, in a single region ([doc](https://learn.microsoft.com/en-us/azure/cosmos-db/consistency-levels)).

## Bucket latency

| Measurement | Source | Result |
|---|---|---|
| S3 Standard, EC2 same region, 500 KiB, n=100 | [TopicPartition](https://topicpartition.io/misc/AWS-S3-PUT-latency-benchmark) | GET p50 26 / p95 39 / p99 86 ms; PUT p50 70 / p95 101 / p99 137 ms |
| S3, 1 KB (vendor) | [Tigris](https://www.tigrisdata.com/blog/benchmark-small-objects/) | read p50 22 / p90 42 ms |
| Conditional GET / metadata (vendor) | [turbopuffer](https://turbopuffer.com/docs/tradeoffs) | about 10 ms; S3 metadata p50 10 / p90 17 ms; GCS 12–18 ms |
| S3 Express | [SlateDB tuning](https://slatedb.io/docs/operations/tuning/) | 5–10 ms (an expectation, not a measurement) |

- **S3 Express One Zone fails the AZ-loss requirement.** It is single-AZ ([doc](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-bucket-high-performance.html)).
- **Conditional writes:** a failed precondition returns 412, and a concurrent conflict returns 409 ([doc](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)).
- **The "~15 CAS/s" figure** is not a published S3 limit. It's consistent with a serial loop at about 70 ms per PUT.
- **GCS** limits each object name to one write per second, and throttles above that with 429s ([quotas](https://docs.cloud.google.com/storage/quotas)).
- **Unpublished:** no source gives GCS or Azure small-object p50/p99, or conditional-PUT latency, on any cloud.

**Conclusions on bucket latency:**
- **Reads:** one uncached S3 GET is about 26 ms p50, which is borderline against a 30 ms budget. A conditional GET (304) is about 10 ms, but still billed as a GET. A short cache TTL avoids the call entirely.
- **Writes:** a CAS write is a GET plus a conditional PUT, so about 100 ms p50, with p99 near 200 ms. Those are estimates.

## Libraries

| Library | Version | Conditional support | wasm32-wasip2 |
|---|---|---|---|
| `object_store` | 0.14.2 (2026-09-15), 91M downloads | S3: Create = `If-None-Match: *`, Update = `If-Match` with a 409 retry. GCS: generation match. Azure: both. **LocalFileSystem: Create only, Update returns NotImplemented.** InMemory: all | `aws-base`, `gcp-base` and `azure-base` compile. A WASI HTTP connector is still needed |
| OpenDAL | 0.59.3, 16.5M downloads | s3, gcs and azblob support `if_match`, `if_not_exists` and read conditionals | memory, s3 and azblob compile; gcs fails (`reqsign-google`/`jwt`); no WASI HTTP transport found |

`wasi:keyvalue` hosts:

| Host | What it provides |
|---|---|
| `wasmtime-wasi-keyvalue` 49.0.1 | In-memory only, no CAS. Forks `-redis` and `-redb` exist at 0.1.0 with about 20 downloads |
| wasmCloud `wash-runtime` | Filesystem, in-memory, NATS and Redis providers |
| Spin 4.2.1 | The only host with CAS (`wasi:keyvalue@0.2.0-draft2`). Providers: DynamoDB, Cosmos, Redis, SQLite. Git-only, tied to spin-core and spin-factors. Its [`key-value-aws`](https://github.com/spinframework/spin/tree/main/crates/key-value-aws) (667 lines) is the reference DynamoDB implementation, with CAS via a version attribute |

No S3 or `object_store` implementation of wasi:keyvalue exists.

## object-log (`~/Developer/Personal/object-log`)

**Size and status:**
- 7,156 non-test lines in `src/`, or 6,208 excluding `sim.rs`.
- v0.1.0, Apache-2.0, not on crates.io.
- Depends only on `object_store` 0.14. The default build compiles for WASIp2; the `aws` feature does not, so use `aws-base` plus a WASI HTTP connector.

**(a) System-state log: strong fit.**
- Deploys are rare, so the commit rate doesn't matter.
- The HEAD pointer is the log's single mutable head, and a rollback is just another append.

**(b) App KV backing: weak fit.**
- A commit is two sequential PUTs, about 140 ms p50, so roughly 7 commits/s per log. The 10 writes/s per-app peak exceeds that.
- p99 is an estimated 200–270 ms, which misses the 200 ms target.
- GCS limits the head object to one write per second.
- The parked KV branch measured Get p50/p95 of 82/132 ms and Set 178/228 ms, with 64 KiB values on AWS.

**(c) KV on object-log:**
- Already exists, parked on branch `cf/park-key-value` (commit 9ba5c12).
- About 1,067 lines in `crates/object-log-kv` (a radix tree), plus about 1,521 lines for a Spin 4.1 provider.

**(d) Risks:**
- The API and on-disk format are pre-release.
- It has been tested on memory, MinIO and AWS S3 only.
- Bucket lifecycle rules must not expire protocol objects.
- The credential needs read, write, list and delete.

## Ranking of v1 app-state options

| Rank | Option | Custom code | Cost at reference load | Warm read | Portability | Correctness |
|---|---|---|---|---|---|---|
| 1 | DynamoDB | Small (port `key-value-aws`) | $0.13–0.19, or $0 provisioned | single-digit ms server side | AWS only | Native CAS and counters, 3 AZs |
| 2 | Object per key on `object_store` | Small to medium (CAS loop, increment, list, batch) | about $0.90–1.00 | about 26 ms uncached; fits with a cache | Best | Good; GCS limits a hot key to 1 write/s |
| 3 | KV on object-log | Medium | about $1.40 or more | 82 ms p50 uncached | Same as above | Misses the write p99 target |
| 4 | SlateDB 0.17 | Library plus a writer-lease layer | PUT-bound | `DbReader` plus cache | Any `object_store` | **Single writer.** Writer-epoch fencing means concurrent Lambda instances fence each other; there is no multi-writer mode ([RFC](https://github.com/slatedb/slatedb/blob/main/rfcs/0001-manifest.md)) |
| 5 | Hosted free tiers | Medium | see below | n/a | Lock-in | Mixed |

Hosted free tiers:
- **Upstash:** costs about $2.20 at the reference load, and free-tier databases aren't replicated.
- **Cloudflare KV:** eventually consistent for up to 60 s, with no atomics.
- **D1:** the free tier fits, but its REST API may be rate limited.
- **Turso:** latency and durability are UNVERIFIED.

## Secrets

**Abstractions:** no cloud-agnostic secrets SDK fits a Lambda.

| Tool | Status | Fit |
|---|---|---|
| `age` crate | 0.12.1, 5.7M downloads | Pure Rust, tiny API. Best fit |
| SOPS | v3.13.3, CNCF sandbox | Go CLI, so useful only for authoring |
| `rops` | 0.1.7, Rust clone of SOPS | Young |
| vals | Go CLI | Not a Rust library |
| Dapr | Needs a sidecar | Doesn't fit Lambda |
| Infisical | Needs a server | Doesn't fit |
| OpenDAL | No secrets service | Not an option |

**Monthly cost** for 10 secrets across 3 apps and 1,000 cold starts:

| Option | Cost |
|---|---|
| age blob in the bucket (10k GETs) | **$0.004** |
| SSM Parameter Store standard (free; 40 TPS default limit; 4 KB values) | **$0** |
| AWS Secrets Manager ($0.40 per secret) | **$4.05** ($1.22 with one JSON secret per app) |
| GCP Secret Manager | **about $0.24** |
| Azure Key Vault Standard | **$0.03** |
| KMS customer-managed key | **+$1.00** |
| Lambda environment variables (AWS-managed key, free, 4 KB total) | **$0** |
