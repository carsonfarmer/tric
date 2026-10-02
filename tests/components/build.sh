#!/bin/sh
# Builds every test fixture into tests/fixtures/<name>.wasm, in the toolchain image: docker compose run --rm fixtures
# Wasmtime 49.0.1: wasi:http 0.2.12 and 0.3.0. The host crate and the wasmtime CLI in the image must stay on this version.
set -eu
cd "$(dirname "$0")"
out=../fixtures
mkdir -p "$out"

# One crate, four fixtures: the feature picks the wasi:http world, and `probe` the routes.
build() { # <name> <features>
  cargo build --release --locked --target wasm32-wasip2 --manifest-path rust/Cargo.toml --features "$2"
  cp "${CARGO_TARGET_DIR:-rust/target}/wasm32-wasip2/release/fixture.wasm" "$out/$1.wasm"
}
build hello-p2 p2
build hello-p3 p3
build probe-p2 p2,probe
build probe-p3 p3,probe

# hello-js: StarlingMonkey, componentized by jco. The engine in jco 1.35.0 exports wasi:http 0.2.10, so the WIT must be 0.2.10 too.
wit=$(mktemp -d)
curl -fsSL https://registry.npmjs.org/@spinframework/wasi-http-proxy/-/wasi-http-proxy-2.0.0.tgz | tar xz -C "$wit"
jco componentize js/index.js --wit "$wit/package/wit/wasi-http@0.2.10.wit" --world-name http-trigger -o "$out/hello-js.wasm"

ls -l "$out"
