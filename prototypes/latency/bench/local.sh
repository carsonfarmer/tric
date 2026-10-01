#!/usr/bin/env bash
# Local (Docker) measurements for the latency spike. Indicative only: Docker on an M-series Mac is not Lambda.
# Usage: bench/local.sh [functional|cold|bucket|portable|warm|lwa|all]     (default: all)
# Raw logs go to out/local/; `python3 bench/stats.py` turns them into the tables in RESULTS.md.
# Needs: docker compose, curl, shasum, out/spinit-host and out/hello_*.wasm (docker/build-host.sh, components/build.sh).
set -euo pipefail
cd "$(dirname "$0")/.."
export COMPOSE_PROJECT_NAME=spinit-spike-latency
PORT=${SPINIT_HOST_PORT:-18080}
OUT=out/local; mkdir -p "$OUT"
COMPONENTS="hello_p3 hello_p2 hello_js"
# Lambda memory size -> Docker limits: Lambda gives 1 vCPU at 1769 MB and scales CPU linearly with memory (512 MB = 0.29, 128 MB = 0.07).
PROFILES="1.0:1769m 0.29:512m 0.07:128m"
WARM_N=${WARM_N:-3000}

start() { # start <component | sha256:hex> <cpus> <mem> <allocator> [precompiled]
  local spec=$1; case $1 in sha256:*) ;; *) spec=/out/$1.wasm;; esac
  HOST_CPUS=$2 HOST_MEM=$3 SPINIT_COMPONENT=$spec SPINIT_ALLOCATOR=${4:-default} SPINIT_PRECOMPILED=${5:-0} \
    docker compose up -d --force-recreate --no-deps host >/dev/null 2>&1
  for _ in $(seq 150); do curl -sf -m 1 "localhost:$PORT/__ready" >/dev/null 2>&1 && return 0; sleep 0.2; done
  echo "host did not become ready" >&2; return 1
}
hostlog() { docker compose logs --no-log-prefix --no-color host 2>&1 | grep '^{'; }
oomkilled() { docker inspect -f '{{.State.OOMKilled}}' "$(docker compose ps -a -q host)"; }
clear_cache() { docker compose run --rm --no-deps -T --entrypoint sh host -c 'rm -rf /cache/*' >/dev/null 2>&1; }
digest() { shasum -a 256 "out/$1.wasm" | cut -d' ' -f1; }
bench_op() { curl -s -m 60 "localhost:$PORT/__bench/$1${2:-}"; echo; }
# first request; writes the host log plus a status line (http code, OOM kill) to $1
first_request() {
  local code; code=$(curl -s -m 600 -o /dev/null -w '%{http_code}' "localhost:$PORT/" || true); sleep 0.5
  { hostlog; echo "{\"event\":\"status\",\"http\":\"${code:-000}\",\"oom\":\"$(oomkilled)\"}"; } > "$1"
}

functional() {
  echo "== functional: MinIO and DynamoDB Local"
  docker compose up -d minio ddb >/dev/null 2>&1
  start hello_p3 1.0 1769m default
  bench_op ddb-create-table >/dev/null || true
  bench_op seed >/dev/null
  { bench_op check; for op in s3-get s3-get-304 s3-put-create s3-put-update ddb-get-eventual ddb-get-strong ddb-put-cond; do bench_op $op '?n=20' | cut -c1-200; done; } | tee "$OUT/functional.log"
}

cold_one() { # cold_one <component> <cpus> <mem>
  local c=$1 cpus=$2 mem=$3
  echo "-- $c cpus=$cpus mem=$mem"
  clear_cache;  start "$c" "$cpus" "$mem" default; first_request "$OUT/cold-$c-$cpus-default-compile.log"
  # a compile that was OOM-killed leaves no artifact; build it with all CPUs so the deserialize run below is still measured
  prime "$c"
  start "$c" "$cpus" "$mem" default;                first_request "$OUT/cold-$c-$cpus-default-deserialize.log"
  if [ "$cpus" = 1.0 ]; then
    start "$c" "$cpus" "$mem" pooling;              first_request "$OUT/cold-$c-$cpus-pooling-deserialize.log"
  fi
}

cold() { # compile vs deserialize per component and CPU share, plus allocator comparison at full CPU
  echo "== cold path"
  for p in $PROFILES; do
    for c in $COMPONENTS; do cold_one "$c" "${p%%:*}" "${p##*:}"; done
  done
}

bucket() { # the cold path from the bucket: blob + compile (full CPU only) vs precompiled artifact, at each CPU share
  echo "== bucket (MinIO) fetch routes"
  docker compose up -d minio >/dev/null 2>&1
  for c in $COMPONENTS; do
    docker compose run --rm --no-deps -T --entrypoint /out/spinit-host host publish "/out/$c.wasm" 2>/dev/null | tee -a "$OUT/publish.log"
  done
  for p in $PROFILES; do
    local cpus=${p%%:*} mem=${p##*:}
    for c in $COMPONENTS; do
      local d; d=sha256:$(digest "$c")
      echo "-- $c cpus=$cpus mem=$mem"
      if [ "$cpus" = 1.0 ]; then clear_cache; start "$d" "$cpus" "$mem" default 0; first_request "$OUT/bucket-$c-$cpus-blob.log"; fi
      clear_cache; start "$d" "$cpus" "$mem" default 1; first_request "$OUT/bucket-$c-$cpus-cwasm.log"
    done
  done
}

portable() { # is a cwasm built with a baseline target (no host-specific ISA flags) accepted by a normal engine?
  echo "== portable cwasm (SPINIT_TARGET)"
  local d; d=$(digest hello_p3)
  docker compose run --rm --no-deps -T -e SPINIT_TARGET=aarch64-unknown-linux-gnu --entrypoint sh host \
    -c "/out/spinit-host precompile /out/hello_p3.wasm /cache/$d.49.0.1.cwasm" | tee "$OUT/portable.log"
  start hello_p3 1.0 1769m default; first_request "$OUT/portable-run.log"; grep -h '"event":"load"' "$OUT/portable-run.log" | tee -a "$OUT/portable.log"
}

prime() { # put .cwasm files in the cache (compiled with all CPUs: JS cannot be compiled at the 128 MB profile, see cold); default: all components
  for c in ${*:-$COMPONENTS}; do
    docker compose run --rm --no-deps -T -e SPINIT_ALLOCATOR=default --entrypoint sh host \
      -c "/out/spinit-host precompile /out/$c.wasm /cache/$(digest "$c").49.0.1.cwasm" >/dev/null
  done
}

warm() { # warm requests through the guest: sequential (c=1) per component and CPU share, plus c=16 and the pooling allocator at full CPU
  echo "== warm"
  prime
  for p in $PROFILES; do
    local cpus=${p%%:*} mem=${p##*:}
    for c in $COMPONENTS; do
      warm_one "$c" "$cpus" "$mem" default 1
      warm_one "$c" "$cpus" "$mem" default 1 50   # paced: back-to-back load exhausts the CPU quota of the small profiles (CFS throttling)
      if [ "$cpus" = 1.0 ]; then warm_one "$c" "$cpus" "$mem" default 16; warm_one "$c" "$cpus" "$mem" pooling 1; fi
    done
  done
  # the host without the guest: the floor for every request (route handled by the host)
  start hello_p3 1.0 1769m default; curl -s -o /dev/null "localhost:$PORT/"
  docker compose run --rm -T oha -n "$WARM_N" -c 1 --no-tui --output-format json http://host:8080/__ready > "$OUT/warm-ready-oha.json"
}
warm_one() { # warm_one <component> <cpus> <mem> <allocator> <concurrency> [requests per second: paced run of 1000]
  local tag=c$5${6:+q$6} n=${6:+1000}; n=${n:-$WARM_N}
  echo "-- $1 cpus=$2 alloc=$4 $tag"
  start "$1" "$2" "$3" "$4"
  for _ in $(seq 30); do curl -s -m 600 -o /dev/null "localhost:$PORT/"; done
  docker compose run --rm -T oha -n "$n" -c "$5" ${6:+-q "$6"} --no-tui --output-format json http://host:8080/ > "$OUT/warm-$1-$2-$4-$tag-oha.json"
  sleep 0.5; hostlog > "$OUT/warm-$1-$2-$4-$tag.log"
}

lwa() { # the host as a Lambda function (Web Adapter extension, Runtime Interface Emulator), invoked with an HTTP API v2 event
  echo "== lwa via RIE"
  docker compose build lwa >/dev/null 2>&1
  for c in hello_p3 hello_p2; do
    HOST_CPUS=1.0 HOST_MEM=1769m SPINIT_COMPONENT=/out/$c.wasm docker compose up -d --force-recreate --no-deps lwa >/dev/null 2>&1; sleep 2
    local url=localhost:${LWA_HOST_PORT:-19001}/2015-03-31/functions/function/invocations
    curl -s -m 120 -XPOST "$url" -d @bench/event.json >/dev/null
    for _ in $(seq 30); do curl -s -m 60 -XPOST "$url" -d @bench/event.json >/dev/null; done
    docker compose run --rm -T -v "$PWD/bench/event.json:/event.json:ro" oha -n "$WARM_N" -c 1 --no-tui --output-format json -m POST -D /event.json http://lwa:8080/2015-03-31/functions/function/invocations > "$OUT/lwa-$c-oha.json"
    sleep 0.5; docker compose logs --no-log-prefix --no-color lwa > "$OUT/lwa-$c.log" 2>&1
  done
}

what=${1:-all}
case $what in
  functional) functional;; cold) cold;; cold-one) shift; cold_one "$@";; bucket) bucket;; portable) portable;; warm) warm;; lwa) lwa;;
  all) functional; cold; bucket; portable; warm; lwa;;
  *) echo "usage: $0 [functional|cold|bucket|portable|warm|lwa|all]" >&2; exit 2;;
esac
docker compose stop host lwa >/dev/null 2>&1 || true
