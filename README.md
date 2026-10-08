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

## A JavaScript app

Experimental. [componentize-qjs](https://crates.io/crates/componentize-qjs) 0.4.5 builds a JavaScript module into a
component, on QuickJS, and this repository's toolchain image has it, patched ([docs/decisions.md](docs/decisions.md)
says why). The module's top level runs once, at build time, and every request starts from the heap it leaves: so read
the environment in the handler, and take secrets from `wasi:random`, as `Math.random` repeats in every request.

```bash
docker compose build dev                    # a few minutes, the first time
mkdir -p hello-js/src hello-js/wit
curl -fsSL 'https://static.crates.io/crates/wasip3/wasip3-0.9.0+wasi-0.3.0.crate' \
  | tar xz -C hello-js/wit --strip-components=2 'wasip3-0.9.0+wasi-0.3.0/wit/deps'
```

The world lists what the app is given, and the module exports the handler:

```wit
// hello-js/wit/world.wit
package example:hello;

world app {
  import wasi:http/types@0.3.0;
  export wasi:http/handler@0.3.0;
}
```

```js
// hello-js/src/main.js
import types from "wasi:http/types@0.3.0";

export const handler = {
  async handle(request) {
    const body = wit.Stream(wit.Stream.U8);
    const trailers = wit.Future(wit.Future.RESULT_OPTION_OTHER_ERROR_CODE);
    const [response, sent] = types.Response.new(types.Fields.fromList([]), body.readable, trailers.readable);
    sent.drop();
    (async () => {
      await body.writable.writeAll(Uint8Array.from("hello from tric\n", (c) => c.charCodeAt(0)));
      body.writable.drop();
      await trailers.writable.write({ tag: "ok", val: null });
    })();
    return response;
  },
};
```

```bash
docker compose run --rm dev componentize-qjs --wit hello-js/wit --js hello-js/src/main.js --world app \
  --output hello-js/hello.wasm
tric dev hello-js/hello.wasm
```

There is no web platform under QuickJS (no `TextEncoder`, `URL`, `fetch` or `console`), so an app speaks WASI:
- each import is a module named for it, with its names in camelCase;
- an `option<T>` is `T | null`, a `u64` is a number, and a `result`'s error is thrown, with the variant as `payload`;
- an exception that escapes the handler answers 500.

[tests/components/js](tests/components/js) has the Rust test app's routes, in JavaScript.

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
