#!/bin/bash
# usage: BIN=... matrix.sh app...   -- runs each app under minihost in 3 linker modes
A=../../apps
BIN=${BIN:-../minihost/target/release/minihost}
wasm_for() { case $1 in
 rust7-hello) echo app-rust7-hello/target/wasm32-wasip2/release/app_rust7_hello.wasm;;
 sdk6-p3rc) echo app-http-rust/target/wasm32-wasip2/release/app_http_rust.wasm;;
 rust-p2-hello) echo app-http-rust-p2/target/wasm32-wasip2/release/app_http_rust_p2.wasm;;
 sink7) echo app-sink7/target/wasm32-wasip2/release/app_sink7.wasm;;
 sink5) echo app-sink5/target/wasm32-wasip2/release/app_sink5.wasm;;
 js) echo app-http-js/dist/app-http-js.wasm;;
 ts) echo app-http-ts/dist/app-http-ts.wasm;;
 jssink) echo app-jssink/dist/app-jssink.wasm;;
 py-p3rc) echo app-http-py/app.wasm;;
 py-p2) echo app-http-py-p2/app.wasm;;
 esac; }
for name in "$@"; do
  cat > m-$name.toml <<EOT
spin_manifest_version = 2
[variables]
greet = { default = "hello" }
[[trigger.http]]
route = "/..."
component = "c"
[component.c]
source = "$A/$(wasm_for $name)"
allowed_outbound_hosts = ["https://example.com"]
key_value_stores = ["default"]
variables = { greeting = "{{ greet }} world" }
EOT
  for mode in ${MODES:-builtin none custom}; do
    pkill -f "minihost m-" 2>/dev/null; sleep 0.5
    (MODE=$mode $BIN m-$name.toml 127.0.0.1:18090 > log-$name-$mode.txt 2>&1 &)
    for i in $(seq 1 300); do grep -q "listening\|^Error" log-$name-$mode.txt && break; sleep 1; done
    body=$(curl -s -m 30 -w " [http=%{http_code}]" http://127.0.0.1:18090/x/y 2>&1 | head -c 120 | tr '\n' ' ')
    err=$(grep -m1 -E "^Error|error:" -A3 log-$name-$mode.txt | tr '\n' ' ' | head -c 300)
    echo "$name [$mode] body=[$body] err=[$err]"
  done
  pkill -f "minihost m-" 2>/dev/null
done
