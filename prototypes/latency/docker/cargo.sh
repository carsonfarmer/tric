#!/bin/sh
# Runs a command in the build image with the named cargo caches, from prototypes/latency.
#   docker/cargo.sh cargo build --release                       # the host crate
#   SRC=components/rust-p2-hello docker/cargo.sh cargo build ...  # another crate (own target dir in the volume)
set -eu
cd "$(dirname "$0")/.."
src="${SRC:-host}"
target=/target; [ "$src" = host ] || target="/target/$(basename "$src")"
exec docker run --rm -e CARGO_HOME=/cargo-home -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" -e CARGO_TARGET_DIR="$target" \
  -v spinit-spike-latency-cargo:/cargo-home -v spinit-spike-latency-target:/target -v "$PWD/$src":/src -v "$PWD/out":/out -w /src \
  ${EXTRA_DOCKER_ARGS:-} spinit-spike-latency-build:1 "$@"
