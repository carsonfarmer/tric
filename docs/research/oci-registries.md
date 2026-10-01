# OCI registries for Wasm components (fact sheet)

> Gathered 2026-10-01 by a research sub-agent from the OCI specs, AWS / Google / Azure / Docker / GitHub docs, the AWS Price List API, the Azure Retail Prices API, crate manifests, GitHub repo metadata, and local read-only measurements. Items marked UNVERIFIED were not confirmed against a primary source.
> Measured latencies come from one laptop (Victoria BC) running a fresh `curl` per request, so each number includes cold DNS, TCP and TLS. They are indicative, not Lambda numbers. Anything labelled ESTIMATE is arithmetic or inference.
> No cloud resources were created. The only cloud calls were read-only (`ecr describe-registry`, `GetAuthorizationToken`) plus anonymous public pulls.

## Bottom line

**Recommendation: keep the bucket as the runtime source of truth, and add OCI only as an optional deploy-time front door (option b).**

| # | Point | Evidence |
|---|---|---|
| 1 | Yes, AWS supports it. ECR private accepts Wasm artifacts. So do GAR, ACR, GHCR and Docker Hub. A Wasm component is a standard OCI artifact (one `application/wasm` layer), and Spin, SpinKube, wasmCloud, Cosmonic and Akamai Functions all distribute that way. Fastly and Wasmer are the exceptions. | Sections 1, 3, 7 |
| 2 | Pulling from a registry at runtime adds round trips and per-cloud auth code. A bucket read is 1 request via `object_store`. A registry read is 2 to 4 sequential requests across 2 to 3 hostnames, plus a different token scheme on each cloud (ECR SigV4 token, GAR OAuth, ACR Entra token). | Sections 3, 4 |
| 3 | Money: ECR and GAR charge $0.10/GB-month against S3's $0.023. That is negligible at component sizes (20 MB is about $0.002/month). The real cost problem is ACR, which has a **fixed fee** of $0.1666/day (about $5/month) and breaks the idle goal of $0.05/month or less. The user's belief about ACR Basic is confirmed. | Section 3 |
| 4 | Public registries do not fit a shared Lambda host. Docker Hub allows 100 anonymous pulls per 6 h per IP, and Lambda egress IPs are shared. ECR Public allows 1 anonymous pull/s per region. GHCR publishes no pull limits (UNVERIFIED). | Section 3 |
| 5 | The cheap win is key naming. Store components at `blobs/sha256/<hex>`. That is the OCI image-layout path, and a Wasm layer's OCI digest is the sha256 of the raw `.wasm`. The bucket then already looks like OCI storage, and the registry-to-bucket copy becomes a pure copy. | Sections 5, 6 |
| 6 | Option (b) is about 30 lines of Rust (a prototype was run and works), or **zero code**: `oras cp --to-oci-layout <ref> ./layout` then `aws s3 sync layout/blobs s3://bucket/blobs`. Both were verified to produce the proposed keys, except the `s3 sync` step, which was not run (no cloud writes). | Section 5 |

**Decisions this suggests**

| When | Action |
|---|---|
| Now (zero cost) | Name bucket keys `blobs/sha256/<hex>` for components. Store release manifests as the exact bytes you hashed (see the gotcha in section 5). |
| Now, optional | Consider making the immutable release manifest a genuine OCI image manifest (`config` = app config JSON, `layers` = components with `application/wasm`, `artifactType` = a spinit type). It costs a few extra fields, and it makes options (b) and (d), and a later `spinit export`, nearly free. The cost is that the manifest schema is constrained to OCI descriptor shapes. |
| Fast follow | `spinit deploy --from oci://<ref>`: copy blobs by digest into the bucket, then continue the normal manifest-and-CAS flow. Gate `oci-client` behind a Cargo feature so the Lambda binary does not carry it (size impact not measured, UNVERIFIED). |
| Later, only on demand | Option (d), a read-only `/v2/` facade over the bucket, so `oras`, `wkg` and `wash` can pull the operator's components. |
| Avoid | Options (c) and (e): runtime pulls from a registry, or a registry as the primary store. |
| Revisit if | Components must be shared with other people's clusters; multi-region replication is wanted (ECR replication); or signed provenance (cosign / Notation) becomes a requirement. Q1 says trusted code, so none of these apply today. |

**One AWS-specific cross-check:** if the host ships as a Lambda *container image* rather than a zip, that image must live in ECR (general AWS rule, not re-checked this session). At roughly 100 MB (UNVERIFIED size) that is about $0.01/month, inside the 500 MB free tier for 12 months. This is a reason to prefer a zip deploy, not a reason to use ECR for components.

## 1. Wasm in OCI: formats

| Format | Manifest / config / layer media types | Producers | Consumers | Notes |
|---|---|---|---|---|
| CNCF TAG Runtime Wasm OCI artifact | `application/vnd.oci.image.manifest.v1+json`; config `application/vnd.wasm.config.v0+json`; one layer `application/wasm` | `wkg`, `wash oci push`, `oci-wasm`, `oras` | wasmCloud, `wkg`, Spin (accepts it), containerd shims | Layer digest = sha256 of the raw `.wasm`, so it can equal a bucket key. Verified live: `ghcr.io/webassembly/wasi/io:0.2.6` has this shape, so WASI interface packages themselves are published to GHCR. |
| Spin app | config `application/vnd.fermyon.spin.application.v1+config`; layers `application/vnd.wasm.content.layer.v1+wasm` (also accepts `application/wasm`) | `spin registry push` | `spin up --from`, SpinKube SpinApp `image`, containerd-shim-spin | Multi-layer: components, static assets, and the locked app config. |
| wasmCloud | Standard Wasm artifact | `wash oci push/pull` (built on the Rust oras / `oci-client` stack, reads Docker credentials) | wasmCloud hosts | Examples are on GHCR. |

[CNCF Wasm OCI artifact spec](https://tag-runtime.cncf.io/wgs/wasm/deliverables/wasm-oci-artifact/)

## 2. Rust crates

Counts are locked packages in a scratch crate, including tokio and reqwest.

| Crate | Version | What it is | Locked packages | Repo pushed / stars | Builds for `wasm32-wasip2` |
|---|---|---|---|---|---|
| `oci-client` | 0.18.0 | Pull/push client (oras-project/rust-oci-client); rustls by default | 213 (193 with `rustls-tls-no-provider`) | 2026-09-21 / 184 | No (aws-lc-sys needs a C sysroot; tokio net `compile_error`) |
| `oci-wasm` | 0.6.0 | Wasm-specific wrapper (bytecodealliance); wraps `oci-client` 0.17, pulls in `wit-component` 0.253 | 241 | 2026-07-16 / 19 | No |
| `wasm-pkg-client` | 0.16 | `wkg` library (bytecodealliance/wasm-pkg-tools); uses `docker_credential` | 283 | 2026-08-19 | No |

- The wasip2 failures do not matter. The host and CLI are native binaries.
- For spinit, `oci-client` alone is enough (pull manifest bytes and blobs). `oci-wasm` adds WIT/component handling that a byte copy does not need.
- [oras-project/rust-oci-client](https://github.com/oras-project/rust-oci-client), [bytecodealliance/rust-oci-wasm](https://github.com/bytecodealliance/rust-oci-wasm), [bytecodealliance/wasm-pkg-tools](https://github.com/bytecodealliance/wasm-pkg-tools)

## 3. Registries: price, auth, limits

Fixed monthly fees are flagged in their own column.

| Registry | Storage | Requests / transfer | Free tier | **Fixed monthly fee** | Runtime-pull auth | Limits that matter | Artifact support |
|---|---|---|---|---|---|---|---|
| **ECR private** | $0.10/GB-month (Price List API, us-east-1, effective 2025-11-01) | No per-request fee. Same-region transfer to Lambda is free. Image signing $0.02 per count. | 500 MB/month, first 12 months only | None | `GetAuthorizationToken` (SigV4, IAM `ecr:GetAuthorizationToken` on `*`) returns a 12 h Basic `AWS:<pw>` token. It must accompany every registry request. Alternative with no registry protocol: `BatchGetImage` + `GetDownloadUrlForLayer` over SigV4 (AWS says the latter "is not generally used by customers"). | Per region: GetAuthorizationToken 500/s, BatchGetImage 2,000/s, GetDownloadUrlForLayer 3,000/s, PutImage 10/s. Max layer 52,000 MiB. 100,000 images per repo. | Accepts any media type (AWS blog "OCI Artifact Support In Amazon ECR"). OCI 1.1 referrers since 2024-08-01. Layer blobs 307-redirect to S3. |
| **ECR Public** | 50 GB always free | 500 GB/month anonymous internet egress, 5 TB authenticated, free to AWS compute | 50 GB | None | Anonymous token from `public.ecr.aws` | Unauthenticated pulls **1/s per region, not adjustable**. Authenticated 10/s. Max layer 10,000 MiB. | Pull of `docker/library/hello-world` measured. Wasm push not tested (no pushes allowed). Public means anyone can pull the components. |
| **GAR** | $0.10/GiB-month ($0.000136986/GiB-hour) | No request charges listed. Same-location transfer free. Cross-region in US/Canada $0.01/GiB, Europe $0.02. Internet egress at Premium tier rates. | First 0.5 GiB | None | OAuth2 access token (ADC / metadata server) | 60,000 general requests/min per project per region; 18,000 writes/min | OCI image format in Docker repos. Wasm/arbitrary artifacts are not explicit in GAR docs, but the ORAS compatibility page lists GAR as supported (UNVERIFIED for Wasm specifically). |
| **ACR Basic** | **Fixed $0.1666/day (about $5.0 to $5.07/month), includes 10 GiB.** Extra storage $0.10/GB-month. Standard $0.6666/day (100 GiB), Premium $1.6666/day (500 GiB). | Not collected (UNVERIFIED) | None | **Yes: about $5/month idle.** Breaks the $0.05/month idle goal. | Entra ID / ACR token | Basic and Standard: 10,000 reads/min per registry (5,000 per identity); ListReferrers 500/min; "best-effort ... not backed by an SLA" | Arbitrary OCI artifacts and referrers via ORAS. Anonymous pull only on Standard and up. |
| **GHCR** | Free for public images. Private-package billing not collected (UNVERIFIED). | No pull limits found in GitHub docs. Secondary sources say unlimited pulls for public images (UNVERIFIED). | n/a | None | Anonymous token for public images (measured); PAT for private | Unpublished | Wasm artifacts verified on `ghcr.io/webassembly/wasi/io:0.2.6`. |
| **Docker Hub** | Free personal tier; other tiers not collected | Anonymous: **100 pulls per 6 h per IPv4 or IPv6 /64.** Personal authenticated 200 per 6 h. Pro/Team unlimited. | n/a | None on free | Bearer token | Lambda IPs are shared, so the anonymous limit is hit by other tenants' traffic | Listed as OCI-artifact capable on the ORAS compatibility page. |

Sources: [ECR pricing](https://aws.amazon.com/ecr/pricing/), [ECR quotas](https://docs.aws.amazon.com/AmazonECR/latest/userguide/service-quotas.html), [ECR Public quotas](https://docs.aws.amazon.com/AmazonECR/latest/public/public-service-quotas.html), [ECR auth](https://docs.aws.amazon.com/AmazonECR/latest/userguide/registry_auth.html), [ECR lifecycle policies](https://docs.aws.amazon.com/AmazonECR/latest/userguide/LifecyclePolicies.html), [GAR pricing](https://cloud.google.com/artifact-registry/pricing), [GAR quotas](https://docs.cloud.google.com/artifact-registry/quotas), [ACR pricing](https://azure.microsoft.com/en-us/pricing/details/container-registry/) (the page showed "$-"; the daily rate was confirmed with the [Azure Retail Prices API](https://prices.azure.com/api/retail/prices)), [ACR SKUs and limits](https://learn.microsoft.com/en-us/azure/container-registry/container-registry-skus), [Docker Hub pull limits](https://docs.docker.com/docker-hub/usage/pulls/), [ORAS compatible registries](https://oras.land/docs/compatible_oci_registries/).

**Storage cost at spinit sizes (ESTIMATE, arithmetic):** 20 MB of components is $0.002/month on ECR or GAR and $0.0005/month on S3 (list price, first 50 TB tier). Even 5 GB of retained releases is $0.50 on ECR vs $0.12 on S3. Storage price is not the deciding factor; fixed fees and extra code are.

## 4. Latency: one bucket GET vs a registry pull

Requests needed to fetch **one component whose digest is already known** (it is in the release manifest):

| Path | Sequential requests | Hostnames | Measured (laptop, cold connection per request) |
|---|---|---|---|
| Bucket GET via `object_store` (S3 / GCS / Blob) | **1** | 1 | Public S3 GET, us-west-2, 16 KB: **91 to 105 ms** total (n=5 to 6; one 207 ms run with 112 ms DNS). AWS documents small-object latency of "roughly 100 to 200 milliseconds" and at least 5,500 GET/s per prefix. |
| ECR private, API path: `GetDownloadUrlForLayer` (SigV4) then S3 presigned GET | 2 | 2 | Not measured (no repo). `GetAuthorizationToken` itself, SigV4 against us-west-2: **85 to 97 ms**; `DescribeRegistry` 116 ms. |
| ECR private, registry path, token cached: GET blob (307) then S3 | 2 | 2 | Not measured |
| ECR private, registry path, cold token: token, blob (307), S3 | 3 | 3 | Not measured |
| ECR private, by tag: token, manifest, blob (307), S3 | 4 | 3 | Not measured |
| ECR Public anonymous (hello-world, has an index so +1 manifest) | 5 | 2 (registry + CloudFront) | Token 64 to 71 ms; index manifest 127 to 161; platform manifest 121 to 149; layer 307 85 to 97; CloudFront 76 to 243. **Chain about 0.4 to 0.5 s.** |
| GHCR anonymous (`webassembly/wasi/io:0.2.6`, 16 KB) | 4 | 2 (`ghcr.io`, `pkg-containers.githubusercontent.com`) | Token 236 to 264 ms; manifest 260 to 299; blob 307 249 to 266; CDN 44 to 226. **Chain about 0.8 to 1.1 s** cold per request. With connection reuse, ESTIMATE about 0.5 to 0.6 s. |

What this means (mostly ESTIMATE):
- The registry API host is farther from the laptop than its CDN (GHCR API about 75 ms RTT vs CDN about 14 ms), which is why the GHCR chain is slow. From Lambda in the same region, round trips are far shorter.
- ESTIMATE: each extra hop adds roughly 10 to 30 ms in-region (new TLS connection plus service time). So a private-ECR pull costs about 20 to 90 ms more than a bucket GET, paid once per component per Lambda environment, which means on every cold start. Against a 500 ms p99 cold-start budget that is real but not fatal. **The ECR private pull path was never measured; do not quote these as measurements.**
- The latency penalty is not the strongest argument against registries on AWS. The strongest arguments are per-cloud auth code, the fixed ACR fee, public-registry limits, and the loss of "the bucket is the cluster".

## 5. Option (b): copy from a registry into the bucket at deploy time

**Zero-code path (verified up to the S3 step).** `oras` 1.3.2: `oras cp --to-oci-layout <ref> ./layout:<tag>` writes `blobs/sha256/<hex>` files. Those names match the proposed bucket keys exactly. `aws s3 sync layout/blobs s3://bucket/blobs` would then be a pure copy (not run; no cloud writes). `layout/index.json` is a single mutable object with the same contention problem as the state object, so do not copy it into the bucket.

**Prototype (about 30 lines; worked).** It used `oci-client` 0.18, `object_store` 0.13, `sha2` 0.11, `hex`, `serde_json`, `tokio` and `anyhow`, with `LocalFileSystem` standing in for the S3 `object_store`. Pulling `ghcr.io/webassembly/wasi/io:0.2.6` anonymously stored the manifest, config and layer. A second run was idempotent (`AlreadyExists` is swallowed), and all three stored blobs' sha256 equalled their key names.

```rust
use oci_client::{manifest::OciImageManifest, secrets::RegistryAuth, Client, Reference};
use object_store::{local::LocalFileSystem, path::Path, Error, ObjectStore, PutMode, PutOptions};
use sha2::{Digest, Sha256};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let r: Reference = std::env::args().nth(1).expect("oci ref").parse()?;
    let store = LocalFileSystem::new_with_prefix("bucket")?;
    let client = Client::default();
    let (raw, digest) = client.pull_manifest_raw(&r, &RegistryAuth::Anonymous, &["application/vnd.oci.image.manifest.v1+json"]).await?;
    let manifest: OciImageManifest = serde_json::from_slice(&raw)?;
    for d in std::iter::once(&manifest.config).chain(&manifest.layers) {
        let mut buf = Vec::new();
        client.pull_blob(&r, d, &mut buf).await?;
        anyhow::ensure!(format!("sha256:{}", hex::encode(Sha256::digest(&buf))) == d.digest, "digest mismatch");
        put_if_absent(&store, &d.digest, buf).await?;
    }
    put_if_absent(&store, &digest, raw.to_vec()).await?; // raw bytes: re-serialising changes the digest
    println!("copied {digest} ({} layers)", manifest.layers.len());
    Ok(())
}
async fn put_if_absent(s: &impl ObjectStore, digest: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
    let key = Path::from(format!("blobs/sha256/{}", digest.trim_start_matches("sha256:")));
    let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
    match s.put_opts(&key, bytes.into(), opts).await { Ok(_) | Err(Error::AlreadyExists { .. }) => Ok(()), Err(e) => Err(e.into()) }
}
```

| Gotcha | Detail |
|---|---|
| **Store manifest bytes verbatim** | Re-serialising `OciImageManifest` with serde produced a different sha256 (`0dd81b1c...` vs `183e9e14...`). Use `pull_manifest_raw` and store those bytes, or the digest no longer matches. |
| Verify digests | Hash each blob before the put, as above, so a registry cannot poison a content-addressed key. |
| Private registries | Auth adds roughly 10 lines (ESTIMATE, UNVERIFIED): read `~/.docker/config.json` or a credential helper with the `docker_credential` crate and map to `RegistryAuth::Basic`. ECR needs `aws ecr get-login-password` or the SDK token; not tried. |
| Config blob | Needed only to keep the artifact intact for re-export; the runtime reads only the layer. |
| Image indexes | A Wasm artifact has no platform index. If a ref resolves to an index, pick a manifest first; the prototype does not handle that. |
| Binary size | The CLI and the Lambda host are one binary (decisions Q18). Put `oci-client` behind a Cargo feature. The extra locked packages beyond what the host already uses were not counted (UNVERIFIED). |

## 6. Option (d): serving the bucket as an OCI registry

| Finding | Detail |
|---|---|
| A pull-only registry is conformant | In the OCI distribution spec, Pull is the only mandatory category; Push, Content Discovery and Content Management are optional. Registries MAY redirect any request (307 to a presigned URL is fine) and SHOULD support `Range`. If the referrers API returns 404, clients must fall back to the `sha256-<hex>` tag schema. |
| A plain static tree works for `oras` (verified) | Layout: `v2/<repo>/manifests/<tag>`, `v2/<repo>/manifests/sha256:<digest>`, `v2/<repo>/blobs/sha256:<hex>`, with the manifest served as `application/vnd.oci.image.manifest.v1+json`. Both `oras cp --from-plain-http` and `oras manifest fetch --plain-http` pulled it (oras 1.3.2). No `/v2/` ping was made, and no `Docker-Content-Digest` header was needed. skopeo, crane and docker behaviour is UNVERIFIED (not installed; docker would use the shared Podman VM, which was not touched). |
| Key-layout conflict | Registry blob paths use `sha256:<hex>` (colon); image layouts use `sha256/<hex>`. S3 has no symlinks. Pick one, or duplicate objects (doubles storage). Tags need duplicated manifest objects. |
| Private bucket | Registry clients do not SigV4-sign. Either make the bucket public-read (or CloudFront), or put a thin read-only `/v2/` handler in the host Lambda that GETs manifests from the bucket and 307s blobs to presigned URLs. ESTIMATE: 80 to 150 lines. |
| No push | There is no push over this facade, so publishing stays `spinit deploy` or `aws s3 sync`. |
| Value | Low for a single trusted operator, who already holds the bucket. It matters only if other systems (SpinKube, wasmCloud, `wkg`) should pull from spinit. |

**Existing registry-on-storage projects**

| Project | Shape | Repo pushed / stars | Fit |
|---|---|---|---|
| `distribution/distribution` | Reference registry, S3/GCS/Azure drivers, always-on process | 2026-10-01 / about 10.6k | Needs a server; not scale-to-zero |
| `project-zot/zot` | Registry with S3 backend, always-on | 2026-10-01 | Same |
| `cloudflare/serverless-registry` | Workers + R2, full push/pull, presigned transfers for large blobs, blobs under 1 MiB streamed by the Worker | 2026-09-11 / 1,461 | Cloudflare only; not AWS Lambda |
| `scality/static-oci-registry` | Read-only static, Go | 2026-10-01 / 0 stars (brand new) | Too new to rely on |
| `NicolasT/static-container-registry` | Read-only, nginx static | 2021-12-01 | Stale |
| `wasmdesk/ociapps` (`ociapps-static`) | Materialises an OCI layout into `v2/<repo>/manifests/<tag>` and `blobs/sha256:<hex>` on a plain static host (GitHub Pages, S3) | pre-release v0.0.0 | Matches the verified static-tree approach, but pre-release |
| `mcronce/oci-registry` | Pull-through cache with S3 | 2024-06 | Stale |
| ORAS blog (2023-10-14), "Lightweight Registry with Oras OCI-Layouts and Object Storage" | `--oci-layout` on s3fs / gcsfuse / azure-storage-fuse FUSE mounts. ORAS cannot read object storage directly. Last write wins; no delete. | n/a | A FUSE workaround, not usable on Lambda |

## 7. Precedents

| Project | How apps are distributed | Source quality |
|---|---|---|
| Spin / SpinKube | OCI artifacts: `spin registry push`, SpinApp `image`, containerd-shim-spin | Primary (Spin docs) |
| wasmCloud | Standard Wasm OCI artifact via `wash oci push/pull`, `wkg`, `oras` | Primary |
| Fermyon Wasm Functions / Akamai Functions | Packaged and stored in a managed OCI registry, then distributed to regions | Secondary: a dev.to article. Akamai techdocs only confirm version increments. |
| Cosmonic | OCI throughout. Its Artifact CRD "can be used to store OCI image contents in a NATS JetStream Object Store, reducing registry load" ([FAQ](https://cosmonic.com/docs/faq)) | Primary. This is a precedent for option (b): registry for ingest, local object store at runtime. |
| Fastly Compute | Uploads a `tar.gz` package containing `bin/main.wasm`; no OCI | Primary |
| Wasmer | Own registry (`wasmer.toml`, `wasmer publish`, registry.wasmer.io); no OCI found | Primary |
| Microsoft Open Source blog, 2024-09-25 | "Distributing WebAssembly components using OCI registries" (ACR, GHCR, Docker Hub, `wkg`) | Primary |

OCI is the de facto distribution format for Wasm components, but nobody uses it as the runtime cache. Cosmonic copies out of it.

## 8. Options

| Option | Extra custom code | Idle cost / 1M req | Cold start | Cloud-agnostic | "Bucket is the cluster" | Verdict |
|---|---|---|---|---|---|---|
| **(a) Bucket only** | None (baseline) | S3 storage only; $0.0004 per 1k GETs | 1 GET per component | Yes (`object_store`) | Yes | Core design |
| **(b) `deploy` accepts an OCI ref and copies blobs into the bucket** | About 30 lines, or 0 with `oras` + `s3 sync`; about +10 for private auth (UNVERIFIED) | Same as (a); the registry is touched at deploy time only | Same as (a) | Yes; works with any registry `oci-client` can reach | Yes | **Add as optional front door** |
| (c) Runtime pulls from a registry, bucket holds state only | Per-cloud auth and an `oci-client` call in the hot path; ECR token refresh | ECR $0.10/GB-month; ACR about $5/month fixed | +2 to 4 hops, ESTIMATE +20 to 90 ms per cold component | Weak: each cloud has a different token scheme | No (cluster = bucket + registry) | Avoid |
| (d) Bucket served as a read-only OCI registry | 0 for a public static tree; ESTIMATE 80 to 150 lines for a private `/v2/` handler | Same as (a) | Same as (a) | Yes | Yes | Optional, low priority. Pull only; no push. |
| (e) Registry as primary store, bucket for state | (c) plus registry push in `deploy`, plus a registry in each OpenTofu module | As (c) | As (c) | Weak | No | Avoid |

**Releases, rollback and GC**

| Option | Releases | Rollback | GC |
|---|---|---|---|
| (a) | Immutable manifest per release (by digest) plus the CAS state object | Repoint the state object to an older manifest digest. No data movement. | No refcounts. Either never delete (ESTIMATE: a 5 MB component across 1,000 releases is 5 GB, about $0.12/month on S3) or mark-and-sweep from reachable manifests (list plus delete). Deleting a blob a manifest still references is the one unsafe act. |
| (b) | Same as (a); the OCI ref is recorded as an annotation | Same as (a) | Same as (a). The registry's copy is independent, so a bucket sweep never affects it. |
| (c) / (e) | Registry tags or digests; the state object records the image ref. Tags are mutable unless immutability is enabled, so pin by digest. | Repoint to an older digest | Registry-native. ECR lifecycle policies are documented, including referrer cleanup within 24 h. GAR cleanup and ACR retention policies were not checked. This is the registry's only real advantage. |
| (d) | Same as (a); tags in the facade are extra duplicated manifests | Same as (a) | Same as (a), plus removing duplicated tag objects |

## 9. UNVERIFIED and not measured

| Item | Status |
|---|---|
| ECR private end-to-end pull latency (and everything from Lambda in-region) | Not measured. No repo was created. All hop costs are ESTIMATES. |
| Lambda container image rule (image must be in ECR) and ECR cost of a 100 MB host image | General AWS knowledge, not re-checked here. |
| GHCR pull limits | None found in official docs; "unlimited for public" comes from secondary sources. |
| GHCR private-package and Docker Hub paid-tier billing | Not collected. |
| Fermyon / Akamai "managed OCI registry" | One secondary article. |
| skopeo, crane and docker against a static `/v2/` tree | Not tested. Only `oras` 1.3.2 was. |
| GAR with Wasm artifacts specifically; ACR transfer/request pricing | GAR docs are silent; ACR not collected. |
| Auth line count (+10) and facade size (80 to 150 lines) | ESTIMATES. |
| Binary-size impact of `oci-client` in the Lambda binary | Not measured. |
| Source URLs | Rebuilt after a context reset. Titles, dates and numbers are accurate; re-open a link before quoting it. The ECR "any media type" statement is in the AWS Containers blog post "OCI Artifact Support In Amazon ECR". |
