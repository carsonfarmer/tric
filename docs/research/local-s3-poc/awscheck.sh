#!/bin/sh
# usage: awscheck.sh <endpoint-url> <bucket>   (runs inside amazon/aws-cli with --entrypoint sh)
EP="$1"; B="$2"; T=/tmp/chk; mkdir -p $T
printf 'v1' >$T/v1; printf 'v2-new' >$T/v2; printf 'v3-cas' >$T/v3
# step <label> <expected-http-status> <aws s3api args...>: prints the command, HTTP status and verdict
step() {
  label="$1"; want="$2"; shift 2
  echo "\$ aws --endpoint-url \$EP s3api $*"
  aws --endpoint-url "$EP" --debug s3api "$@" >$T/out 2>$T/err; rc=$?
  got=$(grep -oE 'HTTP/1\.1" [0-9]{3}' $T/err | tail -1 | grep -oE '[0-9]{3}$')
  if [ "$got" = "$want" ]; then v=PASS; else v=FAIL; fi
  echo "  -> $v $label: HTTP $got (want $want) rc=$rc $(grep -m1 -oE 'An error occurred.*' $T/err | cut -c1-110)"
}
etag() { aws --endpoint-url "$EP" s3api head-object --bucket "$B" --key "$1" --query ETag --output text; }

echo "### 1. create-only (If-None-Match: *)"
step "1a first create" 200 put-object --bucket $B --key cas/k --body $T/v1 --if-none-match '*'
E1=$(etag cas/k); echo "  etag after 1a: $E1"
step "1b second create" 412 put-object --bucket $B --key cas/k --body $T/v2 --if-none-match '*'
echo "  etag after 1b: $(etag cas/k) (must equal $E1)"

echo "### 2. compare-and-swap (If-Match: <etag>)"
step "2a match current etag" 200 put-object --bucket $B --key cas/k --body $T/v3 --if-match "$E1"
E3=$(etag cas/k); echo "  etag after 2a: $E3 (must differ from $E1)"
step "2b match stale etag" 412 put-object --bucket $B --key cas/k --body $T/v2 --if-match "$E1"
echo "  etag after 2b: $(etag cas/k) (must equal $E3)"
step "2c if-match on missing key (AWS: 404)" 404 put-object --bucket $B --key cas/missing --body $T/v2 --if-match "$E1"

echo "### 3. conditional GET (If-None-Match: <etag>)"
step "3a if-none-match current etag" 304 get-object --bucket $B --key cas/k --if-none-match "$E3" $T/dl
step "3b if-none-match other etag" 200 get-object --bucket $B --key cas/k --if-none-match '"deadbeef"' $T/dl

echo "### 4. ListObjectsV2 with continuation tokens (25 keys, page size 10)"
i=0; while [ $i -lt 25 ]; do aws --endpoint-url "$EP" s3api put-object --bucket $B --key "list/obj-$(printf %02d $i)" --body $T/v1 >/dev/null || echo "put $i failed"; i=$((i+1)); done
tok=""; pages=0; total=0; : >$T/keys
while :; do
  if [ -z "$tok" ]; then aws --endpoint-url "$EP" s3api list-objects-v2 --bucket $B --prefix list/ --max-keys 10 --output json >$T/page 2>$T/perr
  else aws --endpoint-url "$EP" s3api list-objects-v2 --bucket $B --prefix list/ --max-keys 10 --continuation-token "$tok" --output json >$T/page 2>$T/perr; fi
  [ $? -ne 0 ] && { echo "  list error: $(head -c 200 $T/perr)"; break; }
  pages=$((pages+1))
  grep -oE '"Key": "[^"]+"' $T/page | cut -d'"' -f4 >>$T/keys
  trunc=$(grep -oE '"IsTruncated": (true|false)' $T/page | grep -oE 'true|false')
  tok=$(grep -oE '"NextContinuationToken": "[^"]+"' $T/page | cut -d'"' -f4)
  [ "$trunc" = "true" ] && [ -n "$tok" ] || break
done
n=$(wc -l <$T/keys | tr -d ' '); u=$(sort -u $T/keys | wc -l | tr -d ' ')
if [ "$pages" = 3 ] && [ "$n" = 25 ] && [ "$u" = 25 ] && sort -c $T/keys 2>/dev/null; then v=PASS; else v=FAIL; fi
echo "  -> $v 4a pages=$pages keys=$n unique=$u sorted=$(sort -c $T/keys 2>/dev/null && echo yes || echo no) (want 3/25/25/yes)"
echo "\$ aws --endpoint-url \$EP s3api list-objects-v2 --bucket $B --prefix list/ --max-keys 10 [--continuation-token <NextContinuationToken>] # looped"
