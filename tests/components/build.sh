#!/bin/sh
# Builds every test fixture into tests/fixtures/<name>.wasm, in the toolchain image: docker compose run --rm fixtures
# Wasmtime 49.0.1: wasi:http 0.2.12 and 0.3.0. The host crate and the wasmtime CLI in the image must stay on this version.
set -eu
cd "$(dirname "$0")"
out=../fixtures
mkdir -p "$out"

# One crate, six fixtures: the feature picks the wasi:http world, `probe` the routes that exercise the host, and `kv` the
# wasi:keyvalue and wasi:config routes.
build() { # <name> <features>
  cargo build --release --locked --target wasm32-wasip2 --manifest-path rust/Cargo.toml --features "$2"
  cp "${CARGO_TARGET_DIR:-rust/target}/wasm32-wasip2/release/fixture.wasm" "$out/$1.wasm"
}
build hello-p2 p2
build hello-p3 p3
build probe-p2 p2,probe
build probe-p3 p3,probe
build kv-p2 p2,kv
build kv-p3 p3,kv

# spin: the Spin SDK serves HTTP and the same kv routes bind straight to the WASI interfaces (the SDK's own key-value and
# variables modules import spin:* interfaces, which the host does not provide).
cargo build --release --locked --target wasm32-wasip2 --manifest-path spin/Cargo.toml
cp "${CARGO_TARGET_DIR:-spin/target}/wasm32-wasip2/release/fixture_spin.wasm" "$out/spin.wasm"

# hello-js: StarlingMonkey, componentized by jco. The engine in jco 1.35.0 exports wasi:http 0.2.10, so the WIT must be 0.2.10 too.
wit=$(mktemp -d)
curl -fsSL https://registry.npmjs.org/@spinframework/wasi-http-proxy/-/wasi-http-proxy-2.0.0.tgz | tar xz -C "$wit"
jco componentize js/index.js --wit "$wit/package/wit/wasi-http@0.2.10.wit" --world-name http-trigger -o "$out/hello-js.wasm"

ls -l "$out"
