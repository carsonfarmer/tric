# tric

A small host for WASI HTTP components that scales to zero. An app is one `wasi:http` 0.3 service, and tric gives
meaning to its URLs and nothing more: no SDK, no bindings, no imports of tric's own. Each request gets a fresh Wasmtime
instance that can reach nothing it was not given.

tric runs components; it does not build them. Anything that exports `wasi:http/handler@0.3` is an app.

## An app

You need Rust 1.96 or later, with the `wasm32-wasip2` target.

```bash
rustup target add wasm32-wasip2
cargo new --lib hello && cd hello
cargo add wasip3@0.9 --features http-compat
cargo add http
printf '\n[lib]\ncrate-type = ["cdylib"]\n' >> Cargo.toml
```

Put this in `src/lib.rs`:

```rust
use wasip3::http::types::{ErrorCode, Request, Response};
use wasip3::http_compat::http_into_wasi_response;

wasip3::http::service::export!(App);
struct App;

impl wasip3::exports::http::handler::Guest for App {
    async fn handle(_request: Request) -> Result<Response, ErrorCode> {
        http_into_wasi_response(http::Response::new("hello from tric\n".to_string()))
    }
}
```

Build it, and serve it with its state in memory:

```bash
cargo build --release --target wasm32-wasip2
tric dev target/wasm32-wasip2/release/hello.wasm   # at http://127.0.0.1:3000
```

`tric dev` takes `-e NAME=VALUE` for the app's environment, and `--allow scheme://host[:port]` for each host it may
call; it may call none otherwise, and never a private address. A directory with a `tric.toml` may list these hosts too
(`allowed_outbound_hosts`), name the `component`, pin `middleware` to plug in front of it (`{ url, digest }`), and map
`[cron]` schedules to paths (`"0 8 * * *" = "/@digest/run"`).

## What an app sees

- **Names.** A path whose first segment starts with `@` (`/@room:42/…`) addresses that name's state, which
  `wasi:keyvalue`'s `open("room:42")` opens. A GET, HEAD or OPTIONS reads a snapshot; any other method is a **turn**,
  which may write that name only.
- **Turns commit at the answer**, unless it is a 5xx, or the instance traps. A turn that loses a race runs again,
  unseen. A busy name answers 429 with `Retry-After`. The name's version is its `ETag`, and `If-Match` is honoured.
- **Calls are fetches.** A request to the app's own origin runs in-process with `Forwarded: for=_tric`; a cycle
  answers 508.
- **Background requests.** A request a turn sends with `Prefer: respond-async` is answered 202 at once, held, and sent
  only if the turn commits, with an `Idempotency-Key`.
- **`Forwarded`** is set by tric on every request, and is the only word on where it came from: the client, `for=_cron`
  or `for=_tric`.

## Running it

`tric dev` runs one app in one process. `tric` itself is installed with `cargo install --locked --path .` in a
checkout.

To run many apps, deploy them to a bucket. Two processes then serve them:
- `tric route` maps `<app>.<domain>` to the app, and mints storage credentials that reach only that app;
- `tric serve` runs the app with those credentials, and holds none of its own.

`docker compose up route` runs both, with MinIO, at `http://<app>.localhost:3000`. In
`docker compose run --rm dev`, `cargo run -- deploy <path>` deploys there.

On AWS, serve gives each app its own Lambda tenant, so an escape from the Wasm sandbox reaches one app only.
[infra/aws](infra/aws/README.md) installs it into an account with one `tofu apply`.

## Developing

Everything runs in Docker, one stack per worktree. `docker compose run --rm test` is the gate: it builds the test
components, checks formatting and lints, and runs the tests.
- [docs/plan.md](docs/plan.md) is the plan.
- [docs/decisions.md](docs/decisions.md) holds the choices it leaves open.

## Licence

[Apache-2.0](LICENSE).
