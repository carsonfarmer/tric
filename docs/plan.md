# Plan: one app per Lambda tenant, and what is left before a remote test

m6 shipped one Lambda function, with one role that reads the whole bucket, for every app. One Wasmtime or Cranelift
escape in any app would therefore reach every app's state, environment and native code. That breaks the design we
agreed, which has two isolation layers. This plan puts them back. It cites the redesign messages by their time, all
from 2026-10-07 and 2026-10-08. The approved 05:23 message wins wherever messages differ.

## Security first

- **What an escape reaches after this plan.** Only the app's own Lambda tenant, which is separate microVMs. It also
  reaches the credentials the router gave that tenant, which cover only that app's prefixes. serve's own role reads no
  part of the bucket.
- **The router is the trusted core.** It can mint credentials for any app, and it runs no guest code. What it does
  expose: Host parsing, one S3 HEAD, STS and Lambda calls, and the parsing of serve's response stream.
- **Still shared, by AWS's design or ours:**
  - serve's execution role, though all it holds is its logs and the router's `outbox` alias;
  - the account's concurrency;
  - the log group;
  - the router itself;
  - cookies across `*.<domain>`, until the domain is on the Public Suffix List.
- **The router's URL stays public, as m6's is.** A caller who skips CloudFront reaches only what CloudFront would route
  them to, and the router strips and sets every header that carries trust. CloudFront's origin access control for
  Lambda would make every client send a SHA-256 of each POST or PUT body, and browsers don't.
- **Self-reported numbers.** The function reports its own storage spend (04:27), so code that escapes could
  under-report that, but not the router's own timing.
- **Background requests sit in Lambda's queue with their headers,** and so do dead letters. Only the platform can read
  either (05:23).

## What was agreed, and where each part stands

Each item names the message that agreed it. "This step" means before the remote test.

**This step**
- Two isolation layers: the Wasm sandbox inside a Lambda tenant per app, with credentials that reach only that app
  (21:59, 22:56).
- A trusted router that finds the app's release and gives it credentials for its own data only (21:59; kept at 22:56).
  It stays because a tenant-isolated function can't have a function URL (04:27).
- The router sets `Forwarded` on every public request, replacing the client's, and strips the internal element, with
  its own gate test (23:54, 05:23, 06:09). serve does this in m6; it moves to the router.
- Background requests as an asynchronous invoke with tenant id = the app, in the event format the router already sends
  (05:23). m6 invokes without a tenant.
- The router times each request end to end (04:27); logged for now.
- Failures come from the `ErrorCode` in the stream's completion event (04:27).
- Only serve compiles native code, in the app's own VM, under a prefix the team can't write: `native/<app>/…` (agreed
  before the redesign, and consistent with it).
- Secrets stay out of the router's code: it only ever HEADs `current` (agreed before the redesign).

**Built, with a later part**
- Dead letters under one platform-only prefix (05:23): built. A `tric` command that filters them by app comes later.
- Logs to CloudWatch with short retention (04:27): built. `tric logs` comes later.

**In other worktrees**
- WebSockets through Pushpin's protocol, as cargo feature `ws`, and on AWS through API Gateway, with the router mapping
  connect, message and disconnect to requests (23:54, 06:09). An agent is building it on `feat/ws`; I wire it into the
  router after this step.
- JS apps, experimental tools allowed (04:27; your 16:02 message). An agent is building it on `feat/js`.

**Agreed, not built, and to confirm (below)**
- Budgets per app and per team, checked by the router (21:59, 22:56), with the function adding storage spend to the
  hourly tally (04:27).
- `https://<team>-<app>.<domain>` (21:59).

### To confirm

None of these blocks isolation. I'll keep building, and these wait for your answer.

1. **Where budget counters live.** The DynamoDB we dropped held state (23:54). Budgets kept their router check (22:56),
   but their counter was also in DynamoDB. The options are DynamoDB for counters only, or an hourly tally object in S3.
   Your earlier answers also predate the redesign: a fixed hourly allowance, budgets in money, and per-team plus per-app
   budgets.
2. **Teams.** These are not built: what a team is, who may deploy to whose apps, and the `<team>-<app>` naming. Until
   then, anyone with the deploy policy can deploy any app, as in m6.
3. **Retrying outbox requests that answer 5xx or 429.** 06:09 said one-time EventBridge Scheduler entries, honouring
   `Retry-After`. m6 retries in-process instead: three rounds over about three minutes, then `failed/`. I'll keep m6's
   rounds for the remote test.
4. **"The plain `lambda_http` stream" (04:27).** I read this as the stream format, which the Lambda Web Adapter also
   emits, so serve keeps the adapter. The alternative is linking `lambda_http` into tric.

## The design for this step

**The path of a request.** CloudFront goes to the router's function URL, in RESPONSE_STREAM mode with the Lambda Web
Adapter. The router calls `InvokeWithResponseStream` on serve with `X-Amz-Tenant-Id: <app>`.

**serve, on AWS:**
- tenancy is `PER_TENANT`;
- it has no URL;
- only the router's role may invoke it;
- its role has its logs and `lambda:InvokeFunction` on the router's `outbox` alias, and nothing else.

**What the router does with a public request:**
1. **It parses the Host strictly.** It takes the app from the first label of `X-Forwarded-Host`, which CloudFront sets.
   Locally it takes the app from `Host`.
2. **It HEADs `apps/<app>/current`** and caches the answer for a few seconds, negative answers included. An unknown app
   gets 404 before any tenant environment exists.
3. **It mints credentials.** It calls STS `AssumeRole` on the app role, with session name `app-<app>` and this
   session policy:
   - read `apps/<app>/*` and `native/<app>/*`;
   - write and delete `apps/<app>/{names,values,failed}/*` and `native/<app>/*`;
   - nothing else: not `current`, releases or components, and nothing outside the app.

   It caches them per app until five minutes before they expire. Role chaining caps them at one hour.
4. **It rewrites the headers:**
   - it removes every `Forwarded`, `X-Forwarded-*`, `X-Amz*` and `X-Tric-*` header the client sent;
   - it sets `Forwarded: for=<client>;host=<host>;proto=<proto>`;
   - it passes the credentials in one `X-Tric-Credentials` header, which serve removes before the app sees anything.
5. **It invokes serve, and streams the response back.** It parses the event stream: the `PayloadChunk` events carry the
   adapter's JSON prelude, eight NUL bytes and then the body; the `InvokeComplete` event carries the `ErrorCode`. Each
   request is logged as one line with the app, the status and the milliseconds.

**serve, behind the router:**
- It refuses a request whose tenant id (`x-amz-tenant-id`, which the adapter sets from the invoke) isn't the Host's app.
- It uses only the credentials that came with the request.
- It takes `Forwarded` as the router set it.
- A 403 on a read under the app's own prefix counts as not found, since without `ListBucket` S3 answers 403 for a
  missing key. A broken policy still shows, because the first read is `current`, so the app then answers 404 instead of
  running with empty state.

**Background requests.**
- At commit, serve invokes the router's `outbox` alias asynchronously, with the app's event.
- The router mints that app's credentials, then invokes serve with tenant = the app.
- A compromised tenant can send an event that names another app, so each pending entry in a head also records the
  SHA-256 of its event. Delivery drops an event whose commit or digest isn't pending. A forged event can then only
  repeat a delivery the other app really committed, and delivery is at least once anyway.
- `failed/<commit>` moves to `apps/<app>/failed/<commit>`.

**Cron.** EventBridge Scheduler invokes the router's unqualified ARN with `{"cron":{"app","path"}}`. The router mints
the app's credentials and sends serve `POST <path>` with `Forwarded: for=_cron`. The router tells the three kinds of
invocation apart like this:
- a URL request is an HTTP event, which the adapter marks;
- the `outbox` qualifier is in the `invoked_function_arn` the adapter passes;
- the outbox and cron each accept only their own event.

serve's role can invoke only `router:outbox`, so serve can't forge a cron event.

**Native code** moves to `native/<app>/<compat>/<sha256>`. The shared `native/<compat>/…` let one app plant code for
another. The deploy policy still writes only `apps/`.

**Locally (compose):**
- `route` sits in front of `serve` over HTTP, and sets `x-amz-tenant-id` itself, since there is no adapter.
- MinIO provides STS, through a non-root `router` user whose policy is the app role's envelope.
- serve gets no store credentials of its own, so every S3 call it makes has to use the router's.
- `AWS_ENDPOINT_URL_STS` and `AWS_ENDPOINT_URL_LAMBDA` point the router at MinIO and at serve's emulator.
- `tric dev` stays one process and is its own front.
- Standalone multi-app `tric serve` with its own credentials goes. `serve` now always sits behind `route`, so there is
  one rule for serve.

**Costs:**
- A request body is capped at about 4.4 MB on AWS: Invoke takes 6 MB, and the body is base64 inside it.
- Every request in flight holds two Lambda environments: the router waiting on serve.
- An app's first request after it goes idle is a full cold start.
- At most 2,500 warm tenant environments per 1,000 of concurrency.

## Acceptance criteria

The gate (`docker compose run --rm test`) proves these locally:

1. **Cross-app denial against MinIO.** With the credentials the router mints for app A:
   - reading B's `current`, writing B's names, or writing `native/B/…` gets 403;
   - writing A's own `current`, releases or components gets 403;
   - A's names, values, `failed/` and `native/A/…` work.
2. **Stripping `Forwarded`** (the gate test 05:23 asked for):
   - A client sends `Forwarded: for=_cron`, `X-Forwarded-For`, `X-Tric-Credentials` and `X-Amz-Tenant-Id: other`
     through `route`.
   - The app sees only the router's `Forwarded`, with the client's address.
   - The app sees no `X-Tric-*` or `X-Amz*` header.
3. **Tenant pinning.** A request straight to serve whose tenant id differs from its Host app is refused, and so is one
   without credentials.
4. **Unknown apps.** An unknown app gets 404 from `route`, and serve is never called.
5. **Forged outbox events.** An outbox event naming app B is dropped unless its commit and digest are pending in B's
   head. A real one is delivered once per commit and digest.
6. **Cron through the router.** It reaches the app as a POST with `for=_cron`.
7. **Native code.** It is written under `native/<app>/…` with the app's own credentials.
8. **The existing e2e suite.** It passes through `route` → `serve` against MinIO, and in memory.

`tofu validate` passes, and the module states each of these:

- serve has `PER_TENANT` tenancy and no URL;
- serve's role has no S3 actions;
- the app role trusts only the router's role;
- the deploy policy writes only `apps/` and the group's schedules;
- the async config and the failure destination are on the router;
- the bucket's lifecycle expires `aws/lambda/async/` and `apps/*/failed/` after 30 days.

The remote test, after you say go to `tofu apply`, checks these:

- an app answers through CloudFront, and streams;
- two apps' tenants differ, and each tenant's credentials can't read the other app's data;
- the outbox and cron work through the router;
- a direct call to the router's URL can't forge `_cron`, `_tric` or credentials;
- the `for=` address is the viewer's.

## Order of work

1. **Finish m6.** Fix the review findings and the slop list on `m6/trim`, and add `--allow` (outbound defaults to none)
   to `dev` and `deploy`. Pass the gate, merge into `main` locally, and rebase this branch.
2. **serve's side.** Per-app prefixes (`native/<app>`, `apps/<app>/failed`), 403 as not found, the credentials header,
   tenant pinning, and the digest in `pending`.
3. **`tric route` with an HTTP backend.** STS minting, header stripping, the HEAD cache and timing. Then compose
   `route`, MinIO's STS user, and criteria 1–8.
4. **The Lambda backend.** `InvokeWithResponseStream`, the event-stream parser, the qualifiers, and cron and the outbox
   through the router.
5. **OpenTofu.** The router, serve, the app role, the policies and the lifecycle; then `tofu validate`.
6. **Docs.** `decisions.md`, the README and the infra README.
7. **Merge `feat/ws` and `feat/js`.** Wire WebSockets into the router.
8. **Ask you before `tofu apply`** for the remote test.

Sources:
- [Lambda tenant isolation](https://docs.aws.amazon.com/lambda/latest/dg/tenant-isolation.html)
- [Invoking with a tenant id](https://docs.aws.amazon.com/lambda/latest/dg/tenant-isolation-invoke.html)
- [InvokeWithResponseStream](https://docs.aws.amazon.com/lambda/latest/api/API_InvokeWithResponseStream.html)
- [CloudFront origin access control for Lambda function URLs](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-restricting-access-to-lambda.html)
- [STS AssumeRole session policies](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRole.html)
