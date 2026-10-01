# Comparables and positioning (fact sheet)

> A research sub-agent gathered this on 2026-09-30 from GitHub (API star and release counts as of that day), HN, and official docs and pricing pages.
> UNVERIFIED marks claims that weren't confirmed against a primary source.

**Bottom line:** nothing combines all five properties: self-hosted in your own account, scale-to-zero, Wasm components, a bucket-only control plane, and Spin/WASI compatibility. The wedge is real but narrow.

## Comparables

| Project | What / overlap | Scale-to-zero, hobby cost | Own cloud account | Components (p2/p3) | Maturity |
|---|---|---|---|---|---|
| [Cloudflare Workers](https://developers.cloudflare.com/workers/platform/pricing/) | Hosted V8 FaaS | Yes; 100k req/day free, $5/mo paid | No | No: WASI experimental, modules only ([docs](https://developers.cloudflare.com/workers/runtime-apis/webassembly/)) | GA |
| [Akamai Functions](https://techdocs.akamai.com/akamai-functions/docs/quotas-and-limits) (ex-Fermyon) | Hosted Spin PaaS | Pricing and free tier unpublished (UNVERIFIED); 128 MiB, 30 s, KV 50 write RPS | No | Spin apps; p3 UNVERIFIED | Public preview, onboarding form ([page](https://www.akamai.com/products/akamai-functions)) |
| [Fastly Compute](https://www.fastly.com/pricing/) | Hosted Wasm edge | 10M req/mo free | No | No timeline as of 2025-07 ([thread](https://community.fastly.com/t/wasm-support-for-component-model-wasip2/3642)); 2026 UNVERIFIED | GA |
| [Wasmer Edge](https://wasmer.io/pricing) | Hosted, WASIX | 1M req free, Pro $10/mo | No | UNVERIFIED | [Wasmer](https://github.com/wasmerio/wasmer) v7.4.2, 21.1k stars |
| Deno Deploy, Vercel, Supabase, Netlify | JS-first hosts | Free: [1M](https://deno.com/deploy/pricing), [1M](https://vercel.com/docs/functions/usage-and-pricing), [500k](https://supabase.com/docs/guides/functions/pricing), [300 credits](https://www.netlify.com/pricing/) | No | None found (UNVERIFIED) | GA |
| [SpinKube](https://github.com/spinframework/spin-operator) | K8s operator for Spin | Needs a cluster | Yes | Spin 4.1 has WASI 0.3.0 ([notes](https://github.com/spinframework/spin/releases/tag/v4.1.0)) | [Spin](https://github.com/spinframework/spin) 6.5k stars, v4.2.1 (2026-09-30); operator 288 stars, v0.6.1 2025-07-09 |
| [wasmCloud 2.x](https://wasmcloud.com/docs/v2.0.0-rc/kubernetes-operator/) | K8s operator, NATS, host pods | Needs a cluster | Yes | p2 native, p3 "on the way" | [2.4k stars](https://github.com/wasmCloud/wasmCloud), v2.10.1 2026-09-24 |
| [Faasta](https://github.com/fourlexboehm/faasta) | Self-hosted wasi:http p3 FaaS | Needs Postgres, S3, Valkey | Yes | p3 service | 228 stars, no releases |
| [Taubyte](https://github.com/taubyte/tau) | P2P serverless | Own servers | Yes | UNVERIFIED | 5.2k stars, v1.1.10 2026-04-22 |
| [Golem](https://github.com/golemcloud/golem) | Durable agent platform | Own servers | Yes | UNVERIFIED | 1.5k stars, v1.5.10 2026-08-24; BSL 1.1 |
| [Lunatic](https://github.com/lunatic-solutions/lunatic) | Erlang-style runtime | n/a | n/a | n/a | Dormant, last release 2023-05-03 |
| [Hyperlight-wasm](https://github.com/hyperlight-dev/hyperlight-wasm) | Micro-VM library for modules and components | Library, not a platform | n/a | Yes | 728 stars, v0.15.0 |
| WasmEdge, Extism | Runtimes / plugin frameworks | n/a | n/a | n/a | [WasmEdge](https://github.com/WasmEdge/WasmEdge) 10.8k, 0.17.1; [Extism](https://github.com/extism/extism) 5.8k, v1.30.0 |
| Fission, OpenFaaS, Knative | K8s FaaS | Needs a cluster | Yes | Wasm UNVERIFIED | [Fission](https://github.com/fission/fission) v1.27.0; [OpenFaaS](https://github.com/openfaas/faas) last release 2025-08-29; [Knative](https://github.com/knative/serving) v1.23.0 |
| [Cosmonic Control](https://cosmonic.com/control/) | Managed wasmCloud | K8s-only | n/a | n/a | n/a |
| Wasm-on-Lambda | [Spin multi-compute PoC](https://github.com/mreferre/spin-wasm-multi-compute) (18 stars, last push 2022-11-06); [WasmEdge via container image](https://wasmedge.org/docs/start/usage/serverless/aws/), no wasi:http shown | Yes | Yes | No | PoCs only |
| SST, OpenNext, Nitric, Encore, Serverless Fw | Own-account Lambda frameworks | Via Lambda | Yes | No | [SST](https://github.com/anomalyco/sst) v4.17.1 2026-07-12; [OpenNext](https://github.com/opennextjs/opennextjs-aws) v4.1.6; [Nitric](https://github.com/nitrictech/nitric) last release 2026-02-04; [Encore](https://github.com/encoredev/encore) v1.58.4 |
| Wing | Infra-from-code | n/a | n/a | n/a | [Company shut down 2025-04-09](https://www.calcalistech.com/ctechnews/article/bj90wnmrjl); [language](https://github.com/winglang/wing) last release 2025-02-03. Klotho, Ampt, Modal BYOC: UNVERIFIED |
| [celld](https://github.com/denoland/celld) | Self-hosted Durable Objects, nodes coordinate via S3 | Nodes always on | Yes | No (V8) | 4.9k stars, v0.6.0 2026-09-26 ([HN](https://news.ycombinator.com/item?id=49185430)) |
| SlateDB, Litestream, turbopuffer, WarpStream | Bucket-as-storage | n/a | n/a | n/a | [SlateDB](https://github.com/slatedb/slatedb) v0.17.0; [Litestream](https://github.com/benbjohnson/litestream) v0.5.17; [turbopuffer](https://turbopuffer.com/docs/architecture) commits via CAS; WarpStream keeps a [cloud metadata store](https://www.automq.com/blog/kafka-compatible-object-storage-streaming-platforms-warpstream-automq-and-more) (competitor blog) |

## Gap analysis

The nearest projects, by property:
- **Bucket control plane:** celld, but it is V8/JS and its nodes are always on.
- **Self-hosted wasi:http p3:** Faasta, but it needs Postgres, S3 and Valkey.
- **Own-account scale-to-zero:** SST and OpenNext, but no Wasm.
- **Spin on Lambda:** a 2022 PoC.
- **Spin/WASI compatibility:** SpinKube and wasmCloud, but both need Kubernetes.

Bucket-only coordination is feasible on all three clouds:
- [S3](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) supports conditional writes.
- [GCS](https://docs.cloud.google.com/storage/docs/request-preconditions) supports `ifGenerationMatch`.
- [Azure](https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations) supports If-Match.

Missing or risky:
- **Cold start:** a 2026-07-31 test measured 2.24 s on first invocation and about 130 ms warm ([post](https://dev.classmethod.jp/en/articles/20260731-lambda-wasm/)). No benchmark for precompiled p2 components was found (UNVERIFIED).
- **Idle floor:** a Route 53 hosted zone costs $0.50/month ([pricing](https://aws.amazon.com/route53/pricing/)), ten times the $0.05 idle target. A Function URL comes with a default domain and needs no zone ([docs](https://docs.aws.amazon.com/lambda/latest/dg/urls-configuration.html)).
- **KV:** Spin has no S3-backed KV provider ([#2606](https://github.com/spinframework/spin/issues/2606), open since 2024-06-26).
- **Ignored source:** one DEV post claims Lambda has first-class Wasm. The [runtimes doc](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtimes.html) lists no Wasm runtime.

## Demand evidence

Direct demand is thin. No threads were found asking for "SpinKube without Kubernetes", "Spin on Lambda" or "wasi:http on Cloud Run".

Adjacent signals:
- [Spin #2606](https://github.com/spinframework/spin/issues/2606) asks for an S3-backed KV. It cites Fermyon KV at about $19.39/month for 2 GB against S3 at $0.023/GB.
- [Spin discussion #1849](https://github.com/spinframework/spin/discussions/1849) asks whether Spin apps can run anywhere. [Discussion #2227](https://github.com/spinframework/spin/discussions/2227) asks for a Lambda-to-Spin conversion tool.
- Faasta's [Show HN](https://news.ycombinator.com/item?id=43789010) got 95 points and 31 comments.
- celld's [HN thread](https://news.ycombinator.com/item?id=49185430) got 286 points and 54 comments. It showed appetite for bucket-coordinated, self-hosted runtimes.

**Fermyon Cloud after the Akamai acquisition:**
- The [acquisition](https://www.akamai.com/newsroom/press-release/akamai-announces-acquisition-of-function-as-a-service-company-fermyon) (2025-12-01) promised continued Spin and SpinKube support. It says nothing about Cloud users.
- [fermyon.com/cloud](https://www.fermyon.com/cloud) still advertises its free tier.
- No sunset notice was found. That is "not found", not "safe".
- Spin and SpinKube are CNCF projects, so the open-source layer does not depend on Akamai.

## Hard questions

**Why not Cloudflare or Akamai free tiers?**
- Cloudflare has no component contract, so you give up WASI portability.
- Akamai Functions is the easiest path for Spin users, but its pricing is unpublished.
- Neither runs in your own account.

**Why not plain Lambda?**
- There's no cost advantage. Lambda's free tier already makes compute $0 at this load, and idle Lambdas cost nothing.
- At the reference load, roughly $1/month is S3 requests either way.
- The real advantages over plain Lambda are a portable WASI contract, one deploy unit for many apps, and pointer-flip deploy and rollback.

**Who benefits:**
- Teams with own-account, compliance or cloud-credit constraints who already hold Wasm components.
- People who want WASI portability across spinit, SpinKube, Akamai and wasmCloud.
- Hobbyists who want a near-free self-hosted PaaS.

The wedge is real but thin. It depends on how many people already ship Wasm components.

## Suggested positioning

**Three bullets:**
- Run WASI components in your own cloud account with no servers, cluster, NATS or database.
- The bucket is the cluster: a deploy is an upload plus a conditional HEAD flip, and rollback is flipping it back.
- WASI-first with Spin-subset compatibility, so the same `.wasm` moves between spinit, SpinKube and Akamai.

**Most differentiating features:**
1. Atomic HEAD-pointer deploy and rollback, with a bucket-only control plane.
2. One shared scale-to-zero host serving many apps, with precompiled components cached.
3. Bucket-backed `wasi:keyvalue`, which fills the gap in Spin #2606.

**Do not build:**
- A Spin clone or CLI.
- A K8s operator.
- Durable execution or actors (celld and Golem own this).
- General IaC (SST does this).
- A CDN or edge network.
- A Wasm runtime (use Wasmtime).
- A JS toolchain (use jco).
- A global multi-region database.
