# Decisions

These are the choices made while building [the plan](plan.md) that the plan doesn't spell out. Each one gives its
reason.

## Isolation

- **The router is two functions on AWS,** built from one package and running as one role. `tric-route` holds the
  function URL. `tric-events` has none, and takes only events, each source's by an alias that only it may invoke:
  - `outbox`, delivery events, from serve;
  - `retry`, a failed delivery's next try, from Scheduler, in a group of its own;
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
  app's tenant for up to 60 s first, waiting for a commit to land. That costs money, and breaks nothing. They leave
  nothing behind either: serve drops them with a 2xx, and only a delivery that fails is kept and scheduled.
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
- **Memory is capped per linear memory:** 256 MiB each, and 4 to a store. Wasmtime's `StoreLimits` has no total for a
  store, and one would still miss an app's calls to itself, each a store of its own, 16 deep, with 64 requests in
  flight. On AWS, what bounds an app is serve's 1 GiB: past it, Lambda ends that tenant's environment, and no other.

## AWS

- **A failed delivery is tried again for 24 hours, through Scheduler.** That is the window of EventBridge's default
  [retry policy](https://docs.aws.amazon.com/eventbridge/latest/userguide/eb-rule-retry-policy.html), 24 hours with
  exponential backoff and jitter, after which the event is dropped. Lambda's own queue holds an event 6 hours at most
  and retries it twice, at its own pace, which a `Retry-After` can't change. The router tries once, in the `outbox`
  invocation. If serve answers 5xx or 429, or the exchange fails, it keeps the event and creates a one-time schedule
  that invokes its `retry` alias; every failed try schedules the next. It never sleeps, because Lambda bills the
  wait, and nothing runs while nothing is due.
- **The wait is a minute, doubling to an hour, and up to a quarter more.** Scheduler keeps time to the minute, so a
  shorter wait would be no shorter, and the doubling makes 25 to 29 tries in 24 hours, as the jitter falls. The
  jitter keeps events that failed together from coming back together. `Retry-After` (RFC 9110, 10.2.3) is a floor, in
  seconds; its date form is not read. A wait that would end after the deadline ends the delivery. EventBridge's
  default also stops at 185 tries; here only the time stops it. Only a 5xx, a 429 and a failed exchange are tried
  again, and any other 4xx is an answer, which a retry would only repeat.
- **A failed event waits in the bucket, and the schedule carries a reference to it.** Scheduler's input is 256 KB at
  most, and an event may be 1 MB. The reference is the app, the commit, the number of tries and the deadline, which
  the router sets once, at the first failure, to 24 hours on.
  - The object is `outbox/<app>/<commit>`, written once by the router, and not again at each failure: in a versioned
    bucket every write would leave a noncurrent version of up to 1 MB. It is deleted when the event is delivered or
    lost. The lifecycle rule expires what is left after 2 days, the least that outlasts the window.
  - The key is made from a label and 32 hex digits, which the router checks again in a reference. A reference is
    only a way to name an object; it makes no other key.
  - A missing object reads as done: an earlier try of the same schedule delivered it. S3 answers 403 for a missing key
    when the caller can't list, and the router can't list `outbox/`, so a 403 reads as missing there too.
- **The schedule's name is `<hash(app/commit)[..32]>-<tries>`.** It matches Scheduler's `[0-9a-zA-Z-_.]+` in 64
  characters or fewer. It is the same each time it is made, so when Lambda runs an invocation again after it failed,
  having created the schedule, Scheduler's 409 says it is made, and that is taken as done. It names the app, so one
  app's can't be another's. A schedule deletes itself once it has run (`ActionAfterCompletion: DELETE`), because a
  completed one counts against the quota until it does.
- **The retries have a Scheduler group and a role of their own.** The retry role may invoke `events:retry` only, and
  the cron role `events:cron` only. The router may create a schedule in the retry group only, and pass only the retry
  role to Scheduler, so it can't make a schedule that runs as `cron`. serve may invoke `outbox` only, so it can make
  none. Each role's trust names the account and its own group's ARN, as Scheduler's documentation asks, so no other
  schedule can assume it. The module makes the two alike, a group and a role for each alias, named `<name>-cron` and
  `<name>-retry`. The router's two functions share its role, so `route` holds these grants as well; it takes no event,
  and the grants are narrow.
- **A delivery event's record is kept two to three days.** serve sends an event's requests only if the name's head holds
  the record its commit made of it, so a record must outlast the last try: Lambda's queue holds an event 6 hours at
  most, the window is 24 hours from the first failure, and Scheduler's minute and Lambda's own two retries of a try add
  minutes. A commit deletes the records of events made two days or more before its own, by the Unix day, so one made
  late in a day lasts two days at the least, which is more than all of that. A record is a key in the name's tree, and
  counts against its budgets as the records of `Idempotency-Key`s do: a name that commits a background request every
  second holds about 260,000, some 6% of its 4 Mi entries. Nothing else limits them, so an app that sends to a host that
  is down is refused nothing. Before, the head held them as `pending`, and at 1,000 undelivered a name refused a new
  background request with 503, so that the head stayed under its 1 MiB.
- **When the router itself fails, an `outbox` or `retry` invocation is tried twice more and then dropped.** The S3
  write or read, or creating the schedule, failing is that: the router answers 503, and Lambda keeps to its
  configuration of two retries and 6 hours, with no destination. A failed `retry` loses the delivery with hours of its
  window left. Lambda's S3 destination would keep the event, but needs `s3:PutObject` on the whole bucket and
  `s3:ListBucket` for the router's role, which is more than it uses, and a record that only the platform reads. A
  router that fails three times in a row is an outage; the logs say so.
- **The router's S3 access is the prefixes it uses.** It gets, puts and deletes under `ws/` and `outbox/`, and
  lists the bucket with `s3:prefix` like `ws/channels/*` only, because on AWS the only listing it does is of one
  channel's sockets, which S3 matches against the request's prefix:
  [S3's `s3:prefix` condition](https://docs.aws.amazon.com/AmazonS3/latest/userguide/amazon-s3-policy-keys.html).
  The listing of `apps/` is the local router's cron. Locally the one policy is also the ceiling of an app's session,
  so it lists `apps/*/values/*` and the versions of `apps/*/names/*` as well, for the sweep, which an app's session
  may list only for its own app.
- **After the last try the router logs a warning** with the app and the commit, and deletes the object. Nothing is
  kept to replay: the record in the name's tree outlasts the window, but the event's bytes are gone.
- **serve and the router run behind the Lambda Web Adapter,** as plain HTTP servers. Their package carries its own
  `bootstrap`, which runs `tric <handler>`, because `provided.al2023` has no wrapper for that. The adapter's
  readiness check is a `GET /`. Any status below 500 counts as ready, including the router's 403.
- **The package is arm64 only,** and `docker compose run --rm package` refuses to build it anywhere else. Graviton
  costs less, and no cross-compiling means no toolchain to keep. The toolchain image is Amazon Linux 2023, the
  runtime's own OS, so the binary links the glibc that Lambda has.
- **Every name the install makes begins with `name`,** `tric` unless it is set. IAM's role names and CloudFront's
  function names are the account's, in every region, so two installs in one account need two names, and two domains.
  The bucket's name, which S3 shares among all accounts, is the name, `-`, and a suffix that OpenTofu makes up, so a
  name is what S3 takes: lowercase letters, digits and hyphens, from a letter, at most 32. `check.sh` reads it from
  the output `name`.
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
- **An `outbox` or `retry` invocation answers 204 once its event is delivered or scheduled again, and 503 only when
  the router fails,** which `AWS_LWA_ERROR_STATUS_CODES` turns into a failed invocation, so Lambda tries it again.
- **Schedules are named for what they are,** as `<hash(app)[..24]>-<hash(body)[..39]>`. A changed schedule is a new
  one: deploy creates the missing schedules first, then deletes the extras. They are in one group, which the install
  makes.
- **An app has 49 cron jobs at most,** and its sweep makes 50 schedules, so deploy reads its schedules in one page of
  100, a failed deploy's extras included, and needs no paging: Scheduler applies the name prefix before it pages, as
  a probe of a group of 104 showed. More than 100 fails the deploy, which says to delete them by hand.
- **A cron expression can't restrict both the day of the month and the day of the week.** POSIX cron runs a job
  when either one matches, and Scheduler can't say that, so tric refuses it everywhere.
- **An AWS call's status is checked in one place.** `Aws::send` answers an error that says what the service answered,
  unless the status is a success or the one its caller names as done already: Scheduler's 409 for a schedule that is
  made, its 404 for one that is gone, and `@connections`' 410 for a socket that is gone. Each caller had a check of its
  own, and Lambda's `Event` invocation took 202 only, where now it takes any success, which is all Lambda answers it
  with.

## Storage and turns

- **A turn runs again only while it is repeatable, and nothing claims a name.** A turn that another commit beat runs
  again, unseen, while tric holds all its body (`REPLAY_MAX`, 6 MiB) and it has sent no unsafe outbound request, and
  until its deadline (`tric::DEADLINE`, 5 s after it began). A body over the buffer can be read once (`Body::Once`), and
  an unsafe request only marks the turn (`Turn::repeatable`). One that may not run again, or whose deadline has passed,
  lands nothing, and answers 503 with `Retry-After: 1`: what it sent stays sent, nothing it wrote landed, and the client
  may try again knowing both. Before, a turn claimed its name at its first irreversible step, or after 1 s, with a put
  of the head that kept other turns off it: a re-run claimed when it opened, a busy name answered 429, a turn whose
  claim ran out (`CLAIM_TTL`, 15 s) as it committed answered 500, and clearing `pending` had to keep clear of a claimed
  head. That was a put of the head for each claim and each release, a lease, two answers and their states, for one gain:
  an irreversible turn was sure to commit unless its claim ran out. Now it is not, which is the cost. On a busy name
  such a turn can lose, and answer 503 though it sent what it sent; and a long turn on a busy name runs again until its
  deadline and then answers 503, where a claim would have held the name for it. A short turn whose body tric holds and
  that sends nothing unsafe, the common case, loses nothing.
- **A delivery event is sent once its record is in the name's tree, and dropped once it can never be.** A commit with
  background requests hands them to the outbox as one event before it puts its head, and writes in its tree, so in
  the same compare-and-swap, the empty key `o/<day>/<digest>`, the digest being the SHA-256 of the event's JSON. serve,
  given the event, drops it unless it is of serve's own app, and then reads the name's head (`name::holds`). If the
  head holds the record, it sends the requests. If not, and the head's commit is no longer the event's `base`, the
  commit of the head the turn read, or `SETTLE` and 30 s more have passed (`outbox::LANDING`), it drops the event.
  Else it reads again, after 50 ms, doubling to 1 s.
  - It is safe because a commit puts its head only over the head it read, by that head's `ETag` in the bucket, and
    each head's bytes are its own commit's. Once the head is another commit's, the base is gone for good, and the
    commit can never land. A head that holds the record descends from the commit that made it, and every commit keeps
    the records of events until two days after their own, longer than an event is tried, so a commit that landed is
    never taken for one that did not.
  - A turn that died during its put, or before it, leaves its event waiting on a head that does not change: `LANDING`
    bounds that wait. The put cannot land after it unless S3 applies it more than 30 s after `SETTLE`, past what
    object_store waits for an answer; such a commit lands with its requests dropped. `LANDING` was there before, for
    the same case.
  - A forged event is dropped. One of another app is not this app's. One whose commit never landed has no record in
    any head. One that names a real commit, whose id was learnt from an `Idempotency-Key`, but holds other requests,
    has a digest no commit recorded. An event sent again byte for byte is the commit's own, and is delivered again
    while its record lasts, with the same requests and `Idempotency-Key`s, which delivery promises anyway.
  - Delivery is at least once, and reads only. A delivery that the router takes for failed after serve sent the
    requests sends them again at the next try. It was so before, up to the moment serve took the commit out of
    `pending` after sending, which dropped a delivery sent again after that; only publishes relied on that, and they
    change (see "A publish may be sent twice").
  - Before, the head held `pending`, a digest and a time for each commit with an event undelivered, and delivery waited
    for the head to move past the event's base, sent, and then took the commit out of `pending` with a put of its own.
    That was a write of the head for each delivery, which had to keep clear of claims; a limit of 1,000 a name; and an
    age, `PENDING_TTL`, for an entry whose delivery was lost. The records need none of these: they ride in the
    commit's own put, and expire by day, as the records of `Idempotency-Key`s do.
- **`deploy` writes with the owner's credentials,** from the environment, not with an app's.
- **A name over its budgets fails at the write that takes it over,** and the write changes nothing. The budgets are the
  data (16 GiB) and the entries (4 Mi) of its tree, counted exactly from the head and the turn's own edits, and the
  guest is told which it passed, which is the one tree error it sees whole. A head is read up to 1 MiB (`HEAD_MAX`),
  which is only a sanity cap: a head is its root node, at most 64 KiB, and a commit's id. `TooLarge` (a 500, as for a
  trap, with the held requests not sent) is a tree too full for the commit's own records.
- **A snapshot is a turn that has answered.** Both read the name as it was, and neither writes, so one type serves
  both. A write after the answer now fails with `access-denied`, the same as a write to a snapshot, where it used to
  fail with `other`. `wasi:keyvalue` names `access-denied` for exactly this.
- **A cache of heads waits for numbers.** A cache in each environment would spare a writing turn its read when the
  cache is current, as `If-Match` catches it when it is not, at the cost of a wasted run. A turn that writes nothing
  would still have to check its read, and S3 Express keeps no versions. So every read and write of a head logs, at
  debug, its `ETag`, size and time; the live run turns that on with `-var log=warn,tric=debug`, and counts, in each
  environment, the reads a cache would have spared and the runs it would have wasted.

## State

What was weighed, and from what. The design is in `docs/plan.md`, under "State: one tree per name".

- **tric gives apps no file system, and keeps their state in keys,** as decided on 13 October 2026. An app keeps its
  state in its names' keys (`wasi:keyvalue`), in the same tree and under the same commit as before. `wasi:filesystem`
  0.3 is still linked, with wasmtime-wasi's stock host and no directory preopened, so a guest whose language imports it
  (Rust's standard library on `wasm32-wasip3` does, and so does the JavaScript runtime) instantiates, and every open
  fails, as a path has nothing to resolve against. tric had a file system of its own over a name's tree, with inodes,
  blocks and directories as keys: a core and its 0.3 binding (`src/fs.rs`, `src/fs/ops.rs`, `src/fs/p3.rs`), 1,708
  lines of code, and a harness for the WebAssembly testsuite (`src/fs/conformance.rs`), 128 more, together 28% of the
  runtime's code. It went for two reasons:
  - its size, against the owner's target of 5,000 lines of code for the runtime;
  - its attack surface, the largest in the runtime: a path walk of tric's own over guest input, with `..`, symlinks
    and their limits, a table of descriptors, and streams that outlived the call that made them.

  It could come back as an opt-in crate over the tree, as `tric-tree` is, and the tree's format would not change for
  it: its prefixes, `f/` and `d/`, are free. What it was built on, what was rejected for it and how it was tested are
  in this file's history before that date. Where a decision below was shaped by it, a line says so.
- **Keys and records are one tree.** A turn commits by one compare-and-swap of one head, and two trees would need two
  heads, which S3 can't change together. One tree is one edit path, one commit and one budget, where two would copy the
  hardest code; it held the file system's inodes and directories too, under `f/` and `d/`. The prefixes keep apart the
  keys of `wasi:keyvalue` (`k/`), the records of `Idempotency-Key`s (`i/`) and those of delivery events (`o/`). The
  cost is that a name larger than one node (64 KiB) reads its keys through the tree, and under that it is a head
  alone, as before.
- **The tree is a copy-on-write B+tree,** as LMDB, bbolt and btrfs have it (from general knowledge: LMDB's page
  wouldn't load). A change writes a new copy of the path from a leaf to the root and edits no object, so readers on
  older roots are undisturbed, and the root is in the head, so the commit stays the head's compare-and-swap.
- **Nothing is addressed by content.** Git, IPFS and Dolt's prolly trees name an object by its hash, which gives
  deduplication and a shape that depends on content alone. tric wants neither: deduplication between tenants would let
  one app learn, from a write's speed, that another holds the same value. The file system had two reasons more: POSIX
  can overwrite but not insert, so equal content doesn't move, and a file's identity is its inode, which a content
  address can't give.

  A link carries a hash to check an object against, not to name it by.
- **SlateDB is rejected for state.** Sources: <https://github.com/slatedb/slatedb> and
  <https://docs.rs/slatedb/0.15.0/slatedb/>. It is a log-structured merge tree on object storage with one writer at a
  time, fenced through its manifest, and its compactor and collector run as background tasks. tric has many
  optimistic writers to a name, serialised by one compare-and-swap, and nothing that runs between requests. Moving
  state onto SlateDB would replace that model and not use it. 0.15.0 depends on `object_store` 0.14, as tric does, so
  it would link, with a large tree of dependencies.
- **Prolly trees and the AT Protocol's MST are rejected.** Sources:
  <https://docs.dolthub.com/architecture/storage-engine/prolly-tree> and <https://atproto.com/specs/repository>.
  Their shape depends on their contents alone, so replicas converge and diff cheaply. tric has one writer at a time to
  a name and no diffs, and a plain B+tree is simpler to reason about.
- **The stock interfaces are linked together, the file system among them, by wasmtime-wasi's `p3::add_to_linker`.**
  It links `cli`, `clocks`, `filesystem`, `random` and `sockets` (`add_to_linker_with_options` in wasmtime-wasi's
  `src/p3/mod.rs`), and the guest's `WasiCtx` is built with no preopen, so the file system has nothing to open. They
  were linked one at a time while tric had a file system of its own, as the stock one could only have been replaced by
  shadowing, which would also have let a mistake of ours hide behind it. A stock interface that a Wasmtime upgrade adds
  is now linked with the rest, where it would have failed at instantiation; Wasmtime is pinned (`=49.0.2`), so an
  upgrade is where to look for one.
- **tric serves WASI 0.3 alone,** as decided on 8 October 2026. Rust's standard library imports 0.3 alone on
  `wasm32-wasip3`, stable from Rust 1.100 (due 12 November 2026), and the JavaScript runtime is built for that target
  too (below). An app built for 0.2, for `wasm32-wasip2` or by StarlingMonkey or ComponentizeJS, is not served: it
  fails at load (`instantiate_pre`), with an error that names the import the linker lacks, such as
  `wasi:cli/environment@0.2.6` (checked with `tric dev`). One version is one binding and one linker. 0.2 was served
  first, and 0.3 beside it, until then; the file system's 0.2 binding and its tests were removed with this decision.
- **Objects are put at a random id, over any object there.** A put that is retried would get 412 on its own object
  under create-if-absent, and 128 random bits won't collide. The link's version id and hash would catch it if they did.
- **A link pins a version and a hash.** The version means a reader gets the object it was pointed at, however the key
  has changed since, and survives the delete marker the sweep adds. The hash means a head vouches for everything under
  it, so a corrupted or swapped object is `io` and not a guest's data. Neither costs a request.
- **A value up to 4 KiB is inline,** in its leaf. It rides on a request the leaf needed anyway, and 4 KiB is a page.
  Larger ones are objects of their own, up to 1 MiB. (It was 1 KiB in the flat head, which had no leaf to ride on.)
- **A node is at most 64 KiB, the root's limit too.** The root is in the head, which every turn reads, so the head is
  small however large the name. A write rewrites one leaf of at most 64 KiB.
- **A thin node merges at the commit, as bbolt's does.** A changed node under a quarter of the node limit joins a
  neighbour (and the two are split again if they do not fit in one), an emptied node is dropped, and a root with a
  single child is replaced by it. So a name that is mostly deleted shrinks, and its reads get shorter. Merging is where
  B-trees go wrong, so the model test (below) is built for it: it uses nodes of about a kilobyte, so that branches
  as well as leaves split and merge, and four levels grow and collapse in a few hundred rounds. It is not done at each
  delete, which would merge a node that the next write fills again, and only nodes the turn changed are looked at, so
  a commit reads no more than the neighbours that thin ones join. A spill that uploads nodes merges first, as the
  commit does (see "A spill of nodes merges first" under the tree crate).
- **A branch's first key is not looked at.** A child's key is the least key that can be in it, and the first child
  has nothing to its left to be least of, so a lookup searches only the keys after it. A merge keeps this true by
  giving the first child of the right-hand branch its parent's separator when the two join, since there it is
  a real bound. The model test found the bug that taught this (a stale first key broke the binary search), so the
  test checks every separator against the keys below it.
- **Nodes are JSON,** as the head is: `serde_json` is linked, and a node can be read with `curl`. A binary codec is a
  new crate for smaller objects, and the format is one function to change.
- **A turn edits the tree directly, as LMDB and bbolt do,** and has no overlay and no cap on its changes. An overlay
  applied at the commit would be a second structure that every read and `list-keys` had to merge with the tree, and
  would need a cap to stay in memory, a limit the guest would meet that nothing else explains. Here the nodes a write
  touches are copied in memory and the rest stay links. Reads and `list-keys` use the one tree. A blind write descends
  at the write, so a GET for each level not cached is paid there and not at the commit.
- **Past its memory budget a turn spills, as LMDB does a dirty page.** The copies, and the large values held for
  upload, count against one budget of 8 MiB (`Config::budget`), checked after each write. Past it, the held values
  upload first, up to 16 at once while the writer waits, and each becomes a link; if the copies are still over the
  budget, what is thin merges, as at the commit, and the changed nodes upload bottom-up and become links too, which
  dirties their parents in turn. Nothing refers to an uploaded object until the commit, so a discard changes nothing
  that a head names. What the turn uploaded, and what it uploaded and then replaced, is left for the sweep. The turn
  holds about its budget and one write however much it writes.
- **A key's value over 4 KiB is held whole,** uploaded when the budget spills it or the commit does, and counted
  against the same budget, as the file system's blocks were. A value is not uploaded at the write, since a value
  written twice in a turn would then put an object for each.
- **Nothing is deleted at a commit, and the sweep is the only collector.** A turn deletes nothing, whether it lands,
  loses its race, is discarded, traps or times out. What its edits replace, and what it uploaded for a head that did not
  land, is left for the sweep. Lance does the same: a write leaves the files of older versions in place, and
  `cleanup_old_versions` deletes those that no version it keeps names
  (<https://github.com/lance-format/lance/blob/main/docs/src/guide/read_and_write.md>). So does Icechunk, where
  `expire_snapshots` marks old snapshots and `garbage_collect` deletes what only they name
  (<https://github.com/earth-mover/icechunk/blob/main/icechunk-python/docs/docs/understanding/expiration.md>).
- **A commit deletes nothing because it cannot always know what is live.** A commit that deletes must know which of its
  objects the head names, and a commit whose answer was lost does not know whether its head landed. A delete chosen then
  can remove a live node, which is data loss; a delete not made costs storage until the sweep. With no deletes, a lost
  answer cannot lose data, and a commit is simpler: there are no lists of what a turn made and replaced, and no deletes
  for a response to wait for before a Lambda is frozen. An earlier tree also stays whole until the sweep takes it, which
  is a window in which a name can be rolled back. The cost is that every object a commit replaces, and every object of a
  turn that does not land, is stored until the sweep, where before a commit deleted it at once.
- **A refusal of a commit's put could be the commit's own write.** A commit is a conditional put of the head, and
  `Store::put` took a refusal (412, 409, or 404 on `If-Match`) as another's write. object_store sends a request again
  after a 5xx, a 429, a 408 or a dropped connection, and a conditional put again after a 409, and does not say that it
  did. So a put that landed but whose answer was lost was sent again, was refused for its own write, and read as a
  conflict: the turn deleted the objects that its landed head names, which is data loss, and ran again though it had
  committed. Icechunk, which writes through object_store too, met the same bug
  (<https://github.com/earth-mover/icechunk/blob/main/design-docs/017-conditional-put-lost-response.md>).
- **That a put which failed may have landed is general knowledge, and not something S3 documents.** No AWS page found
  says that a 5xx may answer a PUT that was applied. RFC 9110 (9.2.2) has a client repeat an idempotent request whose
  answer was lost, as the repeat has the same effect "even if the original request succeeded, though the response
  might differ" (<https://www.rfc-editor.org/rfc/rfc9110#section-9.2.2>). A conditional put is not idempotent in that
  sense, as the repeat is judged against the first one's write. S3 documents 412 for a condition that fails, and 409
  `ConditionalRequestConflict` for a conflicting operation during the upload, which it says to retry
  (<https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html>), and says to retry a 503 Slow Down with
  exponential backoff
  (<https://docs.aws.amazon.com/AmazonS3/latest/userguide/optimizing-performance-design-patterns.html>).
- **tric tries a conditional put again itself, so it knows what a refusal follows.** Conditional puts go through a
  client with object_store's retries off (`max_retries: 0`), which shares its credentials and connections with the one
  for everything else. A store on S3 holds both, and `Store::put` sends a conditional put by the one that does not
  retry (`Store::once`); the memory store, which loses no answer, has the one. It was an `ObjectStore` that wrapped the
  two and passed every other call on, eight methods and a `Display` for one choice that only `Store::put` makes. An
  overwrite, which is idempotent, keeps object_store's retries. `Store::put` tries a conditional put again, as it was,
  after an error that is not a refusal, up to `TRIES` (4) times, from 50 to 100 ms apart and twice as long each time. A
  refusal at the first try is a conflict, as before, and the turn runs again at once. A refusal after a try that may
  have landed is an error, and the caller reads the head to learn what landed.
- **A head names the commit that wrote it, and that id is the name's `ETag`.** A head holds `commit`, the id of the
  commit that wrote it, 128 random bits in hex, which every commit draws afresh. So no head's bytes are ever another's,
  and no head's `ETag` in the bucket is either: S3's `ETag` changes only with an object's contents, and for an object
  put whole, unencrypted or under SSE-S3, it is the MD5 of its bytes
  (<https://docs.aws.amazon.com/AmazonS3/latest/API/API_Object.html>). A put with `If-Match` lands only if the `ETag`
  matches (<https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html>), so a put on a head's
  `ETag` lands on that head or not at all, and once that head is written over, never; and a read after a write that S3
  has applied sees it (<https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel>).
  - The name's `ETag`, for `If-Match` and `If-None-Match` and in a turn's answer, is `"<commit>"`, where it was the
    bucket's. A commit knows it before its put is answered, so one whose answer was lost, and which learns that it
    landed, answers with it as one that heard does; and it changes only when a commit writes the name, as the bucket's
    did. 128 random bits say no more of the head than an MD5 of its bytes.
  - Before, a head held the ids of its last 8 commits: a commit whose base's newest id had been dropped could not tell
    whether it landed, and a claim taken and let go put a head back to its base's bytes, and so to its `ETag`, on
    which a commit sent before could still land. Icechunk reads back a write id from an object's metadata
    (<https://github.com/earth-mover/icechunk/blob/main/design-docs/017-conditional-put-lost-response.md>); a head
    holds its commit's id in its bytes.
- **A commit with no answer reads the head to learn whether it landed.** A commit whose put errs, or has no answer,
  reads the head (`Turn::land`), and what it finds decides only whether the turn is done: nothing is deleted whatever
  it finds.
  - If the head names the commit, it landed. It is done, and answers with its `ETag`.
  - If the head is still the base, by its commit, nothing has landed on it, and the commit is put again, as it was.
    That put lands only if the first has not, as the first one's landing changes the head's `ETag`; if it is refused,
    the head is read again. A commit on a name with no head does the same while there is still none, with
    `If-None-Match: *`.
  - Else another commit's head is there, which may have been written over the commit's or over the base, and the head
    cannot say which. The commit fails with `Unsettled` (below).
  - While S3 errors, or a put again is refused, it reads again, after a wait that starts at 100 ms and doubles to 2 s.
  - The walk back along the head's versions goes. A head held `over`, the version id of the one it was written over,
    and a commit that found another's head read the versions back along it to the one written over its base, which
    said whether the commit was there. That was a field in every head, a GET by version id for each version on the
    way, and the cases where it broke: a version expired, a head deleted, a version put back. It answered only when a
    put lost its answer and another commit then landed within `SETTLE`, which on S3 is rare, and the cost of not
    knowing is a 500 the client can act on.
- **A commit that cannot learn whether it landed answers 500, with no `Retry-After`.** A commit's put, and learning
  whether it landed, take `SETTLE` (30 s) at most from when the put is sent. Past that, or when the head is another
  commit's, the commit fails with `Unsettled`, which is logged at warn with the path and the commit's id. The turn
  answers 500, and is neither done nor run again; its objects are left for the sweep, as every turn's are. A 503 with
  `Retry-After` asks the client to try again (RFC 9110, 15.6.4 and 10.2.3,
  <https://www.rfc-editor.org/rfc/rfc9110#section-15.6.4>), and a turn that landed and runs again has run twice; a
  500 asks nothing of the kind. A commit that fails before its put, in uploading its tree or handing over its event,
  answers 503 with `Retry-After: 1`, as nothing landed, whether or not the turn sent an unsafe request, as a turn that
  loses its race does; and `TooLarge` answers 500, as it never can land. Only an error marked `Unsent` answers 503
  (`tric::failed`), and every other answers 500, so an error that a later change adds after the put is taken for one
  that may have landed, and not for one that surely did not.
  - `SETTLE` is 30 s so that the deadline (5 s), `ANSWER` (10 s) and it fit in CloudFront's 60 s for an answer from the
    origin, and the client hears the 500 and not a 504. A turn that runs again, or uploads slowly, can take longer.
  - object_store's request timeout is 30 s, so a put that waits it out uses all of `SETTLE`, and the turn answers 500.
  - The outbox waits for a commit to land for `SETTLE` and 30 s more, for a put that S3 applies late.
- **Why a commit failed is one enum, `name::Failed`, and an error carries at most one.** It is `TooLarge`, `Unsent` or
  `Unsettled`, which were types of their own, so an error could carry both `TooLarge` and `Unsent`, and
  `tric::failed` had to check for the pair. A failure before the put is marked `Unsent` only if the error is not a
  `Failed` already, so `TooLarge` never is, and `tric::failed` answers 503 for `Unsent` and 500 for any other.
  `Lapsed`, for a claimed turn that lost its claim, went with the claims.
- **A commit that does not land resets the tree itself, under its write lock.** `discard`, which a failed commit
  calls, resets the tree too, so the commit's reset looks redundant; but between the two the lock is free, and a read
  of a key after the answer, by a body that streams after it, would see writes that never landed, where it is promised
  the tree as the commit left it or as it was. A file descriptor's read was the first to need this.
- **What a lost answer still costs.**
  - A commit that cannot learn whether it landed answers 500. If the client runs the turn again all the same, and the
    first put did land, the turn has run twice. That takes S3 failing for `SETTLE`, another commit landing over the
    commit's head before the head is read, a head that is deleted, or a version put back.
  - An answer lost after a commit landed can still lead a client to try again: serve's answer may not reach the
    router, which answers 502; CloudFront answers 504 past its 60 s; a connection may drop. Nothing in the head tells
    such a retry from a new request. Only a key that the client sends with a request, and again with its retry, can,
    and tric takes one: a turn with an `Idempotency-Key` that landed is answered again as it was, and does not run
    (see "Idempotency keys").
  - A 409 on a head is a conflict, and the turn runs again, where object_store sent the put again.
  - A conditional put that times out is tried again, where object_store did not; the tries, and what follows them,
    stop at `SETTLE`.
- **The sweep is built, last, and is the collector.** A commit deletes nothing (the bullet on that above), so what
  leaks is all that no head names any more: what a commit replaced, and what a turn made that did not land, whether
  it trapped, timed out, lost its race or failed, or its host died. The sweep lists the app's names, and for each one
  lists the versions of its head that the bucket keeps, reads them, marks what their trees reach, lists its objects,
  and deletes those that are older than the grace period (an hour at least) and not marked. It is safe, as no version
  kept, and no head put later, can name such an object (the bullets on the history and on the grace), and fails safe,
  as it deletes nothing from a name unless each version listed was read or had expired, and each tree was marked
  whole. It runs in serve with the app's own credentials, so it needs `s3:ListBucket` on `apps/<app>/values/*` and
  `s3:ListBucketVersions` on `apps/<app>/names/*` and no more, each by an `s3:prefix` condition, in four places (the
  router's policy in `src/route.rs`, `infra/aws/main.tf`, `docker/router-policy.json` and the module's test, with
  assertions on the conditions and on the whole set of actions; and `isolates_apps` in `tests/e2e.rs` lists what an
  app's credentials can and cannot list, of keys and of versions). It is triggered every `TRIC_SWEEP_EVERY` (a day by
  default) for each app through the Scheduler, the router and serve, as a retry is, and never by an always-on
  process. It is bounded in each invocation and resumable, and logs what it reclaimed. No check script exists to run
  it from; its live path is the invocation in the bullet after the trigger, and `sweeps_the_names` in `tests/e2e.rs`
  runs it that way against RustFS, over a name of two versions and a delete marker, which it lists as the owner to
  check. That test cannot show a deletion, as nothing in it is an hour old; the unit tests in `src/sweep.rs`, which
  pass the clock in, do.
- **The sweep keeps a name's history.** It keeps what any version of a name's head names for as long as the lifecycle
  keeps that version: the current head's, those it replaced, and those of a name whose head was deleted. So a name can
  be put back as it was at any of them, a deleted one too, and that is the rollback window, whose length is the
  lifecycle's `noncurrent_days`. Marking from the current heads alone would delete a replaced tree's objects an hour
  after the commit, and leave the noncurrent versions of heads naming objects that are gone. Keeping them costs the
  storage of what a name's commits replaced for `noncurrent_days` more, after which the sweep deletes it and the
  lifecycle keeps it as long again, as it always did; and, each run, a listing of each name's versions and a read of
  each. Nothing puts a version back yet, and the bullet on putting one back has what that must keep to.
- **Keeping what any kept version names is how Lance and Icechunk collect.** The bullet on deleting nothing at a
  commit has them: Lance's `cleanup_old_versions` deletes the versions older than a time and then the files that no
  version it keeps names, and Icechunk's `expire_snapshots` drops snapshots older than a time and its
  `garbage_collect` then deletes what only they named. Both keep a list of versions of their own and expire it
  themselves. Here S3 keeps the list, as a head's noncurrent versions, and the lifecycle is the expiry, so the sweep
  writes no list and expires nothing, and no app's credentials can shorten the history. Both also spare by age what a
  writer under way may yet name, as the sweep's `GRACE` does.
- **The sweep's trigger is a schedule per app, every `TRIC_SWEEP_EVERY`, in the cron group, and nothing else.** Deploy
  adds one schedule to an app's cron entries, named and reconciled with them (its name has the hash of its body, so a
  change is a new schedule made before the old one is deleted), with the input `{"app": "<app>", "sweep": true}` where a
  cron entry has `{"app", "path"}`. It targets the events function's `cron` alias, as the cron entries do, so there is
  no new alias, role, group or permission for it. Its time is a minute of the interval, or of the day for days, taken
  from the SHA-256 of the app's name (`sweep::schedule`: daily, as `M H * * *` in UTC, and as `cron(M H * * ? *)` for
  Scheduler), so apps spread over it and no flexible window is needed to avoid a herd; and it is the same time locally,
  where `Route::cron` fires it at that minute, as it fires an app's cron entries (the memory store of `tric dev` keeps
  no versions, so a sweep there deletes nothing). The router's `job` takes either shape and, for a sweep, sends serve a
  `POST` with `Forwarded: for=_sweep` as the app's tenant, with credentials minted for one hour as for any request. It
  answers 204 whatever serve says, as for a cron entry: a sweep that fails is logged and is the next one's, not a retry.
  An app with no release is answered 404 and not swept. Serve dispatches `for=_sweep` after the credentials and the
  tenant are checked and before the app is loaded: a sweep needs the store and none of the app's code, so it reads no
  release, compiles nothing and runs no guest, and an app that never writes a key costs one cold invocation
  an interval that lists an empty prefix and writes nothing.
- **The interval is the operator's: hours that divide a day, or whole days.** `TRIC_SWEEP_EVERY` (`--sweep-every` of
  `deploy` and `route`) is `<n>h` for 1, 2, 3, 4, 6, 8, 12 or 24 hours, or `<n>d` for 1 to 31 days, and `1d` by default;
  anything else is refused with the list of what is taken (`sweep::Every`). Hours that divide a day are an hour field of
  `<first>-23/<n>`, which fires as far apart all day and across midnight; other hours cannot be said in cron without a
  gap that differs at midnight. Days are the days of the month one more than a multiple of `n`, so a month's last
  interval may be shorter, and never longer. The app's minute is the SHA-256 of its name modulo the minutes of the
  interval in hours, or of a day in days, so apps spread as they did over the day, and `1d` and `24h` give the
  expression a daily sweep always had, so the default changes no schedule. Deploy writes the expression into the
  schedule, whose name hashes its body, so a new interval is a new schedule, made before the old one is deleted: it
  reconciles itself, and reaches each app at its next deploy. Locally the router reads the same variable for its own
  tick. The module has it as `sweep_every`, validated to the same intervals, and outputs it as `TRIC_SWEEP_EVERY`, for
  deploy to read with the other outputs. How often is what garbage costs against what a run costs: a run lists and reads
  each name with objects, so an app of few names and many commits gains from hours, and one of many quiet names from
  days.
- **A sweep can be run now, by the same path.** `aws lambda invoke --function-name <name>-events --qualifier cron
  --cli-binary-format raw-in-base64-out --payload '{"app":"<app>","sweep":true}' /dev/stdout` on AWS, and a `POST` of
  it with `x-amzn-lambda-context` naming the `cron` alias to the events listener locally. That adds no endpoint,
  credential or route that a client can reach, and it can only do what the schedule does. Serve logs `swept` at info
  with the app and what it did (names swept, objects and bytes deleted, names skipped and failed, and whether it went
  through every name), and `sweep: reclaimed` for each name it deleted from, so a run says what it reclaimed.
- **`for=_sweep` cannot be reached from outside.** The router writes `Forwarded` itself for every request it sends
  serve. On a client's request `forward` removes the client's and inserts the router's, and an event goes through
  `Route::event`, which takes the word as a `&'static str` its caller names (`for=_cron`, `for=_tric`, `for=_ws` or
  `for=_sweep`), so nothing in a request or an event body can become it. Only the `cron` alias's `{app, sweep: true}`
  makes the router say `for=_sweep`, and that alias is invoked by Scheduler's role (and by the owner's credentials,
  for a run now): any other alias, or none, is a 403, and a `{app, sweep}` with a `path`, or with `sweep` false, is a
  400. Serve takes the word only with the router's credentials and the tenant its host names, which no client has. A
  guest's call to its own origin runs in-process with `for=_tric` and never reaches serve's `handle`. The e2e tests
  send it every way: `drops_forged_events` sends `for=_sweep` in a public `GET` and `POST` (answered by the app, as
  any request is) and in events for each alias, for none and for ones that are none, and asserts that the log shows
  no sweep and so no list and no delete. (Locally the router's own tick sweeps an app at its minute of the day, which
  could be a minute the test runs in. The tick logs `cron: sweep due` before it asks, so the test counts the lines of
  a sweep less those: each tick accounts for one of each kind, and the count does not depend on the time of day.
  Nothing in the code is there for the test.) `isolates_apps` sends it to serve with no credentials, with no tenant
  and with the wrong tenant, and gets a 403 each time.
- **Only a key a tree could have made is deleted.** A candidate is `apps/<app>/values/<name>/<id>`, where `<name>`
  passes `is_name` and `<id>` is 32 lowercase hex digits, and where the key is equal to the one `tree::object` builds
  for that name and id, the one place that builds a `values/` path: the tree reads and writes its objects at
  `tric_tree::object` under the prefix `tree::prefix` gives, and `tree::object` is that. A listed key of any other shape
  (the `.sweep` cursor, a key with a deeper path, a name that no name could be, an id of the wrong length or case, a
  differently escaped spelling) is not a candidate, and is never deleted, whatever its age. The unit tests
  `only_the_key_of_a_tree_object_is_a_candidate` and `deletes_only_keys_a_tree_could_have_made` put such keys in and
  check that they are all still there.
- **A name's heads are the versions listed of its key, and a missing head is told from an error by that listing.**
  The sweep lists the versions of `apps/<app>/names/<name>` with ListObjectVersions, signed by `aws::Aws` as STS and
  Lambda are, since `object_store` has no call for it (`quick-xml`'s `overlapped-lists` reads its `Version` and
  `DeleteMarker` elements, which S3 interleaves), and reads each by its id with `Store::version`, from the store and
  never from a cache of heads: a stale head would not name what a commit since had added. The listing is of the key
  as a prefix, so it has the keys that begin with it as well (`n2` after `n`), which `sweep::listed` drops, and it
  stops at the first page that has one, as a listing is in key order. A delete marker is no root and is not read, as
  S3 answers a read of one by id with 405. Only a 404 says that a version is gone: any other failure to list or read,
  a 403 among them, fails the name, with a warning, and deletes nothing from it. `Aws::send` does not retry, so a
  listing that fails once, as a 503 can, fails the name for that run, which the next run sweeps; the reads go through
  `object_store`, which retries. A version that is gone is an error if it is the latest, which cannot expire within a
  run (the lifecycle keeps what is noncurrent for a day at least), and no root if it is not: it expired after the
  listing, and no commit writes over a version that is not current. A name whose latest version listed is no head
  (there is none, or it is a delete marker), and whose head is there all the same, was missed by the listing or made
  since: `Store::head` looks for it, and if it is there the name fails and waits for the next run. A GET or a HEAD of
  a key that is missing is a 403 and not a 404 to a caller without `s3:ListBucket`
  (<https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html>), and a grant by `s3:prefix` is not one for a
  GET or a HEAD, which has no prefix; `store::found` reads that 403 as missing, and an app may get its own heads, so
  the check is exact. A name with no version listed had no head put before the listing, and a head put later names
  only objects younger than the grace. The grant widens an app's session by the history of its own heads, which are
  names it already knows, and no other app's.
- **Every version of a head that the bucket keeps is a root, and nothing else is.** What points into `values/` is a
  head's tree, and nothing else does. A head also holds `commit`, the id of the commit that wrote it, which is no link.
  The records of `Idempotency-Key`s and of delivery events are values in the tree, and no links either. The outbox and
  retry objects hold an event's requests (under `outbox/`, and not under `values/`), and no link. A snapshot, which a
  read holds, is a copy of a root read from a head and reads by version, which a delete marker does not hide, and for
  at most `TOTAL`, so it needs no mark of its own. A link read from a head or a node is checked to name an object of its
  own name, so a changed head cannot make the sweep mark, or keep, another's. If anything else were ever to point into
  `values/`, the sweep would have to mark through it as well, and `tree::object` is where to look for who builds those
  paths.
- **The grace outlasts the longest turn, by construction.** A head is put by a commit, which lands only if its base is
  the head then (a conditional put on its ETag, so a turn from an older head never lands), and names what its base
  named and what its turn made. The sweep takes `cutoff = now - GRACE` before it lists a name's versions, and reads
  them after that. A head put after the listing is at the end of a chain of commits from the head that was current at
  the listing, so it names what that head named, which is marked, and what the turns of the chain made. Each of those
  turns landed after the listing, so after `now`, and began at most `TOTAL` (300 s) before it landed; in absolute
  terms an invocation lasts 900 s at most (Lambda's limit), and the clocks of Lambda and of S3 may differ by 5 minutes
  at most, which is set well above what AWS keeps. So what a later head names and no version listed does was made
  after `now - 900 s - 300 s`, which is after the cutoff, and the age filter keeps it. So the versions listed are the
  roots. The latest of them cannot expire within a run, as the lifecycle keeps a noncurrent version for a day at
  least; a noncurrent one that expires between the listing and its read is no root, as no head put later descends
  from it. `src/sweep.rs` states the grace as assertions that fail the build: `GRACE` is an hour at least,
  `TOTAL + SKEW < GRACE`, `LAMBDA_MAX + SKEW < GRACE`, and a run (`RUN`, 240 s) and 30 s more fit in `TOTAL`, so a
  limit raised past the grace cannot be built. A node the mark finds in the process cache was verified when it was
  cached, and the cache's key is its path and version, so the mark uses what a read would. Only putting a version
  back breaks the chain, as it descends from no current head: the next bullet has what it must keep to.
- **Putting a version back is no commit, and must keep to three things.** Nothing in tric puts a version back; an
  operator could, by copying a version of `names/<name>` over the key. Such a head names objects that may be older than
  the grace and that the current head does not, and it is safe only because they are marked through the version itself,
  which every run that listed before the put must then read whole. So a put back must be of a version that the lifecycle
  keeps for an hour (`GRACE`) after the put at least, as a run lasts 900 s at most: S3 may expire a version from the
  first midnight UTC that is `noncurrent_days` after its successor was put, and the version is not put back in the hour
  before. And it must be a conditional put on the current head's ETag, as a commit is, so that it writes over no commit.
  And it must carry a fresh `commit` id, as a commit does: the old version's bytes have the old version's `ETag` in the
  bucket, so a commit sent over that version before could still land on the put back, after its delivery event was
  dropped as never to land; and a client's `If-Match` with the old `ETag` would hold again. A tool that puts versions
  back states all three in its own code, and tests them against a run under way. A version put back holds the records of
  the commits up to it, and not of those after, so their delivery events not yet sent are dropped.
- **A name's marks are bounded by their size together, and the valve is a last resort.** The versions of a name's head
  are marked one after another into one set of `u128` (`Head::mark`, which is `Tree::mark`), each reading its tree's
  nodes level by level, `IN_FLIGHT` (16) at once, each checked against its link, and passing over what the set holds.
  So a node that several versions share is read once, and `NODES_MAX` bounds what they read together, not each. A
  commit changes the nodes on the paths from the root to what it changed, so a version after the first reads a node
  a level for each change, one level after another. These figures are estimates, not measurements: a GET of a node
  takes about 25 ms from serve, so a mark reads about 640 nodes a second, and 100,000 nodes (`NODES_MAX`) take about
  156 s of a run's 240. A name with 4 Mi entries of 250 bytes each (keys and links) has
  16,000 to 32,000 nodes of 64 KiB, between full and half full, and takes 25 to 50 s. A name passes the valve only
  with more than 100,000 nodes, which is 3 to 6 GiB of keys, links and inline values in its nodes; the structural
  extreme, 16 GiB of 4 KiB inline values, is 262,000 to 524,000 nodes, which no run reads. The set of ids is about 24
  bytes an id, and a name has 4 Mi ids at most in a tree, and a day or two of commits' more in its versions, so about
  100 MB in serve's 1,024 MB; the nodes in flight are 16 of 64 KiB. The valve has three parts: a name with more than
  `VERSIONS_MAX` versions is not read, a name whose marks would read more than `NODES_MAX` nodes stops marking, and a
  name that is the first of a run and takes longer than the run is passed over, each with a warning that names the
  app and the name, and each deleting nothing, as its marks did not complete. After a mark that stops or fails, the
  set holds part of the trees, and is dropped with the name. Only the first is expected to fire, for a name that puts
  its head all day, and a name that makes one of the others is at its limits.
- **A name's versions are bounded, and a name that puts its head all day is not swept.** `VERSIONS_MAX` (10,000) is the
  most versions of a head, delete markers among them, that a run lists and reads. The listing is 1,000 a request, ten in
  sequence, and the reads are `IN_FLIGHT` at once, some 15 s at 25 ms; the marks after the first read what each commit
  changed, a few nodes one level after another, so 10,000 versions of a deep tree could take longer than a run, and such
  a name is passed over as the first of a run is. The lifecycle keeps a version from one day to two at the default, so a
  name has more than 10,000 only if it commits more often than every 9 to 17 s all that time. Such a name is skipped,
  with a warning, at every run while it is that busy, and its garbage stays until it is quieter: that costs space and
  not data. The alternative is to read fewer versions: a name's versions from the oldest kept are a chain of commits, so
  what any of them names is what the oldest and the current name and what was made since the oldest's `LastModified`
  less the grace, and a sweep could mark those two and keep everything younger. That reads two heads where this reads
  thousands, and keeps more, and rests on the chain, which a version put back breaks; it is not built, and is the choice
  to make if a name is ever that busy.
- **One rule expires what is noncurrent, after `noncurrent_days`, and that is the history.** The module's rule
  `noncurrent` expires every noncurrent version after `noncurrent_days` (a variable, 1 by default, a whole number of 1
  or more, validated) and removes a delete marker that is all that is left of a key. A head's noncurrent versions are
  the sweep's history, so the variable is the rollback window; the rule also expires what the sweep deleted and what
  a deploy replaced. S3 counts whole days from when a version became noncurrent, to the next midnight UTC, so 1 keeps
  a version from 24 to 48 hours, and that day at least is why the latest version listed cannot expire within a run.
  One rule serves both: heads kept longer than the rest would need a rule of their own, and a rule's filter is a
  literal prefix, while each app's heads are under its own `apps/<app>/names/`. A rule for the heads alone needs a key
  layout with every app's heads under one prefix, as `names/<app>/<name>`, which moves every head and every policy's
  prefixes; that is not worth it while the two lengths are the same. Locally `docker/lifecycle.json` keeps 1 day.
- **The sweep's grant is the least that lists what it reads, and it deletes no version.** An app's session may list
  `apps/<app>/values/*` (`s3:ListBucket`), for its names and their objects, and the versions of `apps/<app>/names/*`
  (`s3:ListBucketVersions`), for each name's history, each by an `s3:prefix` condition. It lists no key of `names/`,
  as the sweep no longer finds names there, and no version of `values/` or of anything else. It reads a version with
  `s3:GetObjectVersion`, which it held already for links, and has no `s3:DeleteObjectVersion`: its delete is a delete
  marker, and only the lifecycle removes a version, so no app's credentials, a sweep's or a guest's, can shorten the
  history. The prefix of a listing of versions is matched as that of a listing of keys is, so one of
  `apps/<app>/names/` or of a head's key is allowed, and one of `apps/<app>/`, of `values/`, of another app's or of
  no prefix is refused; `isolates_apps` asks each of RustFS with an app's minted credentials. A listing of versions
  also gives each version's owner, an account's canonical ID; serve reads only the key, the id, whether it is the
  latest and whether it is a marker, and the listing never reaches the guest.
- **A name's objects are listed in one go, and those of a large name sixteen ways.** S3 lists 1,000 keys a request, in
  sequence. A name near `DATA_MAX` in values just over the 4 KiB inline limit has 4 Mi objects: 4,200 requests one
  after another, some 5 minutes at 75 ms each, which is longer than a run, so such a name would be passed over every
  run. But almost every name has a page of objects or fewer, and an idle app must cost nothing it need not, so
  `sweep::unnamed` lists `values/<name>/` once, as before, and if the listing ends within `PAGE` (1,000) keys, those
  are all of it and the name costs one request. If a 1,001st key arrives it drops that listing and lists by digit. An
  id is 128 random bits in hex, so a name's objects are spread evenly over the 16 digits an id may begin with, and
  `sweep::partition` lists each digit's on its own, all at once. The listing of a digit begins after `<before>g`,
  where `before` is the digit before it (`list_with_offset`, which S3 does with `start-after`), and the stream is
  dropped at the first key from `<digit>g`. An id has a hex digit second and `g` follows `f` in key order, so every id
  of the digit lies between those keys and none of another digit does. Each listing is then 263 requests for the 4 Mi,
  some 13 to 26 s at 50 to 100 ms, with 16 requests under way at a time. These figures are assumptions, not
  measurements. The digit is also checked of each id, so that an id is in one listing and only one whichever way a
  store treats the offset, and a key that is no id is passed over, as it was. A listing that errors fails its name,
  and nothing is deleted from it.
  The cost of the first listing is read from `object_store` 0.14.2: its listing is a stream of pages, and
  `stream_paginated` (`client/pagination.rs`) asks for the next page only when the stream is polled past the one it
  has, so a name of 1,000 keys or fewer never makes a second request, and stopping at the 1,001st key makes exactly
  two. A name over a page then costs those two and a request or more for each digit: about 18 where it cost 2, and
  for the largest name the same 4,200 requests, but sixteen at a time. The unit tests plant names of 0, 1, 1,000,
  1,001 and 2,000 ids and count the listings begun (1, and 1 + 16 for the last two) and the keys read (the 1,000, and
  for the larger the 1,001 of the first listing and no more than the keys there are and one past each range); and
  put ids at both ends of every digit's range and in the middle, with keys that are no ids among them, to show that
  each id is found once and that a run deletes each once.
- **The number of names an app has bounds a run, and that is a known limit.** A sweep lists the app's `values/` for the
  names that have objects (a delimiter listing, 1,000 prefixes a request, which `object_store` cannot begin part of
  the way through), and holds them: about 65 bytes a name, so 1M names are 65 MB of serve's 1,024. At 75 ms a
  request, which is an assumption, 1M names are 1,000 requests, some 75 s of a run's 240, and every run lists them
  again; 3M names are a run, and leave no time to sweep. The sweep of a name is the nearer limit: the listing of its
  versions, their reads, a node or more and its listing are four to six requests in sequence, some 150 to 200 ms, so
  a run sweeps 1,200 to 1,500 names. An app with a thousand is swept every run; one with 100,000 is gone through in
  some 70 runs, a pass at a time from the cursor; one with 1M in some 900, as a third of each run is its listing.
  Nothing is lost by that, as what a sweep leaves is only space, and nothing is built for it, as no app is there. When
  one is, the remedies are to sweep several names at once (the reads, the marks and the listing are concurrent
  already; the loop over the names is not) and to list `values/` from the cursor, which needs a request of our own to
  `ListObjectsV2` with `start-after` and a delimiter. `names/` is not listed: a name's heads are found by its own key.
- **The sweep schedule lives and dies with an app's others.** `deploy::wanted` makes the schedules an app is to have,
  its cron entries and its sweep, and `deploy::changes` compares them with those that exist: it creates what is missing
  first and then deletes what is extra, so the first deploy makes the sweep and later ones leave it. There is no command
  that removes an app; removing one is reconciling it to nothing wanted, which deletes every entry, the sweep included,
  so none of an app's is left. A sweep entry that outlived its app would only be answered 404, as the app has no
  release. `src/deploy.rs` tests this where the cron schedules are reconciled: the sweep is made, is the same each
  deploy, stays when a cron job goes, and is deleted with the rest when nothing is wanted; and a new interval is a new
  sweep, made before the old one is deleted, with the cron jobs left as they are. An app has 49 cron jobs at most
  (`CRON_MAX`), so with its sweep it has 50 schedules, and one page of 100 reads them all.
- **The sweep's tests pass the clock in.** `sweep::run` takes the time to measure ages from and the instant to stop
  at, so the unit tests use a time two grace periods on, and tokio's paused time (the dev-dependency feature
  `test-util`) for a run that is out of time, with a store that counts its requests, and a bucket's versions
  (`Kept`) that a test puts, deletes and expires, listed a few at a time with the delete markers first, as RustFS
  lists them. They check that a tree's objects are kept, that a stray goes only when it is past the grace, to the
  millisecond, and that a name whose marks fail loses nothing; and of the history, that what a noncurrent version
  alone names is kept while the version is and goes once it has expired, that a deleted head's objects are kept while
  its versions are, that a delete marker is no root and is not read, that the latest version gone is an error and a
  noncurrent one gone is no root, that a refused read or a head the listing misses loses nothing, that a name's
  versions are its own key's and not a longer one's, that the versions are marked into one set under one bound, and
  that a name over either bound is passed over and logged.
- **The node cache is per process,** a byte-capped LRU (16 MiB, `CACHE_MAX`) of immutable nodes, keyed by the object's
  full path and its version (app, name, id, version), so it can never be stale and a hit is exactly the object that
  a read would fetch, and never one of another app's. A node a commit uploads is put in it, since the next request is
  the one that will read it. It is not the cache of heads above, which waits for numbers: a head is the one mutable
  thing, and the cache never holds it. Large values are cached too, 32 MiB of them, keyed the same way.
- **A turn logs its tree at debug,** next to the head's logs: the GETs, the PUTs, the cache hits, the depth reached and
  the bytes read and written, with the splits, merges and root collapses, so that the cost model in the plan can be
  checked against a real run.
- **The guest sees only what it can act on of a tree error.** A name that is over a budget is told which, with the
  limit. Anything else (a GET that failed, an object that does not match its hash, a node over its limit) is logged in
  full, with the object, its version and the detail, and the guest gets that the name could not be read or written.
  The detail names the app's own storage, which is the host's to know.
- **A link read from a head or a node is checked before it is used.** Its id is 32 lowercase hex digits and its version
  at most 64 plain characters, so a head or node that has been changed cannot name another prefix or inject into a
  request, and it can name only objects of its own name. Unknown fields are refused. The hash checks the bytes.
- **The unit tests use nodes of about a kilobyte,** as a limit of the test and not of the product: with 64 KiB nodes
  a test would need hundreds of thousands of entries to split, let alone merge. The tiny limits (a node of 1,400
  bytes, 8 bytes inline) are what let the same code be run through splits, merges and collapses on a four-level tree
  against a `BTreeMap` of the same operations, and the store checked after every round against what the tree names, so
  that a lost or deleted object fails the test. A mutation (a skipped separator fix) was shown to fail it.
- **A write is admitted under the tree's write lock, so the answer cannot lose one.** `Turn::writable` checks that the
  turn has not answered, takes the tree's write lock, and checks again; the commit marks the turn answered, and then
  takes the lock before it reads the tree. A write that saw the turn open with the lock held is in the commit, and one
  that comes later fails with `access-denied`: none falls between. Lock order, to avoid a deadlock: the tree, then the
  turn's state. This was a gate of its own, an `RwLock` whose permit every change held while it was made, for the file
  system's write streams, which were admitted when the guest made a write and applied by a task of their own later;
  with them gone, the tree's lock is the gate.
- **A `wasi:keyvalue` write is one atomic batch of tree edits,** so that a failure leaves the tree as it was. A tree
  edit that fails for any reason but a budget, or whose caller goes away half way, poisons the tree: every later change
  fails and the turn cannot be committed, as the tree may be half-edited. A budget error (`Full`) changes nothing and
  does not.
- **A large value that is being fetched is waited for, not fetched again.** A value is fetched by one reader at a time
  (`Flight`, in `crates/tree`): one that comes to a value that is being fetched waits, and then looks in the cache,
  where the value is checked against its hash again, so nothing is taken on trust from the one that fetched it. It was
  built for the file system's read-ahead, where a reader faster than the store caught up with its own fetches (19 GETs
  for a file of 12 blocks), and serves now when two reads want one value at once, as a `get-many` that names a key
  twice, or two turns on a name, can. A fetch that is cancelled lets the next in turn fetch, and the gate is gone when
  no one comes to it. The gate counts the flights that come to it, under the map's lock, and a flight counts itself
  out when it is dropped, including one that is cancelled while it waits. An earlier version told by the reference
  count of the gate when to remove it, which was right only if a cancelled read dropped what it waited on before the
  flight, an order that nothing promises. Tests cancel a waiter at the gate while the one that fetches is under way,
  and cancel reads at every moment of a fetch, and the gate must be gone after them. Nodes are not gated: a node
  fetched twice costs one GET of at most 64 KiB.

## Idempotency keys

- **A turn with an `Idempotency-Key` applies once: tric records the outcome with the effect, and keeps no body.** A
  turn's commit writes, in its tree and so in the same compare-and-swap, the key's SHA-256, the request's fingerprint,
  the status and `Location`, at `i/<day>/<key>`. A turn sent again with the key finds the record when it
  opens, and is answered with that status and `Location`, `Idempotent-Replayed: true`, no body and no `ETag`, without
  running. The record lands with the turn's writes or not at all, so a retry is answered as it was only if the first
  landed. Two other designs were weighed:
  - Keeping the whole answer, as Stripe's API v1 keeps "the status code and body of the first request", 500s included
    (<https://docs.stripe.com/api/idempotent_requests>). A body may stream and be of any size, and may hold tokens or
    personal data, which would sit in the name's tree for a day; tric has no notion of a caller, so it could not check
    that a replay goes to whoever had the first answer. A replay with no body tells the client what it needs, that the
    turn landed and how it ended; a GET reads what it made.
  - Leaving it to apps. An app can keep its own record among its keys, which commit with its writes, but each would
    build the same thing again, and the host checks a turn's conditions before the app runs: a retry with `If-Match`,
    whose first try landed and moved the head, would get 412, and not the first answer.
- **The sources.** Stripe v1 keeps a key 24 hours at least, takes up to 255 characters, and errors if the parameters
  differ (above). Stripe v2 runs a request that failed again, rather than replaying the failure, and keeps a key 30
  days (<https://docs.stripe.com/api-v2-overview#idempotency>). EC2's `ClientToken` is up to 64 ASCII characters, and
  a retry with other parameters fails with `IdempotentParameterMismatch`
  (<https://docs.aws.amazon.com/ec2/latest/devguide/ec2-api-idempotency.html>). PayPal's `PayPal-Request-Id` returns
  the latest status of the first request (<https://developer.paypal.com/api/rest/reference/idempotency/>). Google's
  `request_id` returns the earlier response, or the resource's current state once that is no longer possible
  (<https://google.aip.dev/155>). Adyen keeps a key of up to 64 characters 7 to 14 days, and answers a duplicate in
  flight with 422 or 409 (<https://docs.adyen.com/development-resources/api-idempotency>). The IETF draft, which
  expired in April 2026, makes the header a Structured Field String, a fingerprint a MAY, a key reused with another
  payload 422 and one still in flight 409, and says the expiry SHOULD be published
  (<https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header-07>).
- **A key is one field of 1 to 255 bytes of visible ASCII other than a comma, taken as sent, else 400.** 255 is
  Stripe's limit. The value is not parsed as a Structured Field, so `"k"` and `k` are two keys: a client sends the same
  bytes again, and there is nothing to get wrong. No space or tab, which a proxy may trim or fold, and no comma: Lambda
  joins a repeated field's values with commas, so `a,b` from CloudFront could be two fields sent, which tric refuses
  locally; with no comma, a key is the same key, or refused, wherever it comes in. Only the key's SHA-256 is kept, and
  the key is never logged. Only a turn reads it: a snapshot changes nothing, and a request
  without a name commits nothing to record it with. A WebSocket's events do not carry the upgrade's key, which would
  answer each event after the first as the first.
- **The fingerprint is the SHA-256 of the method, a space, the path and query, a line feed and the body.** Headers are
  not in it, so a retry is not told apart by its `Content-Type` or its date. A keyed body past `REPLAY_MAX` (6 MiB) is
  413, as only a kept body can be hashed before the turn runs; hashing a body as it streams into the guest, and
  recording after, would be more code for uploads that a client can send without a key.
- **Every keyed turn that commits records, whatever it changed.** A turn that changed nothing still puts a head, to
  hold the record, so a retry is answered as the first was even when what the first found has changed since: a
  create-if-absent that found the thing there answers 200 again, and does not create it after it was deleted. The cost
  is a put for a keyed turn that changed nothing, which a client that sends a key means as a write. A 5xx, a trap or
  an error discards the turn and records nothing, so a retry runs it again, as Stripe's v2 does. A 4xx that commits
  records its status, as its writes landed.
- **The record is looked for on each pass of `Turn::open`, before the conditions.** So a retry is answered at once, even
  if its `If-Match` no longer holds because its own first try moved the head. Of duplicates sent together, the first to
  commit records; one that ran beside it loses the compare-and-swap, runs again, finds the record and replays. One that
  loses and may not run again, having sent an unsafe request or reached its deadline, answers 503 with `Retry-After` as
  any such turn does, and not the draft's 409: nothing says whose key the turn that beat it was under, and a 503 with
  `Retry-After` already asks for the retry that will then replay.
- **Another request with a key that has a record is 422,** as the draft says, and as EC2's mismatch is an error.
- **Records are kept by the day they were made, for one to two days.** A record is at `i/<day>/<key>`, the day being
  Unix days in six digits, so the records sort by age. A lookup reads yesterday's, today's and tomorrow's, so hosts
  whose clocks differ by up to a day agree. Each commit that puts a head first deletes up to 1,000 records made before
  yesterday, scanning from the oldest and stopping at the first that is not; it reads one key first, so a name with
  nothing to delete pays one lookup. A record is not read after the day after its own, and a name that no turn
  commits to keeps its records until one does, counting against its budgets. A name too full for a record fails the
  commit with `TooLarge`, 500, and not with a 503 that would have the client try what fails again. The draft's SHOULD
  is met by `docs/plan.md`, which states the expiry.
- **The marker is Stripe's `Idempotent-Replayed: true`.** A client that ignores it can take a replay for a new
  outcome: CVE-2026-34210 is mppx accepting a replayed Stripe PaymentIntent as a new payment, as it did not check the
  header (<https://nvd.nist.gov/vuln/detail/CVE-2026-34210>). A replay carries no `ETag`, as the head may have moved.
- **A key changes no status code, and leaves one case.** A turn that sent an unsafe request and did not land has no
  record: one that lost its race, one whose commit failed before its put, or one whose app answered 5xx. Its answer is
  not replayed, so a client that retries it runs it again, and sends again what it sent. An app that calls out should
  pass a key derived from its own on to what it calls, so the far side applies it once.
- **The outbox's `Idempotency-Key: <commit>/<n>` now counts at home.** A background request to the app's own origin
  that is delivered again within a day is applied once. A held request that the app gave a key of its own keeps it,
  and gets no second, which tric would refuse at home with 400: the app's key may be one it derived to dedupe across
  commits, and it knows best.
- **What was made simple, as there are no clients yet:**
  - no `Content-Location` is kept or replayed, as with no body it says little;
  - the key is the bytes sent, checked byte by byte and by its length, with no Structured Field parsing;
  - every keyed commit records, so only a turn that did not commit goes unrecorded;
  - a duplicate in flight that may not run again gets the 503 any such turn does, and not a 409 of its own;
  - the fingerprint hashes a copy of the method, target and kept body (6 MiB at most) with `store::hash`, rather than
    streaming them into a hasher;
  - a `Location` that is not visible ASCII is not recorded;
  - a record's day is its expiry, so it holds no time of its own;
  - nothing is configurable: the limits are constants in `src/name.rs`.

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
- **The local router answers 202 to an outbox event,** then relays it in a task, with the same backoff and 24 hour
  window as on AWS, and in memory: it has no Scheduler and keeps no object, and a restart loses what waits.
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
  given, and wit-dylib's stack in the task's context (`StackPointer::TaskContext`, the option wit-dylib names for
  WASI 0.3 and later).
- **Its runtime is built for `wasm32-wasip3`,** as the runtime the crate embeds imports 0.2. The source is the commit
  tagged v0.4.5 (`e563c6d6`), fetched by sha256 and built by the fixture toolchain, its C by wasi-sdk 34's clang, with
  upstream's flags and an export of `__wasm_library_tls_info`, by which wit-component gives a library thread-local
  storage of its own. `docker/qjs/runtime.patch` changes two things:
  - a task's state between its callbacks is kept in a thread-local, as wit-bindgen 0.62.0 keeps its own, and not in
    the component's context slot 0, which wasi-libc keeps for the task's stack on `wasm32-wasip3`;
  - the WASI 0.1 adapter is not linked, so its state is not reset after the build-time run.

  A unit test runs two requests at once on one instance of the JavaScript fixture (the first waits 200 ms, and the
  second answers while it waits), and each gets its own answer, so each task's state is kept apart.
- **There is no shim for `fetch`, `Request` or `URL`.** They are not WASI, and tric invents nothing.
- **The JavaScript fixture skips `/print` and `/fs`,** which try stdio and a file system with nothing preopened:
  QuickJS has neither.
- **A JavaScript component imports WASI 0.3 alone,** as a Rust one does, and exports an `init` that tric never
  calls.

## WebSockets

An app speaks Pushpin's WebSocket-over-HTTP, and a little of GRIP, so it runs the same where `tric dev` holds the
sockets and where API Gateway WebSocket does, on AWS. A probe on AWS, behind CloudFront, settled the AWS side.

- **Behind the cargo feature `ws`.** Off, nothing changes and nothing more is compiled. On, `tric dev` adds
  tokio-tungstenite, for the handshake, which it checks as RFC 6455 says, and the framing; and hyper-util, for the
  upgrade, which reqwest links already. The AWS package is built with it.
- **The same URL.** CloudFront hides `Upgrade` from its functions, so the viewer-request function takes a request with
  `Sec-WebSocket-Key` as an upgrade, sends it to API Gateway with `updateRequestOrigin`, at `/ws`, and puts the path
  in `X-Forwarded-Path`. A separate host for sockets would be one more name to know.
- **The origin secret is checked at `$connect`.** API Gateway WebSocket has no resource policy and no WAF of its own,
  so the secret is the gate, as it is for the function URL.
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
- **An event is a turn.** A `5xx`, an answer that is not well-formed events, or one that is late (the engine's 10 s)
  ends the socket. The app is told `CLOSE`, with the client's code if it gave one, of a socket the client closed, and
  `DISCONNECT` of one that ended any other way but its own `CLOSE`.
- **A socket's messages run concurrently, and arrive unordered,** as API Gateway invokes them; `tric dev` runs 16 of
  one socket's at once. An app that needs order puts a sequence number in its messages. Ordering them would mean a
  lock or a queue tric would have to invent. `DISCONNECT` can arrive while earlier messages still run: it deletes the
  record, and an event after it finds no socket and is dropped, with a log line.
- **GRIP is a channel and a publish, and no more.** A channel is a name, of the app. A publish is a `POST` to
  `/publish/` at the app's own origin with `Prefer: respond-async`: held, as every background request is, and taken
  after the turn's other held requests, if the turn commits, and taken again if its event is delivered again. Without
  that header, it is a call to the app like any other. Messages are text: `content-bin`, other formats and `action`
  drop the publish, with a log line.
- **A publish may be sent twice.** It was sent at most once, as serve took the commit out of the head's `pending`
  before it handed the publish to the router, so an event delivered again was dropped. The record that lets an event
  through now stays in the name's tree for two days or more, so a delivery that the router takes for failed after
  serve answered (the delete of the waiting object fails, or serve's answer is lost) and that is tried again publishes
  again, as a background request is sent again. Sending once would take a write for each delivery, which is what the
  records saved. An app whose sockets must not see a message twice puts an id in its messages.
- **The subprotocol is echoed.** The app's `OPEN` answer sets `Sec-WebSocket-Protocol`, and tric copies it to the
  handshake's answer; a browser fails the handshake without it.
- **Every handshake header is sent on every event,** less the upgrade's own but for the subprotocols offered, as
  Pushpin does. `Authorization` is one, so the app can check it on every event. A client can vary only its own
  socket's headers. `If-Match`, `If-None-Match` and `Idempotency-Key` are the upgrade's alone: as every event is a
  turn, an `If-Match` would refuse each after the first commit with 412, and a key answer each after the first as the
  first.
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
  it (494 lines). When it landed, `src/ws.rs` was 494 lines, and its hooks elsewhere added 63, most of them the outbox
  handing back what a delivery published, so the budget was missed by 63. No feature was removed.

## The tree crate

The tree is the crate `tric-tree`, in `crates/tree`, and the root is a workspace with tric as its root package. It was
`src/tree.rs`, and moved whole, by `git mv`, before it changed.

- **The crate knows no tric concept:** no apps, names, turns, Lambda or Wasmtime. It is Apache-2.0 and edition 2024,
  as tric is, and both are published from one tag at one version, `[workspace.package]`'s, tree first.
- **Its storage is `object_store`'s `ObjectStore` and a prefix, and no trait of its own.** The tree needs a get by
  version and a put that gives the version back, which every `ObjectStore` has, and nothing more: no store needed a
  trait, so none was made. It deletes nothing, so it asks for no delete. tric's policies stay in `src/store.rs`: a 403
  reads as missing, only a versioned bucket deletes, and the process's S3 clients are shared. The crate takes
  `object_store` by caret, from 0.14.2, and tric pins `=0.14.2`.
- **The root and its swap are the caller's.** `Tree::open` takes a `Root` and `Tree::finish` gives one, and the tree
  keeps it nowhere; after a failed swap the caller calls `Tree::reset`. tric keeps the root in a name's head, and swaps
  the head, as before. A `Root` is the old `Shape`, field for field and as JSON, and the node format is unchanged, so
  every head and node already written reads as it did.
- **Errors are the crate's `Error`, which a caller matches.** `Full` (with `Full::Entries` or `Full::Data` and the
  limit), `TooLarge`, `Corrupt`, `Store` (an `object_store` error) and `Poisoned`; it is a `std::error::Error`, so `?`
  makes it a `wasmtime::Error` in tric. `Full` is only what a write would have gone over, and not an error of its own:
  the `Error` that holds it says so. tric's `fail` matches `Full` where it used to downcast, and tells the guest the
  name's limit in the words it used before. (There were two, of keys and of files.)
- **The limits are one `Config`, with defaults.** The node (64 KiB), inline (4 KiB), budget (8 MiB) and blob (1 MiB)
  defaults are the old constants. Entries and data are unbounded by default, as they are policy: tric's `tree::config()`
  sets 4 Mi and 16 GiB (`ENTRIES_MAX`, `DATA_MAX`), and both stay in tric. How much is under way at once is not a
  limit, and is the crate's constant `IN_FLIGHT` (16), the uploads of a flush and the reads of a mark: it was a field
  of the config, which only ever held 16 and had to be guarded against 0. tric's sweep reads heads that many at a time
  too, and takes the number from the crate (the file system read its blocks so).
- **A path is the caller's prefix and an id.** `tric_tree::object(prefix, id)` is where an object is, directly under
  the prefix, and `tric_tree::number` reads an id back, for a collector that lists the prefix. tric's `tree::prefix`
  is `apps/<app>/values/<name>`, as before.
- **The cache is given, as an `Arc<Cache>`, and is not a static.** A store carries its cache: the S3 stores of a
  process share one, a static in `src/store.rs`, and a memory store has its own, as before. The crate's `Cache` keeps
  16 MiB of nodes and 32 MiB of values by default, the old sizes.
- **The inode counter is a number kept with the root, `Tree::sequence`, whose meaning is the caller's.** A counter
  in the root is what keeps inode numbers from being reused across turns: it is in the root `finish` gives, and a
  `reset` gives back what a lost turn took. Keeping the number and dropping its meaning keeps that guarantee, and the
  JSON of every head written so far (`seq`), while the rule is the file system's own (`fs::ops::alloc`: one after the
  last, and after the root's 1). A key in the tree would have changed what heads hold, and an inode allocator in the
  crate would have been a tric concept. A caller with no use for it never sees it, as 0 is not written. The file
  system was its only caller, so tric now sets no `seq`, and the heads it writes hold none.
- **The hash and the random ids are private helpers of the crate,** the same SHA-256 in hex and 128 random bits in
  hex as `store::hash` and `store::random`, which stay in tric for its heads and commits.
- **A commit deletes nothing, and the crate offers the collector a mark, and nothing to delete with.** The sweep is
  the only collector: its survey, its grace and its deletes stay in tric. `Tree::mark` walks every object a root
  reaches, its nodes and the values that are objects, each node checked as any read is, and at most `max` nodes; it
  adds to a set it is given, and passes over what the set holds, so the marks of several roots share one set and read
  each object once. A mark from an empty set, `Tree::reachable`, was public too, and only the crate's tests called it,
  so it is one of their helpers now.
- **One tree type, and a walk of its own for the mark.** A snapshot is a tree that is not written, and `Tree::fork`
  is the copy that outlives a turn, so a snapshot type would only repeat the reads. The mark does not share the
  scans' walk: it reads level by level, unordered and up to `IN_FLIGHT` at once, and keeps nothing in the cache,
  where a scan reads in key order, to a limit, through the cache. `Tree::fork` was for a file descriptor that outlived
  the answer, and tric calls it no more, nor `Tree::values` and its `Values` or `Tree::entries`, the file system's too.
- **A spill of nodes merges first, as the commit does.** Both are one `flush`: what is thin merges and what is empty
  is dropped (`fix`), a root of one child gives way to it, and then the nodes upload. A spill used to upload the nodes
  as they were, so its upload dropped the empty ones itself and put right a root it had emptied: a second path for
  what `fix` does. A merge at a spill may be filled again by a later write, as one at each delete would be, and may
  read a neighbour, but a spill is past the budget and rare, and most of its merges are ones the commit would make.
- **The tests moved with the tree,** the model test against a `BTreeMap` among them, and its counting store is
  `tric_tree::testing`, behind the `testing` feature, which tric's dev-dependency turns on; tric's tests take it as a
  `Store` from `tree::counting`. Of the tree's 21 tests, 19 are the crate's as they were, and one is renamed for the
  root (`an_empty_tree_is_an_empty_root`, which was `an_empty_tree_is_not_in_the_head`). The test of inode numbers was
  two: the crate's `a_sequence_is_kept_in_the_root`, and the file system's
  `inode_numbers_count_up_from_the_root_and_go_on_from_a_committed_tree`, which went with it. The crate adds
  `a_mark_shares_what_it_has_seen_across_roots`, and a doctest on the memory store.
- **The gate covers the crate:** `fmt` of the workspace, `clippy` of the crate with all its features as well as of
  tric's two feature sets, the crate's tests and doctest, and `cargo publish --workspace --dry-run`, which packages
  the crate and then tric on it.

## Code

- **There is no backward compatibility.** A binary reads only the formats it writes: heads, trees, releases,
  manifests, the outbox's and the sockets' records. A change to any of them is installed fresh, with `tofu destroy`
  and `apply`, and tric carries no shim, alias, default or migration for an older one. The serde defaults that
  remain are a format's own optional fields, or formats that are not tric's (AWS's XML, `credential_process`).
- **Imports are as rustfmt leaves them.** Its `imports_granularity`, which is unstable, made `src` longer at every
  setting (`Module` by 41 lines, `Crate` by 90, `One` by 130), and clippy has no lint for it.
- **The AWS SDK is not used, for now:** S3 stays on `object_store`, and STS, Lambda and Scheduler on tric's own calls.
  A study at d2125b4 put every AWS call on the SDK and passed the gate. It cut `src/` from 2,967 code lines to 2,813,
  and it cost:
  - 36 more crates in the router, the trusted part (257 to 293);
  - a stripped binary 16% larger (27.1 MB to 31.5 MB), a zip 15% larger, and a clean build 48 s longer;
  - 3 of 5 release builds killed for memory on a 7.75 GiB Docker VM, at a peak of 6.8 GiB;
  - a turn run twice: the SDK tries an invoke up to 3 times, so an answer lost after a commit commits again;
  - a response silent for 20 s cut by the SDK's stalled-stream protection, by default, which breaks an SSE app that
    pauses;
  - `tric dev` needing an S3 endpoint, as the SDK has no store in memory.
- **The host and the fixtures have a toolchain each, until 12 November 2026.** The host is built by Rust 1.99.0, and
  the fixtures by 1.100.0-beta.4 (`TRIC_FIXTURE_TOOLCHAIN` in `docker/build.Dockerfile`), as
  `wasm32-wasip3` is stable from 1.100. Both move to 1.100.0 when it is stable, on 12 November 2026. Serving 0.3 alone
  switched nothing on in the engine: its component-model async was already callbacks only, with no stackful async and
  no extra builtins.
