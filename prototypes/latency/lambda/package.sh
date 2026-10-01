#!/bin/sh
# Builds the Lambda zip (run.sh + spinit-host) from out/spinit-host and prints binary and zip sizes.
# The Web Adapter comes from the layer, not the zip. Limits: 50 MB zipped (direct upload), 250 MB unzipped including layers.
set -eu
cd "$(dirname "$0")/.."
[ -x out/spinit-host ] || docker/build-host.sh
rm -f out/spinit-host.zip
docker run --rm -v "$PWD/out":/out -v "$PWD/lambda":/lambda:ro -w /tmp spinit-spike-latency-build:1 \
  sh -c 'rm -rf pkg && mkdir pkg && cp /lambda/run.sh /out/spinit-host pkg/ && cd pkg && zip -q -9 -X /out/spinit-host.zip run.sh spinit-host && ls -l /out/spinit-host /out/spinit-host.zip'
