#!/bin/sh
# Builds the Lambda zip from out/spinit-host and prints binary and zip sizes. The zip holds one file, `bootstrap` (the host binary):
# provided.al2023 runs /var/task/bootstrap, and the Web Adapter layer's wrapper (AWS_LAMBDA_EXEC_WRAPPER=/opt/bootstrap) runs it
# after starting the adapter extension, which does the Runtime API work and proxies to the host over HTTP.
# Limits: 50 MB zipped (direct upload), 250 MB unzipped including layers.
set -eu
cd "$(dirname "$0")/.."
[ -x out/spinit-host ] || docker/build-host.sh
rm -f out/spinit-host.zip
docker run --rm -v "$PWD/out":/out -w /tmp spinit-spike-latency-build:1 \
  sh -c 'rm -rf pkg && mkdir pkg && cp /out/spinit-host pkg/bootstrap && cd pkg && zip -q -9 -X /out/spinit-host.zip bootstrap && unzip -l /out/spinit-host.zip | tail -3 && ls -l /out/spinit-host /out/spinit-host.zip'
