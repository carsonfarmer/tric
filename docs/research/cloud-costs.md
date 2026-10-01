# Cloud costs and limits (fact sheet)

> Gathered 2026-09-30 by a research sub-agent from official pricing pages, the AWS Price List API, and docs.
> Default regions: us-east-1 / us-central1 / eastus. Items marked UNVERIFIED were not confirmed against a primary source.

## AWS

**S3 Standard requests**
- PUT/COPY/POST/LIST: $0.005 per 1k. GET: $0.0004 per 1k. LIST is 12.5× a GET.

**S3 conditional writes**
- `If-None-Match: *` (Aug 2024) and `If-Match` (Nov 2024) work on PutObject and CompleteMultipartUpload.
- A failed condition returns 412; `409 ConditionalRequestConflict` means retry. ([PutObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html))
- Failed 412/409 requests are billed at normal request rates. No URL was captured for this; re-check before relying on it.
- Rate limits are per prefix: 3,500 PUT/s and 5,500 GET/s. No per-key write limit is documented (UNVERIFIED).

**S3 Express One Zone**
- PUT/LIST: $0.00113 per 1k. GET: $0.00003 per 1k.
- Data: $0.0032/GB uploaded, $0.0006/GB retrieved, charged on all bytes.
- Storage: $0.11/GB-mo. These prices include the April 2025 cuts.
- Append uses `x-amz-write-offset-bytes`, which only Express supports.
- Single AZ. No S3 Event Notifications.
- Express is cheaper per PUT only for objects under about 1.2 MB.
- UNVERIFIED: conditional writes on directory buckets; append combined with If-Match; latency and durability figures.

**Lambda**
- arm64: $0.0000133334 per GB-s and $0.20 per 1M requests.
- The free-tier rows (1M requests and 400k GB-s per month) are still in the Price List. Whether they apply to accounts created after the July 2025 free-tier change is inferred, not confirmed.
- Function URLs cost nothing extra. /tmp holds up to 10,240 MB.
- Response streaming works on custom runtimes; streamed bytes cost $0.008/GiB.
- SnapStart is **not** available for `provided.al2023`.
- **Lambda Managed Instances** run on EC2 with multiple concurrent requests per environment. They do **not** scale to zero.
- **Lambda MicroVMs** (June 2026): $0.0000276944 per vCPU-s and $0.0000036667 per GB-s (arm), up to 8 hours. Free tier and semantics are UNVERIFIED.

**CloudWatch Logs**
- Ingestion costs $0.50/GB, and the first 5 GB per month is free.
- Lambda has no documented setting to turn logging off. The practical way is to not grant the function the `logs:*` permissions.

**CloudFront**
- Always free on pay-as-you-go: 10M requests and 1 TB per month. A separate flat-rate Free plan allows 1M requests and 100 GB.
- Origin access control (OAC) works with Lambda Function URLs.
- Traffic from S3 to CloudFront is free. Traffic from S3 to the internet costs $0.09/GB after the first 100 GB.

**Events**
- S3 event notifications are delivered at least once. There is no feature fee; you pay only for the Lambda invocation.
- EventBridge Scheduler: 14M invocations per month free, then $1 per 1M.

## GCP

**Cloud Run**
- Free tier with request-based billing: 2M requests, 180k vCPU-s and 360k GiB-s per month.
- Per-instance concurrency goes up to 1,000, and services scale to zero.
- Cold start for a small Rust container is UNVERIFIED.

**GCS**
- Class A: $0.005 per 1k. Class B: $0.0004 per 1k.
- Always-free tier: 5k Class A and 50k Class B operations per month.
- Preconditions: `ifGenerationMatch=0` creates only if absent; `=N` is compare-and-swap.
- **Failed preconditions (4xx) are generally not billed.**
- **Limit: 1 write per second to the same object name.**

## Azure

**Functions**
- Flex Consumption: 250k executions and 100k GB-s per month free. Per-instance HTTP concurrency defaults to 4–32, and it scales to zero.
- Custom handlers (Rust) work on Linux only.
- Linux Consumption retires on 2028-09-30.

**Blob storage**
- Writes: $0.005 per 1k. Reads: $0.0004 per 1k (Hot LRS).
- `If-Match` and `If-None-Match: *` are supported.
- Append Block takes `x-ms-blob-condition-appendpos`, which gives compare-and-swap append (offset compare). Limit: 50k appends per blob.
- **Failed conditional requests are billed.**
- No always-free tier for blob storage.

## Cloudflare (comparison only)

**Workers**
- Free plan: 100k requests per day and 10 ms of CPU per request.
- Paid plan: $5/month includes 10M requests.
- WASI is "experimental". The docs don't mention the component model, so components would need JS glue such as jco.

**R2**
- Class A: $4.50 per 1M. Class B: $0.36 per 1M. Egress is free.
- Free tier: 10 GB, 1M Class A and 10M Class B operations per month.
- Supports `If-Match` and `If-None-Match: *`.
- **Limit: 1 write per second to the same key** (429 above it).
- Whether a failed condition (412) is billed is UNVERIFIED.

## Reference load: 1M reads + 100k writes per month

Assumes 1 GET per read and 1 PUT per write. Compute fits inside the free tier on every option.

| Stack | Monthly | With 2 PUTs per write (segment + pointer flip) |
|---|---|---|
| S3 + Lambda | ~$0.90 | ~$1.40 |
| GCS + Cloud Run | ~$0.86 | ~$1.33 |
| Azure Blob + Flex | ~$0.90 + ~$0.30 execution | — |
| R2 + Workers | $0 | $0 |

Takeaways:
- PUTs cost about $5 per 1M on all three big clouds. How many writes you batch into one PUT matters more than which provider you pick.
- GCS and R2 limit writes to one per second per object name. That rules out updating a single pointer object on every commit.
