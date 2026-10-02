#!/bin/sh
# Builds the host for linux/arm64 against glibc 2.34 (the provided.al2023 runtime) and copies it to out/spinit-host.
set -eu
cd "$(dirname "$0")/.."
docker/cargo.sh sh -c 'cargo build --release && cp "$CARGO_TARGET_DIR/release/spinit-host" /out/spinit-host.new && mv /out/spinit-host.new /out/spinit-host && ls -l /out/spinit-host'
