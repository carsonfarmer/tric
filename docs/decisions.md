# Decisions

These are the choices made while building [the plan](plan.md) that the plan doesn't spell out. Each one gives its
reason.

## Isolation

- **The router is two functions on AWS,** built from one package and running as one role. `tric-route` holds the
  function URL, and has the origin secret. `tric-events` has no secret and so takes only events:
  - invoked as `outbox`, it takes delivery events;
  - invoked unqualified, it takes cron, for an app with a release only.

  A single function would take both kinds of input, and only a header would keep them apart.
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

- **`tric dev` holds the sockets, behind the cargo feature `ws`.** Off, nothing changes and nothing more is compiled.
  On, it adds tokio-tungstenite, for the handshake's key and the framing, and hyper-util, for the upgrade; hyper-util
  is linked already, by reqwest. The app speaks Pushpin's WebSocket-over-HTTP, so a gateway that holds the sockets on
  AWS can take tric's place with the app unchanged.
- **An event is a turn.** A `5xx`, a busy name's `429`, an answer that is not well-formed events, or one that is late
  (the engine's 10 s) ends the socket with 1011. The app is told `DISCONNECT` of a socket that ends without a `CLOSE`.
  Any answer to `OPEN` that is not events is passed to the client, as Pushpin does.
- **GRIP is a channel and a publish, and no more.** A channel is a name, and any socket may be published to by any
  turn of the app: `tric dev` runs one app. A publish is taken in `dispatch`, so one sent with `Prefer: respond-async`
  waits for its turn's commit in the outbox, as every held request does, and a direct one claims the turn like any
  unsafe request. Messages are text: `content-bin`, other formats and `action` are refused with 400.
- **Every handshake header is sent on every event,** less the upgrade's own, as Pushpin does. `Authorization` is one,
  so the app can check it on every event. A client can vary only its own socket's headers.
- **Threat model.** What only tric says to the app is never believed from anyone else, and nothing a client sends is
  passed on as it is:
  - `forward`, which every request passes, removes a client's `Connection-Id`, `Grip-` and `Meta-` headers, in either
    spelling of their dash, and a `Content-Type` that mentions `websocket-events`. It does so with the feature or
    without it, so an app written for sockets is not fooled where tric holds none, as on AWS. A plain request cannot
    be an event, or a socket's, or GRIP's; the app answers it as any request, and `Forwarded` is still the only
    marker.
  - A socket's id is random, and an id alone speaks for no one: tric writes each event, from its own socket's frames.
  - A publish is taken only with `Forwarded: for=_tric`, which a client cannot send, as `forward` replaces its own.
    `tric serve` and `tric route` never take one.
  - The app's answers are parsed whole and written out again: capitals, `CRLF`, a length of 1 to 8 hex digits, and
    nothing else, or the socket ends. Control messages are `subscribe` and `unsubscribe` of a name, with no field more.
  - Bounds: 1 MiB a message, from either side (1009 to a client that sends more); 2 MiB and 64 events an answer or
    items a publish; 16 channels a socket; 256 sockets, then 503; 30 s for a client to take a write; a socket that
    falls 256 publishes behind is closed (1013).

## WebSockets on AWS: decided, not built

A probe on AWS (API Gateway WebSocket behind CloudFront) settled these. They are built on a branch of their own, and
`tric dev` changes with them, so that an app runs the same in both.
- **The same URL.** CloudFront hides `Upgrade` from its functions, so the viewer-request function takes a request with
  `Sec-WebSocket-Key` as an upgrade, sends it to API Gateway with `updateRequestOrigin`, at `/ws`, and puts the path
  in `X-Forwarded-Path`. A separate host for sockets would be one more name to know.
- **The origin secret is checked at `$connect`.** API Gateway WebSocket has no resource policy and no WAF of its own,
  so the secret is the gate, as it is for the function URL.
- **API Gateway invokes `events` through a `ws` alias,** buffered. The adapter passes a WebSocket event to the router
  whole, and reads `{statusCode, body}` back, which a streaming function can't give.
- **The route response is off.** The function's return is then dropped, so every message to a client goes one way:
  as a publish. The `@connections` endpoint is built from the event's domain name and stage, with no configuration.
- **A connection's record is in S3,** under `ws/<app>/…`, and only the router reads or writes it. There is no
  DynamoDB. It holds the handshake headers, which are replayed on every event, as `tric dev` does. A `ws/` lifecycle
  rule expires records after a day, past the 2-hour most a connection lasts.
- **The client's address comes from `CloudFront-Viewer-Address`,** as on HTTP, and never from `X-Forwarded-For`:
  CloudFront appends to what a client sends there, so its first element can be forged.
- **Publishes are held requests only,** everywhere. They are sent after the turn's other held requests, at most once.
- **A connection's messages run concurrently, and arrive unordered,** as API Gateway invokes them. An app that needs
  order puts a sequence number in its messages. Ordering them would mean a lock or a queue tric would have to invent.
- **`DISCONNECT` can arrive while earlier messages are still running.** It deletes the record, and a later publish to
  the connection gets 410 and is dropped, with a log line.
- **An answer to `OPEN` carries no messages.** On AWS, `$connect` can't send to its own connection before the
  handshake completes, so they would be lost there and only there. One that does fails with 500, in both.
- **The subprotocol is echoed.** The app's `OPEN` answer sets `Sec-WebSocket-Protocol`, and tric copies it to the
  handshake's answer; a browser fails the handshake without it.
- **Messages are text only.** A client's binary frame is closed with 1003. A message to a client that is not valid
  UTF-8 is refused, with a log line, where API Gateway would mangle it silently.
- **Sizes are API Gateway's:** 32 KB a frame and 128 KB a message, each closed with 1009. `tric dev` takes them as
  its WebSocket library's settings, with no code of its own.
- **Closing.** A close from the server reaches the client as 1000. `DISCONNECT` carries the client's own close code,
  where there is one.
- **Keep-alive is the app's.** API Gateway closes a connection idle for 10 minutes, and traffic either way resets
  that, so an app message from either side at least every 9 minutes keeps it open. A connection lasts 2 hours at
  most. `tric dev` keeps no timers.
- **Caps.** `tric dev` keeps its own. On AWS, a stage throttle, a variable of 100 requests a second with a burst of
  200, bounds the cost of a flood, as messages cost $1 a million.
- **Tests** feed the router API Gateway-shaped events, against a stand-in for `@connections`. No emulator matches
  API Gateway closely enough to be worth a container.
- **A budget.** All the WebSocket code, `tric dev`'s and the router's, fits in what `src/ws.rs` is today (494
  lines). No feature is removed now.
