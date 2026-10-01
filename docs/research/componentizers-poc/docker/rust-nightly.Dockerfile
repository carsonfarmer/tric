FROM spinit-research-rust:1.97.1
RUN rustup toolchain install nightly --profile minimal -c rust-src && rustup target add wasm32-wasip3 --toolchain nightly
