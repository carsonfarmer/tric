FROM rust:1.97.1-bookworm
RUN rustup target add wasm32-wasip2
# Re-verified on stable 1.99.0 (released 2026-09-28; no rust:1.99.0 image existed on 2026-10-01):
# RUN rustup toolchain install 1.99.0 --profile minimal -t wasm32-wasip2
