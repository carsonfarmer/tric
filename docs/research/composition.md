# Composition and multi-component apps (fact sheet)

> A research sub-agent gathered this on 2026-10-01 (follow-up to Q39, "exactly one component per app").
> Facts carry a source or an experiment; **Inference** paragraphs are reasoning on top of them. UNVERIFIED marks claims without a primary source or experiment.
> Experiments ran on macOS aarch64 with `wasmtime =49.0.1`, `wac-graph 0.12.0`, `wac-cli 0.11.0`, `wasip3 0.9.0+wasi-0.3.0` and `wit-bindgen 0.62`. Timings are laptop numbers, not Lambda numbers.

## Bottom line

- **Facts:** the portable unit in the ecosystem is one component. Only Spin (and Akamai, which runs Spin) and wasmCloud have a multi-component app model, and both are host features, not standards. Even Spin precomposes into one component by default when pushing to a registry, for portability to other hosts.
- **Facts:** deploy-time `wac plug` costs 17 lines of Rust in-process (measured), about +0.82 MB of release binary and 9 new small crates. A composed p3 async component runs correctly on Wasmtime 49.0.1 (measured, including a three-link chain and the pooling allocator).
- **Inference:** Q39 matches the direction. spinit needs no composition code in v1, and the cheap upgrade path (a repeatable `--plug` flag on `deploy`) never changes the host or the one-component contract.

## 1. How each platform models an app of several components

| Platform (current, date) | Deployable unit | How components connect | When linking happens |
|---|---|---|---|
| **Spin 4.2.1** (2026-09-30; v4.2.0 2026-09-29, v4.1.0 2026-08-26) | `spin.toml`: N components, N triggers (routes) | `[component.X.dependencies]` (satisfy imports from registry, path, URL or another component); `dependencies.middleware = [...]` on `[[trigger.http]]` for HTTP chains; separately, `*.spin.internal` "local service chaining" passes an HTTP request in memory between two components in the same host | Spin composes in the host when it loads the app. `spin registry push` composes **by default** (`--compose=false` to skip) "to maximise compatibility with different Spin runtime hosts". Spin main (4.3.0-pre0) pins Wasmtime 49.0.1 and `wac-graph 0.11.0` |
| **Akamai Functions** (ex Fermyon Wasm Functions) | A Spin app, deployed with `spin aka deploy` | Same as Spin. Docs do not state the Spin version or component limits (UNVERIFIED) | Same as Spin |
| **wasmCloud 2.10.2** (2026-10-01) | A `Workload` (K8s CRD) of one or more components plus optional service components; `WorkloadDeployment` for rollout | The host links components inside one workload in-process through a dynamic linker. Since v2.5.0 (2026-06-30, Wasmtime 46, p3 on by default) p3 streams work across components. wRPC over NATS remains for cross-host calls | Runtime, in-process (v2). v1 was runtime over the network (wRPC/NATS) |
| **Cosmonic Control** | The same wasmCloud `Workload` manifest on Kubernetes; "enterprise control plane for wasmCloud" | Same as wasmCloud | Same as wasmCloud. Search snippet only, not fetched (UNVERIFIED) |
| **Fastly Compute** (Viceroy 0.21.1, 2026-09-17; CLI 16.1.0) | One package per service. Components run through a Fastly-specific world, `fastly:compute/service`, declared stable on 2026-09-17 (Viceroy PR 709) | No multi-component model found. The PR does not mention `wasi:http` | n/a. "One service = one package" is UNVERIFIED (no primary doc found) |
| **Cloudflare Workers** | A Worker; WebAssembly is core modules only. The Wasm docs page does not mention the component model; WASI is "experimental" | Separate Workers call each other through **service bindings** (RPC or HTTP, same thread of the same server, no network hop) | Runtime, between independently deployed Workers |
| **jco 1.35.0 / componentize-js 0.23.0** (2026-09-21), preview3-shim 0.8.0 | Toolchain: emits one component per source app (see [componentizers.md](componentizers.md)) | None built in. Composition is left to `wac` | Build |
| **`wasmtime serve`** (Wasmtime 49.0.1, 2026-09-24) | One component per process (p2 `incoming-handler` or p3 `handler`) | None | n/a |
| **Microsoft: Hyperlight Wasm** (v0.15.0, 2026-09-04) | A guest VM built for one WIT world (`WIT_WORLD` at build time); component-model support is "experimental" in its README | None. Issue 92 (2026-06-15) asked for p2/p3 component support in hyperlight-unikraft and was closed as not planned | n/a |
| **Microsoft: AKS** | WASI node pools: no new pools since 2025-05-05; the documented route is SpinKube (Spin apps) from Azure Marketplace | As Spin | As Spin |

Not covered: no first-party Azure Functions Wasm product was found (UNVERIFIED that none exists).

**Inference:** the split is clean. Platforms that run one request handler per deploy (Fastly, Cloudflare, `wasmtime serve`, Hyperlight, and spinit) have one component or module as the unit. Platforms that run an "app server" (Spin, wasmCloud) added a manifest of components. Cloudflare's answer to "several parts" is several deployables joined by a binding, which is Q39's second option ("deploy as separate apps").

## 2. Composition tools and WASI 0.3

| Tool | State (verified) |
|---|---|
| **wac** | v0.12.0 (2026-09-30), v0.11.0 (2026-09-08), v0.10.1 (2026-06-16). `wac plug` (N plugs into one socket), `wac compose` (WAC language). Crates: `wac-graph`, `wac-types`, `wac-parser`, `wac-resolver`. The official Component Model docs recommend `wac plug` for simple cases and the WAC language for complex ones |
| **`wasm-tools compose`** | Marked "(*deprecated*)" in the wasm-tools README (v1.260.0, 2026-09-30). Deprecation discussed in wasm-tools issues 1555 and 1564 (2024-05). wac is the replacement |
| **WASI-Virt** | WASI **0.2 only** ("Virtualization Component Generator for WASI Preview 2"). Last commits 2026-07-25 (migrated from wasm-compose to wac) and 2026-07-31 (WASI 0.2.12). No tagged releases. No p3 plan found (UNVERIFIED) |
| **`wasi:http` middleware** | WASI 0.3.0 (2026-06-11) and 0.3.1 (2026-08-11) define `world service` (exports `handler`) and `world middleware` (`include service` plus `import handler`). The `client` interface doc says a `client.send` import can be linked directly to a `handler.handle` export, which bypasses the network |
| **Spin's own composer** | `spin-compose`: 728 lines (608 `lib.rs`, 120 `middleware.rs`), mostly capability inheritance and a deny-all adapter. The middleware chain core is about 70 lines: instantiate the innermost handler, then `set_instantiation_argument(.., "wasi:http/handler@0.3.0", upstream)` outward, then `graph.export(..)` |

**What WASI 0.3 changes (Bytecode Alliance, 2026-06-11):** the host owns the one event loop shared by all components. The post says that under 0.2 a component that used streaming or async APIs "couldn't be composed with any other components"; under 0.3 async composes across component boundaries, and the middleware world gives a standard shape for "service chaining" in-process (the post claims milliseconds to nanoseconds).

**Limits that still apply (facts from the experiments below, plus inference):**
- Composition does not merge memories. Each input stays a nested component with its own linear memory (observed: 2 memories for 2 inputs). A pooling allocator capped at `max_memories_per_component = 1` rejects the result (observed error: "The component transitively contains 2 Wasm linear memories, which exceeds the configured maximum of 1"; a 3-link chain is rejected at a cap of 2 and accepted at 3). The default is `u32::MAX`.
- The composed component's imports are the union of the inputs' unresolved imports, so every sub-component sees the same host capabilities. Spin works around this with a deny-all adapter plus `inherit_configuration`. **Inference:** under Q53 (untrusted components), compose only components you would grant the same allow-list, KV and config.
- p2 and p3 do not mix freely. The Component Model docs say 0.3 runtimes may polyfill 0.2 imports at the host boundary, but they do not address composing a p2 caller with a p3 callee. Not tested here.
- Interface version skew (0.3.0 against 0.3.1): wac's semver-compatible matching was noted in an earlier pass over wac's release notes. Not re-tested here (UNVERIFIED).

## 3. Guidance from the standards bodies, and what the community does

**Facts**
- No Bytecode Alliance or WASI subgroup text was found that says "one component per deployable" or "a manifest of many components". The Component Model FAQ does not discuss packaging. The composing page says only that tools are early-stage and recommends wac. This is absence in the pages read, not proof of absence.
- The WASI 0.3 launch post (Bailey Hayes and Yosh Wuyts, 2026-06-11) presents the middleware world as the way to chain services in-process, and says the runtime may compose them. It does not prescribe where composition happens.
- Spin issue 3696 (2026-08-27, "Build and save precomposed component") asks for `spin build` to emit one fully composed binary "usable in any host". A Spin maintainer calls it conceptually easy but lists open questions (multiple components, permissions, files, per-route middleware). Status: investigating.
- wasmCloud's 2026-06-10 community meeting recap: workloads can run several components with per-component config, the runtime relaxed its duplicate-export check, and `wac` is the suggested workaround when several components export the same interface.
- wasmcp (Spin blog, 2025-11-25) builds MCP servers by composing feature components with wac into one `server.wasm`, then runs it on Spin, Wasmtime or SpinKube.
- The WasmCon Europe 2026 schedule page fetched showed a partial session list and none on composition. A full search of talks was not done (UNVERIFIED that none exist). Several SEO blog posts in the search results disagree on basic facts (for example the WASI 0.3 date) and were not used.

**Inference:** the emerging practice is "compose to one component, then deploy that." Spin (the only mature multi-component manifest) is moving toward precomposed single artifacts for portability. wasmCloud's multi-component workloads are a runtime feature for hosts that want per-component config and scaling, and even there wac is the documented tool when linking gets ambiguous.

## 4. Cost to spinit of deploy-time composition

### Code

`spinit deploy` plugging N components into one socket, using the public `wac_graph::plug`. This compiled, ran, and produced bytes identical to `wac plug` (SHA-256 matches; 17 non-blank lines):

```rust
use anyhow::{Context, Result};
use wac_graph::{CompositionGraph, EncodeOptions, plug};
use wac_types::Package;

pub fn plug_components(socket: Vec<u8>, plugs: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>> {
    let mut graph = CompositionGraph::new();
    let socket = Package::from_bytes("socket", None, socket, graph.types_mut()).context("parse socket")?;
    let socket = graph.register_package(socket)?;
    let mut ids = Vec::new();
    for (name, bytes) in plugs {
        let pkg = Package::from_bytes(&format!("plug:{name}"), None, bytes, graph.types_mut())
            .with_context(|| format!("parse plug `{name}`"))?;
        ids.push(graph.register_package(pkg)?);
    }
    plug(&mut graph, ids, socket)?;
    Ok(graph.encode(EncodeOptions::default())?)
}
```

| Item | Estimate |
|---|---|
| `plug_components` | 17 lines (measured) |
| `--plug <path>` repeatable flag, read files, call, store the result as the blob | about 5-8 (estimate) |
| One fixture test (app plus middleware, assert header) | about 15 (estimate) |
| **Total** | **about 25-30 product lines plus a test** |
| `.wac` file support (`wac-parser` + `wac-resolver`: `Document::parse`, resolve packages, `resolution.encode`) | about 25-40 lines (estimate from wac-cli's `compose.rs`, 134 lines mostly CLI; not prototyped) |

- **Dependencies:** +9 crates (`wac-graph`, `wac-types`, `flate2`, `miniz_oxide`, `adler2`, `simd-adler32`, `spdx`, `topological-sort`, `auditable-serde`). `wasmparser` and `wit-parser` stay shared with Wasmtime 49 (0.258.0), so no new duplicates.
- **Binary:** release build 35,933,904 B to 36,755,696 B (about +0.82 MB, +2.3%), default profile (the PoC host with the compose subcommand added).
- **Wasmtime 49 error type:** it is `wasmtime::Error`, not `anyhow::Error`, so the glue needs one `map_err` (hit while building).
- **Chains:** `wac plug` takes one socket. A chain such as app, middleware A, middleware B is two calls, each result fed to the next as the plug.

### What the output looks like

- One component (`wasi:http/handler@0.3.0` export plus the socket's other exports) that nests each input as an inner component, each with its own 3 core modules (measured with `wasm-tools print`: 3 core modules per input, so 6 for the 2-input component and 9 for the 3-chain).
- Imports are the union of the unresolved imports: `wasi:http/types@0.3.0` plus 15 p2 imports at 0.2.9 (`wasi:io`, `wasi:cli`, `wasi:clocks`, `wasi:random`; from Rust std on `wasm32-wasip2`, so a p3 Rust component still needs p2 imports provided by the host). The app's `handler` export is consumed internally; the surface exports are `wasi:http/handler@0.3.0` and a leftover `wasi:cli/run@0.2.0` from the Rust `fn main() {}`, which is harmless. The inner boundary uses the async ABI (7 `[async-lower]handle` lowerings in the composed component).
- Output is deterministic: two runs of `wac plug` on the same inputs were byte-identical (wac 0.11.0 CLI and 0.12.0 library agree).

### Size and compile time (cold start)

Test inputs: a p3 hello app (172,700 B; `wasip3` 0.9, `opt-level = "s"`, LTO, strip) and a p3 middleware that adds a response header (194,159 B; source in the appendix). `wbench`-style harness, 3 compiles each; the first run includes warm-up. The Winch column is what Q54(c) would pay on every cold start.

| Component | .wasm | Cranelift compile | Winch compile | cwasm (Cranelift / Winch) |
|---|---|---|---|---|
| app | 168 KB | 15 ms | 6 ms | 526 / 846 KB |
| middleware | 189 KB | 16 ms | 7 ms | 596 / 996 KB |
| app + middleware (`wac plug`) | 363 KB (+1.4% over the sum) | 30 ms | 12 ms | 1,086 / 1,822 KB |
| app + 2 middleware (chain of 3) | 557 KB (+1.8% over the sum) | 44-50 ms | 18-21 ms | 1,628 / 2,797 KB |

**Fact:** composition adds about 1-2% to the size, and compile time and cwasm size are about the sum of the parts. A composed component compiles as one unit but nothing superlinear showed up at these sizes. **Inference:** in terms of Q47 (500 ms p99 up to about 5 MB), count the composed total, not the app alone. StarlingMonkey JS apps are about 14 MB and p2-only ([componentizers.md](componentizers.md)), so they sit under Q47's looser 1 s target and could join only a p2 composition; that case was neither composed nor compiled here. Lambda timings will differ.

Not measured: instantiation and per-request cost of the extra nested instance, and memory per request with the pooling allocator (each request now allocates one memory per nested component; size `total_memories` for concurrency times parts).

### Does Wasmtime 49 run composed p3 async components?

**Yes, in the cases tested.**
- `minihost` (PoC host on `wasmtime =49.0.1` and `wasmtime-wasi-http` `ProxyHandler`, async features on) served the composed component: HTTP 200, body `hello from rust p3 (wasip3)`, header `x-middleware: 1`. The middleware alone fails to link (`wasi:http/handler@0.3.0` unsatisfied), as expected.
- The 3-link chain served correctly. The composed core uses the async ABI across the inner boundary (`[async-lower]handle`), and the response body stream crosses it (re-wrapped by the middleware).
- The same composed component also served correctly with the pooling allocator, with default limits and with `max_memories_per_component = 4` (composed) or 3 (chain of 3); it fails when the cap is below the memory count (1 for composed, 2 for the chain).
- Indirect: Spin main pins Wasmtime 49.0.1 and `wac-graph 0.11.0` and ships p3 middleware composition.
- **Caveats:** Rust guests only; one request at a time; `wasi:http@0.3.0` only; no p2 composition; no JS guest composed; no soak or concurrency test.

## 5. Recommendation

1. **Keep Q39.** "One component per app, compose at build time" matches the ecosystem: single-handler platforms all deploy one artifact, Cloudflare uses separate deployables joined by a binding, and Spin itself composes before push for portability.
2. **v1: write no composition code.** Document the build-time route: `wac plug --plug dep.wasm main.wasm -o app.wasm` (for p3, the `wasi:http/middleware` world is the sanctioned shape), then `spinit deploy app.wasm`. The experiment above becomes one e2e fixture test.
3. **Fast follow, only on demand:** a repeatable `spinit deploy --plug dep.wasm` that calls `wac_graph::plug` (about 25-30 lines, about +0.82 MB, +9 crates). Output is a plain component, so the host, manifest and content-addressed store do not change. The composed digest is deterministic.
4. **Later, only if asked:** a `.wac` file at deploy (about 25-40 lines, `wac-parser` and `wac-resolver`, not prototyped). Prefer leaving that to the user's build, because the build already has `wac`.
5. **Spin adapter behind a cargo feature (Q31):** accept a `spin.toml` with exactly one component and no `dependencies` or `middleware`, and reject the rest with a clear error. Do not copy `spin-compose` (728 lines, mostly capability inheritance). If ever needed, route Spin middleware through the same `plug` call.
6. **Rule to document:** composed sub-components share the app's capabilities (allow-list, KV, config) and each gets its own linear memory; size the pooling allocator for it (Q53, Q55).
7. **Watch:** Spin issue 3696 (precompose precedent), wasmCloud's p3 dynamic linker, wac's semver matching for `wasi:http` 0.3.x skew.

## Appendix: experiment setup (for reproduction)

Middleware crate (`wasm32-wasip2`, bin, `wasip3 0.9` with `http-compat`, `wit-bindgen 0.62` with `async` and `async-spawn`). `wit/world.wit` is `package local:mw; world mw { include wasi:http/middleware@0.3.0; }`, with `wit/deps/` copied from the `wasip3 0.9.0` crate's `wit/deps`. The `wasip3` crate ships only the `service` world, so the middleware needs its own `generate!`:

```rust
wit_bindgen::generate!({
    path: "wit", world: "mw",
    with: {
        "wasi:http/types@0.3.0": wasip3::http::types,
        "wasi:http/handler@0.3.0": generate,
        "wasi:http/client@0.3.0": wasip3::http::client,
        "wasi:cli/types@0.3.0": wasip3::cli::types, "wasi:cli/stdin@0.3.0": wasip3::cli::stdin,
        "wasi:cli/stdout@0.3.0": wasip3::cli::stdout, "wasi:cli/stderr@0.3.0": wasip3::cli::stderr,
        "wasi:clocks/types@0.3.0": wasip3::clocks::types,
        "wasi:clocks/monotonic-clock@0.3.0": wasip3::clocks::monotonic_clock,
        "wasi:clocks/system-clock@0.3.0": wasip3::clocks::system_clock,
        "wasi:random/random@0.3.0": wasip3::random::random,
        "wasi:random/insecure@0.3.0": wasip3::random::insecure,
        "wasi:random/insecure-seed@0.3.0": wasip3::random::insecure_seed,
    },
});
struct Mw;
impl exports::wasi::http::handler::Guest for Mw {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let resp = wasi::http::handler::handle(request).await?;
        let mut resp = http_from_wasi_response(resp)?;
        resp.headers_mut().insert("x-middleware", http::HeaderValue::from_static("1"));
        http_into_wasi_response(resp)
    }
}
export!(Mw);
fn main() {}
```

The app is `docs/research/componentizers-poc/rust/p3-hello`. Compose with `wac plug --plug app.wasm mw.wasm -o composed.wasm` (the middleware imports `handler`, so it is the socket). Run with `minihost` and a one-component `spin.toml` (`MODE=none`). Source files and binaries from this run live in the session scratchpad, which is not preserved.

## Sources

Spin
- [Spin v4 writing apps (dependencies)](https://spinframework.dev/v4/writing-apps), [HTTP trigger (middleware)](https://spinframework.dev/v4/http-trigger), [manifest reference](https://spinframework.dev/v4/manifest-reference), [outbound HTTP (service chaining)](https://spinframework.dev/v4/http-outbound)
- [Spin registry push `--compose` source](https://github.com/spinframework/spin/blob/main/src/commands/registry.rs), [`spin-compose` crate](https://github.com/spinframework/spin/tree/main/crates/compose), [Spin root Cargo.toml](https://github.com/spinframework/spin/blob/main/Cargo.toml), [issue 3696](https://github.com/spinframework/spin/issues/3696), [Spin releases](https://github.com/spinframework/spin/releases)
- Spin v4.0.0 date: the GitHub tag says 2026-04-20; a blog post read earlier says 2026-06-15. Unresolved.
- [Akamai Functions docs](https://techdocs.akamai.com/akamai-functions/docs/welcome), [wasmcp on Spin](https://spinframework.dev/blog/mcp-with-wasmcp)

wasmCloud and Cosmonic
- [wasmCloud releases](https://github.com/wasmCloud/wasmCloud/releases) (v2.5.0, v2.6.0, v2.10.2), [2026-06-10 community meeting](https://wasmcloud.com/community/2026-06-10-community-meeting/), [v2 announcement](https://wasmcloud.com/blog/wasmcloud-v2-is-here/), [workloads](https://wasmcloud.com/docs/overview/workloads/), [Cosmonic glossary](https://cosmonic.com/docs/glossary/)

Standards and tools
- [WASI 0.3 launch (Bytecode Alliance)](https://bytecodealliance.org/articles/WASI-0.3), [WASI v0.3.0 release](https://github.com/WebAssembly/WASI/releases/tag/v0.3.0), [Component Model: composing](https://component-model.bytecodealliance.org/composing-and-distributing/composing.html), [migrating to p3](https://component-model.bytecodealliance.org/design/migrating-to-p3.html), [FAQ](https://component-model.bytecodealliance.org/reference/faq.html)
- [wac releases](https://github.com/bytecodealliance/wac/releases), [wac-graph `plug.rs`](https://github.com/bytecodealliance/wac/blob/main/crates/wac-graph/src/plug.rs), [wac-cli `compose.rs`](https://github.com/bytecodealliance/wac/blob/main/src/commands/compose.rs)
- [wasm-tools README](https://github.com/bytecodealliance/wasm-tools#readme), issues [1555](https://github.com/bytecodealliance/wasm-tools/issues/1555) and [1564](https://github.com/bytecodealliance/wasm-tools/issues/1564), [WASI-Virt](https://github.com/bytecodealliance/WASI-Virt) (PR 163, PR 164)
- [jco releases](https://github.com/bytecodealliance/jco/releases), [componentize-js releases](https://github.com/bytecodealliance/componentize-js/releases), [Wasmtime releases](https://github.com/bytecodealliance/wasmtime/releases)

Other platforms
- [Fastly Viceroy PR 709](https://github.com/fastly/Viceroy/pull/709), [Viceroy changelog](https://github.com/fastly/Viceroy/blob/main/CHANGELOG.md)
- [Cloudflare Wasm](https://developers.cloudflare.com/workers/runtime-apis/webassembly/), [service bindings](https://developers.cloudflare.com/workers/runtime-apis/bindings/service-bindings/)
- [Hyperlight Wasm README](https://github.com/hyperlight-dev/hyperlight-wasm), [hyperlight-unikraft issue 92](https://github.com/hyperlight-dev/hyperlight-unikraft/issues/92), [AKS WASI node pools](https://learn.microsoft.com/en-us/azure/aks/use-wasi-node-pools)
