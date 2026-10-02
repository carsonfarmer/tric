#!/bin/sh
# usage: rscheck.sh <endpoint> <outfile>   -- creates a fresh bucket, then runs the object_store harness against it
D=/opt/homebrew/bin/docker
B="rs$(date +%s)"
$D run --rm --name torpor-s3check-mkb --network torpor-s3check-net \
  -e AWS_ACCESS_KEY_ID="${S3_KEY:-torpor}" -e AWS_SECRET_ACCESS_KEY="${S3_SECRET:-torpor-secret}" -e AWS_DEFAULT_REGION=us-east-1 \
  amazon/aws-cli --endpoint-url "$1" s3api create-bucket --bucket "$B" >/dev/null || echo "create-bucket $B failed"
$D run --rm --name torpor-s3check-rs --network torpor-s3check-net -v "$PWD/target:/t:ro" \
  -e S3_ENDPOINT="$1" -e S3_BUCKET="$B" -e S3_KEY="${S3_KEY:-torpor}" -e S3_SECRET="${S3_SECRET:-torpor-secret}" \
  --entrypoint /t/debug/s3check rust:1.97.1-bookworm 2>&1 | tee "$2"
