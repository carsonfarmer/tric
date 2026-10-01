# Initial thoughts: SpinKube without Kubernetes

> Brainstorm notes from a claude.ai conversation. Nothing here is decided or validated.
> The ideas below were suggested by Claude, not agreed on by the user. Treat every point as a hypothesis to challenge.

## The ask (user's words)
"i need to find a way to build something like https://www.spinkube.dev, but without kubernetes, and using almost zero always on compute. like, it has to be almost free to run this on aws, azure, or gcp. i don't care which. it should essentially leverage object storage for all state, probably some sort of wal. it has to be so flippin' cheap it's wild."

Follow-up: "you can assume a wasm runtime for all 'components' if that helps at all"

## Stated constraints
- Spin/Wasm workloads, SpinKube-like capabilities
- No Kubernetes
- Near-zero always-on compute; nearly free on AWS, Azure, or GCP (no cloud preference)
- Object storage holds all state, probably via a WAL
- Every component can assume a Wasm runtime

## Idea 1: The bucket is the cluster
- Replace the control plane (etcd, operator, scheduler) with objects.
- Deploy = upload content-addressed components + a manifest, then flip an `apps/<name>/HEAD` pointer with a conditional write. Rollback = flip it back.
- Relies on object-store conditional writes: S3 added put-if-absent (Aug 2024) and If-Match (Nov 2024); GCS and Azure Blob already had equivalents.

## Idea 2: One generic function hosts every app
- A single Lambda (arm64, Rust + Wasmtime) behind a Function URL, routing by Host header.
- Components cached in /tmp by content hash (immutable, never invalidated).
- AOT-compile to native on first load and write it back to the bucket for later cold starts.
- Wizer-style pre-initialization baked in at deploy time.
- Wasm isolation lets many tenants share one warm sandbox.

## Idea 3: WAL as numbered put-if-absent objects
- Each segment `wal/<seq>` is created with If-None-Match: *. The winner owns the slot, giving a total order with no leader.
- Find the tail by probing GET seq+1 until 404 rather than LIST (LIST costs ~12x a GET).
- Shard logs by key hash to reduce contention.
- SQLite variant: snapshot + WAL frames in the bucket; single writer via an epoch lease (CAS).
- Compaction triggered by the data itself: the writer landing on every Nth segment drops a marker, an S3 event fires a compactor, which merges into SSTs (SlateDB-style LSM) and CAS-bumps a manifest.

## Idea 4: Tiny host, everything else a component
- Host imports limited to: blob get / put-if-absent / put-if-match / list, outbound HTTP, clock, random.
- Router, WAL, KV engine, SQLite VFS, compactor, and deployer are all components. Upgrading the platform = uploading a component.
- Storage engines composed with each app at deploy time (e.g. `wac`) behind `wasi:keyvalue` / SQLite interfaces.

## Idea 5: Determinism enables serializable transactions (OCC)
- Execute against log position N, record the read set, buffer writes and outbound side effects (outbox).
- Commit by creating `wal/N+1` with put-if-absent.
- On conflict: if intervening segments don't touch the read set, retry at the next slot without re-executing; otherwise re-execute.
- Respond and release the outbox only after commit.
- Side benefit: logging host-call results enables bit-for-bit replay of production requests.

## Idea 6: Fuel metering as hard cost ceilings
- Wasmtime fuel/epoch limits per request and per tenant, so runaway code can't run up a bill.

## Cost notes (rough, unverified)
- Lambda always-free tier: 1M requests + 400,000 GB-s/month. 1M requests at 128MB × 20ms ≈ 2,500 GB-s.
- Main variable cost is object-store PUTs (~$5/million on S3 Standard). Mitigations floated: at most one segment per request, S3 Express One Zone for a hot WAL (cheaper requests, appends, single-AZ), CDN for cacheable routes.
- Hidden costs to avoid: API Gateway, VPC/NAT Gateway, CloudWatch Logs ingestion.
- Guess: under $1/month at hobby scale, cents when idle.

## Wilder ideas (lower confidence)
- Cloud Run instead of Lambda: per-instance request concurrency allows true group commit (many writes per PUT).
- State as Wasm linear memory: deterministic actors, snapshot memory + replay an input log ("Durable Objects on a bucket"). Breaks Spin's stateless-per-request model.
- Stack free tiers across clouds for stateless routes (cross-cloud egress caveat).
- Run read-only components in the user's browser (via `jco`) against public or presigned objects.

## Known weak spots / open questions
- Commit latency: tens of ms on S3 Standard; per-log throughput ceiling under contention.
- Lambda serves one request per sandbox, so no group commit there.
- Are failed conditional PUTs (412) billed?
- Cold reads hit the bucket; acceptable staleness for cached HEAD/manifest?
- Cross-shard transactions.
- Spin compatibility: embed Spin's runtime crates vs. implement its WIT worlds directly.
- Isolation: shared multi-tenant function vs. one function per tenant.
- Is single-AZ Express One Zone acceptable for a WAL?
- Single-region only.
- Which cloud? Leaning AWS, not decided.
- What workloads/scale is this actually for? Not yet discussed.
