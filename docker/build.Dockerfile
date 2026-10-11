# Toolchain image: Amazon Linux 2023 (glibc 2.34, the same as the provided.al2023 Lambda runtime), plus Rust, the Wasm
# tools and componentize-qjs.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023 AS base
RUN dnf install -y gcc gcc-c++ make cmake perl git tar gzip xz zip findutils clang && dnf clean all
# The toolchain lives in the image; the cargo registry and target dir are named volumes mounted at run time.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.0 \
 && rustup component add rustfmt clippy && rustc --version && ldd --version | head -1
# The fixtures are built by a toolchain of their own, a beta of 1.100, because wasm32-wasip3 is its target and is not
# stable before 1.100 (docs/decisions.md). Its standard library imports WASI 0.3 alone.
ENV TRIC_FIXTURE_TOOLCHAIN=1.100.0-beta.4
RUN rustup toolchain install $TRIC_FIXTURE_TOOLCHAIN --profile minimal --no-self-update --target wasm32-wasip3 \
 && rustc +$TRIC_FIXTURE_TOOLCHAIN --version
# Release binaries for wasm-tools and the wasmtime CLI. The CLI must match the Wasmtime version the host crate pins.
RUN arch=$(uname -m) \
 && curl -fsSL https://github.com/bytecodealliance/wasm-tools/releases/download/v1.260.0/wasm-tools-1.260.0-$arch-linux.tar.gz | tar xz --strip-components=1 -C /usr/local/bin --wildcards '*/wasm-tools' \
 && curl -fsSL https://github.com/bytecodealliance/wasmtime/releases/download/v49.0.2/wasmtime-v49.0.2-$arch-linux.tar.xz | tar xJ --strip-components=1 -C /usr/local/bin --wildcards '*/wasmtime' \
 && wasm-tools --version && wasmtime --version

# componentize-qjs, which makes a JavaScript app a component (experimental; see docs/decisions.md): 0.4.5's sources from
# crates.io, checked by sha256 and patched (qjs/componentize-qjs.patch: Wasmtime 49 for the Wizer run, its own trapping
# of the imports the run does not give, and wit-dylib's stack in the task's context, as WASI 0.3 has it), with the
# lockfile in qjs/. The runtime it embeds imports WASI 0.2, so the fixture is made with the runtime of the qjs-runtime
# stage instead. A stage of its own, so that the toolchain image gains the binary alone; no LTO, for the builder's
# memory.
FROM base AS qjs
COPY qjs/componentize-qjs.patch qjs/Cargo.lock /qjs-src/
RUN set -eu; mkdir /qjs && cd /qjs \
 && fetch() { curl -fsSL -o "$1.crate" "https://static.crates.io/crates/$1/$1-0.4.5.crate" \
      && echo "$2  $1.crate" | sha256sum -c - && tar xzf "$1.crate" && mv "$1-0.4.5" "$3"; } \
 && fetch componentize-qjs 6e3a182c2ed4e216c999def79e11ef9587a98be654334a7ac9696d2ffea25d35 lib \
 && fetch componentize-qjs-cli 517ef5f064ad43c7a46ea8d317041d121db6c8fb3f28681e47669e8c8b240853 cli \
 && git apply /qjs-src/componentize-qjs.patch && cp /qjs-src/Cargo.lock cli/Cargo.lock \
 && cd cli && CARGO_BUILD_JOBS=2 CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
      cargo build --release --locked && target/release/componentize-qjs --help >/dev/null

# wasi-sdk 34, fetched by hash: its clang and libc have the wasm32-wasip3 target.
FROM base AS sdk
RUN set -eu; case $(uname -m) in \
      aarch64) sdk=arm64 sum=f7e243dff54d60bcc576e94d6166b69f410f2500ae4a9ceef34315be10e77971 ;; \
      x86_64) sdk=x86_64 sum=b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4 ;; \
    esac \
 && curl -fsSL -o /sdk.tar.gz https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-34/wasi-sdk-34.0-$sdk-linux.tar.gz \
 && echo "$sum  /sdk.tar.gz" | sha256sum -c - && mkdir /sdk && tar xzf /sdk.tar.gz --strip-components=1 -C /sdk \
 && rm /sdk.tar.gz && /sdk/bin/clang --version

# The componentize-qjs runtime (the QuickJS interpreter and the glue to wit-dylib) for WASI 0.3, which the JavaScript
# fixture is made with: the source at the commit tagged v0.4.5, fetched by hash and patched (qjs/runtime.patch: a task's
# state in thread-local storage, as wasi-libc keeps the component's context slot 0 for the task's stack, and no WASI 0.1
# adapter). It is built for wasm32-wasip3 by the fixture toolchain, its C by the SDK's clang, with upstream's flags, and
# exports the TLS information by which wit-component gives a library thread-local storage of its own. Cargo fetches
# wit-dylib-ffi from Git at the commit Cargo.lock names.
FROM base AS qjs-runtime
COPY --from=sdk /sdk /sdk
COPY qjs/runtime.patch /runtime.patch
ENV CARGO_TARGET_WASM32_WASIP3_RUSTFLAGS="-Clink-arg=-shared -Clink-arg=-Wl,--no-entry -Clink-arg=-Wl,--allow-undefined -Clink-arg=-Wl,--export=__wasm_library_tls_info -Clto=fat -Copt-level=3" \
    CARGO_TARGET_WASM32_WASIP3_LINKER=/sdk/bin/clang CC_wasm32_wasip3=/sdk/bin/clang CFLAGS_wasm32_wasip3="-fPIC -O3" \
    WASI_SDK_PATH=/sdk WASI_SDK=/sdk CARGO_INCREMENTAL=0
RUN set -eu; mkdir /rt && cd /rt \
 && curl -fsSL -o rt.tar.gz https://github.com/andreiltd/componentize-qjs/archive/e563c6d6ae50b087980414015663ca9c948c09bb.tar.gz \
 && echo "80effd71d2c96750f015c9d7773a1b789027d4b15d6184600eebaae470a1e233  rt.tar.gz" | sha256sum -c - \
 && tar xzf rt.tar.gz --strip-components=1 && rm rt.tar.gz && git apply /runtime.patch \
 && cargo "+$TRIC_FIXTURE_TOOLCHAIN" build --release --locked --target wasm32-wasip3 \
      --package componentize-qjs-runtime --no-default-features --features component-model-async \
 && cp target/wasm32-wasip3/release/componentize_qjs_runtime.wasm /runtime.wasm

# The WASI conformance tests of the file system (docs/decisions.md): the wasi-testsuite at one commit, its Rust and C
# tests for WASI 0.1, which the tests componentise with the adapter of the Wasmtime release tric pins, and its Rust
# components for WASI 0.3. Fetched by hash. A stage of its own, so that the toolchain image gains the files alone.
FROM base AS conformance
RUN set -eu; mkdir /conformance && cd /conformance \
 && curl -fsSL -o adapter.wasm https://github.com/bytecodealliance/wasmtime/releases/download/v49.0.2/wasi_snapshot_preview1.command.wasm \
 && echo "09eb9c1a09abb057c61c3dc6979d34277272867610af065246057e1bdf327527  adapter.wasm" | sha256sum -c - \
 && curl -fsSL -o suite.tar.gz https://github.com/WebAssembly/wasi-testsuite/archive/e0aa527fab67f2f311882bcee4f62cc755433b73.tar.gz \
 && echo "b511c0bd77325ae27c93b51059c3b9ed8ac2b35f2872ee94993d39f5595f54cd  suite.tar.gz" | sha256sum -c - \
 && tar xzf suite.tar.gz --strip-components=2 --wildcards '*/tests/rust/testsuite/wasm32-wasip1' '*/tests/c/testsuite/wasm32-wasip1' '*/tests/rust/testsuite/wasm32-wasip3' \
 && rm suite.tar.gz && ls adapter.wasm rust/testsuite/wasm32-wasip1/manifest.json c/testsuite/wasm32-wasip1/manifest.json \
      rust/testsuite/wasm32-wasip3/manifest.json

FROM base
COPY --from=qjs /qjs/cli/target/release/componentize-qjs /usr/local/bin/componentize-qjs
COPY --from=qjs-runtime /runtime.wasm /usr/local/lib/componentize-qjs-runtime.wasm
ENV TRIC_QJS_RUNTIME=/usr/local/lib/componentize-qjs-runtime.wasm
COPY --from=conformance /conformance /opt/conformance
ENV TRIC_CONFORMANCE=/opt/conformance
