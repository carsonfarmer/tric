#!/bin/sh
# Runs a command in the build image with the named cargo caches. Usage: docker/cargo.sh <cmd...>   (from prototypes/latency)
set -eu
cd "$(dirname "$0")/.."
exec docker run --rm -e CARGO_HOME=/cargo-home -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" -e CARGO_TARGET_DIR=/target \
  -v spinit-spike-latency-cargo:/cargo-home -v spinit-spike-latency-target:/target -v "$PWD/host":/src -w /src \
  ${EXTRA_DOCKER_ARGS:-} spinit-spike-latency-build:1 "$@"
