#!/usr/bin/env bash
# Cloud half of the latency spike: forced cold starts, warm requests and raw S3/DynamoDB operation latencies, measured in AWS.
# WRITTEN BUT NOT RUN: the local phase had no credentials by design. Everything here talks to AWS, so read it first.
#
# Prerequisites
#   1. Build:    docker/build-host.sh && components/build.sh && lambda/package.sh
#   2. Deploy:   (cd infra && tofu init && tofu apply)           # or the same through the ghcr.io/opentofu/opentofu image
#   3. Tools:    aws CLI v2, curl >= 7.75 (--aws-sigv4), python3, docker; credentials for the account (AWS_PROFILE or AWS_* variables)
#      The caller needs lambda:InvokeFunctionUrl and lambda:InvokeFunction on the three functions, plus lambda:GetFunctionConfiguration,
#      lambda:UpdateFunctionConfiguration, logs:FilterLogEvents, and s3:PutObject on the bucket (seed uploads the blobs).
#   4. Run:      bench/cloud.sh all            (or one step at a time: seed functional cold warm bench report)
#   5. Teardown: (cd infra && tofu destroy)
#
# Steps
#   seed         upload each test component (blob + precompiled artifact) with `spinit-host publish`, create the KV object and table item
#   functional   conditional-operation checks against real S3 and DynamoDB (/__bench/check)
#   cold         forced cold starts: an environment variable is bumped before every request, which retires every warm execution
#                environment of that function. Modes: precompiled (artifact from the bucket), precompiled-eager (load in the init
#                phase), precompiled-zstd (the .zst copy from the bucket, decompressed in memory), compile (blob from the bucket,
#                Cranelift in the function)
#   warm         sequential requests to a warm environment through the guest
#   bench        raw storage operations from inside the function (S3 GET/304/PUT create/PUT If-Match, DynamoDB eventual/strong/conditional),
#                plus S3 GET and 304 on 80 KB and 800 KB objects (the global state object: ~78 KB at 1k apps, ~781 KB at 10k)
#   report       Markdown tables from everything collected (python3 bench/cloud_report.py)
# In-function numbers (REPORT lines and the host's JSON log lines, pulled from CloudWatch) are the ones that count. The curl timings
# include the network from here to us-west-2 and are only kept for reference.
#
# Knobs (environment): REGION=us-west-2  MEMS="128 512 1769"  COLD_N=30  WARM_N=300  BENCH_N=200  COMPONENTS="hello_p3 hello_p2 hello_js"
# p99 of 30 cold starts is the maximum; for a defensible p99 run e.g. COLD_N=100 bench/cloud.sh cold 1769 hello_p3 precompiled
# Rough cost of `all` with the defaults: well under $1 (about 600 cold starts and 3000 warm requests of at most a few seconds each at
# 128 to 1769 MB, a few thousand S3 and DynamoDB requests, CloudWatch ingestion of a few MB); see RESULTS.md.
set -euo pipefail
cd "$(dirname "$0")/.."

REGION=${REGION:-us-west-2}
MEMS=${MEMS:-"128 512 1769"}
COLD_N=${COLD_N:-30}
WARM_N=${WARM_N:-300}
BENCH_N=${BENCH_N:-200}
COMPONENTS=${COMPONENTS:-"hello_p3 hello_p2 hello_js"}
OUT=out/cloud; mkdir -p "$OUT"
export AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION AWS_PAGER=

need() { command -v "$1" >/dev/null || { echo "missing: $1" >&2; exit 1; }; }
need aws; need curl; need python3
curl --version | head -1 | python3 -c 'import re,sys; v=re.search(r"curl (\d+)\.(\d+)", sys.stdin.read()); sys.exit(0 if v and tuple(map(int, v.groups())) >= (7, 75) else 1)' ||
  { echo "curl >= 7.75 is needed for --aws-sigv4" >&2; exit 1; }

# Credentials for curl's SigV4 signing and for the publish container; resolved once from whatever the aws CLI is configured with.
creds() {
  if [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then eval "$(aws configure export-credentials --format env)"; fi
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
  [ -z "${AWS_SESSION_TOKEN:-}" ] || export AWS_SESSION_TOKEN
}

# OpenTofu outputs (bucket, table, function names, URLs, log groups), cached in out/cloud/tf.json.
tf_outputs() {
  if command -v tofu >/dev/null; then (cd infra && tofu output -json) > "$OUT/tf.json"
  else docker run --rm -v "$PWD":/w -w /w/infra ghcr.io/opentofu/opentofu:latest output -json > "$OUT/tf.json"; fi
}
out() { # out <key> [memory]
  python3 - "$@" <<'EOF'
import json, sys
v = json.load(open("out/cloud/tf.json"))[sys.argv[1]]["value"]
print(v[sys.argv[2]] if len(sys.argv) > 2 else v)
EOF
}

digest() { echo "sha256:$(shasum -a 256 "out/$1.wasm" | cut -d' ' -f1)"; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# One SigV4-signed request to the function URL of the given memory size. Body goes to $BODY (default: discarded);
# prints "<http code> <seconds>" as measured by curl from here.
call() { # call <memory> <path> [curl args]
  local mem=$1 path=$2; shift 2
  curl -sS -m 400 --aws-sigv4 "aws:amz:$REGION:lambda" --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" \
    ${AWS_SESSION_TOKEN:+-H "x-amz-security-token: $AWS_SESSION_TOKEN"} \
    -o "${BODY:-/dev/null}" -w '%{http_code} %{time_total}\n' "$(out function_urls "$mem")${path#/}" "$@"
}

# Merge KEY=VALUE pairs into the function's environment and wait for the update to finish. update-function-configuration replaces
# the whole environment, hence the read-merge-write. Any configuration change retires the existing execution environments.
setenv() { # setenv <memory> KEY=VALUE...
  local fn; fn=$(out function_names "$1"); shift
  aws lambda get-function-configuration --function-name "$fn" --query 'Environment.Variables' --output json |
    python3 -c 'import json, sys; v = json.load(sys.stdin) or {}; v.update(x.split("=", 1) for x in sys.argv[1:]); print(json.dumps({"Variables": v}))' "$@" \
    > "$OUT/env-$fn.json"
  aws lambda update-function-configuration --function-name "$fn" --environment "file://$OUT/env-$fn.json" >/dev/null
  aws lambda wait function-updated --function-name "$fn"
}

# CloudWatch lines (REPORT lines and the host's JSON lines) for [start, end] of a run, one per line, into out/cloud/<label>.log.
# Log delivery lags by several seconds, so wait before the first pull.
collect() { # collect <label> <memory>
  local label=$1 mem=$2
  aws logs filter-log-events --log-group-name "$(out log_groups "$mem")" \
    --start-time "$(cat "$OUT/$label.start")" --end-time "$(( $(cat "$OUT/$label.end") + 5000 ))" --output json |
    python3 -c 'import json, sys; [print(l) for e in json.load(sys.stdin)["events"] for l in e["message"].splitlines() if l.strip()]' > "$OUT/$label.log"
  echo "   $label: $(grep -c '^REPORT' "$OUT/$label.log" || true) REPORT lines, $(grep -c '"event":"req"' "$OUT/$label.log" || true) host request lines"
}

seed() {
  creds; tf_outputs
  local bucket; bucket=$(out bucket)
  echo "== seed: blobs and precompiled artifacts into s3://$bucket"
  for c in $COMPONENTS; do
    # Built in Docker; SPINIT_TARGET pins the compile to a baseline arm64 target so the artifact does not depend on this machine's CPU.
    docker run --rm -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN -e AWS_REGION="$REGION" \
      -e SPINIT_BUCKET="$bucket" -e SPINIT_TARGET=aarch64-unknown-linux-gnu -v "$PWD/out":/out:ro \
      --entrypoint /out/spinit-host public.ecr.aws/lambda/provided:al2023 publish "/out/$c.wasm" | tee -a "$OUT/publish.log"
  done
  echo "== seed: KV object and table item (through the largest function)"
  local top; top=$(echo "$MEMS" | tr ' ' '\n' | sort -n | tail -1)
  BODY="$OUT/seed.json" call "$top" "__bench/seed"; cat "$OUT/seed.json"; echo
}

functional() {
  creds; [ -f "$OUT/tf.json" ] || tf_outputs
  local top; top=$(echo "$MEMS" | tr ' ' '\n' | sort -n | tail -1)
  echo "== functional: conditional operations on real S3 and DynamoDB"
  BODY="$OUT/check.json" call "$top" "__bench/check"; python3 -m json.tool "$OUT/check.json"
}

cold() { # cold <memory> <component> <mode> [n]
  local mem=$1 comp=$2 mode=$3 n=${4:-$COLD_N} pre eager zst label
  case $mode in
    precompiled) pre=1 eager=0 zst=0;; precompiled-eager) pre=1 eager=1 zst=0;; precompiled-zstd) pre=1 eager=0 zst=1;; compile) pre=0 eager=0 zst=0;;
    *) echo "mode: precompiled | precompiled-eager | precompiled-zstd | compile" >&2; return 2;;
  esac
  label=cold-$comp-$mode-$mem
  echo "-- $label (n=$n)"
  setenv "$mem" "SPINIT_COMPONENT=$(digest "$comp")" "SPINIT_PRECOMPILED=$pre" "SPINIT_EAGER=$eager" "SPINIT_ZSTD=$zst"
  : > "$OUT/$label.csv"
  now_ms > "$OUT/$label.start"
  for i in $(seq "$n"); do
    setenv "$mem" "COLD_NONCE=$label-$i-$(now_ms)"
    echo "$i $(call "$mem" /)" >> "$OUT/$label.csv"          # <i> <http code> <seconds from here>
  done
  now_ms > "$OUT/$label.end"; sleep 20; collect "$label" "$mem"
}

warm() { # warm <memory> <component> [n]
  local mem=$1 comp=$2 n=${3:-$WARM_N} label; label=warm-$comp-$mem
  echo "-- $label (n=$n)"
  setenv "$mem" "SPINIT_COMPONENT=$(digest "$comp")" SPINIT_PRECOMPILED=1 SPINIT_EAGER=0 SPINIT_ZSTD=0 "COLD_NONCE=$label-$(now_ms)"
  for _ in $(seq 20); do call "$mem" / >/dev/null; done      # the first one is the cold start, the rest settle the environment
  : > "$OUT/$label.csv"
  now_ms > "$OUT/$label.start"
  for i in $(seq "$n"); do echo "$i $(call "$mem" /)" >> "$OUT/$label.csv"; done
  now_ms > "$OUT/$label.end"; sleep 20; collect "$label" "$mem"
}

bench_ops() { # bench_ops <memory>: raw storage latencies from inside the function; the response bodies are the results
  local mem=$1 op kb
  echo "-- bench $mem"
  for kb in 1 80 800; do BODY=/dev/null call "$mem" "__bench/seed?kb=$kb" >/dev/null; done
  for op in s3-get s3-get-304 s3-put-create s3-put-update ddb-get-eventual ddb-get-strong ddb-put-cond; do
    BODY=/dev/null call "$mem" "__bench/$op?n=5" >/dev/null                      # connections and SDK clients warmed first
    BODY="$OUT/bench-$mem-$op.json" call "$mem" "__bench/$op?n=$BENCH_N" >/dev/null
  done
  for kb in 80 800; do for op in s3-get s3-get-304; do                          # the same reads on the size of the global state object
    BODY=/dev/null call "$mem" "__bench/$op?n=5&kb=$kb" >/dev/null
    BODY="$OUT/bench-$mem-$op-${kb}kb.json" call "$mem" "__bench/$op?n=$BENCH_N&kb=$kb" >/dev/null
  done; done
}

all_cold() {
  local mem c
  for mem in $MEMS; do
    for c in $COMPONENTS; do cold "$mem" "$c" precompiled; done
    cold "$mem" hello_p3 precompiled-eager; cold "$mem" hello_js precompiled-zstd
    # Blob + Cranelift in the function: p3 and p2 at every size, the 13 MB JS component only at the full vCPU (it does not fit 128 MB).
    cold "$mem" hello_p3 compile; cold "$mem" hello_p2 compile
  done
  cold 1769 hello_p3 precompiled-zstd; cold 1769 hello_js compile
}

all_warm() { local mem c; for mem in $MEMS; do for c in $COMPONENTS; do warm "$mem" "$c"; done; done; }
all_bench() { local mem; for mem in $MEMS; do bench_ops "$mem"; done; }

what=${1:-all}; shift || true
case $what in
  seed) seed;;
  functional) functional;;
  cold) creds; [ -f "$OUT/tf.json" ] || tf_outputs; if [ $# -ge 3 ]; then cold "$@"; else all_cold; fi;;
  warm) creds; [ -f "$OUT/tf.json" ] || tf_outputs; if [ $# -ge 2 ]; then warm "$@"; else all_warm; fi;;
  bench) creds; [ -f "$OUT/tf.json" ] || tf_outputs; if [ $# -ge 1 ]; then bench_ops "$1"; else all_bench; fi;;
  report) python3 bench/cloud_report.py "$OUT";;
  all) seed; functional; all_cold; all_warm; all_bench; python3 bench/cloud_report.py "$OUT" | tee "$OUT/report.md";;
  *) echo "usage: $0 [seed|functional|cold [<mem> <component> <mode> [n]]|warm [<mem> <component> [n]]|bench [<mem>]|report|all]" >&2; exit 2;;
esac
