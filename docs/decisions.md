# Decisions

These are the choices made while building [the plan](plan.md) that the plan doesn't spell out. Each one gives its
reason.

## Isolation

- **The router is two functions on AWS,** built from one package and running as one role. `tric-route` holds the
  function URL. `tric-events` has none, and takes only events, each source's by an alias that only it may invoke:
  - `outbox`, delivery events, from serve;
  - `cron`, cron jobs, from Scheduler, for an app with a release only;
  - `ws`, sockets' events, from API Gateway.

  An invocation of no alias, or of another, gets 403. Both have the origin secret: `events` checks it when a socket
  opens, and an event shaped as a function URL's request needs it too. A single function would take both kinds of
  input, and only a header would keep them apart.
- **The origin is guarded by a shared secret, not by CloudFront's origin access control.** With OAC on a function
  URL, clients would have to send `x-amz-content-sha256` with every body. The router compares the secret's SHA-256
  digests, so a closer guess takes no longer to check, and an empty secret counts as none. A request that goes around
  CloudFront is refused, but still costs an invocation.
- **The viewer's address comes from `CloudFront-Viewer-Address`,** which the `AllViewerExceptHostHeader` policy
  forwards. The app's host comes from `X-Forwarded-Host`, which a CloudFront function sets from `Host`, replacing
  any the viewer sent. A request without either is refused.
- **The tenant id travels as `X-Amz-Tenant-Id`.** The router sends the app's name both as Lambda's tenant id and as
  that header in the event. serve answers 403 when the header differs from the Host's app, and when credentials are
  missing. It checks what the router wrote, so it guards against the router's mistakes; Lambda's tenancy is what keeps
  apps apart.
- **The STS session name is the app.** STS allows 2 to 64 characters, which leaves no room for a prefix on top of a
  63-character label, so an app's name is 2 characters at least.
- **Credentials last an hour,** the most for a role assumed by a role. The router uses them for 45 minutes after it
  mints them.
- **An escape from serve's sandbox can invoke the `outbox`,** as serve's role may, with events that name another app.
  They are delivered only if that app's head holds their digest, so they are dropped, but each may hold the other
  app's tenant for up to 30 s first, waiting for a commit to land. That costs money, and breaks nothing.
- **Native code is the app's own.** `native/<app>/…` is written with the app's credentials, so tampered native code
  reaches only the tenant that wrote it. serve compiles again when the native code won't deserialize. Components are
  checked against their digest when they are loaded. `<compat>` is a SHA-256 of Wasmtime's own compatibility hash
  (its version, configuration and target), as Wasmtime's cache keys native code: Rust's `DefaultHasher` may change
  from one release to the next.
- **An outbound request goes to a public address only.** Besides the private and special IPv4 ranges, that rules out
  IPv6 outside `2000::/3`, and the special-purpose blocks inside it, in IANA's registry: `2002::/16` (6to4, which
  embeds an IPv4 address), `2001::/23` (Teredo's `2001::/32` is in it), `2001:db8::/32` and `3fff::/20`. An
  IPv4-mapped IPv6 address is judged as its IPv4 address.
- **The domain is exact,** port included. `<app>.localhost:3000` is an app; `<app>.localhost` is not. An unknown app
  gets 404, and serve is never called for it.
- **Lambda's own permissions.** With `authorization_type = NONE`, the AWS provider grants `lambda:InvokeFunctionUrl`
  to anyone. It grants `lambda:InvokeFunction` only with `lambda:InvokedViaFunctionUrl`, so the router can't be
  invoked directly.

## AWS

- **The dead letters are under `aws/lambda/async/`.** That is the prefix Lambda's S3 failure destination always
  writes to. Lambda takes the destination only if the router's role may write the whole bucket and list it, so it
  may. That adds little: the router writes `apps/` and `native/` already, as any app, and a component is checked
  against its digest when it is loaded. The router is trusted either way.
- **serve and the router run behind the Lambda Web Adapter,** as plain HTTP servers. Their package carries its own
  `bootstrap`, which runs `tric <handler>`, because `provided.al2023` has no wrapper for that. The adapter's
  readiness check is a `GET /`. Any status below 500 counts as ready, including the router's 403.
- **The package is arm64 only,** and `docker compose run --rm package` refuses to build it anywhere else. Graviton
  costs less, and no cross-compiling means no toolchain to keep. The toolchain image is Amazon Linux 2023, the
  runtime's own OS, so the binary links the glibc that Lambda has.
- **The router invokes serve with `InvokeWithResponseStream`.** It sends a function-URL event (payload format 2.0)
  carrying the tenant id. The answer is a JSON prelude, then 8 NULs, then the body. Failures come from
  `InvokeComplete`'s error code.
- **A header value that isn't UTF-8 is passed on lossily,** with U+FFFD for the bad bytes: a function-URL event is
  JSON, which holds text only. Refusing the request would fail a client over a header the app may never read.
- **The event stream's CRCs go unchecked,** and its headers are read as strings only, the one type Lambda sends: TLS
  already refuses a corrupted stream, and a header of any other type fails the answer.
- **The router's concurrency is capped,** at `route_concurrency`, 200 by default, reserved from the account's 1,000.
  Each router calls one serve, so a flood of client requests holds at most about 400 of the 1,000, and the rest stays
  for cron and the outbox; requests past 200 get 429. Reserved concurrency is free and keeps nothing warm, so scale
  to zero is untouched. AWS keeps at least 100 unreserved, so an account still at a new account's limit of 10 sets
  `-1`. serve and `events` reserve none: they share what is left.
- **A delivery that fails answers 503,** which `AWS_LWA_ERROR_STATUS_CODES` turns into a failed invocation, so
  Lambda retries it.
- **Schedules are named for what they are,** as `<hash(app)[..24]>-<hash(body)[..39]>`. A changed schedule is a new
  one: deploy creates the missing schedules first, then deletes the extras. They are in one group, which the install
  makes.
- **An app has 50 cron jobs at most,** so deploy reads its schedules in one page of 100, a failed deploy's extras
  included, and needs no paging: Scheduler applies the name prefix before it pages, as a probe of a group of 104 showed.
  More than 100 fails the deploy, which says to delete them by hand.
- **A cron expression can't restrict both the day of the month and the day of the week.** POSIX cron runs a job
  when either one matches, and Scheduler can't say that, so tric refuses it everywhere.

## Storage and turns

- **A re-run claims its name when it opens.** A turn that runs past 1 s claims its name mid-run. A turn whose claim
  fails is doomed: on a conflict it re-runs, and on a busy name it answers 429.
- **Clearing `pending` never writes over a claimed head,** because that would break the claim. The 6-hour age limit
  clears what is left.
- **A delivery event is dropped at once** when its turn's start version has moved on and its commit id isn't in
  `pending`.
- **`deploy` writes with the owner's credentials,** from the environment, not with an app's.
- **A name over 1 MiB fails at its commit,** not at the write that took it over: the request answers 500, as for a
  trap, and its held requests are not sent. 413 would blame the client's request, and this is the app's doing.
- **A snapshot is a turn that has answered.** Both read the name as it was, and neither writes, so one type serves
  both. A write after the answer now fails with `access-denied`, the same as a write to a snapshot, where it used to
  fail with `other`. `wasi:keyvalue` names `access-denied` for exactly this.

## Local

- **The local store is RustFS,** pinned by digest. MinIO's community edition is archived, and its images are gone.
  It refuses a session token that is missing or tampered with, though with 500, not 403.
- **The local store keeps a lifecycle rule,** as the bucket on AWS does. `tric dev` keeps its state in memory, so it
  leaves nothing behind. The e2e suite's deploys and deletes on RustFS do, as noncurrent versions, which the rule
  expires after a day. RustFS has no volume, so recreating its container clears it too.
- **The local `router` user on RustFS is broader than the router's role on AWS.**
  - RustFS's `AssumeRole` takes no role: it narrows the caller's own policy, so the user holds what the app role holds
    on AWS.
  - It may also list the bucket, because the local router ticks cron itself.
- **The local router answers 202 to an outbox event,** then relays it with backoff, honouring `Retry-After`, as
  Lambda's async invoke would.
- **serve takes the scheme from the router's `Forwarded`.** `proto=https` gives `https`, and anything else gives
  `http`. The router sets `https` on AWS and `http` locally.
- **A middleware `url`** is fetched when it is `http(s)`. Anything else is a path, relative to `tric.toml`'s
  directory. Its `digest` is always required, so neither a redirect nor the host can change what runs.
- **A middleware fetch goes as an app's request to any host would:** to a public address only, with no user name in
  the URL. It follows redirects as the Fetch standard does (301, 302, 303, 307 and 308, and at most 20), checking
  each hop again, and takes only a 2xx. So `tric dev` on someone else's project makes no request to your network.
- **serve loads an app under that app's own lock,** so one app's slow compile holds up no other's. On AWS a serve
  runs one app; locally one serve runs them all.

## JavaScript

- **A JavaScript app is built with componentize-qjs 0.4.5, patched.** It is the only toolchain found that makes
  WASI 0.3 components: StarlingMonkey and ComponentizeJS make 0.2 ones. As published it can't build a world that
  imports an async function, so `docker/build.Dockerfile` builds it from crates.io, checked by sha256, with a lockfile
  and `docker/qjs/componentize-qjs.patch`: Wasmtime 49.0.2 for its build-time run, which traps the imports it isn't
  given.
- **There is no shim for `fetch`, `Request` or `URL`.** They are not WASI, and tric invents nothing.
- **The JavaScript fixture skips `/print` and `/fs`,** which try tric's own stdio and files: QuickJS has neither.
- **A JavaScript component also imports the WASI 0.2 interfaces its runtime stands on,** as a Rust one does, and
  exports an `init` that tric never calls.

## WebSockets

An app speaks Pushpin's WebSocket-over-HTTP, and a little of GRIP, so it runs the same where `tric dev` holds the
sockets and where API Gateway WebSocket does, on AWS. A probe on AWS, behind CloudFront, settled the AWS side.

- **Behind the cargo feature `ws`.** Off, nothing changes and nothing more is compiled. On, `tric dev` adds
  tokio-tungstenite, for the handshake's key and the framing, and hyper-util, for the upgrade; hyper-util is linked
  already, by reqwest. The AWS package is built with it.
- **The same URL.** CloudFront hides `Upgrade` from its functions, so the viewer-request function takes a request with
  `Sec-WebSocket-Key` as an upgrade, sends it to API Gateway with `updateRequestOrigin`, at `/ws`, and puts the path
  in `X-Forwarded-Path`. A separate host for sockets would be one more name to know.
- **The origin secret is checked at `$connect`.** API Gateway WebSocket has no resource policy and no WAF of its own,
  so the secret is the gate, as it is for the function URL. `events` holds the secret for that alone, and still has
  no URL.
- **API Gateway invokes `events` through its `ws` alias,** buffered. The adapter passes a WebSocket event to the
  router whole, and reads `{statusCode, headers}` back, which a streaming function can't give. Routes are selected by
  `$request.body.action`, API Gateway's usual expression, which means nothing here: every route has the one
  integration, and the router goes by the event's `eventType`.
- **The route response is off.** The function's answer is then dropped, so every message to a client goes one way,
  through `@connections`. Its URL, the stage's, is `TRIC_WS`, which the install sets on `events`: a publish comes
  from a delivery event, which has no socket's event to build the URL from.
- **The router sends serve each event,** as it does a delivery event: a `POST` with `Forwarded: for=_ws` and the
  app's tenant credentials, of JSON `[record, id, event]`. serve takes it only for the Host's app, and runs the
  app's turn of it. `tric dev` runs the same code, but holds the sockets and runs the turn itself.
- **A socket's record and subscriptions are in the bucket,** under `ws/`, which only the router reads or writes.
  There is no DynamoDB.
  - The record is at `ws/connections/<id>`, by the id alone, as a message's event carries nothing more. It holds the
    app, the URL and the handshake headers, which are replayed on every event.
  - A subscription is an empty object at `ws/channels/<app>/<channel>/<id>`, and a publish lists the channel.
  - Ids are base64url in keys, as API Gateway's may hold `/` and `=`.
  - A `ws/` lifecycle rule expires both after a day, past the 2 hours a connection lasts at most; the local bucket's
    lifecycle has it too. An end deletes the record. A subscription goes when a publish finds its socket gone, or
    when it expires.
  - The router's role may get and delete `ws/*`; it could already put and list.
  - `tric dev` keeps them in memory, in a store of their own where a delete deletes, as no snapshot reads them.
- **Opening.** The app accepts with `200` of events, `OPEN` first, then only subscriptions. An answer with a message
  as well is refused with 502: `$connect` can't send to its own socket before the handshake completes, so the message
  would be lost there and only there. Any other answer refuses the socket with the app's own 4xx or 5xx, else 502,
  and no body. The subscriptions are written at once, and then the record, so an event finds a socket only once it is
  whole. If a write fails, the app is told `DISCONNECT`, and the client gets 503.
- **The query is rebuilt on AWS.** API Gateway gives `$connect` the query as a map only, so the router writes it out
  again, sorted by name and percent-encoded, and `X-Forwarded-Path` is the path alone. An app that reads the query
  as pairs sees the same in both.
- **The client's address comes from `CloudFront-Viewer-Address`,** as on HTTP, and never from `X-Forwarded-For`:
  CloudFront appends to what a client sends there, so its first element can be forged.
- **An event is a turn.** A `5xx`, a busy name's `429`, an answer that is not well-formed events, or one that is late
  (the engine's 10 s) ends the socket. The app is told `CLOSE`, with the client's code if it gave one, of a socket
  the client closed, and `DISCONNECT` of one that ended any other way but its own `CLOSE`.
- **A socket's messages run concurrently, and arrive unordered,** as API Gateway invokes them; `tric dev` runs 16 of
  one socket's at once. An app that needs order puts a sequence number in its messages. Ordering them would mean a
  lock or a queue tric would have to invent. `DISCONNECT` can arrive while earlier messages still run: it deletes the
  record, and an event after it finds no socket and is dropped, with a log line.
- **GRIP is a channel and a publish, and no more.** A channel is a name, of the app. A publish is a `POST` to
  `/publish/` at the app's own origin with `Prefer: respond-async`: held, as every background request is, and taken
  after the turn's other held requests, at most once, if the turn commits. Without that header, it is a call to the
  app like any other. Messages are text: `content-bin`, other formats and `action` drop the publish, with a log line.
- **The subprotocol is echoed.** The app's `OPEN` answer sets `Sec-WebSocket-Protocol`, and tric copies it to the
  handshake's answer; a browser fails the handshake without it.
- **Every handshake header is sent on every event,** less the upgrade's own but for the subprotocols offered, as
  Pushpin does. `Authorization` is one, so the app can check it on every event. A client can vary only its own
  socket's headers.
- **Closing.** A close from the server, by the app's `CLOSE` or an answer that ends the socket, reaches the client
  as 1000, as API Gateway's does. A client's binary frame closes its socket with 1003, and one too large with 1009.
- **A socket that falls behind misses messages.** API Gateway gives no sign of it, so neither does `tric dev`: a
  socket 64 messages behind loses what comes next, with a log line.
- **Keep-alive is the app's.** API Gateway closes a connection idle for 10 minutes, and traffic either way resets
  that, so an app message from either side at least every 9 minutes keeps it open. A connection lasts 2 hours at
  most. `tric dev` keeps no timers.
- **Threat model.** What only tric says to the app is never believed from anyone else, and nothing a client sends is
  passed on as it is:
  - `forward`, which every request passes, removes a client's `Connection-Id`, `Grip-` and `Meta-` headers, in either
    spelling of their dash, and a `Content-Type` that mentions `websocket-events`. A plain request cannot be a
    socket's event, or GRIP's, and `Forwarded` is still the only marker.
  - A socket's id speaks for no one: tric writes each event, from its own socket's frames or from API Gateway's
    events. `tric dev`'s ids are random.
  - `for=_ws` reaches serve only from the router, which replaces any `Forwarded` a client sends. Only API Gateway may
    invoke `events:ws`, from its own stage, and an opening needs the origin secret, which `forward` removes, with
    every `X-Tric-` header, before the app sees the headers.
  - The app's answers are parsed whole and written out again: capitals, `CRLF`, a length of 1 to 8 hex digits, and
    nothing else, or the socket ends. Control messages are `subscribe` and `unsubscribe` of a name, with no field more.
  - Only the app subscribes a socket, so a socket's channels have no cap but an answer's: each is an empty object.
  - Bounds: API Gateway's 32 KiB a frame and 128 KiB a message, from either side, which `tric dev` takes as its
    WebSocket library's settings; 2 MiB and 64 events an answer, or items a publish; in `tric dev`, 256 sockets, then
    503, and 30 s for a client to take a write. On AWS, a stage throttle, variables of 100 requests a second and a
    burst of 200, bounds the cost of a flood, as messages cost $1 a million.
- **Not here:** API Gateway's logs, which need an account-wide role and cost money; and an emulator of API Gateway,
  as none matches it closely enough to be worth a container. The tests feed the router API Gateway's events, against
  a stand-in for `@connections`.
- **The size.** The budget was all the WebSocket code, `tric dev`'s and the router's, in what `src/ws.rs` was before
  it (494 lines). `src/ws.rs` is 494 lines, and its hooks elsewhere add 63, most of them the outbox handing back what
  a delivery published, so the budget is missed by 63. No feature was removed.
