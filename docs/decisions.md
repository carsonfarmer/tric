# Decisions

What tric is and why, as built. The grilling log that led here, rounds 1 to 9, is at
[91bc4c2](https://github.com/carsonfarmer/tric/blob/91bc4c2/docs/decisions.md); this file replaces it, and wins where
the two disagree.

## Principles

1. **Invent nothing.** Use a standard where one exists: HTTP semantics (RFC 9110), `Prefer` (RFC 7240), `Forwarded`
   (RFC 7239), `Retry-After` and 429 (RFC 6585), 508 (RFC 5842), the Idempotency-Key draft, Fetch Metadata, POSIX
   cron, `wasi:http` 0.3, `wasi:keyvalue`, `wasi:cli/environment`, and the component model's own composition.
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

- A **read** (`GET`, `HEAD`, `OPTIONS`) sees a snapshot of every name it opens, each read once and never waiting.
- A **turn** (`POST`, `PUT`, `PATCH`, `DELETE`) on `/@N` can write N and only N. Its writes are buffered and committed
  atomically when the app answers, before the response's head leaves: a 1xx to 4xx commits, a 5xx, a trap or a
  timeout discards. Other names it opens are snapshots. A request with no name reads any name and writes none.
- A write to another name is `access-denied`; a write after the answer is an error.
- Turns on one name are serializable. tric runs them optimistically and commits with a compare-and-swap on the
  name's head; a turn that loses reruns with the same request. So that a rerun is never seen, a turn **claims** the
  name (writes a lease into the head) before anything it cannot repeat: an unsafe outbound request, a request body over
  6 MB, or 1 s of running. A rerun claims before it starts. Turns wait while another holds the claim (polling every
  25 to 50 ms), and get `429` with `Retry-After: 1` after 5 s.
- Every named response carries the head's `ETag` unless the app set one. A turn evaluates `If-Match` and
  `If-None-Match` against it first, and answers `412` when they fail.

A name's **head** is one JSON object, at most 1 MiB: its values (up to 1 KiB inline, larger ones as their own
objects, named by a random key and pinned by version), its pending outbox commits, and its claim. Keys are at most
256 bytes and values at most 1 MiB; a write that would take the head past 1 MiB fails when it is made.

Superseded value objects are deleted when the commit that replaced them lands, and only in a versioned bucket, where
the delete leaves a noncurrent version that snapshots still read and the bucket's lifecycle expires. An unversioned
store (the in-memory one `tric dev` uses) keeps them.

## The outbox

A request an app sends with `Prefer: respond-async` during an open turn is held, not sent: the app gets `202` with
`Preference-Applied: respond-async` at once, and the requests the turn held (1 MiB of them at most) are delivered, in
order, only if the turn commits. Elsewhere `Prefer` is passed on like any header.

Delivery is at least once. Each request carries `Idempotency-Key: <commit>/<n>`. A 2xx to 4xx is done; a 5xx, a 429
or a network error retries the whole event, with backoff from 1 s to 5 min, honouring `Retry-After`, for 6 h. The
event is enqueued before the commit lands and names the head version the commit replaces; the deliverer waits for the
head to move past it and drops the event unless the commit is in the head's pending set, then removes it from the set
when done. Local hosts run a relay in-process; on Lambda an event is an asynchronous `Invoke` of the function itself.

## Requests to itself, and `Forwarded`

A request from an app to its own host runs in-process. A turn that would reach a name already in the chain gets
`508`; chains are at most 16 deep. tric overwrites `Forwarded` on every request it serves: `for=<peer>;host=<host>;
proto=<proto>` from the network (`X-Forwarded-Proto` and `X-Forwarded-Host` are honoured from the proxy in front),
`for=_tric` from the app itself or the outbox, and `for=_cron` from cron.

## Middleware

`tric.toml` lists middleware, outermost first: `middleware = [{ path = "…" }, { url = "…", digest = "sha256:…" }]`.
Each exports and imports `wasi:http/handler`; `deploy` composes the chain with `wac-graph`, plugging the app into the
innermost, so a release is one component. First-party `guard` middleware checks `Sec-Fetch-Site` (refusing
cross-site unsafe requests) and, given `GUARD_JWKS`, a bearer JWT; `for=_tric` and `for=_cron` pass.

## Cron

`[cron]` in `tric.toml` maps POSIX cron expressions (5 fields, UTC) to paths: `"0 8 * * *" = "/@digest/run"`. Each
fires as a `POST` with `Forwarded: for=_cron`. Local hosts tick in-process; on Lambda every entry is an EventBridge
Scheduler schedule that `deploy`, `release` and `env` keep in step with the release that runs.

## The CLI

- `tric dev [PATH]`: serve the app at PATH (a directory with `tric.toml`, or a `.wasm`) on `127.0.0.1:3000`, with an
  in-memory store unless `--store`, `-e K=V` for its environment, relay and cron included.
- `tric serve`: serve every app in `--store`, each at the host whose first label is its name.
- `tric deploy PATH [-e K=V]`: compose and upload a release, and run it. Deploying the same thing twice is one release.
- `tric release APP ID`, `tric releases APP` (the last 10), `tric env APP [K=V …]` (an empty value removes).

## The bucket

| Key | Holds |
|---|---|
| `apps/<app>/current` | the release the app runs, its last 10 releases, and its environment |
| `apps/<app>/releases/<id>` | a release: its component's hash, allowed outbound hosts and cron; `id` is its hash |
| `apps/<app>/components/<sha256>` | a component, after composition |
| `apps/<app>/names/<name>` | a name's head |
| `apps/<app>/values/<random>` | a large value |
| `native/<compat>/<sha256>` | native code a host made of a component, for hosts of the same build |
| `install` | on AWS, what the install is: its function and its Scheduler role |

Releases and components are never deleted. The bucket is versioned, with noncurrent versions expiring after 7 days
and expired delete markers removed.

## AWS

One Lambda function, `tric serve` behind the Lambda Web Adapter, behind a function URL, behind CloudFront at
`*.<domain>`; one versioned S3 bucket. The function invokes itself asynchronously for the outbox, with 2 retries, a 6 h
maximum age and failures recorded in the bucket. A request is an event (outbox or cron) when the adapter marks it as
one: it sets `x-amzn-request-context` on every request it forwards, to `null` exactly when it came from `Invoke`, and
a viewer can't set it. `tofu apply` in `infra/aws` installs all of it into a fresh account.

Deferred: Lambda tenant isolation (a tenant-isolated function has no function URL), GCP and Azure, and WebSockets
over HTTP (Pushpin's protocol), which will be a feature flag.
