# tric

A small host for WASI HTTP components that scales to zero. One process serves every app in a bucket, each at its own
subdomain; on AWS that process is one Lambda function behind CloudFront and the bucket is S3, so an idle install costs
its storage and nothing else. Each request gets a fresh Wasmtime instance that can reach nothing it was not given, and
state is the bucket's too: each app's key-value data, kept in **names** that requests change one at a time.

tric runs components; it does not build them. Anything that exports `wasi:http/handler@0.3` is an app.

## An app

You need Rust 1.96 or later, with the `wasm32-wasip2` target, and Docker.

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

Build it, and name it in a `tric.toml` beside `Cargo.toml`:

```bash
cargo build --release --target wasm32-wasip2
printf 'name = "hello"\ncomponent = "target/wasm32-wasip2/release/hello.wasm"\n' > tric.toml
```

`tric.toml` may also list the hosts the app may call (`allowed_outbound_hosts = ["https://api.example.com"]`),
middleware to wrap it in, and cron schedules (`[cron]`, `"0 8 * * *" = "/@digest/run"`).
[docs/decisions.md](docs/decisions.md) says what each does, and what an app may import.

## Running it

**One app**, with its state in memory, until Ctrl-C:

```bash
cargo install --locked --git https://github.com/carsonfarmer/tric tric
tric dev                                   # the app in this directory, at http://127.0.0.1:3000
```

**A local install**, which is what an install on AWS is but for Lambda and EventBridge Scheduler: every app in one
bucket (MinIO, in Docker), each at `http://<app>.localhost:3000`, with cron ticking in-process. Nothing to install but
Docker. From a clone of this repository:

```bash
docker compose up -d serve
docker compose run --rm -v "$PWD/../hello:/app" tric deploy /app
curl hello.localhost:3000
```

The `tric` service is the CLI, on the local install: `deploy`, `releases`, `release` and `env`, as on AWS. Apps,
releases and state outlive the containers, in the `s3` service's volume; `docker compose down -v` removes them.

**Lambda, locally**: the build that runs on AWS, `dist/tric.zip`, on Lambda's own image with its emulator, against the
local install's bucket. Lambda takes events, not HTTP, so send it one as a Function URL or EventBridge Scheduler would:

```bash
docker compose run --rm release            # dist/tric.zip, for Lambda's arm64
docker compose up -d --build lambda
curl -d '{"version":"2.0","rawPath":"/","headers":{"x-forwarded-host":"hello.localhost"},
  "requestContext":{"http":{"method":"GET","path":"/"}}}' localhost:9000/2015-03-31/functions/function/invocations
curl -d '{"cron":{"app":"hello","path":"/"}}' localhost:9000/2015-03-31/functions/function/invocations
```

The outbox's events are invocations of the function itself, which the emulator cannot take, so there a turn that holds
requests fails.

## On AWS

`tofu apply` in [infra/aws](infra/aws/README.md) installs tric into an account: a bucket, a function, CloudFront at
`*.<domain>`, a Scheduler group, a policy for whoever deploys, and a budget alert. The CLI then deploys to it as to the
local install, with `TRIC_STORE` naming its bucket. That README covers the security model, upgrades, failures, undoing
deletes, costs and spinning it down.

## Developing

Everything runs in Docker, one stack per worktree. `docker compose run --rm test` is the gate CI runs: it builds the
test components, checks formatting and lints, and runs the tests, the end-to-end ones against MinIO as well as in
memory. [compose.yaml](compose.yaml) lists the rest, and [docs/decisions.md](docs/decisions.md) is what tric is and why.

## Licence

[Apache-2.0](LICENSE).
