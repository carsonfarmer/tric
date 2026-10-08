# Decisions

These are the choices made while building [the plan](plan.md) that the plan doesn't spell out. Each one gives its
reason.

## Isolation

- **The router is two functions on AWS,** built from one package and running as one role. `tric-route` holds the
  function URL, and has the origin secret. `tric-events` has no secret and so takes only events:
  - invoked as `outbox`, it takes delivery events;
  - invoked unqualified, it takes cron.

  A single function would take both kinds of input, and only a header would keep them apart.
- **The origin is guarded by a shared secret, not by CloudFront's origin access control.** With OAC on a function
  URL, clients would have to send `x-amz-content-sha256` with every body. The router compares the secret's SHA-256
  digests, so a closer guess takes no longer to check.
- **The viewer's address comes from `CloudFront-Viewer-Address`,** which the `AllViewerExceptHostHeader` policy
  forwards. The app's host comes from `X-Forwarded-Host`, which a CloudFront function sets from `Host`, replacing
  any the viewer sent. A request without either is refused.
- **The tenant id travels as `X-Amz-Tenant-Id`.** serve answers 403 when it differs from the Host's app, and when
  credentials are missing.
- **The STS session name is the app.** STS allows 64 characters, which leaves no room for a prefix on top of a
  63-character label.
- **Credentials last an hour,** the most for a role assumed by a role. The router uses them for 45 minutes after it
  mints them.
- **Native code is the app's own.** `native/<app>/…` is written with the app's credentials, so tampered native code
  reaches only the tenant that wrote it. serve compiles again when the native code won't deserialize. Components are
  checked against their digest when they are loaded.
- **The domain is exact,** port included. `<app>.localhost:3000` is an app; `<app>.localhost` is not. An unknown app
  gets 404, and serve is never called for it.
- **Lambda's own permissions.** With `authorization_type = NONE`, the AWS provider grants `lambda:InvokeFunctionUrl`
  to anyone. It grants `lambda:InvokeFunction` only with `lambda:InvokedViaFunctionUrl`, so the router can't be
  invoked directly.

## AWS

- **The dead letters are under `aws/lambda/async/`.** That is the prefix Lambda's S3 failure destination always
  writes to. The router's role may write there and list the bucket, which is what Lambda needs. The router is trusted
  either way.
- **serve and the router run behind the Lambda Web Adapter,** as plain HTTP servers. Their package carries its own
  `bootstrap`, which runs `tric <handler>`, because `provided.al2023` has no wrapper for that. The adapter's
  readiness check is a `GET /`. Any status below 500 counts as ready, including the router's 403.
- **The package is arm64 only,** and `docker compose run --rm package` refuses to build it anywhere else. Graviton
  costs less, and no cross-compiling means no toolchain to keep. The toolchain image is Amazon Linux 2023, the
  runtime's own OS, so the binary links the glibc that Lambda has.
- **The router invokes serve with `InvokeWithResponseStream`.** It sends a function-URL event (payload format 2.0)
  carrying the tenant id. The answer is a JSON prelude, then 8 NULs, then the body. Failures come from
  `InvokeComplete`'s error code.
- **A delivery that fails answers 503,** which `AWS_LWA_ERROR_STATUS_CODES` turns into a failed invocation, so
  Lambda retries it.
- **Schedules are named for what they are,** as `<hash(app)[..24]>-<hash(body)[..39]>`. A changed schedule is a new
  one: deploy creates the missing schedules first, then deletes the extras. They are in one group, which the install
  makes.
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

## Local

- **The local `router` user on MinIO is broader than the router's role on AWS.**
  - MinIO's `AssumeRole` narrows the caller's own policy, so the user holds what the app role holds on AWS.
  - It may also list the bucket, because the local router ticks cron itself. MinIO lists prefixes that hold only
    delete markers, so cron skips an app whose `current` is gone.
- **The local router answers 202 to an outbox event,** then relays it with backoff, honouring `Retry-After`, as
  Lambda's async invoke would.
- **serve takes the scheme from the router's `Forwarded`.** `proto=https` gives `https`, and anything else gives
  `http`. The router sets `https` on AWS and `http` locally.
- **A middleware `url`** is fetched when it is `http(s)`. Anything else is a path, relative to `tric.toml`'s
  directory. Its `digest` is always required.

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
