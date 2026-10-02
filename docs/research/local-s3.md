# Local S3-compatible server for integration tests (fact sheet)

> Gathered 2026-10-01 by a research sub-agent from GitHub repo metadata and release lists, Docker Hub, Homebrew, project docs and source greps, and from running the candidates in Docker.
> All runs: Docker Desktop on one Apple-silicon laptop (linux/arm64), one node per server, throwaway credentials, no host ports (containers talk over a private network). Client versions: `amazon/aws-cli` 2.37.8 and `object_store` =0.14.2 (the real client the project uses). Items marked UNTESTED were surveyed only.
> Harness: [`local-s3-poc/`](local-s3-poc/). Nothing was committed.

## Bottom line

**Recommendation: use versitygw v1.8.0 or newer (`versity/versitygw`) as the default local server, with SeaweedFS as the fallback.** RustFS is equally correct but is the youngest.

| # | Point | Evidence |
|---|---|---|
| 1 | Three servers pass every requirement, through both `aws-cli` and `object_store` 0.14.2, **including 20 rounds of 32 concurrent writers racing on one key** (exactly 1 winner every round): versitygw, RustFS, SeaweedFS. | Section 2 |
| 2 | Garage silently ignores `If-None-Match` and `If-Match` on PUT (200, overwrite). Adobe S3Mock honours them sequentially but is **not atomic** under concurrency (18/20 create rounds and 20/20 CAS rounds had more than one winner). Neither is usable for CAS tests. | Section 2 |
| 3 | LocalStack and MinIO are out. The LocalStack image now exits with code 55 without `LOCALSTACK_AUTH_TOKEN`, and its OSS repo is archived. MinIO is archived (last release `RELEASE.2025-10-15`) and AGPL. | Section 1 |
| 4 | versitygw wins on setup and fidelity: smallest image (93 MB vs 355 and 687), one process, one `docker run`, Apache-2.0, a Homebrew formula and a Darwin arm64 tarball, and it matches real AWS on every check (including `404` for `If-Match` on a missing key). | Sections 1, 2 |
| 5 | Pin versitygw to `>= 1.8.0`. That release is where its release notes fix atomic per-key conditional PUT. Earlier versions were not tested. | Section 1 |

## 1. Candidates

| Candidate | Licence | Latest release (maintained?) | Official image | Native install | Conditional PUT / GET 304 / list | Result |
|---|---|---|---|---|---|---|
| **versitygw** | Apache-2.0 | v1.8.0, 2026-09-04 (yes) | `versity/versitygw:v1.8.0` (93 MB) | Homebrew formula; Darwin arm64 tarball on GitHub releases | 1.8.0 notes fix atomic per-key conditional PUT | **Pass, atomic** |
| **RustFS** | Apache-2.0 | 1.0.0, 2026-09-16, daily previews (yes, but young) | `rustfs/rustfs:1.0.0` (355 MB) | Homebrew tap `rustfs/homebrew-tap` (tracks previews, not 1.0.0); macOS aarch64 zip on releases | Earlier "no conditional writes" issues (#791, #1458) are closed | **Pass, atomic** |
| **SeaweedFS** | Apache-2.0 | 4.48, 2026-09-28 (yes, mature) | `chrislusf/seaweedfs:4.48` (687 MB; default `weed mini`) | Homebrew formula (4.47); `weed` darwin_arm64 tarball | Conditional PUT in S3 gateway | **Pass, atomic** (one deviation, below) |
| Garage | AGPL-3.0 | v2.4.1, 2026-09-08 (yes) | `dxflrs/garage:v2.4.1` (95 MB) | Homebrew formula | Docs list "no conditional writes" as a known gap | **Fail** (ignores preconditions) |
| Adobe S3Mock | Apache-2.0 | 5.2.3, 2026-09-19 (yes) | `adobe/s3mock:5.2.3` (223 MB) | None (JVM jar / Docker / Testcontainers only) | Implements conditional PUT and GET | **Fail** (not atomic) |
| LocalStack | Apache-2.0 (archived OSS code); unified image is proprietary | OSS repo archived, last OSS v4.14.0, 2026-02-26; image `latest` is 2026.8.5 | `localstack/localstack` (1.9 GB) | Homebrew formula deprecated | n/a | **Excluded**: exits 55, "License activation failed", needs `LOCALSTACK_AUTH_TOKEN` |
| MinIO | AGPL-3.0 | Archived 2026-04-25, last `RELEASE.2025-10-15` | None maintained | None maintained | n/a | **Excluded** (the premise of this survey) |
| pgsty/minio (community fork) | AGPL-3.0 | `RELEASE.2026-09-16` | Docker, RPM, DEB | RPM / DEB | UNTESTED | AGPL; UNTESTED |
| MiniStack | MIT | v1.5.20, 2026-10-01 | Docker | not checked | Changelog: conditional PUT since 1.3.46 | UNTESTED |
| moto server | Apache-2.0 | 5.2.3; image pushed 2026-08-22 | `motoserver/moto` | `pip install 'moto[server]'` | Source: `If-Match` handled, `If-None-Match` only for `*`; no atomicity guarantee found | UNTESTED, likely not atomic |
| Zenko CloudServer | Apache-2.0 | 9.3.22 | Docker Hub `zenko/cloudserver` last pushed 2023-01-10 | from source (Node) | No `PutObject` conditional evidence found | UNTESTED |
| s3proxy | Apache-2.0 | 4.1.1; image pushed 2026-10-02 | `andrewgaul/s3proxy` | jar | Depends on the backend | UNTESTED |
| Ceph RGW | LGPL-2.1 / 3.0 | Active | Large multi-daemon stack | Linux packages | Supported upstream | Not tried: too heavy for a test fixture |

## 2. Empirical results

Server start commands (container names are prefixed `torpor-s3check-`; network `torpor-s3check-net`; creds `torpor` / `torpor-secret`, throwaway):

```sh
D=/opt/homebrew/bin/docker
$D network create torpor-s3check-net

# versitygw (posix backend on tmpfs; the bucket must be created afterwards)
$D run -d --name torpor-s3check-versitygw --network torpor-s3check-net --tmpfs /data \
  -e ROOT_ACCESS_KEY=torpor -e ROOT_SECRET_KEY=torpor-secret \
  versity/versitygw:v1.8.0 --port :7070 posix /data            # http://torpor-s3check-versitygw:7070

# RustFS
$D run -d --name torpor-s3check-rustfs --network torpor-s3check-net \
  -e RUSTFS_ACCESS_KEY=torpor -e RUSTFS_SECRET_KEY=torpor-secret \
  rustfs/rustfs:1.0.0                                          # http://torpor-s3check-rustfs:9000

# SeaweedFS (image default CMD is `mini -dir=/data`; S3_BUCKET pre-creates the bucket)
$D run -d --name torpor-s3check-seaweedfs --network torpor-s3check-net \
  -e AWS_ACCESS_KEY_ID=torpor -e AWS_SECRET_ACCESS_KEY=torpor-secret -e S3_BUCKET=chk \
  chrislusf/seaweedfs:4.48                                     # http://torpor-s3check-seaweedfs:8333

# Adobe S3Mock (no auth)
$D run -d --name torpor-s3check-s3mock --network torpor-s3check-net adobe/s3mock:5.2.3   # :9090
```

Garage needs a layout and a key imported through its CLI first (`garage layout assign/apply`, `garage key import`, `garage bucket create`, `garage bucket allow`) and a `garage.toml`; its throwaway key is not reproduced here.

### 2a. aws-cli checks (exact commands)

`local-s3-poc/awscheck.sh <endpoint> <bucket>` runs these inside `amazon/aws-cli` and prints the HTTP status of each call. The bucket is created first with `aws --endpoint-url $EP s3api create-bucket --bucket chk`.

```sh
D=/opt/homebrew/bin/docker; cd docs/research/local-s3-poc
$D run --rm --name torpor-s3check-mkb --network torpor-s3check-net \
  -e AWS_ACCESS_KEY_ID=torpor -e AWS_SECRET_ACCESS_KEY=torpor-secret -e AWS_DEFAULT_REGION=us-east-1 \
  amazon/aws-cli --endpoint-url http://torpor-s3check-versitygw:7070 s3api create-bucket --bucket chk
$D run --rm --name torpor-s3check-cli --network torpor-s3check-net -v "$PWD:/w:ro" \
  -e AWS_ACCESS_KEY_ID=torpor -e AWS_SECRET_ACCESS_KEY=torpor-secret -e AWS_DEFAULT_REGION=us-east-1 \
  --entrypoint sh amazon/aws-cli /w/awscheck.sh http://torpor-s3check-versitygw:7070 chk
```

The individual calls (`$EP` is the endpoint, `$E1`/`$E3` are ETags read with `s3api head-object --query ETag --output text`):

```sh
aws --endpoint-url $EP s3api put-object --bucket chk --key cas/k --body v1 --if-none-match '*'      # 1a: 200
aws --endpoint-url $EP s3api put-object --bucket chk --key cas/k --body v2 --if-none-match '*'      # 1b: 412, etag unchanged
aws --endpoint-url $EP s3api put-object --bucket chk --key cas/k --body v3 --if-match "$E1"         # 2a: 200, new etag
aws --endpoint-url $EP s3api put-object --bucket chk --key cas/k --body v2 --if-match "$E1"         # 2b: 412 (stale), etag unchanged
aws --endpoint-url $EP s3api put-object --bucket chk --key cas/missing --body v2 --if-match "$E1"   # 2c: AWS returns 404
aws --endpoint-url $EP s3api get-object --bucket chk --key cas/k --if-none-match "$E3" out          # 3a: 304
aws --endpoint-url $EP s3api get-object --bucket chk --key cas/k --if-none-match '"deadbeef"' out   # 3b: 200
aws --endpoint-url $EP s3api list-objects-v2 --bucket chk --prefix list/ --max-keys 10 \
  [--continuation-token <NextContinuationToken>]                                                    # 4: 25 keys -> 3 pages
```

| Check | versitygw 1.8.0 | RustFS 1.0.0 | SeaweedFS 4.48 | S3Mock 5.2.3 | Garage 2.4.1 |
|---|---|---|---|---|---|
| 1a create new key | 200 | 200 | 200 | 200 | 200 |
| 1b create existing (`If-None-Match: *`) | 412 | 412 | 412 | 412 | **200 (overwritten)** |
| 2a `If-Match` current etag | 200 | 200 | 200 | 200 | 200 |
| 2b `If-Match` stale etag | 412 | 412 | 412 | 412 | **200 (overwritten)** |
| 2c `If-Match` on missing key (AWS: 404) | 404 | 404 | **412** | 404 | **200 (created)** |
| 3a GET `If-None-Match` current etag | 304 | 304 | 304 | 304 | 304 (see note) |
| 3b GET `If-None-Match` other etag | 200 | 200 | 200 | 200 | 200 |
| 4 ListObjectsV2, 25 keys, page size 10 | 3 pages, 25 unique, sorted | same | same | same | same |

Note on Garage 3a: the scripted check showed a 200 only because step 1b had silently overwritten the object, so the ETag it used was stale. Re-run directly against the current ETag it returns 304.

### 2b. object_store 0.14.2 harness (exact commands)

`local-s3-poc/s3check-rs` drives the real client: `PutMode::Create` (sends `If-None-Match: *`), `PutMode::Update(UpdateVersion { e_tag })` (sends `If-Match`), `GetOptions { if_none_match }`, a paginated `list` of 2,500 keys plus `list_with_offset`, and the concurrency race below. Endpoint settings: `with_allow_http(true)`, region `us-east-1`, path-style addressing (the default when an endpoint is set).

```sh
D=/opt/homebrew/bin/docker; cd docs/research/local-s3-poc
mkdir -p target
$D run --rm --name torpor-s3check-rust-build -v "$PWD:/work" -w /work/s3check-rs \
  -e CARGO_HOME=/work/target/cargo-home -e CARGO_TARGET_DIR=/work/target rust:1.97.1-bookworm cargo build
./rscheck.sh http://torpor-s3check-versitygw:7070 out.txt    # creates a fresh bucket, runs the harness
./rscheck.sh http://torpor-s3check-rustfs:9000 out.txt
./rscheck.sh http://torpor-s3check-seaweedfs:8333 out.txt
```

| Harness section | versitygw | RustFS | SeaweedFS | S3Mock | Garage |
|---|---|---|---|---|---|
| 1 `PutMode::Create` on existing key -> `AlreadyExists`, body unchanged | pass | pass | pass | pass | fail |
| 2 `PutMode::Update` stale etag -> `Precondition`, body unchanged; missing key -> `Precondition` | pass | pass | pass | pass | fail |
| 3 GET `if_none_match` current etag -> `NotModified`; other etag -> 200 | pass | pass | pass | pass | fail (stale etag after the silent overwrite; direct check returns 304) |
| 4 list 2,500 keys paginated, none lost or duplicated; `list_with_offset` | pass | pass | pass | pass | pass |
| 5a 20 rounds x 32 concurrent `Create` on one key: exactly 1 winner | **0/20 wrong** | **0/20 wrong** | **0/20 wrong** | 18/20 wrong | 20/20 wrong (32 winners) |
| 5b 20 rounds x 32 concurrent `Update` from the same etag: exactly 1 winner | **0/20 wrong** | **0/20 wrong** | **0/20 wrong** | 20/20 wrong | 20/20 wrong (32 winners) |

Section 5 matters most: a server that passes the sequential checks but is not atomic (S3Mock) would pass a naive test suite and then hide a lost-update bug.

### 2c. Cleanup

Only `torpor-s3check-*` containers, the `torpor-s3check-net` network and four anonymous volumes created by those containers were removed. The three pre-existing containers (`object-log-ci-diagnostic`, two `spincast-showcase-minio-*`) were not touched, and no Docker machine setting was changed. Pulled images (`amazon/aws-cli`, `rust:1.97.1-bookworm`, `rustfs`, `seaweedfs`, `versitygw`, `garage`, `s3mock`, `localstack`) were left in place.

## 3. Recommendation

| Criterion | versitygw | RustFS | SeaweedFS |
|---|---|---|---|
| Developer setup | One `docker run` or `brew install versitygw`; 93 MB image; one process; posix backend on a directory or tmpfs; bucket created by one `create-bucket` call | One `docker run`; 355 MB; single binary; Homebrew tap tracks previews, so prefer the Docker `1.0.0` tag | One `docker run` (default `weed mini`); 687 MB; several internal roles (master, volume, filer, S3); `S3_BUCKET` pre-creates the bucket |
| Licence | Apache-2.0 | Apache-2.0 | Apache-2.0 |
| S3 fidelity on the four requirements | Matches AWS on every check, including `404` on `If-Match` for a missing key | Same | `412` instead of `404` for `If-Match` on a missing key; `object_store` maps both to `Precondition`, so it is invisible to this client |
| Atomic under races | Yes (fix in 1.8.0) | Yes | Yes |
| Maturity risk | v1.x, backed by Versity; the atomicity fix is only a month old | 1.0.0 is two weeks old; earlier versions lacked conditional writes | Oldest and most widely used |

**Pick versitygw `>= v1.8.0`.** It has the least setup, the smallest image, a permissive licence, and it reproduces AWS behaviour exactly on every check, including the concurrency race. Run it in the repo's test compose file with `--tmpfs` so each run starts empty. Keep SeaweedFS as the second opinion if versitygw ever regresses (it is the most battle-tested and its only deviation is harmless for `object_store`). Re-evaluate RustFS in a few months once 1.x has had time to settle.

Do not use Garage (AGPL, ignores preconditions), S3Mock (non-atomic), LocalStack (token-gated) or MinIO (archived, AGPL) for this.

**Caveats**

- Real S3 is still the reference. A local server proves protocol conformance for these four behaviours on one node, not S3's eventual consistency, throttling or multi-region behaviour. Keep one smoke run against a real bucket.
- Each server was run once per check on one node. The race test (640 racing writers per server) is strong evidence, not proof, of atomicity.
- Not tested: versitygw below 1.8.0, RustFS previews, SeaweedFS in multi-filer mode, pgsty/minio, MiniStack, moto, CloudServer, s3proxy, Ceph.
- The test credentials above are throwaway values for a local container. They are not secrets.
