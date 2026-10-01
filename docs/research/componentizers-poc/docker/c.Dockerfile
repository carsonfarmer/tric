FROM spinit-research-rust:1.97.1
RUN curl -sL https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-34/wasi-sdk-34.0-arm64-linux.tar.gz | tar xz -C /opt && mv /opt/wasi-sdk-34.0-arm64-linux /opt/wasi-sdk
RUN cargo install wit-bindgen-cli --version 0.62.0 --locked && wit-bindgen --version
