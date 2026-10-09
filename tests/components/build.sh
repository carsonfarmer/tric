#!/bin/sh
# Builds the test fixtures into tests/fixtures/<name>.wasm, in the toolchain image: docker compose run --rm fixtures
# One crate, two components: `app` (the routes that exercise the host) and `guard` (a wasi:http/middleware); and `js`,
# which has the routes of `app` again, in JavaScript.
set -eu
cd "$(dirname "$0")"
out=../fixtures
mkdir -p "$out"

# The guard's WIT names wasi:http/middleware@0.3.0, whose packages wasip3 ships: use that copy rather than vendor another.
cargo fetch --locked --manifest-path rust/Cargo.toml
rm -rf rust/wit/deps
cp -R "$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/wasip3-0.9.0* | head -1)/wit/deps" rust/wit/deps

for name in app guard; do
  cargo build --release --locked --target wasm32-wasip2 --manifest-path rust/Cargo.toml --features "$name"
  cp "${CARGO_TARGET_DIR:-rust/target}/wasm32-wasip2/release/fixture.wasm" "$out/$name.wasm"
done

# The same packages, and tric's wasi:keyvalue, for the JavaScript app. componentize-qjs runs the app's top level at
# build time and keeps the heap that leaves: see docs/decisions.md.
rm -rf js/wit/deps
cp -R rust/wit/deps js/wit/deps
cp -R ../../wit/keyvalue js/wit/deps/keyvalue
componentize-qjs --wit js/wit --js js/src/app.js --world app --output "$out/js.wasm"
ls -l "$out"
