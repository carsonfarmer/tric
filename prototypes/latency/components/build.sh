#!/bin/sh
# Rebuilds the three test components in Docker into out/ and prints their sha256 and imports.
# Sources are copies of the host-research apps (hello-world Spin SDK apps, Spin-specific bits removed).
#   rust-p3-hello  Rust, spin-sdk 7 `http_service`, wasi:http/handler@0.3
#   rust-p2-hello  Rust, spin-sdk 5 `http_component`, wasi:http/incoming-handler@0.2
#   js-hello       JavaScript (itty-router) componentized with jco/StarlingMonkey, 0.2 incoming-handler
set -eu
cd "$(dirname "$0")/.."
mkdir -p out
for c in rust-p3-hello rust-p2-hello; do
  SRC=components/$c docker/cargo.sh sh -c 'cargo build --release --locked --target wasm32-wasip2 && cp "$CARGO_TARGET_DIR"/wasm32-wasip2/release/*.wasm /out/'
done
docker run --rm -e npm_config_cache=/npm-cache -v spinit-spike-latency-npm:/npm-cache -v "$PWD/components/js-hello":/src -v "$PWD/out":/out -w /src node:22-bookworm-slim \
  sh -c 'npm ci --no-audit --no-fund && npm run build && cp dist/app-http-js.wasm /out/hello_js.wasm && rm -rf node_modules build dist'
# wasm-tools 1.260.0 (release binary, cached in the cargo volume); every import must be wasi:*
docker/cargo.sh sh -c 'set -e
  [ -x /cargo-home/bin/wasm-tools ] || { mkdir -p /cargo-home/bin; curl -fsSL https://github.com/bytecodealliance/wasm-tools/releases/download/v1.260.0/wasm-tools-1.260.0-aarch64-linux.tar.gz | tar xz -C /tmp; cp /tmp/wasm-tools-*/wasm-tools /cargo-home/bin/; }
  for f in /out/hello_*.wasm; do
    echo "$f $(sha256sum $f | cut -d" " -f1) $(stat -c%s $f) bytes"
    /cargo-home/bin/wasm-tools component wit $f | grep -E "^ +import " | grep -v "import wasi:" && { echo "non-wasi import in $f"; exit 1; } || true
  done'
