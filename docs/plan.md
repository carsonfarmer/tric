# Plan: one app per Lambda tenant

## The principles

1. **Invent nothing.** Every behaviour maps to a standard: `wasi:http` 0.3, `wasi:keyvalue`, safe and unsafe methods,
   ETags, `Prefer: respond-async`, `Forwarded`, 429 and 508.
2. **As little code as possible.** A small codebase matters more than features.
3. **Scale to zero, always.** Nothing runs, or costs, while idle.
4. **tric runs components; developers build them.**

An app is one `wasi:http` 0.3 service. tric gives meaning to its URLs and nothing more: `/@name` is state, a turn
commits at the answer, and background requests, cron and WebSockets arrive as requests.

**Isolation.** There are two layers, so an escape from the Wasm sandbox reaches one app only:
- the sandbox itself;
- a Lambda tenant per app, whose credentials reach only that app.

A small trusted router hands out those credentials and runs no app code. No app or team gets infrastructure of its
own: no role, function or apply per app.

**No backward compatibility.** Code that already follows the design stays: names, turns, the outbox, cron and
middleware. The single shared function, its infrastructure and multi-app `tric serve` go.

## Security first

- **What an escape reaches:** that app's tenant, which is its own microVMs, and the credentials the router gave it,
  which cover only that app's prefixes. serve's own role reads no part of the bucket.
- **The router is the trusted core.** It can mint any app's credentials. It runs no guest code, and what it exposes is
  Host parsing, one S3 HEAD, the STS and Lambda calls, and the parsing of serve's response stream.
- **Still shared:**
  - serve's execution role, which holds only its logs and the right to invoke the router's `outbox` alias;
  - the account's concurrency;
  - the log group;
  - the router;
  - cookies across `*.<domain>`.
- **The router's URL is public.** CloudFront's signed access to Lambda would make every client send a SHA-256 of each
  POST body, which browsers don't. A caller who skips CloudFront gains nothing, because the router sets every header
  that carries trust.
- **Background requests and dead letters keep their headers.** Only the platform can read Lambda's queue and the
  dead-letter prefix.
- **Outbound requests default to none.** An app reaches only the hosts it lists. Private ranges and metadata endpoints
  stay blocked always.

## The design

**A request:**
1. CloudFront sends it to the router's function URL, which streams.
2. The router takes the app from the first label of the host, strictly.
3. It HEADs `apps/<app>/current`, cached for a few seconds, misses included. An unknown app gets 404, so no tenant is
   ever created for it.
4. It mints credentials with STS `AssumeRole` on one app role, with session name `app-<app>` and a session policy that
   reaches only `apps/<app>/{names,values}/*` (read and write), the rest of `apps/<app>/*` (read), and
   `native/<app>/*` (read and write). These are cached until five minutes before they expire.
5. It removes the client's `Forwarded`, `X-Forwarded-*`, `X-Amz*` and `X-Tric-*` headers. It sets
   `Forwarded: for=<client>;host=<host>;proto=<proto>`, and passes the credentials in `X-Tric-Credentials`.
6. It calls `InvokeWithResponseStream` on serve with tenant id = the app, and streams the answer back. Failures come
   from the stream's `ErrorCode`. It logs the app, the status and the milliseconds.

**serve:**
- Its tenancy is per tenant. It has no URL, and only the router may invoke it.
- It refuses a request whose tenant id isn't the Host's app, or that carries no credentials.
- It removes `X-Tric-Credentials` before the app sees anything, and makes every S3 call with those credentials.
- It reads a 403 under the app's own prefix as not found, since S3 answers 403 for a missing key without `ListBucket`.
- It stays a plain HTTP server; on Lambda, the Lambda Web Adapter translates.

**Background requests:**
- At the answer, serve async-invokes the router's `outbox` alias with the event, then commits. The commit records the
  event's SHA-256 in the head's `pending`.
- The router mints that app's credentials and invokes serve with tenant id = the app.
- Delivery drops an event unless its commit and digest are in `pending`. So a compromised app can't send requests in
  another app's name.
- A failed delivery fails the router's invocation. Lambda retries it twice, and then writes it to the on-failure
  destination, which is the platform-only dead-letter prefix.

**Cron:** EventBridge Scheduler invokes the router with the app and the path. The router sends serve a POST with
`Forwarded: for=_cron`. serve may invoke only `router:outbox`, so serve can't forge a cron event.

**Native code** lives under `native/<app>/<compat>/<sha256>`, written by the app's own tenant, so no app can plant code
for another.

**Locally:**
- compose runs `route` in front of `serve` over HTTP; `route` sets the tenant id itself.
- MinIO provides STS through a `router` user.
- serve has no store credentials of its own.
- `tric dev` stays a single process.

**Costs:**
- A request body is capped at about 4.4 MB on AWS.
- Every request in flight holds two environments.
- An idle app's first request is a full cold start.
- At most 2,500 warm tenants per 1,000 of concurrency.

## Acceptance criteria

The local gate (`docker compose run --rm test`) proves:

1. **Cross-app denial on MinIO.** With app A's credentials:
   - B's `current`, B's names and `native/B/…` are refused;
   - so are writes to A's own `current`, releases and components;
   - A's names, values and `native/A/…` work.
2. **Stripping.** A client sends `Forwarded: for=_cron`, `X-Forwarded-For`, `X-Tric-Credentials` and `X-Amz-Tenant-Id`
   through `route`. The app sees only the router's `Forwarded`, and no `X-Tric-*` or `X-Amz*` header.
3. **Pinning.** serve refuses a tenant id that differs from the Host's app, and a request without credentials.
4. **Unknown apps.** `route` answers 404 without calling serve.
5. **Forged outbox events.** An event naming app B is dropped unless its commit and digest are pending in B's head.
6. **Cron.** It reaches the app through `route` as a POST with `for=_cron`.
7. **Native code.** It is written under `native/<app>/…` with the app's credentials.
8. **The e2e suite** passes through `route` → `serve`.

`tofu validate` passes, and the module shows:
- serve is per tenant, with no URL;
- serve's role has no S3 access;
- the app role trusts only the router;
- deploy writes only `apps/` and the schedules;
- the outbox's async config and failure destination are on the router;
- lifecycle expires the dead letters.

The remote test, once you say go to `tofu apply`, checks:
- an app answers through CloudFront, and streams;
- two apps run in different tenants, and neither one's credentials can read the other's data;
- the outbox and cron work;
- a direct call to the router can't forge `_cron`, `_tric` or credentials;
- `for=` is the viewer's address.

## Order of work

1. **serve's side:**
   - per-app prefixes;
   - 403 read as not found;
   - the credentials header;
   - tenant pinning;
   - the digest in `pending`;
   - outbound defaulting to none.

   Delete what the router replaces.
2. **`tric route`** with the HTTP backend; compose with MinIO STS; criteria 1–8.
3. **The Lambda backend:** `InvokeWithResponseStream` and the event stream; cron and the outbox through the router.
4. **OpenTofu** for the router, serve, the app role and the lifecycle; `tofu validate`.
5. **Docs:** `decisions.md` and the READMEs.
6. **Merge `feat/ws` and `feat/js`;** route WebSockets through the router.
7. **Ask before `tofu apply`.**

**Not in this build:** budgets, teams, retrying outbox requests on 429 through Scheduler, `tric logs`, and a dead-letter
filter in `tric`.
