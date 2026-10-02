# Writing a torpor app

This page is for someone writing an app that runs on torpor. torpor calls a WASI HTTP component once per request, in a fresh instance, so nothing in memory survives from one request to the next. All state goes in KV, which torpor keeps in object storage (S3 in production). Working examples are in [`tests/components/rust/`](../tests/components/rust/) (plain `wit-bindgen`) and [`tests/components/spin/`](../tests/components/spin/) (Spin SDK).

**Today:** `torpor serve` runs one app from a directory and keeps KV data in memory. It is lost when the process stops. The S3-backed host, `deploy` and `secrets` commands come later. Everything below describes the S3-backed host unless it says otherwise.

## The manifest

An app is a directory with a `torpor.toml` and a component. Run it with `torpor serve [DIR] [--listen ADDR]` (defaults: `.` and `127.0.0.1:3000`).

```toml
name = "hello"
component = "hello.wasm"
allowed_outbound_hosts = ["https://api.example.com"]

[config]
greeting = "hi"
```

| Field | Meaning |
|---|---|
| `name` | The app's name, which cannot be empty. It is also the prefix of the app's KV data, so renaming an app starts it with empty stores. |
| `component` | Path to the `.wasm` component, relative to the manifest. |
| `allowed_outbound_hosts` | Hosts the app may call. Absent or empty means no outbound requests at all. See [Outbound HTTP](#outbound-http). |
| `[config]` | Keys and values the app reads through `wasi:config`. Values must be strings. |

- **Typos:** an unknown field is an error, so a misspelled `allowed_outbound_hosts` stops `torpor serve` at start-up instead of leaving the app with no outbound access. So does a bad allow-list entry.
- **Secrets** never go in the manifest. `torpor serve` turns each environment variable `TORPOR_VAR_<KEY>` into the config key `<key>`, lowercased: `TORPOR_VAR_API_KEY=s3cret` gives the app `api_key`. A variable overrides the same key in `[config]`, and the app reads both through `wasi:config` as one flat set. Use lowercase keys in `[config]`, or an override will not match. The `deploy` and `secrets` commands, which replace this, come later.

## Interfaces

| Interface | Version | What the app gets |
|---|---|---|
| `wasi:http` | p2 `incoming-handler` (0.2.x) or p3 `handler` (0.3.0) | Export one of them. torpor picks whichever the component exports. Outbound requests use the standard outgoing handler (see [Outbound HTTP](#outbound-http)). |
| `wasi:keyvalue` | `0.2.0-draft2` | `store`, `atomics` and `batch`. There is no `watch`. |
| `wasi:config` | `0.2.0-rc.1` | `get` and `get-all` over `[config]` plus the `TORPOR_VAR_` variables. |

The other standard WASI interfaces (clocks, random numbers, stdout and stderr) work, but the app sees an empty world: no environment variables, no arguments, no files and no sockets. Output goes to the host's log (see [Limits](#limits)). A component that imports an interface torpor does not provide, such as `wasi:keyvalue/watch` or any `spin:*` interface, is refused when it loads.

The WIT files for KV and config are in this repo's `wit/keyvalue/` and `wit/config/`. Copy them into your project.

### Plain `wit-bindgen`

Use `wit-bindgen` for KV and config, and the `wasip3` (or `wasip2`) crate for HTTP. Build a `cdylib` with `--target wasm32-wasip2`. The fixture in `tests/components/rust/src/lib.rs` has both the p2 and the p3 handler.

```toml
wit-bindgen = "0.62"
wasip3 = { version = "0.9", features = ["http-compat"] }   # wasip2 = "2" for a p2 handler
http = "1"
```

```rust
use wasip3::{http::types::{ErrorCode, Request, Response}, http_compat::http_into_wasi_response};

wit_bindgen::generate!({
    inline: "package example:app; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["wit/keyvalue", "wit/config"],
    generate_all,
});
use wasi::{config::store as config, keyvalue::{atomics, store}};

wasip3::http::service::export!(Component);
struct Component;
impl wasip3::exports::http::handler::Guest for Component {
    async fn handle(_request: Request) -> Result<Response, ErrorCode> {
        let bucket = store::open("visits").unwrap();
        let n = atomics::increment(&bucket, "home", 1).unwrap();
        let greeting = config::get("greeting").unwrap().unwrap_or_default();
        http_into_wasi_response(http::Response::new(format!("{greeting} #{n}")))
    }
}
```

### Spin SDK

Use the SDK for HTTP only. Its default features, its key-value module and its variables module import `spin:*` interfaces that torpor does not provide, and its own `wasi:config` import is a different draft (`0.2.0-draft-2024-09-27`) that does not link against `0.2.0-rc.1`. Bind KV and config with the SDK's re-exported `wit_bindgen`, as above but with `runtime_path`:

```toml
spin-sdk = { version = "7", default-features = false, features = ["http"] }
```

```rust
use spin_sdk::{http::{IntoResponse, Request}, http_service, wit_bindgen};

wit_bindgen::generate!({
    inline: "package example:app; world kv { include wasi:keyvalue/imports@0.2.0-draft2; include wasi:config/imports@0.2.0-rc.1; }",
    path: ["wit/keyvalue", "wit/config"],
    runtime_path: "::spin_sdk::wit_bindgen::rt",
    generate_all,
});

#[http_service]
async fn handle(request: Request) -> impl IntoResponse { /* wasi::keyvalue::store, wasi::config::store, as above */ }
```

JavaScript components built with `jco` serve HTTP. KV and outbound requests from JavaScript have not been tested yet.

## KV consistency

KV is one S3 object per key, and nothing is cached: every call goes to the store. Everything below follows from that.

A **host** is one running torpor process. In production several hosts can serve the same app at once.

### One key

| Call | What you get |
|---|---|
| `get`, `exists` | The value in the store when the call ran, whichever host wrote it. |
| `cas::new`, then `current` | `new` reads the store and `current` returns what it read. |
| `swap` | Succeeds only if the key is unchanged since `cas::new`, checked by the store with `If-Match`. Atomic across all hosts. A lost swap returns `cas-failed` with a new handle that holds the latest value. A swap on a missing key succeeds only if the key is still missing, and a swap on a key deleted since `cas::new` loses. |
| `increment` | Atomic across hosts. A missing key counts as 0. It retries a lost race up to 16 times, then fails with `too much contention`. |
| `set`, `delete` | In the store before the call returns. The last writer wins. `delete` of a missing key is not an error. |

- **Counters** are 8 bytes, a little-endian `i64`, as in Spin. `increment` on any other length fails with `not a counter`, and on overflow with `overflow`. Read a counter with `get` and decode the 8 bytes.
- **`swap` compares content.** On S3 the ETag is normally a hash of the value, so if a value changes and changes back between your read and your swap, the swap still succeeds. Keep a version number inside the value if that matters.
- **Batches are not atomic.** `get-many`, `set-many` and `delete-many` do one key at a time, so they save no round trips. If `set-many` fails part-way, the keys before the failure stay written, and other callers can see the partial result. Keys are checked before anything is written, but a value over 1 MiB is only found when its turn comes, after the earlier keys are already in the store.
- **A write that fails** may still have reached the store, and so may part of a batch. After an error, treat the key as unknown and read it again.
- **No transactions across keys.** Use `increment` or `cas` instead of `get` then `set`.
- **Hot keys on GCS:** GCS takes about one write a second to any one object and throttles faster writes, which the host retries with backoff, so they slow down and can fail. Spread a busy counter over several keys. S3 and Azure have no such limit.

### Listing keys

`list-keys` returns a page of up to 1,000 keys. When the page is full it also returns a cursor, which is the last key on that page. Pass it back for the next page, and stop when the cursor is none. A bucket with exactly 1,000 keys returns a full page and then an empty one, so an empty page at the end is normal. Keys come back in the order of their stored names, and the store percent-encodes characters such as `/`, `%`, `?` and non-ASCII text in those names. For keys that use such characters the order can differ from a plain sort of the keys, so do not rely on it.

Each page is one LIST of the store, so it includes every write that finished before it, from any host.

### Limits

| | |
|---|---|
| Keys | 1 to 256 bytes (UTF-8), any characters. An empty or longer key returns `other("a key is 1 to 256 bytes")`. |
| Values | At most 1 MiB (1,048,576 bytes). A larger one returns `other(...)` and writes nothing. |
| Store names | `[a-z0-9-]{1,64}`, else `no-such-store`. Any such name opens, with no manifest field. Stores belong to one app, and no app can see another's. |
| Pages | At most 1,000 keys. |
| `get-many` result | At most 16 MiB of values in one call, else `other("get-many returns 16777216 bytes or less")`. Nothing is returned, so ask for fewer keys at a time. |

KV failures come back as error values, not traps.

## What KV costs

On S3 every store call is billed. The prices below are us-east-1 list prices and vary by region: PUT, LIST and POST are $0.005 per 1,000 requests, and GET is $0.0004 per 1,000. DELETE is free. Storage and data transfer are extra.

| Operation | Store calls | Request cost per 1,000 operations |
|---|---|---|
| `open`, `current` | none | $0 |
| `get`, `exists`, `cas::new` | 1 GET | $0.0004 |
| `set` | 1 PUT | $0.005 |
| `delete` | 1 DELETE | $0 |
| `increment` | GET, PUT, and both again for each lost race | $0.0054 |
| `swap` | 1 PUT. A lost swap costs a PUT, then 1 GET for the new handle | $0.005 |
| `get-many`, `set-many`, `delete-many`, N keys | N of the single call | N times the single call |
| `list-keys` | 1 LIST per page | $0.005 |

For scale, 100,000 `set`s a month is $0.50 in requests, and a million `get`s is $0.40. Reads add up on a hot key: one read 100 times a second all month is about 260 million GETs, or about $104.

### Listing is the expensive operation

**One LIST costs 12.5 GETs, and a LIST on every request adds up to $5 per million requests.** Each page of 1,000 keys is its own LIST, so a store of 2,500 keys read through three pages costs three LISTs.

Do not list on the hot path of a read-heavy page. If most requests need the same list, keep it in a key of its own, update it with `cas`, and `get` it.

## Outbound HTTP

Outbound requests are denied by default. Use the standard `wasi:http` outgoing handler (p2 `outgoing-handler`, p3 `client`) or any HTTP client built on it. A request goes out only if it matches an entry in `allowed_outbound_hosts`, and then only to public addresses.

### The allow list

An entry is `scheme://host[:port]`, in the same shape as Spin's but a subset of it.

| Entry | Allows |
|---|---|
| `https://api.example.com` | That host, https, port 443. |
| `https://*.example.com` | Any subdomain at any depth, https, port 443. It does **not** match `example.com`, so list that too if you need it. |
| `https://example.com:8443` | Port 8443 only. |
| `http://localhost:3000` | Parses, but can never succeed: `localhost` resolves to a loopback address, which is always blocked. |
| `*://*:*` | Any http or https request. The address rule below still applies. |

- The scheme is `http` or `https`. The port defaults to 80 or 443 from the scheme.
- The host is an exact name or address, or `*.` and a domain. Case does not matter, and a trailing dot (`example.com.`) does not match. The match is on the host as the guest wrote it, before any DNS lookup.
- Unlike Spin, there is no `*` scheme, port or host other than in `*://*:*`, and no port ranges, `{{ }}` templates, CIDR hosts or `self`. An entry with any of those, a path (even a trailing `/`), a query or a user name stops the app at start-up.
- A request URL with a user name (`https://user@example.com/`) fails whatever the list says. Send credentials in an `Authorization` header.

### Blocked addresses

Private, loopback, link-local and metadata addresses are **always blocked, even if listed**. The host looks the name up itself, judges every address it got back, and connects only to those addresses. So a public name that points at `10.0.0.5` is refused, and a DNS answer that changes between lookups cannot slip past the check. A name with **any** blocked address is refused, which is stricter than Spin (it drops the blocked ones and uses the rest). There is no way to reach a private address, including a service inside your own network.

Blocked: `10/8`, `172.16/12`, `192.168/16`, loopback, `169.254/16` (which holds the cloud metadata endpoints), `100.64/10`, `0/8`, multicast, `240/4` and up, `192.0.0.0/24` and `198.18/15`. For IPv6, everything outside `2000::/3` and `2002::/16`. An IPv4-mapped IPv6 address is judged as its IPv4 address.

### What the guest sees

| Case | Error code |
|---|---|
| Not on the allow list, or the list is empty | `HttpRequestDenied` |
| The URL has a user name | `HttpRequestUriInvalid` |
| A resolved address is blocked | `DestinationIpProhibited` |
| The name does not resolve | `DnsError` |
| The TCP connection fails | `ConnectionRefused` |
| Any TLS failure: bad, expired or self-signed certificate, wrong name, failed handshake | `TlsProtocolError` |
| The reply is not valid HTTP/1.1, or the connection drops mid-exchange | `HttpProtocolError` |

- **HTTP/1.1 only.** A server that needs HTTP/2 cannot be reached. Certificates are checked against a built-in set of public root certificates (TLS 1.2 and 1.3). There are no custom CAs or client certificates.
- **Redirects are not followed.** The guest gets the 3xx response. If your HTTP client follows it, the next hop is a new request, checked against the allow list and the address rule again.
- **Time:** everything counts against the 10 s request deadline. The timeouts in `request-options` are ignored. A server that never answers ends the whole request with a 500 at 10 s.

## Limits

| Limit | Value | When it is hit |
|---|---|---|
| Time per request | 10 s, from the start of instantiation to the end of the response. Outbound calls, KV calls and a streaming body all count. | The request ends with an empty `500`. The cause is logged. |
| Memory | 256 MiB, all of the request's memories together | Growth past it is refused. The guest usually aborts, and the request ends with a `500`. |
| Requests in flight | 64 per app on each host | The next request waits for a free slot. The wait is not part of its 10 s. |
| Data passed to one call | 32 MiB of strings and lists a guest hands to a single host call, such as `set-many` or `wasi:http` `fields.from-list` | The call traps and the request ends with a `500`. |
| Live resources | 256 per request (handles to buckets, CAS operations, and `wasi:http` fields, requests, responses and bodies) | `store::open` and `cas::new` return `other("resource table has no free keys")`. A `wasi:http` call that needs a new handle fails the request with a `500`. |
| Component shape | At most 16 instances, 16 tables and 4 memories, and 100,000 elements in a table | Instantiation fails, so every request gets a `500`, or the growth is refused. Parts composed into one component all count. |
| Log output | 64 KiB per stream (stdout, stderr) per request | The stream closes, so later writes to it fail. A guest that treats a failed write as fatal (Rust's `println!` panics) ends the request. Log little. |

- **Failures** before the guest has responded (a trap, the deadline, a limit) give an empty `500`. The guest's own output is logged either way. The 10 s deadline also cuts a response body that is still streaming, so read request bodies and write responses promptly.
- **Handles:** Rust drops them as they go out of scope. A garbage-collected guest may hold them until its collector runs, which is not yet tested.
- **Logs:** each stream is logged once per request, when the request ends, as one JSON line tagged with the app. stdout is logged at `info` and stderr at `warn`. The default level is `warn`, so stdout shows only when the host runs with `RUST_LOG=info`.
