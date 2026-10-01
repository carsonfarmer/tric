#!/bin/sh
# Runs inside a linux container (curl + /wt/wasmtime present).
# usage: run-case.sh <component.wasm> [extra `wasmtime serve` flags...]
# Serves the component on stock Wasmtime with keyvalue+config enabled, curls "/" once, prints result.
WASM=$1; shift
/wt/wasmtime serve --addr 127.0.0.1:8080 \
  -S keyvalue -S config \
  -S config-var=greeting=hello -S keyvalue-in-memory-data=seed=s1 \
  "$@" "$WASM" >/tmp/serve.log 2>&1 &
PID=$!
i=0
while [ $i -lt 100 ]; do
  kill -0 $PID 2>/dev/null || break
  curl -s -o /dev/null --max-time 1 http://127.0.0.1:8080/ 2>/dev/null && break
  i=$((i+1)); sleep 0.1
done
if kill -0 $PID 2>/dev/null; then
  echo "== response (first request incl. compile+instantiate, then second)"
  curl -s -i --max-time 30 -w '\n[time_total=%{time_total}s]\n' http://127.0.0.1:8080/
  curl -s -o /dev/null --max-time 30 -w '[second request time_total=%{time_total}s]\n' http://127.0.0.1:8080/
else
  echo "== server exited before serving"
fi
kill $PID 2>/dev/null; wait $PID 2>/dev/null
echo "== server log"
head -c 3000 /tmp/serve.log
