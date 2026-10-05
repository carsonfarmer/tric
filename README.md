# tric

SpinKube without Kubernetes: a small host for WASI HTTP components that scales to zero. One AWS Lambda function serves
every app, each at its own subdomain, and everything it knows (apps, releases, secrets, KV data) lives in S3, so an idle
install costs only its storage. Each request gets a fresh Wasmtime instance that can reach nothing it was not given.

## Quick start

From nothing to an app at `https://hello.<domain>`. You need Rust 1.96 or later, Docker, an AWS account with the AWS CLI
signed in to it, and a domain to serve the apps under.

**1. Install the CLI.**

```bash
cargo install --locked --git https://github.com/carsonfarmer/tric tric
```

**2. Write an app.** Any component that exports `wasi:http` will do; this one is Rust.

```bash
rustup target add wasm32-wasip2
cargo new --lib hello && cd hello
cargo add wasip3 --features http-compat
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

Then build it, and describe it in a `tric.toml` beside `Cargo.toml`:

```bash
cargo build --release --target wasm32-wasip2
printf 'name = "hello"\ncomponent = "target/wasm32-wasip2/release/hello.wasm"\n' > tric.toml
```

**3. Run it locally.** `tric serve` serves the app in the current directory at `hello.localhost:3000`, until Ctrl-C:

```bash
tric serve
```

In another terminal:

```bash
curl hello.localhost:3000
```

**4. Install tric on AWS**, with `AWS_PROFILE` set to a profile that may administer the account. The domain needs a
public Route 53 zone in the account. If it has none yet, make one, and point the domain at the four name servers it
prints:

```bash
aws route53 create-hosted-zone --name <domain> --caller-reference "<domain>-$(date +%s)"
```

The install's settings go in `infra/aws/terraform.tfvars`, which git ignores: the domain, and an email address for the
budget alert. An account whose Lambda concurrency quota is under 122 (`aws lambda get-account-settings`), as a new
account's is, needs `concurrency = { serve = -1, compile = -1 }` there too.

```bash
cd .. && git clone https://github.com/carsonfarmer/tric && cd tric
printf '%s\n' 'domain = "<domain>"' 'budget = { emails = ["<you@example.com>"] }' > infra/aws/terraform.tfvars
docker compose run --rm release
docker compose run --rm tofu init
docker compose run --rm tofu apply
```

The build takes a few minutes (more on an x86 machine, where Docker emulates arm64), and the apply about five more,
most of them CloudFront's. [infra/aws/README.md](infra/aws/README.md) has the details.

**5. Release the app.** Still in the clone, point the CLI at the install (us-west-2 is the module's default region),
then publish and release from the app's directory:

```bash
export AWS_REGION=us-west-2 TRIC_STORE=$(docker compose run --rm -T tofu output -raw store) TRIC_NATIVE=true
eval "$(aws configure export-credentials --format env)"
cd ../hello && tric release $(tric publish)
curl https://hello.<domain>
```

`tric publish` uploads the app and waits for its native code; `tric release` serves it. Within 5 s it is live.

## Then

- [Writing apps](docs/apps.md): the manifest, releases and secrets, KV and its consistency, outbound HTTP, limits,
  and composing components.
- [Running an install](infra/aws/README.md): teams, upgrades, garbage and undoing deletes, the trust model, costs, and
  spinning it down.
- [The plan](docs/plan.md) and [the decisions](docs/decisions.md) behind it.

## Developing

Everything runs in Docker. `docker compose run --rm test` builds the test components and runs the tests;
[compose.yaml](compose.yaml) lists the rest.

## Licence

[Apache-2.0](LICENSE).
