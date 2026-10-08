# Decisions

What tric is and why, as built. The grilling log that led here, rounds 1 to 9, is at
[91bc4c2](https://github.com/carsonfarmer/tric/blob/91bc4c2/docs/decisions.md); this file replaces it, and wins where
the two disagree.

## Principles

1. **Invent nothing.** Use a standard where one exists: HTTP semantics (RFC 9110), `Prefer` (RFC 7240), `Forwarded`
   (RFC 7239), `Retry-After` and 429 (RFC 6585), 508 (RFC 5842), the Idempotency-Key draft, POSIX cron, `wasi:http`
   0.3, `wasi:keyvalue`, `wasi:cli/environment`, and the component model's own composition.
2. **As little code as possible.** When two designs are otherwise close, the smaller one wins.
3. **Scale to zero, always.** An idle install costs its storage and nothing else: no timers, no polling, no warm
   anything. A hard requirement.
4. **tric runs components; developers build them.** No build step, no language toolchains, no templates.

And: no backward compatibility until a first release; no asymmetry (one rule for every case of a kind); no
"practical" limits on teams or apps; state is airtight or it is not offered.

## The app

An app is one component that exports `wasi:http/handler@0.3` and may import:

- `wasi:http/client@0.3`, for outbound HTTP to the hosts it allows, and to itself;
- `wasi:keyvalue@0.2.0-draft2` (`store`, `atomics`, `batch`), for state (below);
- `wasi:cli/environment`, for its config and secrets, which are one thing: environment variables;
- the rest of WASI 0.2 and 0.3 that a language's standard library needs, with nothing granted: no files, no
  sockets, no inherited environment.

Each request gets a fresh instance, under hard limits: 256 MiB of memory, 10 s from instantiation to the response's
head, 300 s in all, 64 requests in flight per app, 64 KiB of stdout and of stderr (logged after the request). Only
callback-style async is enabled (no stackful async, no extra async builtins).

## Names and turns: the state model

State belongs to **names**. A path starting `/@name` (1 to 128 of `A-Za-z0-9._~:-`, starting with a letter or digit)
addresses the name; the path reaches the app unchanged, and the app opens the name's bucket with
`wasi:keyvalue/store.open("name")`.

- A **read** is a request with a safe method (`GET`, `HEAD`, `OPTIONS`, `TRACE`). It sees a snapshot of every name it
  opens, each read once and never waiting.
- A **turn** is a request with any other method, on `/@N`. It can write N and only N. Its writes are buffered and
  committed atomically when the app answers, before the response's head leaves: a status under 500 commits, a 5xx, a
  trap or a timeout discards. Other names it opens are snapshots. A request with no name reads any name and writes
  none.
- A write to another name is `access-denied`; a write after the answer is an error.
- Turns on one name are serializable. tric runs them optimistically and commits with a compare-and-swap on the
  name's head; a turn that loses reruns with the same request. So that a rerun is never seen, a turn **claims** the
  name (writes a lease into the head) before anything it cannot repeat: an unsafe outbound request, a request body over
  6 MiB (which it does not keep), or 1 s of running. A rerun claims before it starts. Turns wait while another holds
  the claim (polling every 25 to 50 ms), and get `429` with `Retry-After: 1` after 5 s. A claim lapses 15 s after it
  is taken, 5 s past the longest a turn can take to answer.
- Every named response carries the head's `ETag` unless the app set one. A turn evaluates `If-Match` and
  `If-None-Match` against it first, and answers `412` when they fail.

A name's **head** is one JSON object, at most 1 MiB: its values (up to 1 KiB inline, larger ones as their own
objects, named by a random key and pinned by version), its pending outbox commits, and its claim. Keys are at most
256 bytes and values at most 1 MiB; a write that would take the head past 1 MiB fails when it is made. `list-keys`
pages by 1,000, and one `get-many` returns at most 16 MiB.

Superseded value objects are deleted when the commit that replaced them lands, and only in a versioned bucket, where
the delete leaves a noncurrent version that snapshots still read and the bucket's lifecycle expires. An unversioned
store (the in-memory one `tric dev` uses) keeps them.

## The outbox

A request an app sends with `Prefer: respond-async` during an open turn is held, not sent: the app gets `202` with
`Preference-Applied: respond-async` at once, and the requests the turn held (1,000,000 bytes of them at most, as
JSON, so that the event fits in Lambda's 1 MB) are delivered, in order, only if the turn commits. Elsewhere `Prefer`
is passed on like any header.

The commit enqueues the requests as an event before it writes the head, naming the head version it writes over, and
adds itself to the head's pending commits. Delivery waits up to 300 s for the head to move past that version (polling
from 50 ms, doubling to 2 s), and drops the event unless the commit is pending, as one that is not did not commit.
It then sends each request until it is done, which a response of 2xx to 4xx but 429 is, in three rounds: at once,
after 60 s and after 120 s more, each request's exchange limited to 60 s, each round starting where the last stopped.
If a request is still not done, the event is written to `failed/<commit>` for the operator. Either way delivery then
takes the commit off the pending ones, trying for 30 s; a pending commit older than 6 h is dropped by the next commit
on the name.

Delivery is at least once: each request carries `Idempotency-Key: <commit>/<n>`, and a body of at most 1 MiB; one
to the app's own host runs in-process, as any request to itself does. Local hosts deliver in a task of their own; on
Lambda an event is an asynchronous `Invoke` of the function itself, which Lambda retries only if the invocation crashes
or times out.

## Requests to itself, and `Forwarded`

A request from an app to its own host runs in-process. A turn that would reach a name already in the chain gets
`508`; chains are at most 16 deep.

tric overwrites `Forwarded` on every request it serves, and strips every `X-Forwarded-*` and `X-Amzn-*` header, so an
app sees where a request came from there and only there. From the network it is
`for=<ip>;host="<host>";proto=<proto>`: the host is the last `X-Forwarded-Host`, else `Host`; the proto the last
`X-Forwarded-Proto` if it is `http` or `https`, else `http`; the address the rightmost in `X-Forwarded-For`, else the
peer's. From the app itself or the
outbox it is `for=_tric`, and from cron `for=_cron`. `X-Amzn-*` goes because Lambda's adapter passes the request's
and the invocation's contexts on in it, and they name the account and the function.

## Middleware

`tric.toml` lists middleware, outermost first: `middleware = [{ path = "…" }, { url = "…", digest = "sha256:…" }]`.
Each exports and imports `wasi:http/handler`; `deploy` composes the chain with `wac-graph`, plugging the app into the
innermost, so a release is one component. A URL's component is checked against its digest, as Subresource Integrity
checks one. tric ships no middleware of its own; the tests' `guard` shows what one looks like.

## Cron

`[cron]` in `tric.toml` maps POSIX cron expressions (5 fields, UTC) to paths: `"0 8 * * *" = "/@digest/run"`. Each
fires as a `POST` to `<proto>://<app>.<domain><path>`, in-process, with `Forwarded: for=_cron`. Local hosts tick
in-process; on Lambda every entry is an EventBridge Scheduler schedule, in the install's group, that `deploy` and
`release` keep in step with the release that runs.

## The CLI

- `tric dev [PATH] [-e K=V …] [--listen ADDR]`: serve the app at PATH (a directory with `tric.toml`, or a `.wasm`) at
  any host, on `127.0.0.1:3000`, with an in-memory store unless `--store`, outbox and cron included.
- `tric serve [--listen ADDR] [--domain DOMAIN]`: serve every app in `--store`, each at `<app>.<domain>`; the domain is
  `localhost` unless set.
- `tric deploy PATH [-e K=V …]`: compose and upload a release, run it, and print its id. Deploying the same thing twice
  is one release.
- `tric release APP ID`, `tric releases APP` (the last 10, marking the one that runs), `tric env APP [K=V …]` (an
  empty value removes; prints the names set).

`--store` is `s3://<bucket>`, or `TRIC_STORE`; credentials, region and endpoint come from the usual `AWS_` variables.

## The bucket

| Key | Holds |
|---|---|
| `apps/<app>/current` | the release the app runs, its last 10 releases, and its environment |
| `apps/<app>/releases/<id>` | a release: its component's hash, allowed outbound hosts and cron; `id` is its hash |
| `apps/<app>/components/<sha256>` | a component, after composition |
| `apps/<app>/names/<name>` | a name's head |
| `apps/<app>/values/<random>` | a large value |
| `native/<compat>/<sha256>` | native code a host made of a component, for hosts of the same build |
| `install` | on AWS, what the install is: its function, Scheduler role and Scheduler group |
| `failed/<commit>` | an outbox whose requests were not all done in their rounds |
| `aws/lambda/async/<function>/…` | on AWS, an event that crashed or timed out three times, or waited 6 h |

Every read is capped and parsed strictly, and a release and a component are checked against their hashes, and every
write has the same caps. Native code cannot be checked, and loading it runs it, so it is trusted as the bucket is:
whoever can write `native/` can run code with the host's rights. Releases and components are never deleted. The bucket
is versioned, with noncurrent versions expiring after 7 days and expired delete markers removed. An upgrade that
changes Wasmtime or its settings changes `<compat>`, so the old native code is unused, and the operator deletes it.

## AWS

One install is one account's: one Lambda function on arm64 running `tric serve` behind the Lambda Web Adapter (its
layer, version 1.1.0), behind a Function URL that streams responses, behind CloudFront at `*.<domain>`; one versioned
S3 bucket; one Scheduler group; a policy for whoever deploys; and a budget alert. `tofu apply` in `infra/aws`
installs all of it into a fresh account, and destroy removes it, leaving only the domain's Route 53 zone, which the
module reads but does not make.

- **One function for everything**: the apps, the outbox and cron. A request is an event when the adapter marks it as
  one: it sets `x-amzn-request-context` on every request it forwards, to `null` exactly when it came from `Invoke`, and
  a viewer cannot set it. An event is `{"outbox": …}` or `{"cron": {"app": …, "path": …}}`, at most 2 MiB.
- **Failures**: the function's asynchronous invocations retry twice, for at most 6 h, and then go to the bucket. A
  failed status does not fail an invocation (there are no `AWS_LWA_ERROR_STATUS_CODES`), so an app's 5xx is never
  retried by Lambda; an outbox that ran out of rounds is in `failed/` instead.
- **CloudFront** passes every viewer header but `Host` on, with the viewer's host in `X-Forwarded-Host`, set by a
  CloudFront function that replaces any the viewer sent; it caches nothing.
- **Who may do what**: the function's role has the bucket, invoking itself and its logs. The `deploy` policy writes
  only under `apps/` and manages the group's schedules, so a deployer ships only what runs in the sandbox; native code
  and `install` are the function's and the administrators'.
- **No concurrency is reserved by default**, as a new account's quota may be only 10; `concurrency` caps it.

Not yet checked live, as only a deployed install can: whether the rightmost `X-Forwarded-For` through CloudFront and
the Function URL is the viewer's address or CloudFront's (if CloudFront's, `for` names CloudFront, and wants its
`CloudFront-Viewer-Address`); that responses stream through CloudFront; and that the function's `Invoke` of itself,
its failure destination and Scheduler's invocations all work with the permissions the module grants.

## Running it locally

Everything runs in Docker, with MinIO for S3, versioned as an install's bucket is:

- `docker compose run --rm test`, the gate CI runs, with the end-to-end tests against MinIO as well as in memory;
- `docker compose up -d serve`, a local install: what an install on AWS is but for Lambda and Scheduler, every app in
  the bucket `local` at `<app>.localhost:3000`, with `docker compose run --rm tric …` as its CLI;
- `docker compose up -d --build lambda`, Lambda locally: `dist/tric.zip` on `provided.al2023` with the adapter and
  Lambda's emulator, taking events as Lambda does, on the local install's bucket. The emulator cannot take the
  outbox's `Invoke`, so a turn that holds requests fails there.

Deferred: Lambda tenant isolation (a tenant-isolated function has no function URL), GCP and Azure, and WebSockets
over HTTP (Pushpin's protocol), which will be a feature flag.
