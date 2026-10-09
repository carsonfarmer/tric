# tric: the build plan

**This is the only plan. After any compaction, re-read this file before doing anything else.**

This is a full rewrite. The target is the system described below, and nothing else:
- Old code is not a reference for behaviour; the approved design is.
- A piece of old code is copied in only if it already does exactly what a section below says. It is re-read line by
  line when it is copied.
- There is no backward compatibility: not with old buckets, old heads, old commands or old infrastructure.

## The principles

1. **Invent nothing.** Every behaviour maps to a standard.
2. **As little code as possible.** A small codebase matters more than features.
3. **Scale to zero, always.** Nothing runs, or costs, while idle.
4. **tric runs components; developers build them.**

**The one idea.** An app is one `wasi:http` 0.3 service. tric gives meaning to the app's URLs and nothing more:
- no SDK, no bindings and no proprietary imports;
- every feature is either HTTP semantics on the app's own origin or a standard WASI import.

## Security, up front

- **Two layers of isolation**, so an escape from the Wasm sandbox reaches one app only:
  - the sandbox;
  - a Lambda tenant per app, whose storage credentials reach only that app.
- **The router is the trusted core.** It mints any app's credentials and runs no app code.
- **serve's own role has no storage access at all.** Its only permissions are its logs and invoking the router's
  `outbox` alias.
- **Outbound requests default to none.** An app reaches only the hosts it lists. Private ranges and metadata
  endpoints are always blocked.
- **`Forwarded` is the only internal marker.** The router replaces it on every request. `for=_cron` and `for=_tric`
  can come only from tric.
- **Middleware runs with all of the app's capabilities.** A middleware component is pinned by `sha256:` digest.
- **Background requests are stored until delivered, headers included.** They sit in Lambda's queue and, after failed
  retries, in a dead-letter prefix only the platform can read. Delivery is at least once.
- **Still shared between apps:** the account's concurrency, the log group, the router, and cookies across
  `*.<domain>`.

## The system

### What an app sees

- **Names.** A path whose first segment starts with `@` (`/@room:42/…`) addresses that name's state.
  - Safe methods (GET, HEAD, OPTIONS) read a snapshot and never wait.
  - Any other method is a **turn** on the name.
  - A request without a name can read any name and write none.
- **State.** The import is `wasi:keyvalue`, where `open(name)` opens a name:
  - the turn's own name is writable;
  - any other name opens as a read-only snapshot.
- **Turns commit at the answer.**
  - A turn sees the name's last commit plus its own writes.
  - Writes are held in memory until the handler returns its Response.
  - Any status except 5xx commits the writes and the held background requests together, before a single header
    leaves.
  - A 5xx, a trap or an error discards both.
  - After the answer, writes fail, and the body streams on, read-only.
- **Turns are optimistic.**
  - If another turn commits first, the instance is discarded and the request re-runs, which no one can observe.
  - A turn claims its name at its first irreversible step: an unsafe outbound request, reading past the body
    buffer, or running too long.
  - A busy name answers 429 with `Retry-After`.
- **Versions are ETags.** The head's version is the ETag, and `If-Match` is honoured, with 412 on a mismatch.
- **Calls are fetches.**
  - A `fetch` to the app's own origin runs in-process, as a request with `Forwarded: for=_tric`.
  - An unsafe call claims the caller's name first.
  - A cycle answers 508.
- **Background requests.** An outbound request sent during a turn with `Prefer: respond-async` (RFC 7240):
  - gets `202` and `Preference-Applied: respond-async` at once;
  - is held, and sent only if the turn commits;
  - carries `Idempotency-Key: <commit>/<n>`;
  - is delivered at least once, in order within its turn.

  A turn's held requests total at most 1 MB.
- **Cron.** `[cron]` in `tric.toml` maps POSIX crontab fields to a path. That path gets a `POST` with
  `Forwarded: for=_cron`.
- **`Forwarded`.** Every request carries exactly one `Forwarded` header, set by tric:
  - `for=<client>;host=<host>;proto=<proto>` from outside;
  - `for=_cron` for cron;
  - `for=_tric` for tric's own calls.
- **Config and secrets** are environment variables (`wasi:cli/environment`). They are set from the CLI and stored with
  each release.
- **Middleware** is listed in `tric.toml`, outermost first, as `{ url, digest }`. It is plugged in front of the app with
  `wac-graph` at deploy, and in `tric dev`, so tric always runs a single component.
- **WebSockets** are behind the cargo feature `ws`, using Pushpin's WebSocket-over-HTTP:
  - each message is a `POST` to `/@name` with `Content-Type: application/websocket-events`;
  - `tric dev` holds the sockets;
  - on AWS, API Gateway WebSocket holds them (later).

### `tric.toml`

The file is optional. `tric dev app.wasm` works with no file at all, and the app is named after the file, or after the
directory when a file is used. It has four optional keys:
- `component`;
- `allowed_outbound_hosts`;
- `middleware`;
- `[cron]`.

`--allow <host>` (repeatable) on `dev` and `deploy` adds to `allowed_outbound_hosts`.

### Storage

There is one bucket. Versioning is on from the start.

```
apps/<app>/current               the release: component digest, allowed hosts, cron, env; deploy writes it
apps/<app>/components/<sha256>   the component, after middleware; deploy writes it
apps/<app>/names/<name>          a head; changed only by If-Match / If-None-Match: *
apps/<app>/values/<unique>       a value too big to inline; written once, read by version id
native/<app>/<compat>/<sha256>   compiled code, written by the app's own tenant on a miss
aws/lambda/async/                the dead letters: Lambda's on-failure records, under its fixed prefix (platform only)
```

- **A head** is plain JSON with three parts:
  - the values: small ones inline, others as `{key, version}`;
  - `pending`: commit id → {SHA-256 of its delivery event, time};
  - an optional claim.
- **Values.**
  - A commit deletes the values it replaces. With versioning, that only adds a delete marker, so old heads still
    read them by version id.
  - A turn that loses its race deletes its own uploads.
  - One lifecycle rule expires noncurrent versions after N days and removes expired delete markers. There is no GC
    code.
- **`pending`.** Each commit drops pending entries older than Lambda's maximum event age (6 h).
- **A 403 is read as not found.** S3 answers 403 for a missing key when the caller can't list.

### `tric route`: the router (trusted, runs no app code)

**For each client request:**
1. Take the app from the first label of the Host, strictly: a DNS label, followed by exactly `.<domain>`.
2. HEAD `apps/<app>/current`, cached for a few seconds, misses included. An unknown app gets 404, and no tenant is ever
   created for it.
3. Mint credentials with STS `AssumeRole` on the one app role:
   - the session name is the app (STS allows 64 characters, too few for `app-` and a 63-character label);
   - the session policy grants:
     - `apps/<app>/{names,values}/*`: read and write;
     - the rest of `apps/<app>/*`: read;
     - `native/<app>/*`: read and write.

   They are cached per app until 15 minutes before they expire.
4. Remove the client's `Forwarded`, `X-Forwarded-*`, `X-Amz*` and `X-Tric-*`. Then:
   - set `Forwarded: for=<client>;host=<host>;proto=<proto>`;
   - set `X-Tric-Credentials`, holding the credentials in AWS's `credential_process` JSON format;
   - set the tenant id to the app.
5. Send it to serve and stream the answer back. Log the app, the status and the milliseconds.

**Events:**
- **Cron.** Scheduler invokes the router with `{app, path}`. The router sends serve `POST <path>` with
  `Forwarded: for=_cron`.
- **Outbox.** serve invokes the router's `outbox` alias with a delivery event. The router sends it to serve as `POST`
  with `Forwarded: for=_tric`.
- **The two inputs are kept apart.**
  - The router accepts outbox events only when invoked as `outbox`, and cron events only otherwise.
  - serve may invoke only `outbox`, so a compromised app can't forge cron.

**Backends:**
- HTTP (local): a plain proxy to `tric serve`.
- Lambda (AWS): `InvokeWithResponseStream` on serve with `TenantId` = the app, which needs:
  - the request sent as a function-URL event;
  - the event stream decoded into its prelude, then 8 NULs, then the body;
  - failures taken from `InvokeComplete`'s `ErrorCode`.

### `tric serve`: the runtime (one app per tenant)

- **It refuses:**
  - a request whose tenant id isn't the Host's app;
  - a request without credentials.
- **It removes `X-Tric-Credentials`** before the app sees anything, and makes every storage call with those
  credentials.
- **It loads the app:**
  1. read `current`;
  2. load `native/<app>/<compat>/<sha>` if present;
  3. otherwise fetch the component, compile it and write the native code.
- **It handles a `POST` carrying `Forwarded: for=_tric` as a delivery event.** Only the router can set that marker.
  1. Wait for the sender's commit: `If-None-Match` on the version the turn started from, up to the turn deadline.
  2. Drop the event unless its commit id and its digest are in `pending`.
  3. Send each request in order:
     - to the app's own origin: in-process, as a turn;
     - anywhere else: over the network, under the allow list.
  4. Clear the commit from `pending`.

  Any 5xx, 429 or network error answers 5xx. The router's invocation then fails, Lambda retries it twice, and then it
  goes to the dead letters.
- **At a turn's answer, with background requests:** invoke the router's `outbox` alias first, then commit, recording
  the digest in `pending`.
- **It is a plain HTTP server.** On Lambda, the Lambda Web Adapter translates.

### Local

- **`tric dev [path] [--allow host]… [-e K=V]…`** runs one app in one process:
  - state in memory;
  - `Forwarded` set from the socket's peer;
  - cron ticking in-process;
  - background requests through an in-process relay that backs off and honours `Retry-After`.
- **compose** has three services:
  - `route` on `<app>.localhost:3000`, with the HTTP backend; it ticks cron itself and relays outbox events with the
    same backoff;
  - `serve`, with no storage credentials of its own;
  - RustFS, with versioning and STS through a `router` user.
- **`tric deploy [path] [--allow host]… [-e K=V]…`:**
  1. plug in the middleware;
  2. compile, to check the component;
  3. write `components/<sha>`, then `current`;
  4. on AWS, sync the app's schedules.

### AWS (OpenTofu)

- **The router:** two functions from one package, on one role:
  - `route`, a function URL with streaming behind CloudFront at `*.<domain>`, which takes only requests carrying
    CloudFront's origin secret;
  - `events`, which takes only events, plus an `outbox` alias. The alias carries the async config: two retries, a
    6 h maximum event age, and the bucket as the on-failure destination.
- **serve:** per tenant (`PER_TENANT`), with no URL. Only the router may invoke it.
- **The roles:**
  - the app role, which trusts only the router's role;
  - the router's role, which can assume the app role, HEAD `current` and invoke serve;
  - serve's role, with its logs and `events:outbox` only;
  - the Scheduler role, which can invoke `events`, unqualified.
- **The bucket:** versioning, the lifecycle rules (noncurrent versions, delete markers, the dead letters), and short log
  retention.
- One `tofu apply` installs it all into a fresh account. The apply waits for the user's go-ahead.

## Changes the isolation forces on the approved design

1. **Outbox events go through the router.** serve can't mint credentials, so "invoke our own function" becomes
   "invoke the router's `outbox` alias". The router then invokes serve with the tenant id.
2. **The digest in `pending`.** Without it, an app that learnt another app's commit id (from an `Idempotency-Key` sent
   to it) could forge that app's deliveries.
3. **`Prefer: respond-async` with no commit is not applied.** This covers a request without a name, or one sent after
   the answer.
   - The request goes out at once, as an ordinary fetch, with no `Preference-Applied`. RFC 7240 lets a server ignore a
     preference.
   - Without a commit there is nothing in the sender's head to check, so such an event could be forged.

## Not in this build

- teams and per-team publishing;
- budgets and per-app limits;
- retrying 429 or 5xx later through Scheduler;
- `tric logs`;
- a dead-letter command;
- release history commands;
- previews and forks;
- `Accept-Datetime`;
- `wasi:filesystem`;
- S3 Express;
- head caches;
- GCP and Azure;
- the `guard` middleware.

## Order of work

Each step ends with the gate passing (`docker compose run --rm test`) and a commit.

1. **The fresh tree.**
   - Delete everything that isn't the plan: the source, the tests, the infrastructure, the docs and the compose
     services.
   - Write the engine (callback-only async), names and turns, `wasi:keyvalue`, outbound, the outbox, cron and
     `tric dev`.
   - The e2e suite proves the app semantics on `tric dev`.
2. **`tric serve` and `tric route` over HTTP,** with compose and RustFS's STS, and the acceptance criteria below.
3. **The Lambda backend:** `InvokeWithResponseStream` and its event stream, events through the Lambda Web Adapter,
   and the `outbox` alias check.
4. **OpenTofu,** then `tofu validate` and the module checks below.
5. **Docs:** `docs/decisions.md` and the READMEs.
6. **JS and WebSockets,** from `feat/js` and `feat/ws`, on the new tree.
7. **Ask the user before `tofu apply`,** then run the remote checks.

## Acceptance criteria

**The local gate:**
1. **Cross-app denial, on RustFS.** With app A's credentials:
   - B's `current`, B's names and `native/B/…` are refused;
   - so are writes to A's own `current` and components;
   - A's names, values and `native/A/…` work.
2. **Stripping.** A client sends `Forwarded: for=_cron`, `X-Forwarded-For`, `X-Tric-Credentials` and `X-Amz-Tenant-Id`
   through `route`. The app sees only the router's `Forwarded`, and no `X-Tric-*` or `X-Amz*` header.
3. **Pinning.** serve refuses a tenant id that differs from the Host's app, and a request without credentials.
4. **Unknown apps.** `route` answers 404 without calling serve.
5. **Forged outbox events.** An event naming app B is dropped unless its commit and digest are pending in B's head.
6. **Cron** reaches the app through `route` as a `POST` with `for=_cron`.
7. **Native code** is written under `native/<app>/…` with the app's credentials.
8. **The app semantics** pass through `route` → `serve`: names, turns, the outbox, self-fetch, middleware and
   outbound.

**`tofu validate`** passes, and the module shows:
- serve is per tenant, with no URL;
- serve's role has no S3 access;
- the app role trusts only the router;
- the outbox alias carries the async config and the failure destination;
- lifecycle expires noncurrent versions and the dead letters.

**The remote test, after the user says go:**
- an app answers through CloudFront, and streams;
- two apps run in different tenants, and neither one's credentials read the other's data;
- the outbox and cron work;
- a direct call to the router can't forge `_cron`, `_tric` or credentials;
- `for=` is the viewer's address.
