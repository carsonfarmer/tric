# Build image: Amazon Linux 2023 == the glibc (2.34) of the provided.al2023 Lambda runtime.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y gcc gcc-c++ make cmake perl git tar gzip xz zip findutils clang && dnf clean all
# Toolchain lives in the image; the cargo registry and target dir are named volumes mounted at run time.
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
 && rustup target add wasm32-wasip2 && rustc --version && ldd --version | head -1
