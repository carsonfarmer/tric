# Toolchain image: Amazon Linux 2023 (glibc 2.34, the same as the provided.al2023 Lambda runtime), plus Rust, the Wasm
# tools and componentize-qjs.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS base
RUN dnf install -y gcc gcc-c++ make cmake perl git tar gzip xz zip findutils clang && dnf clean all
# The toolchain lives in the image; the cargo registry and target dir are named volumes mounted at run time.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 \
 && rustup target add wasm32-wasip2 && rustup component add rustfmt clippy && rustc --version && ldd --version | head -1
# Release binaries for wasm-tools and the wasmtime CLI. The CLI must match the Wasmtime version the host crate pins.
RUN arch=$(uname -m) \
 && curl -fsSL https://github.com/bytecodealliance/wasm-tools/releases/download/v1.260.0/wasm-tools-1.260.0-$arch-linux.tar.gz | tar xz --strip-components=1 -C /usr/local/bin --wildcards '*/wasm-tools' \
 && curl -fsSL https://github.com/bytecodealliance/wasmtime/releases/download/v49.0.2/wasmtime-v49.0.2-$arch-linux.tar.xz | tar xJ --strip-components=1 -C /usr/local/bin --wildcards '*/wasmtime' \
 && wasm-tools --version && wasmtime --version

# componentize-qjs, which makes a JavaScript app a component (experimental; see docs/decisions.md): 0.4.5's sources from
# crates.io, checked by sha256 and patched (qjs/componentize-qjs.patch: Wasmtime 49 for the Wizer run, and its own
# trapping of the imports the run does not give), with the lockfile in qjs/. A stage of its own, so that the toolchain
# image gains the binary alone; no LTO, for the builder's memory.
FROM base AS qjs
COPY qjs /qjs-src
RUN set -eu; mkdir /qjs && cd /qjs \
 && fetch() { curl -fsSL -o "$1.crate" "https://static.crates.io/crates/$1/$1-0.4.5.crate" \
      && echo "$2  $1.crate" | sha256sum -c - && tar xzf "$1.crate" && mv "$1-0.4.5" "$3"; } \
 && fetch componentize-qjs 6e3a182c2ed4e216c999def79e11ef9587a98be654334a7ac9696d2ffea25d35 lib \
 && fetch componentize-qjs-cli 517ef5f064ad43c7a46ea8d317041d121db6c8fb3f28681e47669e8c8b240853 cli \
 && git apply /qjs-src/componentize-qjs.patch && cp /qjs-src/Cargo.lock cli/Cargo.lock \
 && cd cli && CARGO_BUILD_JOBS=2 CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
      cargo build --release --locked && target/release/componentize-qjs --help >/dev/null

FROM base
COPY --from=qjs /qjs/cli/target/release/componentize-qjs /usr/local/bin/componentize-qjs
