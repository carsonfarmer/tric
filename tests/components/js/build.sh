#!/bin/sh
# Builds the JavaScript app into tests/fixtures/js.wasm, in the toolchain image; ../build.sh runs it, once it has
# fetched the WASI packages the world imports. componentize-qjs runs the app's top level at build time and keeps the
# heap that leaves: see docs/decisions.md.
set -eu
cd "$(dirname "$0")"
rm -rf wit/deps
cp -R ../rust/wit/deps wit/deps
cp -R ../../../wit/keyvalue wit/deps/keyvalue
componentize-qjs --wit wit --js src/app.js --world app --output ../../fixtures/js.wasm
