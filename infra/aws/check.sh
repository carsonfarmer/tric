#!/usr/bin/env bash
# shellcheck disable=SC2086 # unquoted words split on purpose ($run is digits; the lists of checks are words)
# The plan's remote checks, against an install from `tofu apply`. Run this copy in the checkout that applied the
# install, as its tofu state is there, with `main`'s HEAD checked out, the account's credentials in the environment,
# TF_VAR_domain set, the applied package in dist/tric.zip, and the fixtures built (`docker compose run --rm fixtures`).
# It runs from the checkout's root, and uses compose's project there (COMPOSE_PROJECT_NAME, or compose's default):
#
#   bash infra/aws/check.sh        # the checks, about 30 minutes
#   bash infra/aws/check.sh late   # 65 minutes to 3 hours later: CloudTrail's record of a minted policy, and a
#                                  # sweep of what the run left an hour old
#
# It deploys four apps (alpha, beta, gamma, delta), each of which has a sweep schedule in the cron group. It deletes
# every schedule it made (a retry's last one deletes itself when it fires) and the objects it planted by hand (one of
# them by the late sweep, which it is left for), and changes nothing else. Every name it writes carries the run's
# nonce, so it can run again. It prints PASS or FAIL for each check, and NOTE for what to read by eye, and keeps
# heads-RUN.json, routes-RUN.json and tree-RUN.json in target/live, and late.env there for `late`. It never prints
# credentials, and reads tofu's outputs, never its state.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 2
P=$(docker compose config --format json 2>/dev/null | jq -r .name) && [ -n "$P" ] || {
  echo "FAIL  no compose project here"
  exit 2
}
live=$PWD/target/live
work=$live/apps
mkdir -p "$work"
fails=0 checks=0
pass() { checks=$((checks + 1)); printf 'PASS  %s\n' "$1"; }
fail() { checks=$((checks + 1)); fails=$((fails + 1)); printf 'FAIL  %s%s\n' "$1" "${2:+: $2}"; }
note() { printf 'NOTE  %s\n' "$*"; }
die() { printf 'FAIL  %s\n' "$*"; exit 2; }
is() { if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "got [$2], want [$3]"; fi; }
summary() { echo; echo "$checks checks, $fails failed"; }
# What `aws ...` answered: OK, its error code, or "CLI: <text>" if it never reached AWS.
awscode() {
  local e
  if e=$("$@" 2>&1 >/dev/null); then echo OK; return; fi
  sed -n 's/.*An error occurred (\([A-Za-z]*\)).*/\1/p' <<<"$e" | head -1 | grep . || echo "CLI: $(head -c 120 <<<"$e")"
}

echo "== preflight"
out() { docker compose -p $P run --rm -T tofu output -raw "$1" </dev/null 2>/dev/null; }
for k in TRIC_BUCKET TRIC_SCHEDULES TRIC_EVENTS TRIC_SCHEDULER_ROLE TRIC_SWEEP_EVERY AWS_REGION; do
  v=$(out $k) && [ -n "$v" ] || die "tofu output $k is empty: is the install applied, and TF_VAR_domain set?"
  export "$k=$v"
done
# The prefix of every name the install made, `tric` unless it was applied with another.
NAME=$(out name) && [ -n "$NAME" ] || die "tofu output name is empty: is the install applied?"
DOMAIN=${TF_VAR_domain:?set TF_VAR_domain}
ACCOUNT=$(aws sts get-caller-identity --query Account --output text) || die "no AWS credentials in the environment"
FN="arn:aws:lambda:$AWS_REGION:$ACCOUNT:function"
B3="arn:aws:s3:::$TRIC_BUCKET"

# The session policy the router mints an app's credentials with: src/route.rs's `policy`. The last statement is the
# sweep's: a listing of the app's values, and of its names' heads, and nothing else.
session() {
  jq -cn --arg b "$B3" --arg a "$1" '{Version: "2012-10-17", Statement: [
    {Effect: "Allow", Action: ["s3:GetObject", "s3:GetObjectVersion", "s3:PutObject", "s3:DeleteObject"],
     Resource: ["\($b)/apps/\($a)/names/*", "\($b)/apps/\($a)/values/*", "\($b)/native/\($a)/*"]},
    {Effect: "Allow", Action: ["s3:GetObject", "s3:GetObjectVersion"], Resource: ["\($b)/apps/\($a)/*"]},
    {Effect: "Allow", Action: ["s3:ListBucket"], Resource: [$b],
     Condition: {StringLike: {"s3:prefix": ["apps/\($a)/values/*", "apps/\($a)/names/*"]}}}]}'
}
# What the run leaves for `late`: its nonce, the key of the stray it planted and when, the objects of hist-RUN, and
# those of them that its second write made.
state=$live/late.env

# The checks are of the code on main, which the package was built from: so the checkout has to be at main's HEAD.
[ "$(git rev-parse HEAD)" = "$(git rev-parse main)" ] ||
  die "this checkout is not at main's HEAD ($(git rev-parse --short main)): git checkout --detach main"
aws lambda invoke --generate-cli-skeleton input 2>/dev/null | jq -e 'has("TenantId")' >/dev/null ||
  die "this AWS CLI has no --tenant-id"
ip=$(curl -4 -fsS --max-time 10 https://checkip.amazonaws.com) || die "no public IPv4 address"
[ -f tests/fixtures/app.wasm ] || die "no tests/fixtures/app.wasm: docker compose -p $P run --rm fixtures"
docker image inspect $P-build >/dev/null 2>&1 || die "no $P-build image: docker compose -p $P run --rm fixtures"
run=$(date +%s)
note "run $run; the account's Lambda concurrency is \
$(aws lambda get-account-settings --query AccountLimit.ConcurrentExecutions --output text)"
# Whether serve logs at debug, which the tree's and the heads' numbers need: RUST_LOG's value is never printed.
debug=no
case $(aws lambda get-function-configuration --function-name $NAME-serve \
  --query Environment.Variables.RUST_LOG --output text) in *tric=debug*) debug=yes ;; esac
want=$(openssl dgst -sha256 -binary dist/tric.zip | base64)
for f in route events serve; do
  is "$NAME-$f runs dist/tric.zip" \
    "$(aws lambda get-function-configuration --function-name $NAME-$f --query CodeSha256 --output text)" "$want"
done
mtime() { stat -f %m "$1" 2>/dev/null || stat -c %Y "$1"; }
code="src wit Cargo.toml Cargo.lock docker/bootstrap docker/build.Dockerfile" # what goes into dist/tric.zip
if [ "$(mtime dist/tric.zip)" -ge "$(git log -1 --format=%ct -- $code)" ]; then
  pass "dist/tric.zip is newer than the last commit to the code"
else fail "dist/tric.zip is newer than the last commit to the code" "rebuild: docker compose -p $P run --rm package"; fi
git diff --quiet HEAD -- $code || note "the code has uncommitted changes"

A=https://alpha.$DOMAIN
B=https://beta.$DOMAIN
get() { curl -4 -sS --max-time 70 "$@"; }
post() { curl -4 -sS --max-time 70 -X POST "$@"; }
code() { curl -4 -sS -o /dev/null -w '%{http_code}' --max-time 70 "$@"; }
# The value of key $3 in name $2 of app $1, as JSON; "ERR" when the answer isn't {"ok": ...}.
value() {
  local r
  r=$(get "$1/@$2/kv?op=get&key=$3" | jq -c 'if type == "object" and has("ok") then .ok else "ERR" end' 2>/dev/null)
  echo "${r:-\"ERR\"}"
}
# Waits up to $4 seconds for key $3 in name $2 of app $1 to hold an echo, which it prints.
wait_echo() {
  local end=$((SECONDS + $4)) v
  while [ $SECONDS -lt $end ]; do
    v=$(value "$1" "$2" "$3")
    if [ "$v" != null ] && [ "$v" != '"ERR"' ] && [ -n "$v" ]; then jq -r . <<<"$v"; return 0; fi
    sleep 3
  done
  return 1
}
header() { jq -r --arg n "$1" '[.headers[] | select(.[0] == $n) | .[1]] | join(" | ")'; }
# Invokes function $1 with payload $2, and further arguments; prints the payload of the answer.
invoke() {
  local fn=$1 payload=$2 o
  shift 2
  o=$(mktemp)
  aws lambda invoke --function-name "$fn" --cli-binary-format raw-in-base64-out --cli-read-timeout 120 \
    --payload "$payload" "$@" "$o" >/dev/null || { rm -f "$o"; return 1; }
  tr -d '\000' <"$o"
  rm -f "$o"
}
status() { grep -aoE '"statusCode": ?[0-9]+' | head -1 | grep -oE '[0-9]+$'; }
now() { echo $(($(date +%s) * 1000)); }
# The items of a list that the AWS CLI printed as text, one to a line: none for an empty list, which it prints as None.
listed() { tr '\t' '\n' | grep -v '^None$' | grep .; }
# The number of events in function $1's log since $2 (ms) that match $3.
nlogs() {
  aws logs filter-log-events --log-group-name "/aws/lambda/$NAME-$1" --start-time "$2" --filter-pattern "$3" \
    --query 'length(events)' --output text
}
# Waits up to $4 seconds for function $1's log since $2 to have $5 lines (one, by default) that match filter pattern $3.
awaitpat() {
  local end=$((SECONDS + $4)) n
  while [ $SECONDS -lt $end ]; do
    n=$(nlogs "$1" "$2" "$3")
    [[ $n =~ ^[0-9]+$ ]] && [ "$n" -ge "${5:-1}" ] && return 0
    sleep 5
  done
  return 1
}
# The same for a line with the text $3.
awaitlog() { awaitpat "$1" "$2" "\"$3\"" "$4"; }
# The number of invocations of function $1 since $2 (ms) that have ended, by their REPORT lines.
reports() {
  local n
  n=$(nlogs "$1" "$2" REPORT)
  echo "${n:-?}"
}
# Waits up to $4 seconds for function $1 to have $3 invocations or more end since $2 (ms).
awaitreports() { awaitpat "$1" "$2" REPORT "$4" "$3"; }
# The number of lines of function $1's log since $2 (ms) that are of a sweep: the router's "sweep", serve's "swept", and
# any warning that begins "sweep:" (and not serve's "sweep: reclaimed" for a name it deleted from, which is no warning).
sweeplines() {
  local p='($.fields.message = "sweep") || ($.fields.message = "swept")'
  nlogs "$1" "$2" "{ $p || (\$.level = \"WARN\" && \$.fields.message = \"sweep:*\") }"
}
# The fields of the last "swept" line of app $1 in serve's log since $2 (ms), as compact JSON; nothing if there is none.
sweepreport() {
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-serve --start-time "$2" \
    --filter-pattern "{ \$.fields.message = \"swept\" && \$.fields.app = \"$1\" }" --query 'events[].message' \
    --output json | jq -c '[.[] | fromjson | .fields] | last // empty'
}
# The first $2 (all, by default) hex digits of the SHA-256 of $1.
sha() { printf '%s' "$1" | openssl dgst -sha256 -r | cut -c1-"${2:-64}"; }
# App $1's minute for its sweep, of an interval of $2 minutes: src/sweep.rs's `schedule`, which takes the first 16 hex
# digits of the app's SHA-256 as a number (here as two halves, so that it fits) modulo the minutes of a day, or of the
# hours of TRIC_SWEEP_EVERY.
sweepminute() {
  local h
  h=$(sha "$1" 16)
  echo $(((16#${h:0:8} % $2 * 4294967296 + 16#${h:8:8}) % $2))
}
# The expression of app $1's sweep schedule, every TRIC_SWEEP_EVERY: "M H-23/N * * *" every N hours, "M H */N * *" every
# N days, as src/cron.rs's `eventbridge` writes them for Scheduler, which lists the hours or days, or has "*" for all.
sweepexpr() {
  local n=${TRIC_SWEEP_EVERY%?} m list
  case $TRIC_SWEEP_EVERY in
    *h)
      m=$(sweepminute "$1" $((n * 60)))
      list=$(seq $((m / 60)) $n 23 | paste -sd, -)
      [ $n = 1 ] && list='*'
      echo "cron($((m % 60)) $list * * ? *)"
      ;;
    *d)
      m=$(sweepminute "$1" 1440)
      list=$(seq 1 $n 31 | paste -sd, -)
      [ $n = 1 ] && list='*'
      echo "cron($((m % 60)) $((m / 60)) $list * ? *)"
      ;;
  esac
}
# The names of app $1's schedules in the cron group, which all begin with the hash of its name (src/deploy.rs).
schedules() {
  aws scheduler list-schedules --group-name "$TRIC_SCHEDULES" --name-prefix "$(sha "$1" 24)-" \
    --query 'Schedules[].Name' --output text | listed
}
# Schedule $1 of the cron group, as JSON.
schedule() { aws scheduler get-schedule --group-name "$TRIC_SCHEDULES" --name "$1" --output json; }
# The names of the schedules of app $1 that are its sweep: those whose input is {"app", "sweep": true}.
sweepsof() {
  local n
  for n in $(schedules "$1"); do
    schedule "$n" | jq -e --arg a "$1" '.Target.Input | fromjson == {app: $a, sweep: true}' >/dev/null && echo "$n"
  done
}
# The keys under prefix $1 of the bucket, one to a line, in order.
under() {
  aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix "$1" --query 'Contents[].Key' --output text | listed | sort
}
# Asks for alpha's sweep as Scheduler does, by the cron alias with {app, sweep}, and checks that the ask was taken.
asksweep() {
  local o
  o=$(mktemp)
  is "the cron alias takes the ask, and its function does not fail" "$(aws lambda invoke --function-name $NAME-events \
    --qualifier cron --cli-binary-format raw-in-base64-out --cli-read-timeout 330 \
    --payload '{"app":"alpha","sweep":true}' --query '[StatusCode, FunctionError]' --output text "$o" |
    tr '\t' ' ')" "200 None"
  rm -f "$o"
}

if [ "${1:-}" = late ]; then
  since=$(date -u -v-3H +%FT%TZ 2>/dev/null || date -u -d '3 hours ago' +%FT%TZ)
  minted=$(aws cloudtrail lookup-events --lookup-attributes AttributeKey=EventName,AttributeValue=AssumeRole \
    --start-time "$since" --max-results 50 --query 'Events[].CloudTrailEvent' --output json |
    jq -r '.[] | fromjson | select(.requestParameters.roleSessionName == "alpha")
      | .requestParameters.policy // empty' |
    head -1)
  if [ -n "$minted" ]; then
    is "CloudTrail's session policy for alpha is the code's" "$(jq -cS . <<<"$minted")" "$(session alpha | jq -cS .)"
  else fail "CloudTrail's session policy for alpha" "no AssumeRole for alpha in 3 hours, or no policy in it"; fi

  echo "== sweep, an hour on (what is old, and what only a replaced head names)"
  # The run left hist-RUN written twice, so that what its first tree alone has is named by no head, and a stray beside
  # it that nothing names. An hour on, all of it is old enough to delete: a sweep must take the stray, and keep the tree
  # that the head names now whole. So this is the sweep on S3 as it is: its listing of the heads, its reads, and its
  # deletes, with the app's credentials and their conditions on the prefix.
  [ -f "$state" ] || die "no $state: run the checks first, and this in 65 minutes to 3 hours"
  aged='' at=0 hist='' made=''
  # shellcheck source=/dev/null
  . "$state" # run, aged, at, hist, made
  wait=$((at + 3900 - $(date +%s)))
  [ $wait -le 0 ] || die "too early: the stray is not an hour old to a sweep; again in $(((wait + 59) / 60)) minutes"
  is "hist-$run has its trees and the stray, as the run left them" "$(under apps/alpha/values/hist-$run/ | xargs)" \
    "$(printf '%s\n' $hist $aged | sort | xargs)"
  since=$(now)
  asksweep
  if awaitpat serve "$since" '{ $.fields.message = "swept" && $.fields.app = "alpha" }' 90; then
    report=$(sweepreport alpha "$since")
    is "none was skipped, none failed, and it was done" "$(jq -r '[.skipped, .failed, .done] | join(" ")' \
      <<<"$report")" "0 0 true"
    note "the sweep's report is $report"
  else fail "serve reports alpha's sweep" "no 'swept' line in serve's log in 90 s"; fi
  is "and serve logged no warning of the sweep's" "$(sweeplines serve "$since")" 1
  is "the stray, an hour old and named by nothing, is deleted" "$(under "$aged" | grep -c .)" 0
  left=$(under apps/alpha/values/hist-$run/)
  is "of hist-$run, what is left is what the run left, with all that its second write made" \
    "$(comm -13 <(printf '%s\n' $hist | sort) <(echo "$left") | grep -c .)/$(comm -23 <(printf '%s\n' $made | sort) \
    <(echo "$left") | grep -c .)" 0/0
  note "of hist-$run's $(printf '%s\n' $hist | grep -c .) objects, $(echo "$left" | grep -c .) are left: what the \
first write alone made is named by no head, and goes"
  is "hist-$run's key reads as it was last written, from the tree its head names" "$(value "$A" hist-$run k)" '"two"'
  if [ "$(under "$aged" | grep -c .)" != 0 ]; then
    aws s3api delete-object --bucket "$TRIC_BUCKET" --key "$aged" >/dev/null || fail "delete $aged"
  fi
  rm -f "$state"
  summary
  exit $((fails > 0))
fi

echo "== deploy"
toml() {
  mkdir -p "$work/$1" && printf 'component = "/src/tests/fixtures/app.wasm"\n%s' "${2:-}" >"$work/$1/tric.toml"
}
deploy() {
  if docker run --rm -v "$PWD:/src" -v ${P}_cargo:/opt/cargo/registry -v ${P}_target:/src/target -v "$work:/work" \
    -w /src -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN -e AWS_REGION \
    -e TRIC_BUCKET -e TRIC_SCHEDULES -e TRIC_EVENTS -e TRIC_SCHEDULER_ROLE -e TRIC_SWEEP_EVERY \
    $P-build cargo run --locked --release -q -- deploy "/work/$1" </dev/null; then pass "deploy $1"
  else fail "deploy $1"; fi
}
# Alpha's cron, removed by deploying alpha without it (deploy syncs the schedules even when there are none): its sweep
# stays, as every app's does.
uncrond=
uncron() {
  [ -n "$uncrond" ] && return
  uncrond=1
  toml alpha
  deploy alpha
}
# What the run made and tric never removes, once each. No tric command removes an app, and so none its sweep (deploy
# keeps the schedules of the release it deploys, the sweep among them), so the run deletes the schedules of its four
# apps, and the retries of its delivery (one-time schedules, which delete themselves when they have fired), and the
# objects it planted by hand: with a delete marker, which the bucket's lifecycle rules take with the version in a day.
unscheduled=
unschedule() {
  [ -n "$unscheduled" ] && return
  unscheduled=1
  local a n
  for a in alpha beta gamma delta; do
    for n in $(schedules $a); do
      aws scheduler delete-schedule --group-name "$TRIC_SCHEDULES" --name "$n" >/dev/null || fail "delete $a's schedule"
    done
    is "no schedule of $a is left" "$(schedules $a | grep -c .)" 0
  done
  if [ -n "${prefix:-}" ]; then
    for n in $(aws scheduler list-schedules --group-name $NAME-retry --name-prefix "$prefix" \
      --query 'Schedules[].Name' --output text | listed); do
      aws scheduler delete-schedule --group-name $NAME-retry --name "$n" >/dev/null || fail "delete a retry"
    done
    is "no retry of the run's delivery is left" "$(aws scheduler list-schedules --group-name $NAME-retry \
      --name-prefix "$prefix" --query 'length(Schedules)' --output text)" 0
  fi
}
planted= # the keys of the objects the run put in the bucket, each after a space
tornup=
teardown() {
  [ -n "$tornup" ] && return
  tornup=1
  local k
  uncron
  unschedule
  for k in $planted; do
    aws s3api delete-object --bucket "$TRIC_BUCKET" --key "$k" >/dev/null || fail "delete $k"
    is "the planted $k is deleted" "$(aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix "$k" \
      --query KeyCount --output text)" 0
  done
}
toml alpha "$(printf '\n[cron]\n"* * * * *" = "/@ticks-%s/echo"\n' "$run")"
toml beta
toml gamma
hosts='"https://checkip.amazonaws.com", "http://127.0.0.1:3000", "http://169.254.169.254"'
toml delta "allowed_outbound_hosts = [$hosts]
"
deploy alpha
trap teardown EXIT
deploy beta
deploy gamma
deploy delta
note "schedules: $(aws scheduler list-schedules --group-name "$TRIC_SCHEDULES" --query 'Schedules[].Name' \
  --output text)"

echo "== sweep schedules (each app's, in the cron group)"
# An app's sweep is a schedule in the cron group, every TRIC_SWEEP_EVERY, at the minute src/sweep.rs's `schedule` gives
# it of that interval, from the SHA-256 of its name; its input is {"app", "sweep": true}, and it runs the cron alias as
# Scheduler's role (src/deploy.rs's `wanted`). The apps with a cron job have its schedule too.
for a in alpha beta gamma delta; do
  jobs=0
  [ $a = alpha ] && jobs=1
  is "$a has a schedule for its sweep, and for each of its $jobs cron jobs" "$(schedules $a | grep -c .)" $((jobs + 1))
  is "$a has one sweep" "$(sweepsof $a | grep -c .)" 1
  n=$(sweepsof $a | head -1)
  if [ -n "$n" ]; then
    sched=$(schedule "$n")
    is "$a's sweep is at $(sweepexpr $a)" "$(jq -r .ScheduleExpression <<<"$sched")" "$(sweepexpr $a)"
    is "$a's sweep is in UTC, in no flexible window, and on" "$(jq -r '[.ScheduleExpressionTimezone // "UTC",
      .FlexibleTimeWindow.Mode, .State] | join(" ")' <<<"$sched")" "UTC OFF ENABLED"
    is "$a's sweep is in the cron group, and runs the cron alias as Scheduler's role" "$(jq -r \
      --arg g "$TRIC_SCHEDULES" --arg t "$TRIC_EVENTS" --arg r "$TRIC_SCHEDULER_ROLE" \
      '.GroupName == $g and .Target.Arn == $t and .Target.RoleArn == $r' <<<"$sched")" true
  else fail "$a's sweep schedule can be read"; fi
done
# A schedule is named for all it is, so a deploy that changes nothing changes none.
before=$(schedules beta | sort)
deploy beta
is "a deploy again leaves beta's schedules as they were" "$(schedules beta | sort)" "$before"

echo "== through CloudFront"
s=$(code "$A/")
[ "$s" = 400 ] && note "400 from the router: CloudFront-Viewer-Address didn't arrive"
[ "$s" = 403 ] && note "403: the origin secret or X-Forwarded-Host didn't arrive"
is "alpha answers" "$(get "$A/")" hello
is "beta answers" "$(get "$B/")" hello
echo=$(get "$A/echo" -H 'Forwarded: for=_cron' -H 'X-Forwarded-For: 9.9.9.9' -H "X-Forwarded-Host: beta.$DOMAIN" \
  -H "X-Forwarded-Host: beta.$DOMAIN" -H 'X_Forwarded_For: 9.9.9.9' -H 'X-Tric-Credentials: {}' \
  -H 'X-Amz-Tenant-Id: beta' -H 'CloudFront-Viewer-Address: 9.9.9.9:1' -H 'X-Tric-Origin: guess' \
  -H 'Connection-Id: c' -H 'Meta-User: u' -H 'Content-Type: application/websocket-events')
if jq -e '.uri | strings' >/dev/null 2>&1 <<<"$echo"; then
  is "the app is alpha, at https" "$(jq -r .uri <<<"$echo")" "$A/echo"
  is "one Forwarded, the router's, with the viewer's address" "$(header forwarded <<<"$echo")" \
    "for=$ip;host=\"alpha.$DOMAIN\";proto=https"
  is "no X-Forwarded-*, X-Amz*, X-Tric-* or a socket's marks reach the app" \
    "$(jq -r '[.headers[][0] | ascii_downcase | gsub("_"; "-")
      | select(test("^(x-forwarded-|x-amz|x-tric-|connection-id|grip-|meta-)"))] | join(" ")' <<<"$echo")" ""
  is "no websocket-events content type reaches the app" "$(header content-type <<<"$echo")" ""
  note "the headers alpha sees: $(jq -r '[.headers[][0]] | join(" ")' <<<"$echo")"
else fail "alpha echoes a request" "got [$(head -c 200 <<<"$echo")]"; fi
read -r first total < <(curl -4 -sS -o /dev/null --max-time 30 -w '%{time_starttransfer} %{time_total}\n' \
  "$A/stream?n=3")
if awk -v f="$first" -v t="$total" 'BEGIN { exit !(t - f > 1.5) }'; then
  pass "the body streams (head at ${first}s, end at ${total}s)"
else fail "the body streams" "head at ${first}s, end at ${total}s"; fi
is "an unknown app is 404" "$(code "https://nobody-$run.$DOMAIN/")" 404
# A keyed write sent twice writes once, and the second answer is marked as the first's, given again.
idem="$A/@idem-$run/kv?op=incr&key=n"
is "a keyed write answers" "$(code -X POST "$idem" -H "Idempotency-Key: k-$run")" 200
again=$(post "$idem" -H "Idempotency-Key: k-$run" -D - -o /dev/null | tr -d '\r')
is "a keyed write sent again is answered as it was, and marked" \
  "$(awk 'NR == 1 { s = $2 } tolower($1) == "idempotent-replayed:" { r = $2 } END { print s, r }' <<<"$again")" \
  "200 true"
is "a keyed write sent twice writes once" "$(post "$A/@idem-$run/kv?op=incr&key=n&delta=0" | jq -c .ok)" 1

echo "== around CloudFront"
url=$(aws lambda get-function-url-config --function-name $NAME-route --query FunctionUrl --output text)
[[ $url == https://* ]] || die "$NAME-route has no URL"
is "the router's URL without the secret is 403" "$(code "${url}echo" -H "X-Forwarded-Host: alpha.$DOMAIN")" 403
is "the router's URL with a guessed secret is 403" "$(code "${url}echo" -H "X-Forwarded-Host: alpha.$DOMAIN" \
  -H 'X-Tric-Origin: guess' -H 'CloudFront-Viewer-Address: 9.9.9.9:1')" 403
is "the router's URL with an empty secret is 403" "$(code "${url}echo" -H "X-Forwarded-Host: alpha.$DOMAIN" \
  -H 'X-Tric-Origin;' -H 'CloudFront-Viewer-Address: 9.9.9.9:1')" 403
# Lambda's own 403 has a body ({"Message":"Forbidden"}); the router's has none.
is "the 403 is the router's" "$(get "${url}echo" -H "X-Forwarded-Host: alpha.$DOMAIN" | wc -c | tr -d ' ')" 0
for f in $NAME-route $NAME-serve $NAME-events; do
  is "$f exists" "$(awscode aws lambda get-function --function-name $f)" OK
done
for f in $NAME-serve $NAME-events "$NAME-events --qualifier outbox"; do
  is "$f has no URL" "$(awscode aws lambda get-function-url-config --function-name $f)" ResourceNotFoundException
  is "$f has no resource policy, so only roles granted it may invoke it" \
    "$(awscode aws lambda get-policy --function-name $f)" ResourceNotFoundException
done
is "the router's resource policy invokes only through its URL" "$(aws lambda get-policy --function-name $NAME-route \
  --query Policy --output text | jq -c '[.Statement[] | select(.Action == "lambda:InvokeFunction")
  | .Condition.Bool["lambda:InvokedViaFunctionUrl"]] | (length > 0) and all(. == "true")')" true
# A cron job sent straight to the router, as this admin: its effect is checked under "forged events".
invoke $NAME-route "{\"app\":\"beta\",\"path\":\"/@forged-$run/echo\"}" >/dev/null

echo "== tenants"
is "serve is per tenant" "$(aws lambda get-function-configuration --function-name $NAME-serve \
  --query TenancyConfig.TenantIsolationMode --output text)" PER_TENANT
# A function-URL event, as the router sends serve: Host $1, tenant header $2, and $3 as more headers.
event() {
  local more=${3:-}
  jq -cn --arg host "$1" --arg tenant "$2" --argjson more "${more:-null}" '{version: "2.0", routeKey: "$default",
    rawPath: "/echo", rawQueryString: "", isBase64Encoded: false,
    headers: ({host: $host, "x-amz-tenant-id": $tenant, forwarded: "for=192.0.2.1"} + ($more // {})),
    requestContext: {http: {method: "GET", path: "/echo", protocol: "HTTP/1.1", sourceIp: "192.0.2.1",
    userAgent: "check"}}}'
}
fake=$(jq -cn '{"x-tric-credentials": ({Version: 1, AccessKeyId: "ASIAXXXXXXXXXXXXXXXX", SecretAccessKey: "x",
  SessionToken: "x", Expiration: "2030-01-01T00:00:00Z"} | tojson)}')
# serve's status for Host $1, tenant header $2, more headers $3, in Lambda tenant $4.
st() { invoke $NAME-serve "$(event "$1" "$2" "$3")" --tenant-id "$4" | status; }
s=$(st "alpha.$DOMAIN" alpha "$fake" alpha)
if [ -n "$s" ] && [ "$s" != 403 ]; then pass "made-up credentials in alpha's own tenant get past serve's gate ($s)"
else fail "made-up credentials in alpha's own tenant get past serve's gate" "status [$s]"; fi
is "serve refuses another app's Host in alpha's tenant" "$(st "beta.$DOMAIN" alpha "$fake" alpha)" 403
is "serve refuses a tenant header that isn't the Host's app" "$(st "alpha.$DOMAIN" beta "$fake" alpha)" 403
is "serve refuses alpha's request in beta's tenant" "$(st "alpha.$DOMAIN" alpha "$fake" beta)" 403
is "serve refuses a request without credentials" "$(st "alpha.$DOMAIN" alpha "" alpha)" 403
e=$(aws lambda invoke --function-name $NAME-serve --cli-binary-format raw-in-base64-out \
  --payload "$(event "alpha.$DOMAIN" alpha)" /dev/null 2>&1 >/dev/null)
if grep -qi tenant <<<"$e"; then pass "serve can't be invoked without a tenant"
else fail "serve can't be invoked without a tenant" "$(head -c 160 <<<"${e:-it ran}")"; fi
post "$A/@n-$run/kv?op=set&key=k&value=alpha" >/dev/null
post "$B/@n-$run/kv?op=set&key=k&value=beta" >/dev/null
post "$A/@n-$run/kv?op=set&key=only-alpha&value=1" >/dev/null
is "alpha reads its own state" "$(value "$A" n-$run k)" '"alpha"'
is "beta reads its own state, not alpha's" "$(value "$B" n-$run k)" '"beta"'
is "alpha reads its own key" "$(value "$A" n-$run only-alpha)" '"1"'
is "beta doesn't see alpha's key" "$(value "$B" n-$run only-alpha)" null
keys=$(aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix native/ --query 'Contents[].Key' --output text)
apps=$(tr '\t' '\n' <<<"$keys" | cut -d/ -f2 | sort -u)
if grep -qx alpha <<<"$apps" && grep -qx beta <<<"$apps"; then pass "native code is under each app's own prefix"
else fail "native code is under each app's own prefix" "got [$(xargs <<<"$apps")]"; fi
# Distinct log streams of serve with a "probe-stdout" line since $1.
streams() {
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-serve --start-time "$1" \
    --filter-pattern '"probe-stdout"' --query 'events[].logStreamName' --output text | tr '\t' '\n' | grep . | sort -u
}
t=$(now); get "$A/print" >/dev/null; sleep 20; sa=$(streams "$t")
t=$(now); get "$B/print" >/dev/null; sleep 20; sb=$(streams "$t")
if [ -n "$sa" ] && [ -n "$sb" ] && [ -z "$(comm -12 <(echo "$sa") <(echo "$sb"))" ]; then
  pass "alpha and beta ran in different execution environments"
else
  fail "alpha and beta ran in different execution environments" "alpha [$(xargs <<<"$sa")] beta [$(xargs <<<"$sb")]"
fi

echo "== credentials (IAM's policy simulator)"
is "$NAME-app has one inline policy and none attached" "$(aws iam list-role-policies --role-name $NAME-app \
  --query 'length(PolicyNames)' --output text)/$(aws iam list-attached-role-policies --role-name $NAME-app \
  --query 'length(AttachedPolicies)' --output text)" 1/0
role=$(aws iam get-role-policy --role-name $NAME-app --policy-name "$(aws iam list-role-policies --role-name $NAME-app \
  --query 'PolicyNames[0]' --output text)" --query PolicyDocument --output json)
# What the role's policy allows of action $1 on resource $2 of the bucket (the bucket itself, if $2 is empty), narrowed
# by the session policy $4 if one is given. A listing's condition is on its prefix, $3, which the simulator has to be
# told (and a listing without one is simulated without the key, as a bare listing is).
rolesim() {
  aws iam simulate-custom-policy --policy-input-list "$role" ${4:+--permissions-boundary-policy-input-list "$4"} \
    --action-names "$1" --resource-arns "$B3${2:+/$2}" \
    ${3:+--context-entries "ContextKeyName=s3:prefix,ContextKeyType=string,ContextKeyValues=$3"} \
    --query 'EvaluationResults[0].EvalDecision' --output text
}
# The same for alpha's credentials: the role's policy, narrowed by alpha's session policy.
s3sim() { rolesim "$1" "${2:-}" "${3:-}" "$(session alpha)"; }
# The same for a listing of the bucket under the prefix $1, and a listing of its versions.
s3list() { s3sim s3:ListBucket "" "$1"; }
s3vers() { s3sim s3:ListBucketVersions "" "$1"; }
for c in "s3:GetObject apps/alpha/names/x" "s3:PutObject apps/alpha/names/x" "s3:PutObject apps/alpha/values/x" \
  "s3:DeleteObject apps/alpha/values/x" "s3:GetObject apps/alpha/current" "s3:GetObject apps/alpha/components/x" \
  "s3:PutObject native/alpha/c/x"; do
  is "alpha's credentials allow $c" "$(s3sim $c)" allowed
done
# The sweep's listings (src/route.rs's `policy`, infra/aws/main.tf's app role): alpha's values, and its names' heads;
# and no other prefix, not the app's whole prefix, not a prefix that only begins like one, and not a bare listing; and
# no listing of versions at all.
for p in apps/alpha/values/ apps/alpha/values/n-1/ apps/alpha/names/ apps/alpha/names/n-1; do
  is "alpha's credentials allow listing $p" "$(s3list $p)" allowed
done
for p in apps/beta/names/ apps/beta/values/ apps/alpha/ apps/alph apps/alpha apps/alpha/components/ \
  apps/alpha/current apps/alphabet/values/ apps/alphabet/names/ apps/ native/alpha/ outbox/alpha/ ws/channels/; do
  is "alpha's credentials deny listing $p" "$(s3list $p)" implicitDeny
done
is "alpha's credentials deny listing with no prefix" "$(s3list "")" implicitDeny
for p in apps/alpha/names/ apps/alpha/values/ apps/alpha/ ""; do
  is "alpha's credentials deny listing the versions under ${p:-the bucket}" "$(s3vers "$p")" implicitDeny
done
for c in "s3:GetObject apps/beta/names/x" "s3:GetObject apps/beta/values/x" "s3:GetObjectVersion apps/beta/names/x" \
  "s3:GetObject apps/beta/current" "s3:GetObject apps/beta/components/x" "s3:GetObject native/beta/c/x" \
  "s3:PutObject apps/beta/names/x" "s3:PutObject apps/beta/values/x" "s3:DeleteObject apps/beta/values/x" \
  "s3:PutObject native/beta/c/x" "s3:PutObject apps/alpha/current" "s3:PutObject apps/alpha/components/x" \
  "s3:GetObject apps/x" "s3:PutObject apps/x" "s3:PutObject native/x" "s3:GetObject outbox/alpha/x" \
  "s3:PutObject outbox/alpha/x" \
  "s3:GetObject apps/alphabet/names/x" "s3:PutObject apps/alphabet/names/x" "s3:PutObject native/alphabet/c/x" \
  "s3:DeleteObjectVersion apps/alpha/names/x" "s3:DeleteObjectVersion apps/alpha/values/x" \
  "s3:PutLifecycleConfiguration" "s3:PutBucketVersioning" "s3:ListBucket"; do
  is "alpha's credentials deny $c" "$(s3sim $c)" implicitDeny
done
# The role alone, as if the router gave a session too wide a policy: still no release or component is writable.
for c in "s3:PutObject apps/alpha/current" "s3:DeleteObject apps/alpha/current" \
  "s3:PutObject apps/alpha/components/x"; do
  is "$NAME-app itself denies $c" "$(rolesim $c)" implicitDeny
done
# The role alone lists any app's values and names, which is why the session narrows it to one app's; and nothing else.
for p in apps/alpha/values/ apps/beta/values/ apps/alpha/names/ apps/beta/names/; do
  is "$NAME-app itself allows listing $p, which the session narrows to one app's" "$(rolesim s3:ListBucket "" $p)" \
    allowed
done
for p in apps/alpha/ apps/alpha/components/ native/alpha/ outbox/alpha/ ws/channels/; do
  is "$NAME-app itself denies listing $p" "$(rolesim s3:ListBucket "" $p)" implicitDeny
done
for p in apps/alpha/names/ apps/alpha/values/ apps/alpha/ native/alpha/ outbox/alpha/; do
  is "$NAME-app itself denies listing the versions under $p" "$(rolesim s3:ListBucketVersions "" $p)" implicitDeny
done
is "$NAME-app itself denies listing with no prefix" "$(rolesim s3:ListBucket)" implicitDeny
is "$NAME-app itself denies listing versions with no prefix" "$(rolesim s3:ListBucketVersions)" implicitDeny
for k in names values; do
  is "$NAME-app itself denies deleting a version of apps/alpha/$k/x" \
    "$(rolesim s3:DeleteObjectVersion apps/alpha/$k/x)" implicitDeny
done
# What role $1 may do with action $2 on resource $3 (and for a listing, under the prefix $4).
psim() {
  aws iam simulate-principal-policy --policy-source-arn "arn:aws:iam::$ACCOUNT:role/$1" --action-names "$2" \
    --resource-arns "$3" ${4:+--context-entries "ContextKeyName=s3:prefix,ContextKeyType=string,ContextKeyValues=$4"} \
    --query 'EvaluationResults[0].EvalDecision' --output text
}
is "the router may assume $NAME-app" "$(psim $NAME-route sts:AssumeRole "arn:aws:iam::$ACCOUNT:role/$NAME-app")" allowed
is "the router may invoke serve" "$(psim $NAME-route lambda:InvokeFunction "$FN:$NAME-serve")" allowed
SCHED="arn:aws:scheduler:$AWS_REGION:$ACCOUNT:schedule"
for t in "s3:PutObject $B3/outbox/alpha/x" "s3:DeleteObject $B3/outbox/alpha/x" \
  "scheduler:CreateSchedule $SCHED/$NAME-retry/x"; do
  is "the router's role may $t" "$(psim $NAME-route $t)" allowed
done
# The listing of ws/channels/ needs its prefix, which a simulation without one lacks: so a bare listing is denied, and
# a listing under that prefix is allowed, and under an app's values (the sweep's) is not: that is the apps' alone.
is "the router's role may list ws/channels/" "$(psim $NAME-route s3:ListBucket "$B3" ws/channels/)" allowed
for p in apps/alpha/values/ apps/alpha/names/ apps/alpha/ outbox/; do
  is "the router's role is denied listing $p" "$(psim $NAME-route s3:ListBucket "$B3" $p)" implicitDeny
  is "and listing the versions under it" "$(psim $NAME-route s3:ListBucketVersions "$B3" $p)" implicitDeny
done
for t in "lambda:InvokeFunction $FN:$NAME-events" "lambda:InvokeFunction $FN:$NAME-events:outbox" \
  "s3:GetObject $B3/apps/alpha/names/x" "s3:PutObject $B3/apps/alpha/names/x" "s3:PutObject $B3/x" \
  "s3:ListBucket $B3" "s3:DeleteObject $B3/apps/alpha/current" "scheduler:CreateSchedule $SCHED/$TRIC_SCHEDULES/x" \
  "iam:PassRole arn:aws:iam::$ACCOUNT:role/$NAME-cron"; do
  is "the router's role is denied $t" "$(psim $NAME-route $t)" implicitDeny
done
is "serve may invoke the outbox alias" "$(psim $NAME-serve lambda:InvokeFunction "$FN:$NAME-events:outbox")" allowed
for t in "lambda:InvokeFunction $FN:$NAME-events" "lambda:InvokeFunction $FN:$NAME-serve" \
  "lambda:InvokeFunction $FN:$NAME-route" "s3:GetObject $B3/apps/alpha/current" "s3:PutObject $B3/apps/alpha/names/x" \
  "s3:GetObject $B3/native/alpha/x" "s3:ListBucket $B3" "sts:AssumeRole arn:aws:iam::$ACCOUNT:role/$NAME-app" \
  "lambda:InvokeFunction $FN:$NAME-events:retry" "scheduler:CreateSchedule $SCHED/$NAME-retry/x"; do
  is "serve's role is denied $t" "$(psim $NAME-serve $t)" implicitDeny
done
for p in apps/alpha/values/ apps/alpha/names/ ws/channels/; do
  is "serve's role is denied listing $p" "$(psim $NAME-serve s3:ListBucket "$B3" $p)" implicitDeny
  is "and listing the versions under it" "$(psim $NAME-serve s3:ListBucketVersions "$B3" $p)" implicitDeny
done
# Each Scheduler role invokes its own alias of the events function, and nothing else.
for r in "$NAME-cron cron" "$NAME-retry retry"; do
  set -- $r
  is "$1 may invoke the $2 alias" "$(psim $1 lambda:InvokeFunction "$FN:$NAME-events:$2")" allowed
  for t in events events:outbox events:cron events:retry serve; do
    [ "$t" = "events:$2" ] ||
      is "$1 may not invoke $NAME-$t" "$(psim $1 lambda:InvokeFunction "$FN:$NAME-$t")" implicitDeny
  done
done

echo "== infrastructure"
is "the app role trusts only the router's role" "$(aws iam get-role --role-name $NAME-app \
  --query 'Role.AssumeRolePolicyDocument.Statement[].Principal.AWS' --output text)" \
  "arn:aws:iam::$ACCOUNT:role/$NAME-route"
is "$NAME-serve has one inline policy and none attached" "$(aws iam list-role-policies --role-name $NAME-serve \
  --query 'length(PolicyNames)' --output text)/$(aws iam list-attached-role-policies --role-name $NAME-serve \
  --query 'length(AttachedPolicies)' --output text)" 1/0
for q in outbox retry; do
  is "the $q alias retries twice, keeps an event 6 h, and has no destination" \
    "$(aws lambda get-function-event-invoke-config --function-name $NAME-events --qualifier $q \
    --query '[MaximumRetryAttempts, MaximumEventAgeInSeconds, DestinationConfig.OnFailure.Destination]' \
    --output text | tr '\t' ' ')" "2 21600 None"
done
is "the bucket blocks public access" "$(aws s3api get-public-access-block --bucket "$TRIC_BUCKET" --query \
  'PublicAccessBlockConfiguration.[BlockPublicAcls, IgnorePublicAcls, BlockPublicPolicy, RestrictPublicBuckets]' \
  --output text | tr '\t' ' ')" "True True True True"
is "the bucket is versioned" "$(aws s3api get-bucket-versioning --bucket "$TRIC_BUCKET" --query Status --output text)" \
  Enabled
# The lifecycle rules of infra/aws/main.tf. The sweep deletes a version's key and leaves a delete marker and a version:
# it is these rules that take them, so they are the sweep's other half, and what is noncurrent is kept, to be read by
# version, the days of tofu's `noncurrent_days` (TF_VAR_noncurrent_days, 1 if unset). All is read.
lifecycle=$(aws s3api get-bucket-lifecycle-configuration --bucket "$TRIC_BUCKET" --output json)
is "the bucket has the rules noncurrent, outbox and ws, and no more" "$(jq -r '[.Rules[].ID] | sort | join(" ")' \
  <<<"$lifecycle")" "noncurrent outbox ws"
days=${TF_VAR_noncurrent_days:-1}
is "noncurrent versions expire in $days days, in every key of the bucket, and the rule is on" "$(jq -r '.Rules[]
  | select(.ID == "noncurrent") | [.Status, .NoncurrentVersionExpiration.NoncurrentDays, (.Filter.Prefix // ""),
  ((.Filter // {}) | has("Tag") or has("And"))] | join(" ")' <<<"$lifecycle")" "Enabled $days  false"
is "a delete marker with no version behind it expires" "$(jq '.Rules[] | select(.ID == "noncurrent")
  | .Expiration.ExpiredObjectDeleteMarker' <<<"$lifecycle")" true
is "an upload left unfinished is aborted in 1 day" "$(jq '.Rules[] | select(.ID == "noncurrent")
  | .AbortIncompleteMultipartUpload.DaysAfterInitiation' <<<"$lifecycle")" 1
is "outbox/ expires in 2 days, and the rule is on" "$(jq -r '.Rules[] | select(.ID == "outbox")
  | [.Status, (.Filter.Prefix // ""), .Expiration.Days] | join(" ")' <<<"$lifecycle")" "Enabled outbox/ 2"
is "ws/ expires in 1 day, and the rule is on" "$(jq -r '.Rules[] | select(.ID == "ws")
  | [.Status, (.Filter.Prefix // ""), .Expiration.Days] | join(" ")' <<<"$lifecycle")" "Enabled ws/ 1"
is "only outbox/ and ws/ expire current objects, and so no key of apps/ does" "$(jq -r '[.Rules[]
  | select(.Expiration.Days != null) | (.Filter.Prefix // "")] | sort | join(" ")' <<<"$lifecycle")" "outbox/ ws/"
is "only $NAME-route reserves concurrency, 200" "$(for f in route events serve; do
  printf '%s=%s ' "$f" "$(aws lambda get-function-concurrency --function-name "$NAME-$f" \
    --query ReservedConcurrentExecutions --output text)"
done)" "route=200 events=None serve=None "
is "the bucket has no policy" "$(awscode aws s3api get-bucket-policy --bucket "$TRIC_BUCKET")" NoSuchBucketPolicy
# The names of a function's environment variables, never their values.
envs() {
  aws lambda get-function-configuration --function-name "$NAME-$1" --query 'Environment.Variables | keys(@)' \
    --output text | tr '\t' '\n'
}
is "only $NAME-route holds the origin secret" \
  "$(for f in route events serve; do envs $f | grep -qx TRIC_ORIGIN && echo $f; done)" route
for f in route events serve; do
  is "$NAME-$f has no provisioned concurrency" "$(aws lambda list-provisioned-concurrency-configs \
    --function-name $NAME-$f --query 'length(ProvisionedConcurrencyConfigs)' --output text)" 0
done

echo "== outbound and the guest"
G=https://gamma.$DOMAIN
D=https://delta.$DOMAIN
r=$(get "$G/fetch?url=https://checkip.amazonaws.com")
if [[ $r != 200* ]]; then pass "an app that lists no host reaches none"
else fail "an app that lists no host reaches none" "$r"; fi
r=$(get "$D/fetch?url=https://checkip.amazonaws.com")
if [[ $r == 200* && $r != *"$ip"* ]]; then pass "a listed host is reached, from AWS"
else fail "a listed host is reached, from AWS" "$r"; fi
# Not the Runtime API's port: a failure to block there would hang the invocation.
r=$(get "$D/fetch?url=http://127.0.0.1:3000/")
if [[ $r != 200* ]]; then pass "loopback is refused even when listed"
else fail "loopback is refused even when listed" "$r"; fi
r=$(get "$D/fetch?url=http://169.254.169.254/latest/meta-data/")
if [[ $r != 200* ]]; then pass "link-local is refused even when listed"
else fail "link-local is refused even when listed" "$r"; fi
is "the guest sees no environment" "$(get "$A/env" | jq -c .env)" '{}'
# A request on no name has no files: the root is none of the host's, and no name's tree is there for it. Under "files"
# below, a name has its own.
is "a request on no name sees no files" "$(get "$A/fs" | jq -c '[.[] | has("err")] | all')" true

echo "== files (wasi:filesystem, on a copy-on-write tree in S3)"
t_files=$(now)
# The answer on stdin as compact JSON; or, if it was not JSON, what it began with.
fjson() {
  local r
  r=$(cat)
  jq -c . <<<"$r" 2>/dev/null || echo "[not JSON: $(head -c 80 <<<"$r")]"
}
# The fixture's route to its files by $1, and how it says the errors that are checked. By std it is /std-files
# (tests/components/rust/src/std_files.rs), std::fs over wasi-libc, and an error is the io::ErrorKind that std makes of
# it; by the binding it is /files (files.rs), the same ops through wasi:filesystem's own streams, and an error is the
# Debug of the error-code. The names of a route's checks begin with $X, and $I is its place in a list.
by() {
  V=$1
  case $1 in
    std) R=std-files X='' I=0 NOENT=NotFound RO=ReadOnlyFilesystem PERM=PermissionDenied ;;
    binding) R=files X=p3- I=1 NOENT=ErrorCode::NoEntry RO=ErrorCode::ReadOnly PERM=ErrorCode::NotPermitted ;;
  esac
}
# What the route answers to ?$3 of name $2 of app $1: {"ok": v} or {"err": "<kind>: <message>"}, or {"err": "<kind>"}.
# A GET reads the name's files as they are, and is refused if it writes; a POST is a turn on them.
fget() { get "$1/@$2/$R?$3" | fjson; }
fpost() { post "$1/@$2/$R?$3" | fjson; }
# The kind of an answer on stdin: its error's (what comes before ": ", if anything does), or ok, or ? if it is neither.
errkind() {
  jq -r 'if type == "object" and has("err") then (.err | split(": ")[0]) elif type == "object" and has("ok") then "ok"
    else "?" end' 2>/dev/null || echo "?"
}
# The fixture's `sum` of file $3 of name $2 of app $1, with its keys in order.
fsum() { fget "$1" "$2" "op=sum&path=$3" | jq -cS .ok 2>/dev/null; }
# What `sum` answers for the fixture's pattern of $1 bytes (byte i is i % 251), with the string $3 written at the offset
# $2, and then cut or lengthened with zeros to $4 bytes, if that is given: its length, and its FNV-1a in decimal.
patsum() {
  python3 -I -c '
import json, sys
n, at, data = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3].encode()
cut = int(sys.argv[4]) if len(sys.argv) > 4 else n
b = bytearray(i % 251 for i in range(n))
b[at:at + len(data)] = data
b = b[:cut] + bytes(max(0, cut - len(b)))
h = 0xCBF29CE484222325
for x in b:
    h = ((h ^ x) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
print(json.dumps({"fnv": str(h), "len": len(b)}, sort_keys=True, separators=(",", ":")))' \
    "$1" "${2:-0}" "${3:-}" ${4:+"$4"}
}

raced=()
for V in std binding; do
  by $V
  # A turn's files commit with its keys, whole or not at all (a 5xx discards the turn; any other status commits it).
  f=${X}files-$run
  w="op=write&path=/hello.txt&key=k"
  is "$V: a turn writes a file, and a key" "$(fpost "$A" $f "$w&data=hi&value=v")" '{"ok":null}'
  is "$V: a later request reads the file back" "$(fget "$A" $f 'op=read&path=/hello.txt')" '{"ok":"hi"}'
  is "$V: and the key set in the turn that wrote it" "$(value "$A" $f k)" '"v"'
  is "$V: a turn that answers 500 is answered so" \
    "$(code -X POST "$A/@$f/$R?$w&data=gone&value=gone&status=500")" 500
  is "$V: and its file is as it was" "$(fget "$A" $f 'op=read&path=/hello.txt')" '{"ok":"hi"}'
  is "$V: and so is its key" "$(value "$A" $f k)" '"v"'
  fpost "$A" $f 'op=mkdirs&path=/x/y/z&status=500' >/dev/null
  is "$V: a directory made in a turn that answers 500 is none" "$(fget "$A" $f 'op=stat&path=/x' | errkind)" $NOENT
  is "$V: a turn that answers 409 is answered so" \
    "$(code -X POST "$A/@$f/$R?$w&data=kept&value=kept&status=409")" 409
  is "$V: and its file is written" "$(fget "$A" $f 'op=read&path=/hello.txt')" '{"ok":"kept"}'
  is "$V: and its key" "$(value "$A" $f k)" '"kept"'

  # Outside a turn on it, a name's files are read-only; they are the name's and the app's, and a request on no name has
  # none of them.
  is "$V: a GET that writes is refused" "$(fget "$A" $f 'op=write&path=/x&data=1' | errkind)" $RO
  is "$V: and writes nothing" "$(fget "$A" $f 'op=stat&path=/x' | errkind)" $NOENT
  is "$V: beta, at the same name, has none of alpha's files" \
    "$(fget "$B" $f 'op=read&path=/hello.txt' | errkind)" $NOENT
  is "$V: a request on no name has no file of alpha's" "$(get "$A/$R?op=read&path=/hello.txt" | fjson | errkind)" $NOENT

  # A file of blocks (src/fs/ops.rs's BLOCK is 256 KiB, so these 1,000,000 bytes are four), a few bytes in the middle
  # of it written across the end of the first, and a cut: each answered by the length and the hash of what the pattern
  # makes.
  b=${X}big-$run
  is "$V: a turn writes a file of four blocks" "$(fpost "$A" $b 'op=big&path=/big&n=1000000')" '{"ok":1000000}'
  is "$V: a later request sums it as the pattern has it" "$(fsum "$A" $b /big)" "$(patsum 1000000)"
  is "$V: 4 bytes across the end of its first block are written in place" \
    "$(fpost "$A" $b 'op=patch&path=/big&at=262142&data=XYZW')" '{"ok":null}'
  is "$V: and the sum is the pattern's with them" "$(fsum "$A" $b /big)" "$(patsum 1000000 262142 XYZW)"
  is "$V: and 8 bytes across that end are read as they were written" \
    "$(fget "$A" $b 'op=slice&path=/big&at=262140&n=8' | jq -c .ok)" \
    "$(jq -cn '[range(262140; 262148) | if . >= 262142 and . < 262146 then ("XYZW" | explode)[. - 262142]
      else . % 251 end]')"
  is "$V: a patch in a turn that answers 500 is answered so" \
    "$(code -X POST "$A/@$b/$R?op=patch&path=/big&at=5&data=!!&status=500")" 500
  is "$V: and the file is as it was" "$(fsum "$A" $b /big)" "$(patsum 1000000 262142 XYZW)"
  is "$V: a cut to 300,000 bytes is made" "$(fpost "$A" $b 'op=trunc&path=/big&n=300000')" '{"ok":null}'
  is "$V: and the sum is the pattern's, cut" "$(fsum "$A" $b /big)" "$(patsum 1000000 262142 XYZW 300000)"

  # Paths lead down from the root of the name's tree, and nowhere else: not up out of it, nor along a link out of it.
  e=${X}esc-$run
  is "$V: a link to an absolute path is refused" "$(fpost "$A" $e 'op=symlink&path=/abs&to=/etc/passwd' | errkind)" \
    $PERM
  is "$V: a link up and out is made, being only text" "$(fpost "$A" $e 'op=symlink&path=/up&to=../..')" '{"ok":null}'
  for q in 'op=ls&path=/up' 'op=read&path=/up/etc/passwd' 'op=read&path=/../etc/passwd'; do
    is "$V: the files of a name do not reach ${q#op=}" "$(fget "$A" $e "$q" | errkind)" $PERM
  done
  is "$V: and /etc/passwd is none in the name's own tree" "$(fget "$A" $e 'op=read&path=/etc/passwd' | errkind)" $NOENT

  # Turns at one name race: each that answered 200 counted once, and no other did (the rest are refused, with a 503).
  codes=$(mktemp)
  for _ in 1 2 3 4; do
    (for _ in 1 2 3 4 5; do code -X POST "$A/@${X}race-$run/$R?op=incr&path=/n"; echo; done) >>"$codes" &
  done
  wait
  ok=$(grep -c '^200$' "$codes")
  note "$V: the statuses of 20 racing turns on one file: $(sort "$codes" | grep . | uniq -c | xargs)"
  rm -f "$codes"
  is "$V: every racing turn that answered 200 counted once, and no other" \
    "$(fget "$A" ${X}race-$run 'op=read&path=/n' | jq -r '.ok // "ERR"')" "$ok"
  raced[I]=$ok # read back after the sweep
done

# The two routes are one filesystem: what a turn writes through one, the other reads, in place.
by binding
is "binding writes a file of three blocks" "$(fpost "$A" one-$run 'op=big&path=/big&n=600000')" '{"ok":600000}'
by std
is "std sums it as the pattern has it" "$(fsum "$A" one-$run /big)" "$(patsum 600000)"
is "std writes 4 bytes across the end of its first block" \
  "$(fpost "$A" one-$run 'op=patch&path=/big&at=262142&data=XYZW')" '{"ok":null}'
by binding
is "binding sums it with them" "$(fsum "$A" one-$run /big)" "$(patsum 600000 262142 XYZW)"

echo "== outbox"
# One to fail: it waits in outbox/ and is tried again by Scheduler, a minute on and then two. Checked at the end.
failed=$(now)
is "a held request is answered 202" "$(post "$A/@d-$run/fetch?method=POST&async=1&url=$A/@e-$run/echo?status=503")" \
  "202 "
is "a held request is answered 202" "$(post "$A/@f-$run/fetch?method=POST&async=1&url=$A/@g-$run/echo")" "202 "
if echo=$(wait_echo "$A" g-$run echo 90); then
  is "the held request is delivered by tric" "$(header forwarded <<<"$echo")" for=_tric
  note "its Idempotency-Key: $(header idempotency-key <<<"$echo")"
else fail "the held request is delivered within 90 s"; fi
post "$B/@h-$run/fetch?method=POST&async=1&url=$B/@i-$run/echo" >/dev/null
if echo=$(wait_echo "$B" i-$run echo 90); then
  is "beta's held request is delivered by tric" "$(header forwarded <<<"$echo")" for=_tric
else fail "beta's held request is delivered within 90 s"; fi

echo "== forged events (as this admin, who may invoke anything)"
# A delivery goes ahead only once its name's head holds the record its commit made of the event's digest. So one of a
# real commit, j-$run's (its ETag), with requests that commit did not hold is dropped, as is one over a head its name
# never had.
commit=$(post "$B/@j-$run/kv?op=set&key=x&value=1" -D - -o /dev/null | tr -d '\r' |
  awk 'tolower($1) == "etag:" { gsub(/"/, "", $2); print $2 }')
[ -n "$commit" ] || fail "a turn's answer has the name's ETag"
since=$(now)
forged=$(jq -cn --arg h "beta.$DOMAIN" --arg n "j-$run" --arg c "$commit" '{app: "beta", host: $h, name: $n,
  day: (now / 86400 | floor), commit: $c, requests: [{method: "POST", uri: "https://\($h)/@\($n)/echo", headers: [],
  body: ""}]}')
invoke $NAME-events "$forged" --qualifier outbox >/dev/null || fail "invoke the outbox alias"
if awaitlog serve "$since" "no commit recorded it" 60; then pass "a forged delivery is dropped for its digest"
else fail "a forged delivery is dropped for its digest" "no 'no commit recorded it' in serve's log"; fi
is "and stores nothing" "$(value "$B" j-$run echo)" null
since=$(now)
invoke $NAME-events "$(jq -cn --arg n "ghost-$run" '{app: $n, host: "x", name: "g", day: 0, commit: "c",
  requests: []}')" --qualifier outbox >/dev/null
if awaitlog events "$since" "has no release" 60; then pass "a delivery for an app with no release is dropped"
else fail "a delivery for an app with no release is dropped"; fi
since=$(now)
invoke $NAME-events "$(jq -cn --arg h "beta.$DOMAIN" --arg n "nohead-$run" '{app: "beta", host: $h, name: $n,
  day: (now / 86400 | floor), base: "forged", commit: "forged", requests: []}')" --qualifier outbox >/dev/null
if awaitlog serve "$since" "no commit recorded it" 20; then pass "a delivery over a head its name never had is dropped"
else fail "a delivery over a head its name never had is dropped"; fi
invoke $NAME-events "{\"app\":\"beta\",\"path\":\"/@forged-$run/echo\"}" --qualifier outbox >/dev/null
evt=$(jq -cn --arg h "alpha.$DOMAIN" --arg n "x-$run" '{app: "alpha", host: $h, name: $n, commit: "forged",
  requests: [{method: "POST", uri: "https://\($h)/@\($n)/echo", headers: [], body: ""}]}')
is "a delivery over HTTP is a plain POST to the app" "$(post "$A/" -H 'Forwarded: for=_tric' --data "$evt")" hello
for q in "" "--qualifier outbox"; do
  is "$NAME-events refuses an HTTP event ${q:-unqualified}" "$(invoke $NAME-events "$(event "alpha.$DOMAIN" alpha)" $q |
    status)" 403
done
sleep 10
is "the delivery over HTTP delivered nothing" "$(value "$A" x-$run echo)" null
is "a cron job to the router or the outbox alias runs nothing" "$(value "$B" forged-$run echo)" null

echo "== cron (every minute, for alpha)"
if echo=$(wait_echo "$A" ticks-$run echo 150); then
  is "cron's request is a POST" "$(jq -r .method <<<"$echo")" POST
  is "cron's request is tric's" "$(header forwarded <<<"$echo")" for=_cron
else fail "cron fires within 150 s"; fi
# Alpha's job is done: take it off, as a deploy does, which keeps its sweep.
uncron
is "alpha has no cron job now, and still its sweep" "$(schedules alpha | grep -c .)/$(sweepsof alpha | grep -c .)" 1/1

echo "== retries"
# The delivery to e-$run fails (its turn answers 503), so its event waits as outbox/alpha/<commit>, which only the
# router writes, and a one-time schedule in the retry group, named for the commit, tries it again.
commit=''
for k in $(aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix outbox/alpha/ --query 'Contents[].Key' \
  --output text 2>/dev/null | listed); do
  if aws s3 cp "s3://$TRIC_BUCKET/$k" - 2>/dev/null | grep -qa "e-$run"; then commit=${k##*/}; break; fi
done
if [ -n "$commit" ]; then pass "a delivery that fails waits in outbox/alpha/"
else fail "a delivery that fails waits in outbox/alpha/" "no object there names e-$run"; fi
if [ -n "$commit" ]; then
  prefix=$(sha "alpha/$commit" 32)
  is "and a schedule in $NAME-retry is for it" "$(aws scheduler list-schedules --group-name $NAME-retry \
    --name-prefix "$prefix" --query 'length(Schedules)' --output text)" 1
fi
# serve's 503 to each try, in the router's log: the first try, and two that Scheduler made.
tries="{ \$.fields.app = \"alpha\" && \$.fields.message = \"outbox: serve answered 503 Service Unavailable\" }"
if awaitpat events "$failed" "$tries" 360 3; then pass "Scheduler tries a failed delivery again, and again"
else fail "Scheduler tries a failed delivery again, and again" \
  "$(nlogs events "$failed" "$tries") tries in 6 minutes, want 3"; fi
note "the tries' times: $(aws logs filter-log-events --log-group-name /aws/lambda/$NAME-events --start-time "$failed" \
  --filter-pattern "$tries" --query 'events[].timestamp' --output text | tr '\t' '\n' |
  awk 'NR == 1 { t = $1 } { printf "+%ds ", ($1 - t) / 1000 }')"
is "the failed delivery committed nothing" "$(value "$A" e-$run echo)" null
if [ -n "$commit" ]; then
  # As if its window were over: this try fails too, and the next would be past the deadline, so the event is lost.
  since=$(now)
  # The adapter gives back an event's body alone, so what this does is the check, not its status.
  invoke $NAME-events "$(jq -cn --arg c "$commit" --argjson d "$(date +%s)" \
    '{app: "alpha", commit: $c, tries: 9, deadline: $d}')" --qualifier retry >/dev/null || fail "invoke the retry alias"
  if awaitlog events "$since" "failed every try" 30; then pass "a delivery past its window is lost, with a warning"
  else fail "a delivery past its window is lost, with a warning"; fi
  is "and its object is deleted" "$(aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix "outbox/alpha/$commit" \
    --query KeyCount --output text)" 0
fi

echo "== schedules (the run's are deleted, so that no daily sweep falls among the checks that follow)"
unschedule

echo "== heads (the numbers for a cache of heads)"
# Each head's reads and writes, from serve's debug log, in a mix of turns on one name: one writer, one reader, one
# that reads under POST, then four writers at once, which take more environments and conflict. Every event is kept in
# heads-$run.json, in target/live, for a closer look.
if [ $debug != yes ]; then
  fail "serve logs at debug, for the heads' numbers" "RUST_LOG has no tric=debug: apply with -var log=warn,tric=debug"
else
  h=h-$run t=$(now)
  for _ in $(seq 20); do post "$A/@$h/kv?op=incr&key=n" >/dev/null; done
  for _ in $(seq 20); do get "$A/@$h/kv?op=get&key=n" >/dev/null; done
  for _ in $(seq 10); do post "$A/@$h/kv?op=get&key=n" >/dev/null; done
  codes=$(mktemp)
  for w in 1 2 3 4; do
    (for _ in $(seq 10); do code -X POST "$A/@$h/kv?op=incr&key=w$w"; echo; done) >>"$codes" &
  done
  wait
  ok=$(grep -c '^200$' "$codes")
  [ "$ok" = 40 ] || note "of the writers' 40 increments, $((40 - ok)) were refused: $(sort "$codes" | uniq -c | xargs)"
  rm -f "$codes"
  is "the increments that answered 200 are the ones that landed" \
    "$(for w in 1 2 3 4; do value "$A" $h w$w; done | jq -s 'map(tonumber? // 0) | add')" "$ok"
  sleep 30
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-serve --start-time "$t" --filter-pattern "\"$h\"" \
    --output json >"$live/heads-$run.json"
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-route --start-time "$t" \
    --filter-pattern '{ $.fields.message = "route" && $.fields.app = "alpha" }' --output json >"$live/routes-$run.json"
  if python3 -I - "$live/heads-$run.json" "$live/routes-$run.json" <<'PY'
import json, sys
# A cache would keep, in each environment (a log stream), the head it last read or wrote of each name. A writing turn
# could then skip its read when the cache is current (a hit), would run in vain, and conflict, when it is stale (a
# miss), and would read as now when it has nothing; a turn that writes nothing would still have to check its read.
rows = []
for e in json.load(open(sys.argv[1]))["events"]:
    try:
        f = json.loads(e["message"])["fields"]
    except (ValueError, KeyError, TypeError):
        continue
    if f.get("message") in ("read", "put"):
        rows.append((e["logStreamName"], e["timestamp"], f))
rows.sort(key=lambda r: (r[0], r[1]))
known, last, reads, puts, conflicts = {}, {}, [], [], 0
for stream, at, f in rows:
    k, etag = (stream, f["path"]), f.get("etag", "")
    if f["message"] == "read":
        r = {"ms": f["ms"], "at": at, "etag": etag, "seen": known.get(k), "wrote": None}
        reads.append(r)
        known[k], last[k] = etag, r
    else:
        puts.append(f["ms"])
        r = last.get(k)
        if r and r["wrote"] is None and r["etag"] == f.get("base", ""):
            r["wrote"] = at - r["at"]  # the run and the write, which a miss would waste
        if "etag" in f:
            known[k] = etag
        else:
            conflicts += 1
def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else "-"
def note(*s):
    print("NOTE  " + " ".join(str(x) for x in s))
routes = [json.loads(e["message"])["fields"]["ms"] for e in json.load(open(sys.argv[2]))["events"]]
writing = [r for r in reads if r["wrote"] is not None]
hits = [r for r in writing if r["seen"] == r["etag"]]
misses = [r for r in writing if r["seen"] not in (None, r["etag"])]
ms, envs = [r["ms"] for r in reads], len(set(s for s, _, _ in rows))
note(f"heads: {envs} environments; {len(reads)} reads, ms p50 {pct(ms, .5)} p90 {pct(ms, .9)};",
     f"{len(puts)} writes, ms p50 {pct(puts, .5)} p90 {pct(puts, .9)}, {conflicts} conflicts;",
     f"a request at the router, ms p50 {pct(routes, .5)} p90 {pct(routes, .9)}")
saved, wasted = sum(r["ms"] for r in hits), sum(r["wrote"] for r in misses)
note(f"heads: {len(writing)} writing turns: {len(hits)} hits, which would save {saved} ms; {len(misses)} misses,",
     f"which would waste {wasted} ms; {len(reads) - len(writing)} reads that write nothing,",
     f"{sum(r['seen'] == r['etag'] for r in reads if r['wrote'] is None)} of them hits")
sys.exit(0 if len(reads) >= 90 and len(puts) >= 60 else 1)
PY
  then pass "serve's log has every head read and write of the mix"
  else fail "serve's log has every head read and write of the mix" "see $live/heads-$run.json"; fi
fi

echo "== sweep (the objects that no name refers to)"
# One sweep of alpha, as Scheduler asks for it: the cron alias, given {app, sweep}. The router has serve do it, as
# alpha's tenant and with alpha's credentials, and serve logs what it did (src/route.rs's `sweep`, src/serve.rs's
# `sweep`). It deletes an object only if no tree names it and it is an hour old at least (src/sweep.rs's GRACE), and
# this run cannot wait an hour for one to be old enough. So what is checked here is what a sweep must never do (take
# what is named, or what is young) and what it says it did; that it deletes what is old is for src/sweep.rs's tests.
# The names alpha has values under, one to a line; the keys of apps/alpha/ that name this run.
valuenames() {
  aws s3api list-objects-v2 --bucket "$TRIC_BUCKET" --prefix apps/alpha/values/ --delimiter / \
    --query 'CommonPrefixes[].Prefix' --output text | listed
}
mine() { under apps/alpha/ | grep -F -- "-$run"; }
# Puts an object at key $1, as this admin, and has the teardown delete it.
plant() {
  planted="$planted $1"
  printf '{}' | aws s3 cp - "s3://$TRIC_BUCKET/$1" --only-show-errors || fail "plant $1"
}
# What a turn that died after it uploaded would leave: an object at the key of a tree's, with an id of 32 hex digits
# that no tree names; at a name that has a head, and at one that has none.
s1=apps/alpha/values/files-$run/$(openssl rand -hex 16)
s2=apps/alpha/values/stray-$run/$(openssl rand -hex 16)
plant $s1
plant $s2
# For `late`, whose sweep is an hour on: hist-RUN, written twice, so that what its first tree alone has is named by no
# head; and a stray beside it that nothing names, which the run leaves for that sweep to delete.
post "$A/@hist-$run/kv?op=set&key=k&value=one" >/dev/null
h1=$(under apps/alpha/values/hist-$run/)
post "$A/@hist-$run/kv?op=set&key=k&value=two" >/dev/null
h2=$(under apps/alpha/values/hist-$run/)
is "hist-$run's second write kept the first's objects, and added its own" \
  "${h1:+some}/$(comm -23 <(echo "$h1") <(echo "$h2") | grep -c .)/$([ "$h2" != "$h1" ] && echo more)" \
  some/0/more
aged=apps/alpha/values/hist-$run/$(openssl rand -hex 16)
printf '{}' | aws s3 cp - "s3://$TRIC_BUCKET/$aged" --only-show-errors || fail "plant $aged"
made=$(comm -13 <(echo "$h1") <(echo "$h2"))
printf 'run=%s\naged=%s\nat=%s\nhist="%s"\nmade="%s"\n' "$run" "$aged" "$(date +%s)" "${h2//$'\n'/ }" \
  "${made//$'\n'/ }" >"$state"
names=$(valuenames | grep -c .)
before=$(mine)
is "the strays are in the bucket" "$(for k in $s1 $s2; do awscode aws s3api head-object --bucket "$TRIC_BUCKET" \
  --key $k; done | xargs)" "OK OK"
since=$(now) swept_from=$since
asksweep
swept_to=$(now)
# The router says the sweep was answered 204 only when serve had done it and logged it.
if awaitpat events "$since" '{ $.fields.message = "sweep" && $.fields.app = "alpha" && $.fields.status = 204 }' 90 &&
  awaitpat serve "$since" '{ $.fields.message = "swept" && $.fields.app = "alpha" }' 90; then
  pass "the router logs alpha's sweep, answered 204 by serve, and serve logs its report"
else fail "the router logs alpha's sweep, answered 204 by serve, and serve logs its report" "no such lines in 90 s"; fi
report=$(sweepreport alpha "$since")
if [ -n "$report" ]; then
  is "the sweep went through every name alpha has ($names, the strays' among them)" "$(jq -r .swept <<<"$report")" \
    "$names"
  is "none was skipped, none failed, and it was done" "$(jq -r '[.skipped, .failed, .done] | join(" ")' <<<"$report")" \
    "0 0 true"
  note "the sweep deleted $(jq -r .deleted <<<"$report") objects, $(jq -r .bytes <<<"$report") bytes: any that earlier \
runs left and are an hour old; the report is $report"
else fail "serve reports alpha's sweep" "no 'swept' line in serve's log"; fi
is "one sweep, one line of the router's about it" "$(sweeplines events "$since")" 1
is "and one of serve's, with no warning" "$(sweeplines serve "$since")" 1
is "nothing of this run's is deleted, or added" "$(mine)" "$before"
for k in $s1 $s2; do
  is "the stray $k is kept, being younger than the hour's grace" "$(awscode aws s3api head-object \
    --bucket "$TRIC_BUCKET" --key $k)" OK
done
for V in std binding; do
  by $V
  is "$V: a key is read as it was" "$(value "$A" ${X}files-$run k)" '"kept"'
  is "$V: a file is read as it was" "$(fget "$A" ${X}files-$run 'op=read&path=/hello.txt')" '{"ok":"kept"}'
  is "$V: the file of blocks is as it was" "$(fsum "$A" ${X}big-$run /big)" "$(patsum 1000000 262142 XYZW 300000)"
  is "$V: and the count of the racing turns" "$(fget "$A" ${X}race-$run 'op=read&path=/n' | jq -r '.ok // "ERR"')" \
    "${raced[I]}"
done
is "the file both routes wrote is as it was" "$(fsum "$A" one-$run /big)" "$(patsum 600000 262142 XYZW)"

# Nothing else may ask for a sweep. Serve takes the router's `Forwarded: for=_sweep` for the ask, and only the cron
# alias's {app, sweep: true}, which only Scheduler may invoke, makes the router write it (src/route.rs's `job`,
# src/serve.rs's `handle`). So not a client's request, whose Forwarded the router replaces with its own; nor the
# router's function invoked on its own; nor an event for another alias, for which the router writes its own; nor an
# event for the cron alias of another shape, or for an app with no release. The answers of events don't show (the
# adapter gives back the body alone), so the log is the evidence, and this admin, who may invoke anything, stands for
# a forger.
since=$(now)
is "a GET that says Forwarded: for=_sweep is a request like any other" "$(get "$A/" -H 'Forwarded: for=_sweep')" hello
is "so is a POST" "$(post "$A/" -H 'Forwarded: for=_sweep')" hello
ask='{"app":"alpha","sweep":true}'
invoke $NAME-route "$ask" >/dev/null || fail "invoke the router"
for q in "" "--qualifier outbox" "--qualifier retry"; do
  invoke $NAME-events "$ask" $q >/dev/null || fail "invoke events ${q:-unqualified}"
done
for body in '{"app":"alpha"}' '{"app":"alpha","sweep":false}' '{"app":"A/b","sweep":true}' \
  '{"app":"alpha","sweep":true,"path":"/"}' "{\"app\":\"ghost-$run\",\"sweep\":true}"; do
  invoke $NAME-events "$body" --qualifier cron >/dev/null || fail "invoke the cron alias with $body"
done
# 8 invocations of events, and 3 of the router (the two requests above, and its function invoked on its own), each
# ending in a REPORT: so that a sweep among them would have been logged by now.
if awaitreports events "$since" 8 120 && awaitreports route "$since" 3 120; then
  pass "the log has the forged asks' invocations"
else
  fail "the log has the forged asks' invocations" "events has $(reports events "$since") of 8, route \
$(reports route "$since") of 3"
fi
for f in route events serve; do
  is "the forged asks made no sweep ($NAME-$f's log)" "$(sweeplines $f "$since")" 0
done
is "and nothing of this run's is changed" "$(mine)" "$before"

echo "== trees (what the turns of the run asked of the bucket)"
# Serve's debug log has a "tree" line for each turn and name, by the tree's prefix, when the last of the turn's trees is
# gone (crates/tree's Reader): the objects it fetched (`gets`: nodes and blobs, which the line doesn't tell apart, and
# `read` is their bytes), those the process's cache gave instead (`hits`), and those it uploaded (`puts`, nodes and
# blobs, so a commit's bucket writes). The sweep's report is its "swept" line. Kept in tree-$run.json, with the sweep's
# own walks apart.
if [ $debug != yes ]; then
  note "no numbers for the trees: serve does not log at debug (apply with -var log=warn,tric=debug)"
else
  sleep 15
  raw=$(mktemp) swp=$(mktemp)
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-serve --start-time "$t_files" \
    --filter-pattern '{ $.fields.message = "tree" }' --output json >"$raw"
  aws logs filter-log-events --log-group-name /aws/lambda/$NAME-serve --start-time "$t_files" \
    --filter-pattern '{ $.fields.message = "swept" && $.fields.app = "alpha" }' --output json >"$swp"
  if python3 -I - "$raw" "$swp" "$live/tree-$run.json" $run $swept_from $((swept_to + 2000)) <<'PY'
import json, sys
tree, swept, out, run, t0, t1 = sys.argv[1:]
t0, t1 = int(t0), int(t1)
KEYS = ("gets", "hits", "puts", "read", "written", "depth", "splits", "merges", "collapses")
def lines(path):
    for e in json.load(open(path))["events"]:
        try:
            yield e, json.loads(e["message"])["fields"]
        except (ValueError, KeyError, TypeError):
            continue
turns, walks = [], []
for e, f in lines(tree):
    # The line names the tree by its prefix (src/tree.rs's `prefix`): alpha's names' are apps/alpha/values/<name>.
    under, _, name = str(f.get("prefix", "")).rpartition("/")
    if under != "apps/alpha/values" or not name.endswith("-" + run):
        continue
    row = {"at": e["timestamp"], "stream": e["logStreamName"], "name": name}
    row.update({k: f.get(k, 0) for k in KEYS})
    (walks if t0 <= row["at"] <= t1 else turns).append(row)
turns.sort(key=lambda r: r["at"])
sweeps = [dict({k: v for k, v in f.items() if k != "message"}, at=e["timestamp"]) for e, f in lines(swept)]
json.dump({"run": int(run), "turns": turns, "walks": walks, "sweeps": sweeps}, open(out, "w"), indent=1)
def note(*s):
    print("NOTE  " + " ".join(str(x) for x in s))
def total(rows, k):
    return sum(r[k] for r in rows)
def show(label, rows):
    if not rows:
        return note(f"trees: no {label}")
    gets, hits, puts = total(rows, "gets"), total(rows, "hits"), total(rows, "puts")
    commits = sum(1 for r in rows if r["puts"])
    note(f"trees: {label}: {len(rows)} lines; {gets} gets, {hits} cache hits ({100 * hits // max(1, gets + hits)}%),",
         f"{total(rows, 'read')} bytes read; {puts} puts in {commits} commits ({puts / max(1, commits):.1f} a commit),",
         f"{total(rows, 'written')} bytes written; depth {max(r['depth'] for r in rows)}")
show("the turns", turns)
for name in sorted({r["name"] for r in turns}):
    show(name[: -len(run) - 1], [r for r in turns if r["name"] == name])
show("the sweep's walks", walks)
for s in sweeps:
    note("trees: a sweep reported", ", ".join(f"{k} {s[k]}" for k in sorted(s) if k not in ("app", "at")))
ok = True
for prefix in ("", "p3-"):
    big = [r for r in turns if r["name"] == prefix + "big-" + run]
    most = max(big, key=lambda r: r["puts"]) if big else {"puts": "?", "gets": "?"}
    note(f"trees: the commits of {prefix}big, a file of four blocks: the largest took", most["puts"], "puts, and",
         most["gets"], "gets")
    ok = ok and bool(big) and most["puts"] >= 4
sys.exit(0 if ok else 1)
PY
  then pass "a commit of the file of four blocks uploads four objects at least, through each route"
  else fail "a commit of the file of four blocks uploads four objects at least, through each route" \
    "see $live/tree-$run.json"; fi
  rm -f "$raw" "$swp"
fi

echo "== quiet (nothing else is running)"
sleep 30
t=$(now)
invoke $NAME-events "{\"app\":\"ghost-$run\",\"path\":\"/@x/echo\"}" --qualifier cron >/dev/null ||
  fail "invoke the cron alias"
code "https://nobody-$run.$DOMAIN/" >/dev/null
sleep 60
is "cron for an app with no release runs the router once" "$(reports events "$t")" 1
is "an unknown host runs the router once" "$(reports route "$t")" 1
is "and neither runs serve" "$(reports serve "$t")" 0
t=$(now)
sleep 180
for f in route events serve; do is "$NAME-$f didn't run in 3 idle minutes" "$(reports $f "$t")" 0; done

echo "== cleanup (the strays, and the schedules of the run's apps, if any are left)"
teardown
# The strays are deleted with a delete marker each, which the bucket's lifecycle rules take with the version in a day.
note "left by the run: the releases of alpha, beta, gamma and delta (no command removes an app), the keys and files \
under the names that end in -$run, the delete markers of the strays, and ${aged:-no stray}, for the late sweep"

summary
echo "In 65 minutes to 3 hours: bash $0 late"
echo "To remove the install: docker compose -p $P run --rm tofu destroy"
exit $((fails > 0))
