//! Runaway-guest test component: routes that burn CPU or memory, for checking the host's epoch deadline and memory cap.
use spin_sdk::http::{IntoResponse, Request, Response};
use spin_sdk::http_component;
use std::{hint::black_box, time::Instant};

/// Pure compute that the optimizer cannot remove, with no host calls.
fn spin() -> ! {
    let mut i = 0u64;
    loop { i = black_box(i).wrapping_add(1); }
}

/// The same loop with one cheap WASI call (`wasi:clocks/monotonic-clock.now`) per iteration.
fn spin_calls() -> ! {
    loop { black_box(Instant::now()); }
}

/// Allocates and touches 1 MiB chunks until an allocation fails; returns how many MiB it got. With `abort`, uses the
/// infallible allocator instead, so the failure is the guest's own abort (a trap) rather than a handled error.
fn grow(abort: bool) -> usize {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    loop {
        let c = if abort { vec![1u8; 1 << 20] } else {
            let mut c = Vec::new();
            if c.try_reserve_exact(1 << 20).is_err() { return chunks.len(); }
            c.resize(1 << 20, 1);
            c
        };
        chunks.push(black_box(c));
    }
}

#[http_component]
fn handle_loop_p2(req: Request) -> anyhow::Result<impl IntoResponse> {
    let body = match req.path() {
        "/spin" => spin(),
        "/spin-calls" => spin_calls(),
        "/grow" => format!("grew {} MiB before the allocation failed", grow(false)),
        "/grow-abort" => format!("grew {} MiB", grow(true)),
        _ => "ok".to_string(),
    };
    Ok(Response::builder().status(200).header("content-type", "text/plain").body(body).build())
}
