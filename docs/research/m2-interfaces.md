# M2 interfaces for torpor: KV, config and outbound HTTP (fact sheet)

> Gathered 2026-10-01 by a research sub-agent from the unpacked `wasmtime`, `wasmtime-wasi`, `wasmtime-wasi-http` and `wasmtime-wasi-config` 49.0.1 crates, `object_store` 0.14.2, `hyper-util` 0.1.21, `hyper-rustls` 0.27.10, `ip_network` 0.4.1, `spin-sdk` 7.0.0 and `wit-bindgen` 0.62.0 (all from static.crates.io), the `spinframework/spin` repository at git tree 32d1fe70 (files under `crates/`), and by compiling and running probe hosts and guests.
> All runs: Docker only (image `torpor-m2-interfaces-build`: Rust 1.99.0, `wasm32-wasip2`, wasm-tools 1.260.0, wasmtime CLI 49.0.1, aarch64 Linux), one build job at a time. Items marked UNTESTED were read from source only. Line numbers are in the 49.0.1 crates and the other crate versions named above, unless a file is named `plan.md` or is a path in this repo.
> Harness: `sketch/` (the M1 worktree plus the M2 files; the sketches in section 9 are its `src/kv.rs` and `src/outbound.rs` verbatim), `host/` (the same plus test-only switches, used for the outbound and table-cap runs), `probe/` (allow list, filter and `url` probes), `optb/` (option (b), compile only), `spinx/` and `spinx2/` (Spin SDK probes). All live in the session scratchpad (`m2-research/`), not in the repo. The sketch passes 7 integration tests and 3 unit tests. Nothing was committed.

## Bottom line

**Recommendation: write M2 as two new files plus about 23 lines of wiring. `src/kv.rs` binds `wasi:keyvalue@0.2.0-draft2` with `bindgen!` over the vendored WIT and keeps keys, values and the generation object in `object_store` 0.14.2 (`default-features = false`) with conditional puts for `cas`. `src/outbound.rs` implements `WasiHttpHooks::send_request` itself: check the allow list, resolve with `lookup_host`, reject if any address is blocked, dial only the checked addresses, then `hyper::client::conn::http1::handshake`. Do not reuse `default_send_request`. `wasi:config` needs no new code: M1 already links it. The sketch compiles on 49.0.1 and passes its tests, but it is over the plan's line budgets (KV 294 code lines against about 200, outbound 116 against about 60), so say so in the plan before the work starts (section 10).**

| # | Point | Evidence |
|---|---|---|
| 1 | `bindgen!` with `world: "imports"`, `imports: { default: async }` and `with:` for `bucket` and `cas` compiles on 49.0.1. One call, `Imports::add_to_linker::<Host, HasSelf<Host>>(&mut linker, \|h\| h)`, serves p2 and p3 guests. A p3 guest, a p2 guest and a Spin SDK guest all pass every operation. `Host.table` must become `pub(crate)`. | Sections 1, 9.1, 9.3 |
| 2 | CAS maps onto `PutMode::Update(ETag)` (and `PutMode::Create` for an absent key). InMemory checks and writes under one lock, so it is atomic: 32 writers x 50 increments gave exactly 1600, and 8 guests x 10 read-modify-writes gave exactly 80. S3's ETag is an MD5 of the body (UNTESTED), so every generation write must have a different body. | Sections 2, 9.1 |
| 3 | Do not use `default_send_request`: it dials with `TcpStream::connect(&authority)`, so the name is resolved after any check and the checked address is not the dialled one, and its timeouts default to 600 s. Hand-written connect, TLS and `http1::handshake` is about 25 to 30 lines and was run against real hosts. Option (b), hyper-util's `Client`, compiles (not run) but adds three crates and is about 40 lines (UNTESTED estimate). | Section 3 |
| 4 | **Conflicts with Q38.** The `url` crate cannot parse `*://*:*`, `http://*:*` or `host:*`, so "about 20 lines on the `url` crate" does not hold. A hand-written matcher on `http::Uri` is 36 lines with `parse` and passes a table of allowed and denied URLs. `url` is already in the M1 tree. | Section 4 |
| 5 | The address filter is 12 lines of stable `std`. `ip_network` is one line shorter to call but treats `::127.0.0.1`, NAT64, 6to4 and IPv4 multicast as global, so hand-roll it. All 16 targets tried (the plan's five plus 11 more, p2 and p3) return `DestinationIpProhibited`, including a name whose DNS points at `10.0.0.5`. We reject when any address is blocked; Spin rejects only when all are. | Section 5 |
| 6 | `wasmtime-wasi-config` 49.0.1 implements `wasi:config@0.2.0-rc.1`. M1's `engine.rs` already links it. It works for p2, p3 and Spin guests. M2 adds no config code. | Section 6 |
| 7 | A table cap of 256 is safe. A fetch holds 6 slots in p2 and 2 in p3 while live, and a bucket or cas handle holds 1. Even a guest that leaks every handle needs about 124 slots for 20 fetches (p2). Table-full is a value error for KV and a trap (HTTP 500) for `wasi:http`. | Section 7 |
| 8 | Spin SDK 7.0.0 is the latest. Its KV is draft2 and links on our host, but only through its hidden `experimental` module, and its `wasi:config` is `0.2.0-draft-2024-09-27`, which does **not** link on rc.1. Use `spin_sdk::wit_bindgen::generate!` with `runtime_path` for both interfaces next to `#[http_service]`; the existing Spin fixture does this and passes. | Section 8 |
| 9 | Line budgets are not met with the required behaviour: KV 294 code lines (about 200), outbound 116 (about 60), config 0 (about 30), wiring about +23. The sketch lists what each required behaviour costs and what could be cut. | Section 10 |
| 10 | Security surprises, all tested: `getaddrinfo` normalises `127.1`, `0x7f.1` and `2130706433`, so judge resolved addresses, not text. The connection driver must be returned from the hook, not detached, or a dropped store leaves the socket open. A hung DNS lookup is not aborted when the store drops (guest answered at 10 s, process finished at 25 s). | Sections 3, 5, 11 |

## 1. `bindgen!` for `wasi:keyvalue@0.2.0-draft2` on Wasmtime 49.0.1

| Item | Finding | Evidence |
|---|---|---|
| Package and world | `wasi:keyvalue@0.2.0-draft2`, vendored in `wit/keyvalue/`. World `imports` is `store` + `atomics` + `batch`. | `wit/keyvalue/world.wit`, `store.wit`, `atomic.wit`, `batch.wit` |
| Invocation | `bindgen!({ path: "wit/keyvalue", world: "imports", imports: { default: async }, with: {...} })`. Compiles with no warnings. The macro makes `pub struct Imports`, `pub mod wasi::keyvalue::{store, atomics, batch}` in the invoking module. | Section 9.1, lines 25 to 35 |
| `async` | `imports: { default: async }` makes every host function a native `async fn` in the trait, so no `async_trait`. Without `trappable` the functions return the WIT types, for example `Result<T, store::Error>`. Only resource `drop` returns `wasmtime::Result<()>`. | `wasmtime-49.0.1/src/runtime/component/mod.rs:289-292` (docs for `imports`); compiled |
| `with:` | `"wasi:keyvalue/store.bucket": Bucket` and `"wasi:keyvalue/atomics.cas": Cas` make `Resource<Bucket>` and `Resource<Cas>` refer to our own structs, so `table.push(Bucket(name))` works. A resource named in `with` gets no generated type. | `.../component/mod.rs:414-416, 424-427` (docs); compiled |
| Host traits | `store::Host` (`open`), `store::HostBucket` (`get`, `set`, `delete`, `exists`, `list_keys`, `drop`), `atomics::Host` (`increment`, `swap`), `atomics::HostCas` (`new`, `current`, `drop`), `batch::Host` (`get_many`, `set_many`, `delete_many`). All are implemented directly on our `Host`, the store data. | Section 9.1, lines 226 to 330; compiled |
| Error types | `store::Error::{NoSuchStore, AccessDenied, Other(String)}`. `atomics::CasError::{StoreError(Error), CasFailed(Resource<Cas>)}`. `swap` takes the `cas` by value, so the host deletes it from the table and, on loss, pushes a fresh handle into `CasFailed`. | `wit/keyvalue/atomic.wit`, `store.wit`; compiled |
| Linker | `Imports::add_to_linker::<Host, HasSelf<Host>>(&mut linker, \|h\| h)?`. `HasSelf<T>` is the `HasData` impl whose `Data<'a>` is `&'a mut T`, which fits because `Host` implements the traits itself. | `wasmtime-49.0.1/src/runtime/component/has_data.rs:296-306`; section 9.3 |
| Field visibility | `Host.table` is private in M1's `guest.rs`. `kv.rs` is a sibling module and cannot reach a private field, so it becomes `pub(crate)`. | M1 `src/guest.rs:34`; applied in the sketch diff (section 9.3) |
| p2 and p3 together | One linker holds p2 WASI, p3 WASI, wasi:http p2 and p3, `wasi:config` and `wasi:keyvalue`. The linker is shared, so a p3 component that imports `wasi:keyvalue` calls the same host code. Tested with the `kv-p2`, `kv-p3` and `spin` fixtures: all operations pass in all three. | `sketch/tests/m2.rs` `every_op_in_every_guest` |
| Whole-world imports | A guest built with `include wasi:keyvalue/imports` imports `batch` too. If the host leaves `batch` out, instantiation fails, unless the host calls `Linker::define_unknown_imports_as_traps`. UNTESTED. | `wasmtime-49.0.1/src/runtime/component/linker.rs:357` |
| Table calls | `ResourceTable::push` and `delete` return `Result` with `ResourceTableError`. A failed push is turned into `Error::Other(...)` (a value the guest sees), not a trap. | Section 7; `sketch/tests/m2.rs` `errors_are_values_not_traps` |

## 2. `object_store` 0.14 for the KV backend

| Item | Finding | Evidence |
|---|---|---|
| Version and features | 0.14.2 (the version unpacked and tested), pinned `=0.14.2` in the sketch. `default = ["fs"]`, so use `default-features = false`. `InMemory` needs no feature. The `aws` feature (M3) is `aws-base` + `reqwest` + `reqwest/rustls` + `aws-lc-rs`. | `object_store-0.14.2/Cargo.toml:44-49, 82-83` |
| Traits | `ObjectStore` (`put_opts`, `get_opts`, `delete`, `list`, `list_with_offset`...) plus `ObjectStoreExt` (`put`, `get`, `delete` helpers). Import both. | `lib.rs`, compiled |
| Put modes | `PutMode::{Overwrite, Create, Update(UpdateVersion)}`. `Update` carries `e_tag` and `version`. | `lib.rs:1905-1915` |
| CAS on InMemory | `put_opts` takes `storage.write()` around the check and the insert, so the compare-and-set is atomic. `Update` needs an ETag (`Error::MissingETag`) and gives `Error::Precondition` for a key that does not exist. `Create` on an existing key gives `Error::AlreadyExists`. So: existing key uses `Update(etag)`, absent key uses `Create`. | `memory.rs:51, 140-175, 202-221` |
| Race result | 32 writers x 50 increments on InMemory gave exactly 1600. 8 guests x 10 read-modify-writes gave exactly 80 (83 and 98 retries seen). Host `increment` 8 x 10 gave 80. | `sketch/tests/m2.rs` `racing_cas`; probe `store.rs` |
| ETags | InMemory ETag is a counter in quotes, `"N"`, new on every put (also an identical body). S3's ETag is the MD5 of the body (UNTESTED against S3). So on S3 a rewrite of the same bytes keeps the ETag, and a CAS compares content: the ABA case passes, which is still correct for `increment`. The generation object therefore gets a body that always differs (nanoseconds since the epoch). | `memory.rs:127-131, 209-220`; `touch` in section 9.1 |
| S3 conditional put | The default for S3 is `S3ConditionalPut::ETagMatch` (`If-Match` and `If-None-Match: *`). Not run against S3. | `aws/builder.rs:1074`; `aws/mod.rs:260-280` (UNTESTED) |
| Conditional get | `GetOptions { if_none_match: Some(etag) }` returns `Error::NotModified` when current. InMemory honours it (tested: one `GET if-none-match`, no body). On S3 it is a 304 (UNTESTED). | `sketch/tests/m2.rs` `cache_and_etag_costs` |
| Delete | InMemory `delete` of a missing key is `Ok`: `delete_stream` removes from the map and returns the path whatever the result. `LocalFileSystem` returns `NotFound`. S3 is expected to be `Ok` (UNTESTED). The sketch maps any store error to `Error::Other`, so a local-file backend would turn a delete of a missing key into an error; map `NotFound` to success if that backend is ever used. Not an issue for InMemory or S3. | `memory.rs:300-311`; `local.rs:542, 773-783`; `Kv::write` in section 9.1 |
| Listing | `list(prefix)` is recursive and returns keys in the order of their percent-encoded form. `list_with_offset(prefix, offset)` is the cursor primitive: the trait default filters client-side (reads everything), S3 overrides it with a `start-after` request. On InMemory the filter is client-side, so a paging test does not prove the S3 cost. | `lib.rs:1275`; `aws/mod.rs:383` |
| Key encoding | `Path::from_iter(["kv", app, bucket, key])` percent-encodes each part, so `/` becomes `%2F` and a key cannot add path segments. `.` and `..` become `%2E` and `%2E%2E`. Read a key back with `percent_decode_str(location.filename())`. | `path/parts.rs:79-112` |
| Layout | `kv/<app>/<store>/<key>` and `kvgen/<app>/<store>`. The generation lives outside `kv/` so a list of `kv/<app>/<store>` never sees it. This matches the S3 key shape the plan wants. | Section 9.1, `Kv::path` and `Kv::touch` |
| Key length | A 256 B key is at most 768 B encoded (3 B per byte). Worst object key: `kv/` 3 + app 64 + `/` 1 + store 64 + `/` 1 + key 768 = 901 B, under S3's 1024 B limit, if app names are 64 B or less. That limit on app names is an assumption (UNTESTED). | `Kv::path`; S3 object key limit |
| Dependencies | `url` is a hard dependency of `object_store` (`Cargo.toml:266`) and is already in the M1 tree. M2 adds 20 crates (173 to 193 in `cargo tree`, root crate counted in both). For `object_store`: itself, `chrono`, `humantime`, `iana-time-zone`, `itertools`, `num-traits`, `parking_lot`, `parking_lot_core`, `lock_api`, `getrandom` 0.2, `futures-macro`. For outbound TLS: `tokio-rustls`, `rustls`, `rustls-pki-types`, `rustls-webpki`, `ring`, `subtle`, `untrusted`, `zeroize`, `webpki-roots`. | `tree-m1-lib.txt`, `tree-m2-lib.txt` in the scratchpad |
| Wrapper stores | A counting wrapper for tests must implement `put_opts`, `put_multipart_opts`, `get_opts`, `delete_stream`, `list`, `list_with_delimiter` and `copy_opts`. It is test code only (`sketch/tests/common/mod.rs`, `Counting`). | `sketch/tests/common/mod.rs` |

## 3. Outbound send in `wasmtime-wasi-http` 49.0.1 without `default-send-request`

| Item | Finding | Evidence |
|---|---|---|
| The hook | `WasiHttpHooks::send_request(&mut self, Request, Option<RequestOptions>, Fut<()>) -> Fut<(Response, Fut<()>)>`. Without the `default-send-request` feature it has no body, so the embedder must implement it. With the feature it calls `default_send_request`. | `wasmtime-wasi-http-49.0.1/src/ctx.rs:248-267` (with), `:290` (without) |
| One hook for p2 and p3 | p2 calls it and spawns the future with `wasmtime_wasi::runtime::spawn`. p3 calls it from the `handle` implementation. | `p2/http_impl.rs:101-118`; `p3/host/handler.rs:87` |
| Types | `Request = http::Request<WasiBody>`, `Response = http::Response<WasiBody>`, `WasiBody = UnsyncBoxBody<Bytes, Error>`. The second returned future is the connection driver. | `handler.rs:39, 42`; `ctx.rs:155` |
| Abort on drop | `wasmtime_wasi::runtime::spawn` returns an `AbortOnDropJoinHandle`. The hook runs inside a store task, so when the store drops (deadline) the task and the driver are aborted and the socket closes. Tested: a black-hole server saw EOF 1 ms after the guest got its 500, at 10.004 s. | `wasmtime-wasi-49.0.1/src/runtime.rs:41, 55, 86`; `host/tests/net.rs` |
| Error to guest | `Error` maps to `ErrorCode` for p2 and p3. `Connect(io)` becomes `DnsError` when the kind is `AddrNotAvailable` or the message starts "failed to lookup address information", else the hook `p*_error_from_connect` (default `ConnectionRefused`). `Tls(io)` goes to `p*_error_from_tls` (default `TlsProtocolError`). A hyper error unwraps a wrapped `ErrorCode` or `Error` from `source()`, else `HttpProtocolError`. The other `Error` variants map one to one. | `error.rs:10-50`; `p2/error.rs:279-320`; `p3/conv.rs:98-144`; `ctx.rs:321-338, 359-376` |
| Seen by the guest | Not allow-listed: `HttpRequestDenied`. Blocked address: `DestinationIpProhibited`. Name does not resolve: `DnsError`. Connection refused: `ConnectionRefused`. Server closes without a reply: `HttpProtocolError`. Bad certificates (expired, wrong name, self-signed, untrusted): `TlsProtocolError` in every case. Same in p2 and p3. Returning `Error::TlsCertificateError` for a certificate failure is possible, UNTESTED. | `host/tests/net.rs`, `host/tests/tls.rs` |
| (a) Reuse `default_send_request` | **Not safe as is.** It connects with `TcpStream::connect(&authority)`, which resolves with the system resolver inside the connect, so a check made on an earlier lookup does not bind the address dialled (rebinding gap). Its connect, first-byte and between-bytes timeouts default to 600 s. It builds `rustls::ClientConfig::builder()`, which needs one process-wide crypto provider (UNTESTED: two providers in the tree after M3's `aws` feature would be ambiguous). It also needs the `default-send-request` feature, which M1 leaves off on purpose (its comment calls the built-in client a way around the hook). | `default_send_request.rs:56, 60, 64, 66, 75`; M1 `Cargo.toml:42` (comment) |
| (b) hyper-util `Client` + hyper-rustls | Compiles (`cargo check` of `optb/src/lib.rs`, 61 lines, not run). Needs: a `tower_service::Service<Name>` resolver that filters (about 14 lines), `HttpConnector::new_with_resolver`, `enforce_http(false)`, the `HttpsConnectorBuilder` chain, a separate literal-IP check because hyper-util skips the resolver for an IP literal, and a walk down `source()` to find a wrapped resolver error. If one `Client` serves all apps, its pool is shared between apps. The drivers are detached with `tokio::spawn`, so a store drop would not close them. Both points are read from source, UNTESTED. Adds `hyper-util`, `hyper-rustls`, `tower-service` (3 crates). About 40 lines in all. | `hyper-util-0.1.21/src/client/legacy/connect/http.rs:559-560`; `dns.rs:189-203`; `hyper-rustls-0.27.10/src/connector/builder.rs:154, 208, 252, 359` |
| (c) `lookup_host` + `TcpStream` + `tokio-rustls` + `http1::handshake` | **Recommended.** Compiled and run in p2 and p3. Resolve, judge every address, `TcpStream::connect(&addrs[..])` over only those addresses, optional `TlsConnector::connect(ServerName::try_from(host), tcp)` (SNI from the host name), `http1::handshake(TokioIo::new(io))`, spawn the driver with `wasmtime_wasi::runtime::spawn` and return it as the second future, set the URI to the path only (wasmtime-wasi-http has already set the `Host` header; the request head seen by a local server had `host:`). About 25 to 30 lines for connect, TLS and handshake inside the 40-line `send`. Direct dependencies: `tokio-rustls` (features `ring`, `tls12`) and `webpki-roots`. | Section 9.2 lines 99 to 132; `host/tests/net.rs` |
| Same shape as Spin | Spin does the same: `lookup_host`, remove blocked addresses, connect to what is left. | `spin-src/crates/factor-outbound-http/src/wasi.rs:590-625` (Spin repo, tree 32d1fe70) |
| rustls provider | `ClientConfig::builder_with_provider(Arc::new(default_provider()))` with `default_provider` from `tokio_rustls::rustls::crypto::ring`, so no direct `rustls` dependency. Named explicitly because M3's `aws` feature pulls `aws-lc-rs` as well. | Section 9.2 lines 81 to 86; `object_store-0.14.2/Cargo.toml:44-49` |
| Body conversion | Request: wasmtime hands over `Request<WasiBody>`, which hyper's `SendRequest<WasiBody>` accepts (`WasiBody` is a `Body`). Response: `res.map(\|b\| b.map_err(Error::from).boxed_unsync())`. | Section 9.2 line 131; compiled |
| Real hosts | `http://example.com` and `https://example.com` work in p2 and p3. 20 sequential fetches pass in p2 and p3 (see section 7). | `host/tests/net.rs` `outbound` |
| Deadline | A request counts against the 10 s deadline. A server that accepts and never answers: guest 500 at 10.003 s, p2 and p3. `RequestOptions` timeouts are ignored on purpose. | `host/tests/net.rs` |

## 4. Allow list (Spin subset)

| Item | Finding | Evidence |
|---|---|---|
| Spin's grammar | `scheme://host[:port]`, each part may be `*`. `*://*:*` allows all. A bare `*` and `insecure:allow-all` are rejected with a message that points to `*://*:*`. `*.example.com` is `AnySubdomain(".example.com")`: it matches a host that ends with `.example.com` and **not** the bare `example.com`. Default ports: http 80, https 443 (and postgres, mysql, redis, mqtt). Port ranges `a..b` are end-exclusive. Spin also has CIDR hosts, `self`, templates and service chaining; torpor's subset leaves them out. | `spin-src/crates/outbound-networking-config/src/allowed_hosts.rs:121-122, 253, 294, 320, 426-436, 529-533` (Spin tree 32d1fe70) |
| Spin's request match | Spin parses the request URL with `url::Url`, so a numeric host is normalised (`127.1` becomes `127.0.0.1`) before matching. Ours matches the literal text of `http::Uri`, which is stricter, and the address filter judges the resolved address anyway. Spin semantics were read, not run against a Spin host (UNTESTED). | `allowed_hosts.rs`; the `url` run in section 4 |
| `url` on allow items | `url::Url::parse` fails on `*://*:*` ("relative URL without a base"), `http://*:*` and `https://example.com:*` ("invalid port number"). It accepts `https://*`, `https://*.example.com`, `https://*:8443`, `https://a*.example.com` (a `*` inside a label, which Spin rejects with "wildcards are allowed only as subdomains", `allowed_hosts.rs:297-298`, read not run) and `http://[2606:4700::1111]:80`. It drops a default port (`port()` is `None`, so use `port_or_known_default()`) and lowercases the host. It also turns `127.1`, `0x7f.1` and `2130706433` into `127.0.0.1`. So the wildcard scheme and wildcard port items must be split by hand first. That is the matcher, so `url` saves nothing. | `probe/tests/url.rs`, run 2026-10-01, `url` 2.5.8 |
| `url` in the tree | `url` 2.5.8 is already in the M1 tree and is a hard dependency of `object_store`. No new crate either way. | `tree-m1-lib.txt:138` |
| The matcher | 36 lines including `parse`, on `http::Uri` (already a dependency). `parse` splits at `://` and at the last `:`, defaults the port from the scheme (`http` 80, `https` 443, anything else must give a port), allows `*` for the port, and checks the host characters. `allows` takes the URI's scheme and host (host lowercased), the port or its default, and compares scheme, port and host (`*`, `*.suffix` with at least one label in front, or exact). | Section 9.2 lines 23 to 61 |
| Tested | Unit test `allow_matcher` passes: exact host, wrong scheme, wrong port, subdomain, `example.com.evil.com`, `https://example.com@evil.com/` denied and `https://evil.com@example.com/` allowed (the host is what follows the `@`), `evilexample.com`, `https://*.example.com` does not match `https://example.com`, `*://*:*`, `http://localhost:3000`, `https://*:8443`, bracketed IPv6 with a port, uppercase host, and bad items rejected (no scheme, bare `*`, empty, `https://` with no host, `ftp://` with no port, a `*` inside a label, `*.*`, a path, a port over 65535, userinfo, bracketed IPv6 without a port). | `sketch/src/outbound.rs` `allow_matcher` (unit test, passes) |
| Deny by default | An app that lists no hosts has an empty `Vec<Allow>`, and `any` over it is `false`, so every request returns `HttpRequestDenied`. Tested in p2 and p3. | `send` in section 9.2 line 100; `host/tests/net.rs` |
| Edge cases | IPv6 literal items need a port (`http://[2606:4700::1111]:80`) because the last `:` splits the port. `https://[::1]` is rejected by `parse` for that reason. A trailing dot (`example.com.`) does not match `example.com`. An allowed hostname is matched before DNS, so the allow list names hosts, and the address filter (section 5) is what stops a name that resolves to a private address. | `allow_matcher` test |

## 5. Address filter

| Item | Finding | Evidence |
|---|---|---|
| Blocked set | IPv4: `is_private` (10/8, 172.16/12, 192.168/16), `is_loopback`, `is_link_local` (169.254/16, which holds the metadata endpoints 169.254.169.254 and 169.254.170.2), `is_multicast`, first octet 0 or 240 and up (this network, reserved, broadcast), 100.64/10 (shared), 192.0.0.0/24 and 198.18/15 (benchmarking). IPv6: everything outside 2000::/3, plus 2002::/16 (6to4 embeds an IPv4 address). `to_canonical()` first, so `::ffff:a.b.c.d` is judged as the IPv4 address. IPv4-compatible `::a.b.c.d` and NAT64 `64:ff9b::/96` are outside 2000::/3, so they are blocked too. | Section 9.2 lines 63 to 76 |
| Stable methods | `Ipv4Addr::{is_private, is_loopback, is_link_local, is_multicast}` and `IpAddr::to_canonical` are stable. `is_shared`, `is_reserved`, `is_benchmarking` and `is_global` are unstable (E0658; compiled in `stdip/`), hence the three octet tests. | `stdip/b.rs` (E0658 for each of the four) and `stdip/a.rs` (the stable set compiles) |
| Shortest check | `blocked` is 12 lines. | Section 9.2 |
| `ip_network` 0.4.1 | No dependencies, +1 crate. `!IpNetwork::from(ip.to_canonical()).is_global()` is one line, but a differential run against our table showed it treats `::127.0.0.1`, NAT64 `64:ff9b::/96`, 6to4 `2002::/16` and IPv4 multicast as global. Spin uses `ip_network`'s `is_global` and adds an IPv4-compatible recursion for that reason. Hand-roll: no crate, and a clear set. | `ip_network-0.4.1`; `spin-src/crates/outbound-networking-config/src/blocked_networks.rs:16-26, 43-55`; `probe/tests/filter.rs` |
| Table test | Unit test `blocked_table`: 31 addresses blocked, 11 allowed (8.8.8.8, 1.1.1.1, 2606:4700::1111, `::ffff:8.8.8.8`, 172.32.0.1, 100.128.0.1, 100.63.255.255, 192.169.0.1, 198.20.0.1, 223.255.255.255, 2a00:1450:4001::1). Passes. | `sketch/src/outbound.rs` `blocked_table` |
| Plan's targets | Tested through a guest in p2 and p3: `127.0.0.1`, `[::1]`, `[::ffff:127.0.0.1]`, `169.254.169.254`, `10.0.0.1` and also `192.168.1.1`, `127.1`, `2130706433`, `0x7f.1`, `localhost`, `0.0.0.0`, `[fd00::1]`, `100.64.0.1`, `[::ffff:169.254.169.254]`, `rebind.test`, `mixed.test`: all `DestinationIpProhibited`. (The hosts under test were allowed by the allow list, so the filter alone decided.) | `host/tests/net.rs` |
| DNS pointed at a private address | The container's `/etc/hosts` gave `rebind.test` the address 10.0.0.5, and `mixed.test` both 10.0.0.5 and a public address. `getaddrinfo` reads it, so the resolver path was the real one. Both are blocked. | `host/tests/net.rs`; run with the `/etc/hosts` lines added |
| Any or all | We reject when **any** resolved address is blocked, so a name with one private record never reaches the network. Spin removes the blocked addresses, dials the rest and errors only if all are blocked. Ours is stricter. | `spin-src/crates/factor-outbound-http/src/lib.rs:215-232`; `wasi.rs:606-619` |
| Numeric hosts | `getaddrinfo` normalises `127.1`, `0x7f.1` and `2130706433` to 127.0.0.1. So the text is never judged: the resolved address is. | `host/tests/net.rs` |
| IP literals | A literal skips DNS: `host.parse::<IpAddr>()` after trimming `[` and `]`. `lookup_host(("[::1]", port))` would fail, hence the trim. | Section 9.2 lines 104 to 113 |
| Dialled addresses | `TcpStream::connect(&addrs[..])` takes the checked list, so there is no second lookup to rebind. | Section 9.2 line 117 |

## 6. `wasi:config`

| Item | Finding | Evidence |
|---|---|---|
| Crate | `wasmtime-wasi-config` 49.0.1. `WasiConfigVariables` is a `HashMap` wrapper and is not `Clone`. `WasiConfig::from(&vars)` borrows it. `add_to_linker(&mut Linker<T>, fn(&mut T) -> WasiConfig<'_>)`. | `wasmtime-wasi-config-49.0.1/src/lib.rs:112, 141` |
| Version | Its WIT is `include wasi:config/imports@0.2.0-rc.1`, the same as the vendored `wit/config/`. | `wasmtime-wasi-config-49.0.1/wit/world.wit:5`; `wit/config/{store,world}.wit` |
| Linking | M1 already links it: `wasmtime_wasi_config::add_to_linker(&mut linker, \|h: &mut Host\| WasiConfig::from(&h.app.config))`. The `/config?key=K` route (`get`) and the `/config` route (`get-all`) pass in the p2, p3 and Spin SDK guests. | M1 `src/engine.rs:39`; `sketch/tests/m2.rs` `every_op_in_every_guest` |
| M2 code | None. The plan's budget of about 30 lines is not needed. Keys come from the manifest's flat `[config]` plus `TORPOR_VAR_*`, already in `serve.rs`. | M1 `src/serve.rs` |

## 7. ResourceTable cap of 256

| Item | Finding | Evidence |
|---|---|---|
| Rule | `set_max_capacity(RESOURCES)`. `push` reuses a free slot first and fails with `Full` ("resource table has no free keys") only when `entries.len() >= max_capacity` and no slot is free. Deleted slots are free at once (`DELETE_WITH_TOMBSTONE = false`). So the cap bounds the number of live resources, not the number pushed in all. | `wasmtime-49.0.1/src/runtime/component/resource_table.rs:12, 25, 68, 140, 215-227` |
| How measured | Binary search of the smallest cap that lets one request finish (cap N allows N live entries), in p2 and p3, with the `host/` crate's test-only `CAP`. | `host/tests/res.rs` |
| p2 | A hello request needs 4. One normal fetch needs 6. A guest that never drops any handle needs 10, 16 and 124 for 1, 2 and 20 fetches, so 6 more per leaked fetch (4 + 6n). | `host/tests/res.rs` |
| p3 | A hello request needs 2. A fetch needs 2. A guest that never drops needs 3, 4 and 22 for 1, 2 and 20 fetches (2 + n). | `host/tests/res.rs` |
| KV | A bucket handle and a cas handle are one slot each while live. The `kv-p3` and Spin fixtures need 2 (3 while a cas is held). | `host/tests/res.rs` `table_slots`; `sketch/tests/m2.rs` |
| Verdict | 256 is safe for 20 sequential fetches plus 100 KV operations. Worst case, a p2 guest that leaks everything: 124 + 100 = 224. A normal guest uses a handful. A JavaScript (StarlingMonkey) guest may free its handles only when its collector runs, so a burst could hold more slots than the Rust figures. UNTESTED: only Rust guests were run. | Computed from the rows above |
| When full | A failed push in the KV host returns `Error::Other("resource table has no free keys")`, a value the guest can handle. A failed push inside `wasi:http` traps, and the host answers 500. Tested in p2. | `sketch/tests/m2.rs` `errors_are_values_not_traps`; `host/tests/res.rs` |

## 8. Guests: the Spin SDK and plain `wit-bindgen`

| Item | Finding | Evidence |
|---|---|---|
| Latest Spin SDK (Rust) | `spin-sdk` 7.0.0 (2026-08-25), the version in the existing fixture. It re-exports `wasip3` and `wit_bindgen` (`pub use wasip3::{self, wit_bindgen}`), so `spin_sdk::wit_bindgen` is a `wit-bindgen` 0.57.1 here. | `spin-sdk-7.0.0/src/lib.rs:79`; `tests/components/spin/Cargo.lock` |
| Default features | `default` is `http`, `key-value`, `json`, `llm`, `mqtt`, `mysql`, `pg`, `postgres4-types`, `redis`, `sqlite`, `variables`, `export-sdk-language`. Most of them import `spin:*` interfaces, which torpor does not provide. Use `default-features = false, features = ["http"]`. | `spin-sdk-7.0.0/Cargo.toml:41-55` |
| SDK key-value | The SDK's `key_value` module imports `spin:*`. The hidden `experimental` module (`#[doc(hidden)]`, world `spin-sdk-experimental`) imports `wasi:keyvalue@0.2.0-draft2` and links on our host: tested with the `spinx` probe (all operations work). | `spin-sdk-7.0.0/src/lib.rs:81-93`; `wit/sdk.wit:45`; `spinx/` |
| SDK config | The same world imports `wasi:config/store@0.2.0-draft-2024-09-27`. That does **not** link on our host, which has rc.1: instantiation fails on the missing import (tested with `spinx2`). So the SDK's own config cannot be used. | `spin-sdk-7.0.0/wit/sdk.wit:44`; `spinx2/` |
| Way out | Use the SDK only for HTTP (`#[http_service]`) and call `spin_sdk::wit_bindgen::generate!` for the two interfaces, with `runtime_path: "::spin_sdk::wit_bindgen::rt"` so it uses the SDK's runtime, and `generate_all`. The existing fixture does exactly this and passes all operations. The inline world: `include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1;`. | Section 9.5; `tests/components/spin/src/lib.rs`; `every_op_in_every_guest` |
| Plain `wit-bindgen` | `wit-bindgen` 0.62.0 with the same inline world and `generate_all`, next to a `wasip2` (p2 HTTP) or `wasip3` (p3 HTTP) guest. Both pass. No SDK is needed. | `tests/components/rust/src/lib.rs`, `Cargo.toml`; section 9.5 |
| Versions seen | `wit-bindgen` 0.62.0 (plain), 0.57.1 (inside the SDK), `wasip3` 0.7.1 (SDK) and 0.9.0 (latest, used by the plain guest), `wasip2` 2. | `Cargo.lock` files of the fixtures |

## 9. Sketches (compile on 49.0.1)

The code below is `sketch/src/kv.rs` and `sketch/src/outbound.rs` with the test modules cut off. The sketch crate is the M1 worktree plus the M2 files. Its 7 integration tests and 3 unit tests pass. There are no decision numbers in the comments. Limits are named constants. The line numbers in the evidence columns above refer to these listings.

### 9.1 `src/kv.rs` (330 lines, 294 of code)

```rust
//! `wasi:keyvalue@0.2.0-draft2` over an object store: one object per key, and one generation object per bucket for listing.
use crate::guest::Host;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    Error as E, GetOptions, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path,
};
use percent_encoding::percent_decode_str;
use std::{
    collections::HashMap,
    fmt::Display,
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};
use wasmtime::component::Resource;

const KEY_MAX: usize = 256; // bytes, before percent-encoding
const VALUE_MAX: usize = 1 << 20;
const BUCKET_MAX: usize = 64; // bytes in a bucket name
const FRESH: Duration = Duration::from_secs(1); // how long a cached value or list is served without asking the store
const PAGE: usize = 1000; // keys per `list-keys`, the most S3 gives for one LIST
const CACHE_MAX: usize = 32 << 20; // bytes of cached values per app
const RETRIES: usize = 16; // CAS attempts for one `increment`

wasmtime::component::bindgen!({
    path: "wit/keyvalue",
    world: "imports",
    imports: { default: async },
    with: { "wasi:keyvalue/store.bucket": Bucket, "wasi:keyvalue/atomics.cas": Cas },
});
use wasi::keyvalue::{
    atomics::{self, CasError},
    batch,
    store::{self, Error, KeyResponse},
};
type R<T> = Result<T, Error>;

fn other(e: impl Display) -> Error {
    Error::Other(e.to_string())
}

/// The handle for an opened bucket: its name.
pub struct Bucket(String);

/// What a `cas` handle saw: the value and ETag at the time, which `swap` sends back as `If-Match`.
pub struct Cas {
    bucket: String,
    path: Path,
    seen: Option<Bytes>,
    etag: Option<String>,
}

/// One app's view of the object store. Shared by all of its requests, so the caches outlive a request.
pub(crate) struct Kv {
    store: Arc<dyn ObjectStore>,
    app: String,
    cache: Mutex<Cache>,
}
#[derive(Default)]
struct Cache {
    values: HashMap<Path, Seen>,
    bytes: usize,
    lists: HashMap<String, Listing>,
}
struct Seen {
    value: Option<Bytes>,
    etag: Option<String>,
    at: Instant,
}
#[derive(Default)]
struct Listing {
    generation: Option<String>, // ETag of the generation object when the pages were listed
    checked: Option<Instant>,
    pages: HashMap<Option<String>, KeyResponse>, // by cursor
}

impl Kv {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, app: &str) -> Self {
        Self { store, app: app.into(), cache: Default::default() }
    }

    fn path(&self, bucket: &str, key: &str) -> R<Path> {
        if key.is_empty() || key.len() > KEY_MAX {
            return Err(other(format!("a key is 1 to {KEY_MAX} bytes"))); // an empty key would be the bucket's own prefix
        }
        Ok(Path::from_iter(["kv", &self.app, bucket, key])) // each part is percent-encoded, so `/` stays inside it
    }

    fn remember(&self, p: Path, value: Option<Bytes>, etag: Option<String>) {
        let mut c = self.cache.lock().unwrap();
        c.bytes += p.as_ref().len() + value.as_ref().map_or(0, Bytes::len);
        if c.bytes > CACHE_MAX {
            c.values.clear(); // crude, and bounded
            c.bytes = 0;
        }
        c.values.insert(p, Seen { value, etag, at: Instant::now() });
    }

    /// The value and ETag now in the store; `None` when `tag` is still current.
    async fn fetch(&self, p: &Path, tag: Option<String>) -> R<Option<(Option<Bytes>, Option<String>)>> {
        match self.store.get_opts(p, GetOptions { if_none_match: tag, ..Default::default() }).await {
            Ok(r) => {
                let etag = r.meta.e_tag.clone();
                Ok(Some((Some(r.bytes().await.map_err(other)?), etag)))
            }
            Err(E::NotModified { .. }) => Ok(None),
            Err(E::NotFound { .. }) => Ok(Some((None, None))),
            Err(e) => Err(other(e)),
        }
    }

    /// Cached for `FRESH`, then revalidated with `If-None-Match`. A miss is cached too.
    async fn get(&self, p: &Path) -> R<Option<Bytes>> {
        let old = self.cache.lock().unwrap().values.get(p).map(|s| (s.value.clone(), s.etag.clone(), s.at));
        if let Some((value, _, at)) = &old
            && at.elapsed() < FRESH
        {
            return Ok(value.clone());
        }
        let tag = old.as_ref().and_then(|o| o.1.clone());
        let (value, etag) = self.fetch(p, tag).await?.unwrap_or_else(|| old.map(|o| (o.0, o.1)).unwrap_or_default());
        self.remember(p.clone(), value.clone(), etag);
        Ok(value)
    }

    /// Writes every item (`None` deletes), the data first and then the bucket's generation, once.
    async fn write(&self, bucket: &str, items: Vec<(String, Option<Vec<u8>>)>) -> R<()> {
        for (key, value) in items {
            let p = self.path(bucket, &key)?;
            match value {
                Some(v) => drop(self.put(&p, v, PutMode::Overwrite).await?),
                None => {
                    self.store.delete(&p).await.map_err(other)?;
                    self.remember(p, None, None);
                }
            }
        }
        self.touch(bucket).await
    }

    /// Our own writes go into the cache, so we read them back at once. `false` when the condition in `mode` failed.
    async fn put(&self, p: &Path, v: Vec<u8>, mode: PutMode) -> R<bool> {
        if v.len() > VALUE_MAX {
            return Err(other(format!("a value is {VALUE_MAX} bytes or less")));
        }
        let v = Bytes::from(v);
        match self.store.put_opts(p, v.clone().into(), PutOptions { mode, ..Default::default() }).await {
            Ok(r) => {
                self.remember(p.clone(), Some(v), r.e_tag);
                Ok(true)
            }
            Err(E::Precondition { .. } | E::AlreadyExists { .. }) => Ok(false),
            Err(e) => Err(other(e)),
        }
    }

    /// Overwrites the generation with something new, since S3's ETag is a hash of the body, and forgets our listings.
    async fn touch(&self, bucket: &str) -> R<()> {
        let now = UNIX_EPOCH.elapsed().map_err(other)?.as_nanos().to_string();
        self.store.put(&Path::from_iter(["kvgen", &self.app, bucket]), now.into()).await.map_err(other)?;
        self.cache.lock().unwrap().lists.remove(bucket);
        Ok(())
    }

    /// Reads from the store itself, never the cache, so that the ETag is the latest.
    async fn cas(&self, bucket: &str, path: Path) -> R<Cas> {
        let (seen, etag) = self.fetch(&path, None).await?.unwrap_or_default();
        Ok(Cas { bucket: bucket.into(), path, seen, etag })
    }

    /// `false` when somebody else wrote since `c` was read.
    async fn swap(&self, c: &Cas, v: Vec<u8>) -> R<bool> {
        let mode = match &c.etag {
            Some(tag) => PutMode::Update(UpdateVersion { e_tag: Some(tag.clone()), version: None }),
            None => PutMode::Create,
        };
        let won = self.put(&c.path, v, mode).await?;
        if won {
            self.touch(&c.bucket).await?;
        }
        Ok(won)
    }

    /// One page of keys in order. The generation object says whether a cached page is still good, and costs a GET, not a LIST.
    async fn list(&self, bucket: &str, cursor: Option<String>) -> R<KeyResponse> {
        let (tag, due) = match self.cache.lock().unwrap().lists.get(bucket) {
            Some(l) => (l.generation.clone(), l.checked.is_none_or(|t| t.elapsed() >= FRESH)),
            None => (None, true),
        };
        if due {
            let opts = GetOptions { if_none_match: tag.clone(), ..Default::default() };
            let new = match self.store.get_opts(&Path::from_iter(["kvgen", &self.app, bucket]), opts).await {
                Ok(r) => r.meta.e_tag,
                Err(E::NotModified { .. } | E::NotFound { .. }) => tag.clone(),
                Err(e) => return Err(other(e)),
            };
            let mut c = self.cache.lock().unwrap();
            let l = c.lists.entry(bucket.into()).or_default();
            if new != tag {
                *l = Listing { generation: new, ..Default::default() };
            }
            l.checked = Some(Instant::now());
        }
        if let Some(page) = self.cache.lock().unwrap().lists.get(bucket).and_then(|l| l.pages.get(&cursor)) {
            return Ok(page.clone());
        }
        let prefix = Path::from_iter(["kv", &self.app, bucket]);
        let objects = match &cursor {
            Some(last) => self.store.list_with_offset(Some(&prefix), &self.path(bucket, last)?),
            None => self.store.list(Some(&prefix)),
        };
        let name = |m: ObjectMeta| {
            percent_decode_str(m.location.filename().unwrap_or_default()).decode_utf8().map(|s| s.into_owned())
        };
        let mut keys: Vec<String> =
            objects.take(PAGE + 1).map(|m| name(m.map_err(other)?).map_err(other)).try_collect().await?;
        let more = keys.len() > PAGE; // one extra says there is a next page
        keys.truncate(PAGE);
        let page = KeyResponse { cursor: more.then(|| keys[PAGE - 1].clone()), keys };
        let mut c = self.cache.lock().unwrap();
        c.lists.entry(bucket.into()).or_default().pages.insert(cursor, page.clone());
        Ok(page)
    }
}

impl Host {
    fn at(&self, b: &Resource<Bucket>) -> R<(&Kv, &str)> {
        Ok((&self.app.kv, &self.table.get(b).map_err(other)?.0))
    }
}

impl store::Host for Host {
    async fn open(&mut self, name: String) -> R<Resource<Bucket>> {
        let ok = name.len() <= BUCKET_MAX
            && !name.is_empty()
            && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
        if !ok {
            return Err(Error::NoSuchStore);
        }
        self.table.push(Bucket(name)).map_err(other)
    }
}

impl store::HostBucket for Host {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> R<Option<Vec<u8>>> {
        let (kv, bucket) = self.at(&b)?;
        Ok(kv.get(&kv.path(bucket, &key)?).await?.map(|v| v.to_vec()))
    }
    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, vec![(key, Some(value))]).await
    }
    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, vec![(key, None)]).await
    }
    async fn exists(&mut self, b: Resource<Bucket>, key: String) -> R<bool> {
        let (kv, bucket) = self.at(&b)?;
        Ok(kv.get(&kv.path(bucket, &key)?).await?.is_some())
    }
    async fn list_keys(&mut self, b: Resource<Bucket>, cursor: Option<String>) -> R<KeyResponse> {
        let (kv, bucket) = self.at(&b)?;
        kv.list(bucket, cursor).await
    }
    async fn drop(&mut self, b: Resource<Bucket>) -> wasmtime::Result<()> {
        self.table.delete(b).map(|_| ()).map_err(Into::into)
    }
}

impl batch::Host for Host {
    async fn get_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<Vec<(String, Option<Vec<u8>>)>> {
        let (kv, bucket) = self.at(&b)?;
        let mut out = vec![];
        for key in keys {
            let value = kv.get(&kv.path(bucket, &key)?).await?;
            out.push((key, value.map(|v| v.to_vec())));
        }
        Ok(out)
    }
    async fn set_many(&mut self, b: Resource<Bucket>, items: Vec<(String, Vec<u8>)>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, items.into_iter().map(|(k, v)| (k, Some(v))).collect()).await
    }
    async fn delete_many(&mut self, b: Resource<Bucket>, keys: Vec<String>) -> R<()> {
        let (kv, bucket) = self.at(&b)?;
        kv.write(bucket, keys.into_iter().map(|k| (k, None)).collect()).await
    }
}

impl atomics::Host for Host {
    async fn increment(&mut self, b: Resource<Bucket>, key: String, delta: i64) -> R<i64> {
        let (kv, bucket) = self.at(&b)?;
        let path = kv.path(bucket, &key)?;
        for _ in 0..RETRIES {
            let cas = kv.cas(bucket, path.clone()).await?;
            let now = match &cas.seen {
                Some(v) => i64::from_le_bytes((&v[..]).try_into().map_err(|_| other("not a counter"))?), // as Spin stores it
                None => 0,
            };
            let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
            if kv.swap(&cas, next.to_le_bytes().to_vec()).await? {
                return Ok(next);
            }
        }
        Err(other("too much contention"))
    }
    async fn swap(&mut self, c: Resource<Cas>, value: Vec<u8>) -> Result<(), CasError> {
        let cas = self.table.delete(c).map_err(|e| CasError::StoreError(other(e)))?;
        if self.app.kv.swap(&cas, value).await.map_err(CasError::StoreError)? {
            return Ok(());
        }
        // lost: hand back a handle that sees the winner's value
        let fresh = self.app.kv.cas(&cas.bucket, cas.path).await.map_err(CasError::StoreError)?;
        Err(self.table.push(fresh).map_or_else(|e| CasError::StoreError(other(e)), CasError::CasFailed))
    }
}

impl atomics::HostCas for Host {
    async fn new(&mut self, b: Resource<Bucket>, key: String) -> R<Resource<Cas>> {
        let (kv, bucket) = self.at(&b)?;
        let cas = kv.cas(bucket, kv.path(bucket, &key)?).await?;
        self.table.push(cas).map_err(other)
    }
    async fn current(&mut self, c: Resource<Cas>) -> R<Option<Vec<u8>>> {
        Ok(self.table.get(&c).map_err(other)?.seen.as_ref().map(|v| v.to_vec()))
    }
    async fn drop(&mut self, c: Resource<Cas>) -> wasmtime::Result<()> {
        self.table.delete(c).map(|_| ()).map_err(Into::into)
    }
}
```

What the sketch does, and what it cost in store calls (measured with a counting store on InMemory, `sketch/tests/m2.rs` `op_costs`, `cache_and_etag_costs`, `list_costs_and_freshness`):

| Operation | Store calls | Notes |
|---|---|---|
| `set` | PUT data, PUT generation | Data first, then the generation, so a list never misses a key that exists. A crash between the two leaves the list stale until the next write. |
| `get` after own write | 0 | Read your writes: `put` caches the value and ETag. |
| `get`, first, another app | 1 GET | The cache is per app, in `Shared`. |
| `get` within 1 s | 0 | `FRESH`. A miss is cached too. |
| `get` after 1 s | GET with `If-None-Match` | `NotModified` costs no body. |
| `delete` | DELETE, PUT generation | |
| `increment` | GET, PUT, PUT generation | A CAS loop of up to `RETRIES`. Counters are 8-byte little-endian, as Spin stores them. |
| `cas::new` | 1 GET | Always the store itself, never the cache, so the ETag is the latest. |
| `swap` | conditional PUT, PUT generation | A lost swap costs one more GET to build the handle in `CasFailed`. |
| `set-many` of N | N PUTs, 1 PUT generation | |
| `list-keys`, first | GET generation, LIST | |
| `list-keys` within 1 s | 0 | |
| `list-keys` after 1 s, unchanged | 1 conditional GET of the generation | No LIST. |
| `list-keys` after 1 s, changed | GET generation, LIST | |
| `list-keys` after own write | LIST | `touch` drops this app's cached lists, so the writer sees its own write. Other hosts lag by up to 1 s. |

Checked behaviour: all operations in p2, p3 and Spin guests; paging 1500 keys gives 1000 and a cursor, then 500; invalid bucket name gives `NoSuchStore`; an empty key or a key over 256 bytes gives `Error::Other`; a counter on text gives `Other("not a counter")`; the first `increment` of an absent key gives 1; table-full and oversize values are errors, not traps.

### 9.2 `src/outbound.rs` (132 lines, 116 of code)

```rust
//! Outbound HTTP: the allow list, then name lookup and TCP by hand, so only an address that passed the block check is dialled.
use crate::guest::{Fut, Shared};
use http_body_util::BodyExt;
use hyper::Uri;
use std::{
    net::IpAddr,
    sync::{Arc, LazyLock},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, lookup_host},
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, crypto::ring::default_provider, pki_types::ServerName},
};
use wasmtime_wasi_http::{
    Error, RequestOptions, WasiHttpHooks,
    handler::{Request, Response},
    io::TokioIo,
};

/// One allow-list item, `scheme://host[:port]`. A scheme, host or port of `*` matches any, and a host may start with `*.`.
pub(crate) struct Allow {
    scheme: String,
    host: String,
    port: Option<u16>, // None is any
}

impl Allow {
    pub(crate) fn parse(item: &str) -> Result<Self, String> {
        let bad = || format!("bad allowed host {item:?}");
        let (scheme, rest) = item.split_once("://").ok_or_else(bad)?;
        let (host, port) = rest.rsplit_once(':').unwrap_or((rest, ""));
        let port = match (port, scheme) {
            ("*", _) => None,
            ("", "http") => Some(80),
            ("", "https") => Some(443),
            (p, _) => Some(p.parse().map_err(|_| bad())?),
        };
        let name = host.strip_prefix("*.").unwrap_or(host);
        if host != "*" && (name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.[]:".contains(&b)))
        {
            return Err(bad());
        }
        Ok(Self { scheme: scheme.into(), host: host.to_ascii_lowercase(), port })
    }

    fn allows(&self, uri: &Uri) -> bool {
        let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else { return false };
        let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });
        let host = host.to_ascii_lowercase();
        (self.scheme == "*" || self.scheme == scheme)
            && self.port.is_none_or(|p| p == port)
            && match self.host.strip_prefix('*') {
                Some("") => true,
                Some(suffix) => host.len() > suffix.len() && host.ends_with(suffix),
                None => host == self.host,
            }
    }
}

/// True for everything that is not a public address: private, loopback, link-local (the metadata endpoints), shared,
/// reserved and multicast IPv4, and IPv6 outside 2000::/3. An IPv4-mapped IPv6 address is judged as its IPv4 address.
fn blocked(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            let o = a.octets();
            a.is_private() || a.is_loopback() || a.is_link_local() || a.is_multicast() || o[0] == 0 || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64) // 100.64.0.0/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && o[1] & 0xfe == 18) // 198.18.0.0/15
        }
        IpAddr::V6(a) => a.segments()[0] & 0xe000 != 0x2000 || a.segments()[0] == 0x2002, // 2002::/16 embeds an IPv4 address
    }
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

static TLS: LazyLock<TlsConnector> = LazyLock::new(|| {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.into() };
    let provider = Arc::new(default_provider()); // named, because a second provider in the build would make the default ambiguous
    let config = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap();
    Arc::new(config.with_root_certificates(roots).with_no_client_auth()).into()
});

/// The hooks of one store.
pub(crate) struct Outbound(pub(crate) Arc<Shared>);

impl WasiHttpHooks for Outbound {
    /// Timeouts are the store's 10 s deadline, so `RequestOptions` is ignored.
    fn send_request(&mut self, req: Request, _: Option<RequestOptions>, _: Fut<()>) -> Fut<(Response, Fut<()>)> {
        let app = self.0.clone();
        Box::new(async move { send(&app, req).await })
    }
}

async fn send(app: &Shared, mut req: Request) -> Result<(Response, Fut<()>), Error> {
    if !app.allow.iter().any(|a| a.allows(req.uri())) {
        return Err(Error::HttpRequestDenied);
    }
    let tls = req.uri().scheme_str() == Some("https");
    let host = req.uri().host().unwrap_or_default().trim_matches(['[', ']']).to_owned(); // `[::1]` to `::1`
    let port = req.uri().port_u16().unwrap_or(if tls { 443 } else { 80 });
    // Resolve here, judge every address, and dial only those addresses: there is no second lookup to rebind.
    let addrs: Vec<_> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![(ip, port).into()],
        Err(_) => lookup_host((host.as_str(), port))
            .await
            .map_err(|_| Error::DnsError { rcode: None, info_code: None })?
            .collect(),
    };
    if addrs.iter().any(|a| blocked(a.ip())) {
        return Err(Error::DestinationIpProhibited);
    }
    let tcp = TcpStream::connect(&addrs[..]).await.map_err(Error::Connect)?;
    let io: Box<dyn Io> = match tls {
        true => Box::new(
            TLS.connect(ServerName::try_from(host).map_err(|_| Error::HttpRequestUriInvalid)?, tcp)
                .await
                .map_err(Error::Tls)?,
        ),
        false => Box::new(tcp),
    };
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    let driver = wasmtime_wasi::runtime::spawn(conn); // stops when the response is dropped, which is when the store is
    *req.uri_mut() =
        req.uri().path_and_query().map_or("/", |p| p.as_str()).parse().map_err(|_| Error::HttpRequestUriInvalid)?; // the wire wants the path only
    let res = sender.send_request(req).await?;
    Ok((res.map(|b| b.map_err(Error::from).boxed_unsync()), Box::new(async move { driver.await.map_err(Error::from) })))
}
```

### 9.3 Wiring (diff against the M1 worktree at `4075986`)

`src/lib.rs` gains two lines, `mod kv;` and `mod outbound;`. The diffs for `guest.rs`, `engine.rs` and `Cargo.toml` are exact (taken from the compiled sketch):

```diff
--- a/src/guest.rs
+++ b/src/guest.rs
@@ -1,2 +1,4 @@
 //! Runs each request in a fresh store, under hard limits: 10 s, 256 MiB, nothing inherited, no outbound HTTP.
+use crate::kv::Kv;
+use crate::outbound::{Allow, Outbound};
 use http_body_util::BodyExt;
@@ -11,5 +13,5 @@
 use wasmtime_wasi_config::WasiConfigVariables;
-use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Request, Response, ShouldAccept};
+use wasmtime_wasi_http::handler::{HandlerState, Instance, ProxyHandler, ProxyPre, Response, ShouldAccept};
 use wasmtime_wasi_http::handler::{WorkerExpiration, WorkerState, WorkerStatus};
-use wasmtime_wasi_http::{Error, RequestOptions, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};
+use wasmtime_wasi_http::{Error, WasiHttpCtx, WasiHttpCtxView, WasiHttpView};
 
@@ -25,2 +27,4 @@
     pub(crate) config: WasiConfigVariables,
+    pub(crate) kv: Kv,
+    pub(crate) allow: Vec<Allow>,
 }
@@ -29,6 +33,6 @@
 pub(crate) struct Host {
-    table: ResourceTable,
+    pub(crate) table: ResourceTable,
     wasi: WasiCtx,
     http: WasiHttpCtx,
-    hooks: Deny,
+    hooks: Outbound,
     pub(crate) app: Arc<Shared>,
@@ -69,10 +73,3 @@
 
-/// No outbound HTTP yet; M2's allow list goes in this `send_request`. Never `default_hooks()`: it ignores the socket checks.
-struct Deny;
-type Fut<T> = Box<dyn Future<Output = Result<T, Error>> + Send>;
-impl WasiHttpHooks for Deny {
-    fn send_request(&mut self, _: Request, _: Option<RequestOptions>, _: Fut<()>) -> Fut<(Response, Fut<()>)> {
-        Box::new(async { Err(Error::HttpRequestDenied) })
-    }
-}
+pub(crate) type Fut<T> = Box<dyn Future<Output = Result<T, Error>> + Send>;
 
@@ -121,4 +118,11 @@
 impl App {
-    pub(crate) fn new(name: &str, engine: Engine, pre: ProxyPre<Host>, config: BTreeMap<String, String>) -> Self {
-        let app = Arc::new(Shared { name: name.into(), config: config.into_iter().collect() });
+    pub(crate) fn new(
+        name: &str,
+        engine: Engine,
+        pre: ProxyPre<Host>,
+        config: BTreeMap<String, String>,
+        kv: Kv,
+        allow: Vec<Allow>,
+    ) -> Self {
+        let app = Arc::new(Shared { name: name.into(), config: config.into_iter().collect(), kv, allow });
         Self(ProxyHandler::new(State { engine, pre, app, permits: Arc::new(Semaphore::new(MAX_INFLIGHT)) }))
@@ -161,3 +165,3 @@
             http: WasiHttpCtx::new(),
-            hooks: Deny,
+            hooks: Outbound(self.app.clone()),
             app: self.app.clone(),
```

```diff
--- a/src/engine.rs
+++ b/src/engine.rs
@@ -5,3 +5,3 @@
     Config, Result,
-    component::{Component, Linker},
+    component::{Component, HasSelf, Linker},
 };
@@ -39,2 +39,3 @@
         wasmtime_wasi_config::add_to_linker(&mut linker, |h: &mut Host| WasiConfig::from(&h.app.config))?;
+        crate::kv::Imports::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |h| h)?;
         Ok(Self { engine, linker })
@@ -44,3 +45,15 @@
     /// exports either `wasi:http/handler` (p3) or `incoming-handler` (p2).
-    pub fn load(&self, name: &str, wasm: impl AsRef<[u8]>, config: BTreeMap<String, String>) -> Result<App> {
+    pub fn load(
+        &self,
+        name: &str,
+        wasm: impl AsRef<[u8]>,
+        config: BTreeMap<String, String>,
+        store: std::sync::Arc<dyn object_store::ObjectStore>,
+        allow: &[&str],
+    ) -> Result<App> {
+        let allow = allow
+            .iter()
+            .map(|a| crate::outbound::Allow::parse(a))
+            .collect::<Result<_, _>>()
+            .map_err(wasmtime::Error::msg)?;
         let pre = self.linker.instantiate_pre(&Component::new(&self.engine, wasm)?)?;
@@ -50,3 +63,3 @@
         };
-        Ok(App::new(name, self.engine.clone(), pre, config))
+        Ok(App::new(name, self.engine.clone(), pre, config, crate::kv::Kv::new(store, name), allow))
     }
```

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -27,5 +27,11 @@
 http-body-util = "0.1"
-hyper = "1"
+hyper = { version = "1", features = ["client", "http1"] }
+bytes = "1"
+futures-util = "0.3"
+object_store = { version = "=0.14.2", default-features = false }
+percent-encoding = "2"
+tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }
+webpki-roots = "1"
 serde = { version = "1", features = ["derive"], optional = true }
-tokio = { version = "1", features = ["sync", "time"] }
+tokio = { version = "1", features = ["sync", "time", "net", "rt"] }
 toml = { version = "1", optional = true }
@@ -45,3 +51,5 @@
 [dev-dependencies]
-tokio = { version = "1", features = ["io-util", "macros", "net", "rt-multi-thread"] }
+async-trait = "0.1"
+serde_json = "1"
+tokio = { version = "1", features = ["io-util", "macros", "net", "rt-multi-thread", "time"] }
 
```

`serve.rs` needs an allow-list field in the manifest and an object store for the dev mode. This diff is exact and was compile-checked (`cargo check --bins`, with the default features) against the M1 `serve.rs`; it was not run, because the sketch has no served-app test:

```diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -2,4 +2,5 @@
 use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
+use object_store::memory::InMemory;
 use serde::Deserialize;
-use std::{collections::BTreeMap, convert::Infallible, env, fs, path::Path, path::PathBuf};
+use std::{collections::BTreeMap, convert::Infallible, env, fs, path::Path, path::PathBuf, sync::Arc};
 use tokio::net::TcpListener;
@@ -18,2 +19,4 @@
     #[serde(default)]
+    allowed_hosts: Vec<String>, // `scheme://host[:port]`; none means no outbound HTTP
+    #[serde(default)]
     config: BTreeMap<String, String>,
@@ -22,6 +25,7 @@
 pub async fn run(dir: &Path, listener: TcpListener) -> Result<()> {
-    let Manifest { name, component, mut config } = toml::from_str(&fs::read_to_string(dir.join(MANIFEST))?)?;
+    let Manifest { name, component, allowed_hosts, mut config } = toml::from_str(&fs::read_to_string(dir.join(MANIFEST))?)?;
     // Secrets never go in the manifest: `TORPOR_VAR_<KEY>` sets `key` and overrides `[config]`.
     config.extend(env::vars().filter_map(|(k, v)| Some((k.strip_prefix(VAR_PREFIX)?.to_lowercase(), v))));
-    let app = Engine::new()?.load(&name, fs::read(dir.join(component))?, config)?;
+    let hosts: Vec<_> = allowed_hosts.iter().map(String::as_str).collect();
+    let app = Engine::new()?.load(&name, fs::read(dir.join(component))?, config, Arc::new(InMemory::new()), &hosts)?;
     loop {
```

`tests/m1.rs` calls `Engine::load` at two places (the `load` helper at line 9 and the `wat` test at line 71). Both need the two new arguments, an `InMemory` store and an empty allow list. Also exact and compile-checked (`cargo check --test m1`, not run):

```diff
--- a/tests/m1.rs
+++ b/tests/m1.rs
@@ -3,3 +3,4 @@
 use hyper::{StatusCode, body::Bytes};
-use std::{fs, time::Duration, time::Instant};
+use object_store::memory::InMemory;
+use std::{fs, sync::Arc, time::Duration, time::Instant};
 use torpor::{App, Engine};
@@ -8,3 +9,3 @@
     let wasm = fs::read(format!("tests/fixtures/{name}.wasm")).unwrap();
-    Engine::new().unwrap().load(name, wasm, Default::default()).unwrap()
+    Engine::new().unwrap().load(name, wasm, Default::default(), Arc::new(InMemory::new()), &[]).unwrap()
 }
@@ -70,3 +71,3 @@
     );
-    let app = Engine::new().unwrap().load("wat", wat, Default::default()).unwrap();
+    let app = Engine::new().unwrap().load("wat", wat, Default::default(), Arc::new(InMemory::new()), &[]).unwrap();
     let start = Instant::now();
```

The comment on line 1 of `guest.rs` still says "no outbound HTTP". Reword it to "outbound HTTP only to the allow list". The `Deny` comment goes away with the type, in the `guest.rs` diff above. The lib keeps no config format: it takes `&[&str]` and an `Arc<dyn ObjectStore>`.

### 9.4 What the sketch tests

| Test | Proves |
|---|---|
| `every_op_in_every_guest` | Every KV operation and `wasi:config` in the p2, p3 and Spin fixtures. |
| `errors_are_values_not_traps` | Bad bucket name, bad key, over-size value, wrong counter and a full table come back as `Err`, not a trap. |
| `paging` | 1500 keys: 1000 and a cursor, then 500. |
| `racing_cas` | 8 guests x 10 read-modify-writes: exactly 80. 8 x 10 host `increment`: 80. |
| `cache_and_etag_costs` | The `get` table above. |
| `list_costs_and_freshness` | The `list-keys` table above, including a list within 1 s of a write. |
| `op_costs` | The write rows above. |
| unit `blocked_table`, `allow_matcher` | Sections 4 and 5. |

### 9.5 Guest side: `wit_bindgen::generate!` for KV and config

A Spin SDK guest (`Cargo.toml`: `spin-sdk = { version = "7", default-features = false, features = ["http"] }`):

```rust
use spin_sdk::{http::{IntoResponse, Request}, http_service, wit_bindgen};

wit_bindgen::generate!({
    inline: "package torpor:fixture; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["../../../wit/keyvalue", "../../../wit/config"],
    runtime_path: "::spin_sdk::wit_bindgen::rt",
    generate_all,
});

#[http_service]
async fn handle(request: Request) -> impl IntoResponse { /* use wasi::keyvalue::store, wasi::config::store */ }
```

A plain guest (`wit-bindgen = "0.62"`, no `runtime_path`):

```rust
wit_bindgen::generate!({
    inline: "package torpor:fixture; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["../../../wit/keyvalue", "../../../wit/config"],
    generate_all,
});
```

Both are the working fixtures in `tests/components/{spin,rust}`. In the code, `config::get(key)` and `config::get_all()` are plain calls, and `store::open("name")` gives the bucket.

## 10. Line budget

| Module | Plan budget | Sketch lines | Code lines | Notes |
|---|---|---|---|---|
| `outbound` | about 60 | 132 | 116 (96 without the `use` block) | `Allow` 36, `blocked` 12, `Io` and TLS 8, hook and `send` 40 |
| `kv` | about 200 | 330 | 294 (273 without `use` and consts) | `use` and consts 21, bindgen and types 44, `Kv` basics 19, `fetch` and `get` 23, `write`, `put`, `touch` 33, `cas` and `swap` 15, `list` 39, store trait impls 41, batch 19, atomics 39 (the one line left over is a closing brace) |
| `config` | about 30 | 0 | 0 | Already in M1 |
| wiring | none | | about +23 Rust, +8 Cargo.toml | `guest.rs` +4 net, `engine.rs` +13 net, `lib.rs` +2, `serve.rs` +4 |
| Total | about 290 | | about 410 + 23 | |

What the required behaviour costs, from the sketch (estimates read from the listing). Cuts are UNTESTED.

| Behaviour | Lines | Possible cut |
|---|---|---|
| Value cache with ETag revalidation (`Cache`, `Seen`, `remember`, `get`) | about 28 | None without dropping the plan's 1 s cache |
| `list-keys` with the generation object, cached pages | about 40 (about 15 uncached) | Drop the per-cursor page cache: -8 |
| `batch` | about 19 | -18, but a plain `wit-bindgen` guest that imports the whole world then fails to instantiate unless the host calls `define_unknown_imports_as_traps` |
| `CACHE_MAX` bound | 6 | -6, but the cache then has no bound |
| `Allow::parse` character check | 4 | -4, but a bad item then parses and never matches |
| `increment` as a CAS loop, `cas` and `swap` | about 54 | Needed |

The budgets cannot be met together with the required behaviour. A fair target is about 300 lines of KV and about 115 of outbound.

## 11. Risks and surprises

| Item | Finding | Evidence |
|---|---|---|
| Cache scope | The plan says "per-instance". There is one store per request, so a per-store cache would never hit. The sketch keeps the cache per app per process, in `Shared`. Its bytes (`CACHE_MAX`, 32 MiB) are outside the guest memory cap and multiply by the number of apps. | Section 9.1 `Kv`; M1 `guest.rs` |
| Staleness | A write is seen at once by the writing process (read your writes), but another host can serve an old value for up to 1 s, and a list can lag by up to 1 s. A crash or a failed batch between the data and the generation writes leaves the list stale until the next write. | Section 9.1 table |
| Empty key | An empty key would be the bucket's own prefix. The sketch rejects empty keys. | `Kv::path` |
| `gen` is reserved | Rust edition 2024 reserves `gen`, so the field is `generation`. | Compile error, fixed |
| Resolver | `getaddrinfo` normalises numeric hosts, so judge the resolved address. `lookup_host` needs brackets stripped from IPv6. | Section 5 |
| Hung DNS | A blocking `getaddrinfo` is not aborted when the store drops. The guest got its 500 at 10.006 s, but the process finished at 25.4 s: the lookup holds a blocking-pool thread. A lookup timeout of its own is not in the sketch. | `host/tests/net.rs:93` (`hung_dns`; the run points `resolv.conf` at a black hole) |
| Driver | The connection driver must be returned as the second future so wasmtime spawns it with abort-on-drop. A plain `tokio::spawn` would outlive the store. | `wasmtime-wasi-49.0.1/src/runtime.rs:41, 55`; section 3 |
| Signature changes | `Engine::load` takes an object store and an allow list. `App::new` takes `Kv` and `Vec<Allow>`. `Manifest` gets an allow-list field. `Host.table` becomes `pub(crate)`. The scratch `main.rs` needs the new `load` signature. | Section 9.3 |
| S3 limits | Worst-case object key is 901 B (S3 allows 1024 B). S3 ETags are MD5, so the generation body must differ each time, and a CAS can see ABA (correct for `increment`). Unchecked against S3. | Section 2 |
| A set is two PUTs | Data, then generation, one after the other. | Section 9.1 table |
| Spin counters | Spin stores counters as 8 little-endian bytes and panics on a wrong length (`expect("incorrect length")`). We return an error. | `spin-src/crates/key-value-spin/src/store.rs:342` |
| Delete of a missing key | InMemory and S3 say `Ok`, `LocalFileSystem` says `NotFound`. | Section 2 |
| `list_with_offset` | The trait default reads and filters client-side. S3 overrides it. A paging test on InMemory does not prove the S3 cost. | Section 2 |
| Table full | A KV failure is a value, a `wasi:http` failure is a trap. | Section 7 |
| rustls provider | Name it explicitly (`builder_with_provider`), because the `aws` feature brings `aws-lc-rs` as well. | Section 3 |
| Allow list vs Q38 | Q38's "about 20 lines on the `url` crate" does not hold. The `url` crate cannot parse the wildcard items. | Section 4 |
| Any vs all | We block a host if any address is blocked. Spin blocks only if all are. | Section 5 |
| Spin SDK | Default features import `spin:*`. The `experimental` config import does not link on rc.1. | Section 8 |
| Crates | M2 adds 20 crates (173 to 193). Option (b) would add 3 more. | Section 2 |
| Bad TLS certificate | The guest sees `TlsProtocolError`, never `TlsCertificateError`. Mapping it needs `Error::TlsCertificateError`, UNTESTED. | Section 3 |

## 12. Unverified or open

| Item | Status |
|---|---|
| Any of this against real S3 (ETag is MD5, conditional put, `If-None-Match` get, `start-after` listing, `PutResult.e_tag` present) | UNTESTED. InMemory only. M3. |
| `define_unknown_imports_as_traps` as the way to drop `batch` | UNTESTED |
| Spin semantics (matching, ports, blocked networks) | Read from source only, not run against a Spin host |
| Running the `serve.rs` change and the updated `tests/m1.rs` | Compile-checked only (`cargo check --bins`, `--test m1`); the sketch has no served-app test and the M1 tests were not run against the M2 lib |
| JavaScript (StarlingMonkey) guest use of KV and outbound; table use with GC-delayed drops | UNTESTED. Rust guests only. |
| `Error::TlsCertificateError` mapping | UNTESTED |
| An app-name length limit of 64 B (for the 901 B key bound) | Assumption |
| Option (b) behaviour (pool sharing, driver lifetime) | Compiled with `cargo check` only |
| Lookup timeout for a hung resolver | Not designed. Would need a `tokio::time::timeout` around `lookup_host`, which does not free the blocking thread. |
