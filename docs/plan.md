# tric: the build plan

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
- **An app has no files.** `wasi:filesystem` is linked so that a guest whose language imports it instantiates, but no
  directory is preopened, so no path reaches anything. An app's state is its names' keys: every object stays under
  `apps/<app>/` with the app's own credentials, and is read by version and checked against its hash.
- **`Forwarded` is the only internal marker.** The router replaces it on every request. `for=_cron`, `for=_tric` and
  `for=_ws` can come only from tric.
- **Middleware runs with all of the app's capabilities.** A middleware component is pinned by `sha256:` digest.
- **Background requests are stored until delivered, headers included.** They sit in Lambda's queue and, once a delivery
  has failed, in `outbox/` in the bucket, which only the router writes, until it is delivered or 24 h are over.
  Delivery is at least once.
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
  - any other name opens as a read-only snapshot;
  - a key is 1 to 256 bytes, and its value up to 1 MiB; a name holds up to 16 GiB of values in up to 4 Mi keys (see
    "State", below).
- **No files.** `wasi:filesystem` 0.3 is linked, so that a language whose standard library imports it instantiates, but
  nothing is preopened: every open fails, and a turn's state is its keys.
- **Turns commit at the answer.**
  - A turn sees the name's last commit plus its own writes.
  - Writes are held until the handler returns its Response: small ones in memory, and large values in objects that
    nothing refers to until the commit.
  - Any status except 5xx commits the writes and the held background requests together, before a single header
    leaves.
  - A 5xx, a trap or an error discards both.
  - After the answer, writes fail, and the body streams on, read-only.
- **Turns are optimistic.**
  - If another turn commits first, the instance is discarded and the request re-runs, which no one can observe.
  - It re-runs until its deadline, 5 s after it began, and only while it is repeatable: while tric holds all its
    body, which is 6 MiB or less, and it has sent no unsafe outbound request.
  - A turn that loses a race and may not re-run lands nothing, and answers 503 with `Retry-After`. What it sent stays
    sent.
- **The `ETag` is the name's last commit.** It is the id of the commit that last wrote the name, and `If-Match` is
  honoured, with 412 on a mismatch.
- **`Idempotency-Key` applies a turn once.** A key is one field of 1 to 255 visible ASCII bytes other than a comma,
  else 400.
  - A turn that commits records in its head's tree, whatever it changed, the key's SHA-256, the request's fingerprint
    (method, path and query, body), its status and its `Location`, for at least a day.
  - The same request with the same key is then answered with that status and `Location`, `Idempotent-Replayed: true`
    and no body, without running or waiting; another request with the key gets 422.
  - A turn that answers 5xx records nothing. A keyed body too large to keep is 413.
  - Safe methods, and requests without a name, ignore the header.
- **Calls are fetches.**
  - A `fetch` to the app's own origin runs in-process, as a request with `Forwarded: for=_tric`.
  - An unsafe call is an unsafe outbound request, after which the caller does not re-run.
  - A cycle answers 508.
- **Background requests.** An outbound request sent during a turn with `Prefer: respond-async` (RFC 7240):
  - gets `202` and `Preference-Applied: respond-async` at once;
  - is held, and sent only if the turn commits;
  - carries `Idempotency-Key: <commit>/<n>`, unless the app gave it its own, so one to the app's own origin that is
    delivered again within a day is applied once;
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
  - on AWS, API Gateway WebSocket holds them, at the same URL, and the router sends serve their events.

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
apps/<app>/names/<name>          a head: the tree's root and the budgets; changed only by If-Match / If-None-Match: *
apps/<app>/values/<name>/<id>    a node or large value of that name; written once, read by version id
native/<app>/<compat>/<sha256>   compiled code, written by the app's own tenant on a miss
outbox/<app>/<commit>            a delivery event that failed, waiting for its retry (router only)
ws/connections/<id>              a socket's record: its app, URL and handshake headers (router only)
ws/channels/<app>/<channel>/<id> a socket's subscription to a channel (router only)
```

- **A head** is plain JSON with these parts:
  - `tree`, which is absent while the name is empty, and holds:
    - `root`: the name's tree, its top node held inline (see "State");
    - `bytes` and `entries`: the name's budgets, exact at each commit;
  - `commit`: the random id of the commit that wrote it, new to each, so that no head's bytes, and so no head's `ETag`
    in the bucket, are ever another's, and a commit whose answer was lost can read the head to learn whether it
    landed. It is the name's `ETag` to clients.
- **Objects.** Each node or large value is immutable, named by a random id under its name, and read by the
  version id in the link that points to it, and checked against that link's SHA-256 and length.
  - A turn deletes nothing, whether it lands, loses its race, fails or is discarded: what a commit replaces, and what
    a turn that does not land made, is left for the sweep; see "Garbage". With versioning, a delete only adds a delete
    marker, so old heads still read objects by version id.
  - One lifecycle rule expires noncurrent versions after `noncurrent_days` (1 by default) and removes expired delete
    markers. A head's noncurrent versions are its history, which the sweep keeps whole.
- **A 403 is read as not found.** S3 answers 403 for a missing key when the caller can't list.

### State: one tree per name

A name's state is one copy-on-write B+tree: ordered keys in immutable objects, with the root inline in the head.
`wasi:keyvalue` is the view of it an app has. The turn model is unchanged, and the head's one compare-and-swap commits
or fails everything a turn did.

**What a key holds.** An app keeps its state in keys, within these bounds:
- A key is 1 to 256 bytes (`KEY_MAX`), and its value up to 1 MiB (`VALUE_MAX`, and the tree's `Config::blob`).
- A value of 4 KiB or less sits inline in its leaf, and costs no request of its own. A larger one is an object of its
  own, written once, read whole at its version and checked against its hash, and kept in the process's cache of values
  (32 MiB).
- A name holds up to 16 GiB of values (`DATA_MAX`) in up to 4 Mi keys (`ENTRIES_MAX`), the records below included.
- A `get-many` answers with up to 16 MiB of values (`BATCH_MAX`), reading up to 32 large ones at once.
- A turn holds up to 8 MiB of its writes, and spills the rest to the store (below), so it can write as much as the name
  holds.

**Why a tree, and why one:**
- One compare-and-swap commits a turn's keys and its records (of its `Idempotency-Key`, and of its delivery events)
  together, or neither. Two trees would need two heads, which S3 can't change together.
- The cost: a name whose tree is larger than one node (64 KiB) reads its keys through the tree, one more GET per level
  when cold, and its commits need one more PUT round per level. A tree that is a root alone, the common case, costs
  what a name costs today. The flat map this replaces held up to 1 MiB in the head and rewrote all of it at each
  commit.

**The format.** Keys are UTF-8 strings, ordered bytewise:

```
k/<key>              a key of `wasi:keyvalue`: its value
i/<day>/<key>        the record of an `Idempotency-Key`: the request's fingerprint, the status, `Location`
o/<day>/<digest>     the record of a delivery event, which is empty
```

- `<day>` is the Unix day a record was made, six decimal digits, `<key>` the key's SHA-256 in hex, and `<digest>` the
  SHA-256 of the event's JSON, whose `day` is the record's. A lookup of a key reads yesterday's, today's and tomorrow's,
  so clocks may differ by a day. A commit that puts a head first deletes up to 1,000 records of keys made before
  yesterday, and as many of events made two days or more before, from the oldest, so a key's record lasts one to two
  days, and an event's two to three, past the window its delivery is tried in. Records count against the name's budgets,
  and a name too full for one fails the commit with 500.
- A **leaf** is a sorted list of (key, item), and a **branch** a sorted list of (separator, link) to its children: the
  separator of a child is at most every key in it and above every key in the child before; the first child's is not
  looked at. Each node is JSON of at most 64 KiB (`Config::node`). An item is data (base64, inline up to 4 KiB) or a
  link.
- A **link** is `{id, v, h, n}`: a random 128-bit id (the object is `values/<name>/<id>`), the S3 version id (none in
  the local memory store), and the SHA-256 and length of the bytes. An object is read at its version and checked for
  both, so the head pins every node and value below it by hash. A link read from a head or a node is checked before it
  is used: the id is 32 lowercase hex digits, so it can't name another prefix, and the version is short and plain.
- A **head** is `{tree: {root, bytes, entries}, commit}`. The root node is inline, so the top of every lookup costs no
  request. `bytes` counts the data in values, and `entries` the keys, both exact: a commit lands only over the head it
  read, so the deltas it counts while editing are the totals.
- A turn's size estimates a link at its longest (208 bytes), so a node never encodes to more than the size it was
  split by. A branch holds about 300 links, and a leaf several hundred small entries (a large value's link is about
  230 bytes) but only a dozen or so values near the inline limit. So 4 Mi keys need three levels, the root included,
  or four when most of them hold values near 4 KiB.

**Editing.** A turn edits the tree itself, as LMDB and bbolt do in a write transaction. There is no overlay:
- A write descends from the root. Each node on the path is loaded (a GET if it is not cached, unless it is already in
  memory), copied, and changed. Nodes it did not touch stay links.
- Reads and `list-keys` use the same tree, so they see the turn's own writes and nothing more. A blind write descends
  at the write, so a GET per uncached level is paid there, not at the commit.
- An overfull node splits in halves by size, up to the root, which grows the tree by a level.
- At the commit, and when a spill uploads nodes, a changed node under a quarter full merges with a neighbour (and the
  two are split again if they do not fit in one), and an emptied node is dropped, as bbolt does. A root with one child
  is replaced by it. So the tree shrinks as it is emptied, and a name that is mostly deleted does not stay sparse.
- After the answer, a write fails with `access-denied`, and so does any write to a snapshot. A write fails early when
  the name's data or entries pass their budgets, counted exactly from the tree and the turn's edits. A batch over a
  budget changes nothing.

**Spilling.** The copies in memory, and large values held for upload, count against one budget of 8 MiB
(`Config::budget`), checked after each write. Past it, as LMDB spills dirty pages:
1. The held values upload, up to 16 at once, and the writer waits. Each item becomes a link.
2. If the copies still pass the budget, what is thin merges and what is empty is dropped, as at the commit, and the
   changed nodes upload bottom-up and become links, so each parent is changed, and in memory, in its turn.

What a turn uploaded and then replaced stays in the store, for the sweep. So a turn's memory stays about its budget
plus one write, however much it writes. A key's value over 4 KiB is held whole, and uploaded when the budget spills it
or the commit does.

**The commit,** at the answer:
1. Merge what is thin, and drop what is empty, from the leaves up.
2. Upload every node still in memory and every held value, bottom-up with at most 16 in flight, so that each branch
   embeds the links of its children.
3. Put the head, with the new root inline and the new budgets, by compare-and-swap, as now.

A tree that is a root alone has no node rounds: a commit is its large values, as today, and the head. A turn that
changed nothing puts nothing.

**The read path:**
1. The head is read as now: one GET, with its root node in it.
2. A lookup descends from the root. A node not in memory is one GET, at its version, checked by its hash. Nodes read are
   kept by the process in a cache of immutable nodes, byte-capped (16 MiB), keyed by the object's full path and its
   version (app, name, id, version), so an entry is never stale and never crosses apps.
3. A large value is one GET more, whole, as its hash needs all of it, and is kept in the cache of values (32 MiB), keyed
   the same way.

**Requests, and time.** AWS gives roughly 100 to 200 ms for a small object, which is not measured here, as no AWS calls
were made. A round is one such trip, and S3 bills per request, not by size. D is the tree's depth, a root alone being 1.

| What | S3 requests |
|---|---|
| A turn starts | 1 GET, the head, as now |
| A lookup in a root-only tree | none more |
| A lookup below the root | 1 GET per node not cached, D - 1 at most, in sequence |
| A value of 4 KiB or less | none more: it is inline in its leaf |
| A larger value | 1 GET, unless cached |
| A write below the root | the GETs of its path, D - 1 at most, unless cached or already copied |
| A commit | PUTs of held values (parallel), D - 1 rounds of node PUTs, 1 PUT of the head |

So a cold read of a large value takes D + 1 trips (3 or 4 in a big name), and a warm one the head's alone. A commit of
a changed large value takes D + 1 (the values, a round per level, then the head). A name under 64 KiB takes what it
takes today. A write of one key to a name of 1 GiB puts its value if that is large, a leaf of at most 64 KiB, the
branches above it and the head, and not the 1 GiB.

**Seeing it.** At debug, each turn logs its tree next to the head: GETs, PUTs, cache hits, depth, and the bytes read and
written, with the splits, merges and collapses of the root.

**Snapshots.** A read at an older head keeps working while newer commits land: every object is immutable and read by
version, and a commit deletes nothing.

**Garbage** is an object that no head a reader could be on refers to:
1. **Not the committer.** A turn deletes nothing, whether it lands, is discarded, is doomed, loses its race or fails. A
   commit whose answer is lost can't know which of its objects the head names, and a delete chosen then can remove a
   live node (`docs/decisions.md`).
2. **The lifecycle rule,** which the bucket has: a deleted object is a noncurrent version, and goes a day or two
   later (`noncurrent_days`, in whole days to midnight UTC). A replaced head is a noncurrent version too, and the
   versions it keeps are a name's history.
3. **The sweep,** last of the stages, and the only collector: it deletes what no head the bucket keeps names (below).

*Why it is safe for readers.* A reader holds a head it read at most 300 s ago (`TOTAL`) and reads objects by version id.
A delete adds a marker and leaves that version readable, and the lifecycle waits a day at least.

*What leaks.* Everything no head names any more, as nothing is deleted at a commit: what a commit replaced, and what a
turn made that did not land, whether it trapped, timed out, lost its race or failed, or its host died. It waits for the
sweep.

*The sweep* is a mark-and-sweep with a grace period, run by serve with the app's own credentials:
- It lists the app's names, and for each lists the versions of its head that the bucket keeps (ListObjectVersions),
  the current one, those it replaced and those of a deleted head, reads each by its id, and marks what their trees
  name into one set, under one bound. It then lists `values/<name>/` (in one request, if it holds a page of objects or
  fewer, and else as 16 listings at once, one for each first digit of an id, so that a name of millions of objects
  takes less than a run), and deletes what is older than the grace period (an hour at least) and not marked. No later
  head can refer to such an object, as it descends by commits from a head listed, and no attempt lasts an hour, which
  the build asserts against the limits of a turn. Only a key a tree could have made is ever deleted, and a head is
  read fresh. So a name can be put back as any version the lifecycle keeps, which is its rollback window; nothing
  puts one back yet, and `docs/decisions.md` has what that must keep to.
- It needs `s3:ListBucket` on the app's `values/*` and `s3:ListBucketVersions` on its `names/*` only, each by an
  `s3:prefix` condition, in the router's policy (`src/route.rs`), `infra/aws/main.tf`, `docker/router-policy.json` and
  the module's test, with assertions. The listing of a head's versions is also what tells a missing head from a
  refused read, which a GET can't. It has no `s3:DeleteObjectVersion`, so only the lifecycle shortens the history.
- It runs every `TRIC_SWEEP_EVERY` (a day by default; hours that divide a day, or days) for each app, from the
  Scheduler through the router and into serve, as a retry does, and never always-on. It deletes nothing from a name
  unless every version listed was read or had expired and every tree was marked whole. A run is bounded (240 s), and
  resumes where the last stopped. It logs what it reclaimed. A request to the events function's `cron` alias, with
  `{"app", "sweep": true}`, runs it on demand.
- The design of the trigger, and the arguments for its safety, are in `docs/decisions.md`.

**Limits.** All are policy: the representation has none of its own.

| Limit | Value | Why |
|---|---|---|
| Node | 64 KiB | the root is in the head, which every turn reads; a write rewrites one leaf |
| Inline | 4 KiB | a value this small costs no request of its own |
| A key | 256 bytes | as now (`KEY_MAX`); it sits in its leaf |
| A key's value | 1 MiB | as now (`VALUE_MAX`); one object, read whole |
| A name's data | 16 GiB | `DATA_MAX`: 16 Ki values of 1 MiB |
| A name's entries | 4 Mi | three levels, or four of values near 4 KiB |
| Held and copied, per turn | 8 MiB | memory, per turn: spilled when passed |

These are the numbers to revisit once they are measured: the node, inline and value sizes, and the 16 GiB and 4 Mi
budgets. A turn has no cap on its changes: it spills.

**Security:**
- A guest names a key, never an object: keys live inside nodes. Objects are under `apps/<app>/values/<name>/`, written
  and read with the app's own credentials. The name passes `is_name`, so it holds no `/`, and an id is random: nothing
  the guest controls forms a key. A link read back is checked before it is used, so a changed head or node can't point
  outside the prefix. The sweep's `ListBucket` on `values/` and `ListBucketVersions` on `names/` are the only IAM
  change, and list no other prefix.
- Reads are checked against the link's hash and length. A mismatch is an error, and the object, version and detail are
  logged and never given to the guest.
- Nothing is content-addressed, so there is no deduplication, within an app or between apps, and no way to learn from a
  write's speed that another tenant holds the same value.
- The guest sees wasmtime-wasi's stock interfaces, its file system among them, with no directory preopened: a path has
  nothing to resolve against, so it reaches neither the host's files nor the tree.
- A snapshot is read-only, and every size above is capped, so a guest can't take the host's memory or requests beyond a
  turn's limits.

**Structure and size.**

| File | What | Lines |
|---|---|---|
| `crates/tree` (was `src/tree.rs`) | nodes, links, edits, spill, merge, scans, the cache, the mark | 1,080 (built) |
| `src/name.rs`, `src/tric.rs`, `src/store.rs` | the turn over the tree; reading the head back | +70 (built) |
| `src/sweep.rs`, and the trigger | versions, mark, listing, delete, cursor; schedule, router, serve | 480 (built) |

The tree came to 1,080 lines, against an estimate of 800, because spilling, merging, the cache and the log are in it.
The sweep came to about 480 against 300, the walk and its cap, the shape of the keys it may delete, the listing by
digit, the cursor, the router's and serve's part, and the reconciling of an app's schedules, which gave deploy its first
tests, being the difference. The tree and `name.rs` came first and alone: the key tests' assertions hold on them.

A file system over the same tree, `wasi:filesystem` with inodes, blocks and directories as keys, was built in stages
too, with a core and a binding of about 1,700 lines of code (`src/fs.rs`, `src/fs/`) and the WebAssembly testsuite's
file system tests passing. It was removed on 13 October 2026; why is in `docs/decisions.md`.

**Stages.** Each was committed on its own and passed the whole gate: the tree, with `wasi:keyvalue` on it; the file
system, later removed; the sweep, with its trigger; and WASI 0.3 alone.

**Tests:**
- **Our own:** the tree against a `BTreeMap`, over random operations with tiny nodes so that it splits, merges and
  collapses, and grows to four levels, with what is in the store checked against what the tree names after every round;
  request counts with a counting `ObjectStore`, to show that a write puts only what it changed, and that a failed
  upload leaves nothing; a tampered object; links that try to leave the prefix.
- **e2e, on RustFS:** a 5xx discards and anything else commits; turns on one name serialise, and one that may not
  re-run answers 503; outside a turn on it, a name is read-only; a value too large to inline is kept as it was by a
  discarded turn, and whole through the sweep; and a guest that opens a file fails, and leaves its name no head and no
  object. Each test runs on `tric dev` and on a stack.

**Alternatives, rejected** (reasons and sources in `docs/decisions.md`): SlateDB; prolly trees; an overlay applied at
the commit.

**Decided,** by the choices behind the design (each with its reason in `docs/decisions.md`):
1. **The sweep is built,** last, for the leaks above, and keeps a name's history: `s3:ListBucket` limited to the app's
   `values/`, and `s3:ListBucketVersions` to its `names/`.
2. **The numbers** above, to be revisited once measured.
3. **No files.** An app keeps its state in keys; `wasi:filesystem` is linked with nothing preopened.
4. **A turn edits the tree and spills at its budget,** with no cap on its changes.
5. **Thin nodes merge,** as bbolt's do.
6. **JSON nodes,** not a binary codec, which would be a new crate.
7. **Nothing is deleted at a commit,** and the sweep is the only collector, so a lost answer cannot lose data.
8. **A name over 64 KiB reads its keys through the tree,** a GET more per level when cold, and not from its head.
9. **WASI 0.3 alone,** since 8 October 2026. 0.2 came first and 0.3 after, by stage, and 0.2 was then removed.

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
- **Retries.** On AWS the router tries a delivery once, in the `outbox` invocation. If that fails with a 5xx, a 429 or a
  failed exchange, it keeps the event in the bucket as `outbox/<app>/<commit>` and creates a one-time EventBridge
  Scheduler schedule, in a group of its own, which invokes the router's `retry` alias with a reference to it: the app,
  the commit, the number of tries and the deadline. Each try goes to serve as an outbox event does, and serve delivers
  it only if the name's head holds the record its commit made of the event's digest. A failure schedules the next try.
  - The window is 24 h from the first failure, as EventBridge's default retry policy has it, with exponential backoff
    (a minute, doubling to an hour) and jitter. `Retry-After` is a floor.
  - Any other 4xx is an answer, and is final. A delivered or dropped event deletes its object.
  - After the window, the router logs a warning with the app and the commit, and deletes the object. The record in
    the name's tree expires as the others do.
  - Locally, `outbox::relay` follows the same backoff and window in the router's memory, and a restart loses them.
- **The three inputs are kept apart.**
  - The router accepts outbox events only when invoked as `outbox`, retries only as `retry`, and cron events only
    otherwise.
  - serve may invoke only `outbox`, so a compromised app can't forge cron or a retry, and only the retry group's role
    may invoke `retry`.

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
  1. Drop the event unless it names this app.
  2. Wait for the sender's commit: read the name's head until it holds `o/<day>/<digest>`, the record the commit
     made of the event. Drop the event if the head, without the record, is no longer the one the turn started from,
     or still is after `SETTLE` (30 s) and 30 s more, as then the commit can never land.
  3. Send each request in order:
     - to the app's own origin: in-process, as a turn;
     - anywhere else: over the network, under the allow list.

  The record stays, so an event delivered again sends its requests again, with the same `Idempotency-Key`s.

  A 5xx, a 429 or a failed exchange answers 5xx, and the router retries as above. Any other answer is final.
- **It handles a `POST` carrying `Forwarded: for=_ws` as a socket's event,** which only the router sends: the
  socket's record, its id and the event. It runs the turn of it at the URL the socket opened at.
- **At a turn's answer, with background requests:** invoke the router's `outbox` alias first, then commit, recording
  the event's digest in the name's tree.
- **It is a plain HTTP server.** On Lambda, the Lambda Web Adapter translates.

### Local

- **`tric dev [path] [--allow host]… [-e K=V]…`** runs one app in one process:
  - state in memory;
  - `Forwarded` set from the socket's peer;
  - cron ticking in-process;
  - background requests through an in-process relay that backs off and honours `Retry-After`, for 24 h.
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
  - `events`, which takes only events, each source's by its own alias: `cron`, `outbox`, `retry` and `ws`. The `outbox`
    and `retry` aliases carry the async config: two retries and a 6 h maximum event age, and no destination. If the
    router itself fails on one of them, Lambda tries twice more and then drops the event.
- **serve:** per tenant (`PER_TENANT`), with no URL. Only the router may invoke it.
- **Sockets:** API Gateway WebSocket, which CloudFront sends a request with `Sec-WebSocket-Key`. It invokes
  `events:ws`, buffered, with no route response, behind a stage throttle.
- **The roles:**
  - the app role, which trusts only the router's role;
  - the router's role, which can assume the app role, HEAD `current`, keep `ws/` and `outbox/`, list `ws/channels/`,
    invoke serve, send to and close its own API's sockets, and create schedules in the retry group only, passing only
    the retry role;
  - serve's role, with its logs and `events:outbox` only;
  - the cron role, which can invoke `events:cron` only;
  - the retry role, which can invoke `events:retry` only.
- **The bucket:** versioning, the lifecycle rules (noncurrent versions, delete markers, `outbox/`, `ws/`), and short
  log retention.
- One `tofu apply` installs it all into a fresh account. The apply waits for the user's go-ahead.

## Changes the isolation forces on the approved design

1. **Outbox events go through the router.** serve can't mint credentials, so "invoke our own function" becomes
   "invoke the router's `outbox` alias". The router then invokes serve with the tenant id.
2. **The event's digest in the sender's tree.** Without it, an app that learnt another app's commit id (from an
   `Idempotency-Key` sent to it) could forge that app's deliveries. An event sent again byte for byte is the commit's
   own, and is delivered again, as any delivery may be.
3. **`Prefer: respond-async` with no commit is not applied.** This covers a request without a name, or one sent after
   the answer.
   - The request goes out at once, as an ordinary fetch, with no `Preference-Applied`. RFC 7240 lets a server ignore a
     preference.
   - Without a commit there is nothing in the sender's head to check, so such an event could be forged.

## Not in this build

- teams and per-team publishing;
- budgets and per-app limits;
- `tric logs`;
- release history commands;
- previews and forks;
- `Accept-Datetime`;
- a file system;
- S3 Express;
- head caches;
- GCP and Azure.

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
5. **Forged outbox events.** An event naming app B is dropped unless B's head holds the record of its digest.
6. **Cron** reaches the app through `route` as a `POST` with `for=_cron`.
7. **Native code** is written under `native/<app>/…` with the app's credentials.
8. **The app semantics** pass through `route` → `serve`: names, turns, `Idempotency-Key`, the outbox, self-fetch,
   middleware and outbound.

**`tofu validate`** passes, and the module shows:
- serve is per tenant, with no URL;
- serve's role has no S3 access;
- the app role trusts only the router;
- the outbox and retry aliases carry the async config and no destination;
- the router's S3 access is `ws/`, `outbox/` and a listing of `ws/channels/`, and it creates schedules in the retry
  group only, passing only the retry role, which can invoke `events:retry` only;
- lifecycle expires noncurrent versions and `outbox/`.

**The remote test, after the user says go:**
- an app answers through CloudFront, and streams, and `for=` is the viewer's address;
- two apps run in different tenants, and neither one's credentials read the other's data: IAM's policy simulator
  shows it for the minted session policy, and for the role alone that no release or component is writable;
- a direct call to the router can't forge `_cron`, `_tric`, `_sweep` or credentials;
- a keyed turn sent twice applies once, and the second answer is marked `Idempotent-Replayed: true`;
- the outbox, its retries and cron work;
- the guest has no files, and a turn that tries to write one leaves its name no head;
- values too large to inline are kept, each an object of its own, and read back whole after the sweep;
- racing turns each count once;
- each app's sweep is in the cron group at its minute, every `TRIC_SWEEP_EVERY`, and one sweep, through the cron
  alias, runs as the app's tenant: it keeps what a tree of any version kept names and what is under an hour old, and
  reports what it did; nothing else can ask for one;
- the lifecycle rules, roles and environments are as the module says;
- for decisions to come, the run keeps the heads' reads and writes in a mix of turns, and what each turn fetched and
  put;
- the run deletes what it made, and a later pass finds the minted policy in CloudTrail.
