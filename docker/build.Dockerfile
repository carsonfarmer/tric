# Toolchain image: Amazon Linux 2023 (glibc 2.34, the same as the provided.al2023 Lambda runtime), plus Rust and the Wasm tools.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y gcc gcc-c++ make cmake perl git tar gzip xz zip findutils clang && dnf clean all
# The toolchain lives in the image; the cargo registry and target dir are named volumes mounted at run time.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
 && rustup target add wasm32-wasip2 && rustup component add rustfmt clippy && rustc --version && ldd --version | head -1
# Release binaries for wasm-tools and the wasmtime CLI. The CLI must match the Wasmtime version the host crate pins.
RUN arch=$(uname -m) \
 && curl -fsSL https://github.com/bytecodealliance/wasm-tools/releases/download/v1.260.0/wasm-tools-1.260.0-$arch-linux.tar.gz | tar xz --strip-components=1 -C /usr/local/bin --wildcards '*/wasm-tools' \
 && curl -fsSL https://github.com/bytecodealliance/wasmtime/releases/download/v49.0.2/wasmtime-v49.0.2-$arch-linux.tar.xz | tar xJ --strip-components=1 -C /usr/local/bin --wildcards '*/wasmtime' \
 && wasm-tools --version && wasmtime --version
