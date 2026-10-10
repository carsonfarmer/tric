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
- **An app's files are its name's tree, and nothing else.** A descriptor is an inode number in that tree, never a host
  path or file descriptor. A path walk can't leave the tree: `..` stops at the directory the walk began at, and
  absolute paths and symlink targets are refused. Every object stays under `apps/<app>/` with the app's own
  credentials, and is read by version and checked against its hash.
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
  - any other name opens as a read-only snapshot.
- **Files.** The import is `wasi:filesystem`, 0.2 and 0.3, a real writable file system as Rust's `std::fs` and
  wasi-libc expect one:
  - the turn's own name is the one preopen, `/`; a snapshot is mounted read-only; a request without a name has none;
  - it is the same tree as the keys, so a turn's file writes and key writes commit or fail together;
  - sizes are bounded by stated limits, not by memory: a turn reads only the blocks it touches and writes only the
    blocks it changes (see "Files and state", below).
- **Turns commit at the answer.**
  - A turn sees the name's last commit plus its own writes.
  - Writes, of keys and of files, are held until the handler returns its Response: small ones in memory, and a large
    file's blocks in objects that nothing refers to until the commit.
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

  A turn's held requests total at most 1 MB. A name with 1,000 commits still undelivered refuses a new background
  request: the guest's fetch gets `503`, and the request is not sent, now or later.
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
apps/<app>/values/<name>/<id>    a node, block or large value of that name; written once, read by version id
native/<app>/<compat>/<sha256>   compiled code, written by the app's own tenant on a miss
outbox/<app>/<commit>            a delivery event that failed, waiting for its retry (router only)
ws/connections/<id>              a socket's record: its app, URL and handshake headers (router only)
ws/channels/<app>/<channel>/<id> a socket's subscription to a channel (router only)
```

- **A head** is plain JSON with these parts:
  - `tree`, which is absent while the name is empty, and holds:
    - `root`: the name's tree, its top node held inline (see "Files and state");
    - `bytes` and `entries`: the name's budgets, exact at each commit;
    - `seq`: the last inode number given out, which no inode is given again (absent until a file is made);
  - `pending`: commit id → {SHA-256 of its delivery event, time};
  - an optional claim.
- **Objects.** Each node, block or large value is immutable, named by a random id under its name, and read by the
  version id in the link that points to it, and checked against that link's SHA-256 and length.
  - A commit deletes the objects it replaces. With versioning, that only adds a delete marker, so old heads still
    read them by version id.
  - A turn that loses its race, fails or is discarded deletes the objects it made.
  - One lifecycle rule expires noncurrent versions after N days and removes expired delete markers.
  - The one thing left is an attempt whose host died before it deleted; see "Garbage".
- **`pending`.** Each commit drops pending entries older than the retry window (24 h) and an hour. A name holds at most
  1,000 live ones, so its head stays an eighth under its 1 MiB; the 1,001st background request is refused (503), and no
  old entry is ever dropped for it.
- **A 403 is read as not found.** S3 answers 403 for a missing key when the caller can't list.

### Files and state: one tree per name

A name's state, keys and files together, is one copy-on-write B+tree: ordered keys in immutable objects, with the root
inline in the head. `wasi:keyvalue` and `wasi:filesystem` (0.2 and 0.3) are two views of it. The turn model is
unchanged, and the head's one compare-and-swap commits or fails everything a turn did.

**Why one tree, and not one for keys and one for files:**
- One compare-and-swap commits a turn's keys and files together, or neither. Two trees would need two heads, which S3
  can't change together, or one head holding both roots, which is this design.
- It is one tree to edit, one commit, one pair of garbage lists and one budget. A second tree would copy the hardest
  code.
- Key prefixes keep the two apart, so a key write rewrites no file leaf, only the branches above its own.
- The cost: a name whose tree is larger than one node (64 KiB) reads its keys through the tree, one more GET per level
  when cold, and its commits need one more PUT round per level. A tree that is a root alone, the common case, costs
  what a name costs today. The flat map this replaces held up to 1 MiB in the head and rewrote all of it at each
  commit.
- An app that never opens a file makes no request for the file system, and a name with no files has no `f/` or `d/`
  keys.

**The format.** Keys are UTF-8 strings, ordered bytewise:

```
k/<key>              a key of `wasi:keyvalue`: its value
f/<ino>              an inode: kind, link count, size, times; a symlink's target; a directory's parent
f/<ino>/<block>      a block of a file's data
d/<dir>/<name>       a directory entry: the inode it names, and that inode's kind
```

- `<ino>` and `<dir>` are decimal and `<block>` is 16 hex digits, so the blocks of a file sort as numbers. (Nothing
  depends on the order of inode numbers: a prefix ends in its `/`.) Inode 1 is the root directory, empty until written.
  New inodes count up from the head's `seq` and are never reused, so a number names one file for the life of the name,
  which `is-same-object` and a stale descriptor both rely on. A name emptied by deletes keeps its `seq`, so its head
  is `{"seq": N}`, not empty.
- An inode sits next to its blocks, and a directory's entries sit together, so an `open` and the first reads of a small
  file touch one leaf, and a `readdir` is a range scan. A guest's file names live inside leaves, never in object keys.
- A **leaf** is a sorted list of (key, item), and a **branch** a sorted list of (separator, link) to its children: the
  separator of a child is at most every key in it and above every key in the child before; the first child's is not
  looked at. Each node is JSON of at most 64 KiB (`NODE_MAX`). An item is data (base64, inline up to 4 KiB) or a link;
  an inode and a dirent are JSON, so inline.
- A **link** is `{id, v, h, n}`: a random 128-bit id (the object is `values/<name>/<id>`), the S3 version id (none in
  the local memory store), and the SHA-256 and length of the bytes. An object is read at its version and checked for
  both, so the head pins every node and block below it by hash. A link read from a head or a node is checked before it
  is used: the id is 32 lowercase hex digits, so it can't name another prefix, and the version is short and plain.
- A **head** is `{tree: {root, bytes, entries, seq}, pending, claim}`. The root node is inline, so the top of every
  lookup costs no request. `bytes` counts the data in values and blocks, and `entries` the keys, both exact: a commit
  lands only over the head it read, so the deltas it counts while editing are the totals.
- A turn's size estimates a link at its longest (208 bytes), so a node never encodes to more than the size it was
  split by. A branch holds about 300 links, and a leaf several hundred entries (a directory entry is about 60 bytes, a
  block's link 230), so 4 Mi entries need three levels at the most, the root included.

**Editing.** A turn edits the tree itself, as LMDB and bbolt do in a write transaction. There is no overlay:
- A write descends from the root. Each node on the path is loaded (a GET if it is not cached, unless it is already in
  memory), copied, and changed; its object goes on `dead`. Nodes it did not touch stay links.
- Reads, `readdir` and `list-keys` use the same tree, so they see the turn's own writes and nothing more. A blind write
  descends at the write, so a GET per uncached level is paid there, not at the commit.
- An overfull node splits in halves by size, up to the root, which grows the tree by a level.
- At the commit, a changed node under a quarter full merges with a neighbour (and the two are split again if they do not
  fit in one), and an emptied node is dropped, as bbolt does. A root with one child is replaced by it. So the tree
  shrinks as it is emptied, and a name that is mostly deleted does not stay sparse.

**Spilling.** The copies in memory, and file data and large values held for upload, count against one budget of 8 MiB
(`BUDGET`), checked after each write. Past it, as LMDB spills dirty pages:
1. The held blocks and values upload, up to 16 at once, and the writer waits. Each item becomes a link.
2. If the copies still pass the budget, the changed nodes upload bottom-up and become links, so each parent is changed,
   and in memory, in its turn.

Whatever a turn made and then replaced is on `made`, and, as it was replaced, on `dead` too. So a turn's memory stays
about its budget plus one write, however much it writes. A key's value over 4 KiB is held like a block, and uploaded
when the budget spills it or the commit does.

**The commit,** at the answer:
1. Merge what is thin, and drop what is empty, from the leaves up.
2. Upload every node still in memory and every held value, bottom-up with at most 16 in flight, so that each branch
   embeds the links of its children.
3. Put the head, with the new root inline and the new budgets, by compare-and-swap, as now.

A tree that is a root alone has no node rounds: a commit is its large values, as today, and the head. A turn that
changed nothing puts nothing.

**What the file system does:**
- A descriptor is an inode number and its open flags. A path walk goes component by component from the descriptor it
  starts at, with a stack of the directories entered: `..` pops, and popping past the start is `not-permitted`, as the
  WASI path-resolution rules say. Symlinks are followed up to 40 deep, then `loop`; an absolute target is
  `not-permitted`. A directory keeps its `parent` only so that `rename` can refuse to move it into itself.
- File data is in fixed blocks of 256 KiB (`BLOCK`). A hole is an absent block and reads as zeros, and a short block
  is zero-padded to the file's size. So growing a file rewrites nothing, and truncating rewrites the one block at the
  new end and drops those beyond it. A write of a whole block reads nothing; a partial one reads its block first.
- Hard links join files (and symlinks) only, within the one tree, with a link count. A directory reports a link count
  of 1, as btrfs does. `is-same-object` and `metadata-hash` come from the inode number. Times are the host's clock:
  an inode has a creation, modification and change time, and the access time is the creation's unless `set-times`
  set it, as a read changes nothing. `sync`, `sync-data` and `advise` succeed and do nothing: the commit is the
  durability.
- **A file unlinked while open** is as on Linux. Its inode stays in the tree at a link count of 0, and is read and
  written through the descriptors that hold it. The last close in the turn removes it. The commit leaves out an inode
  still at 0.
- **A descriptor still open on such a file at the answer** keeps reading it. The commit forks its tree (a root and
  the shared objects, nothing copied) before it removes the orphans, and, once the head has landed, those descriptors
  read through the fork. So there is no copy of the file and no cap on links. The fork reads blocks by version, which a
  delete marker does not hide, so it is exact in a versioned bucket; the memory store, which deletes nothing, reads
  anyway. If the turn is discarded, or loses its race, what the turn made is gone and they get `io`.
- After the answer, a write fails with `read-only`, and so does any write to a snapshot. A change that was admitted
  before the answer lands before the commit does; one after it fails. A stream's write is admitted when the guest
  makes it.
- A write fails early with `insufficient-space` when the name's data or entries pass their budgets, counted exactly
  from the tree and the turn's edits. A batch over a budget changes nothing.

**The read path:**
1. The head is read as now: one GET, with its root node in it.
2. A lookup descends from the root. A node not in memory is one GET, at its version, checked by its hash. Nodes read are
   kept by the process in a cache of immutable nodes, byte-capped (16 MiB), keyed by the object's full path and its
   version (app, name, id, version), so an entry is never stale and never crosses apps.
3. A file read fetches the blocks in range in parallel, whole, as their hash needs all of each.

**Requests, and time.** AWS gives roughly 100 to 200 ms for a small object, which is not measured here, as no AWS calls
were made. A round is one such trip, and S3 bills per request, not by size. D is the tree's depth, a root alone being 1.

| What | S3 requests |
|---|---|
| A turn starts | 1 GET, the head, as now |
| A lookup in a root-only tree | none more |
| A lookup below the root | 1 GET per node not cached, D - 1 at most, in sequence |
| A file of 4 KiB or less | none more: it is inline in its leaf |
| A read of N blocks | N GETs, in parallel |
| A partial-block write | 1 GET of that block, unless cached or held |
| A write below the root | the GETs of its path, D - 1 at most, unless cached or already copied |
| A commit | PUTs of held blocks (parallel), D - 1 rounds of node PUTs, 1 PUT of the head |
| Deleting G objects | ceil(G / 1000) DeleteObjects, after the compare-and-swap |

So a cold read of a byte in a big file takes D + 1 trips (3 or 4), and a warm one the head's and the block's. A commit
of a changed block takes D + 1 (the blocks, a round per level, then the head). A name under 64 KiB, of keys or not,
takes what it takes today. A 1-byte write to a 10 MiB file puts one 256 KiB block, a leaf of at most 64 KiB, the
branches above it and the head, and not the 10 MiB.

**Seeing it.** At debug, each turn logs its tree next to the head: GETs, PUTs, cache hits, depth, and the bytes read and
written, with the splits, merges and collapses of the root.

**Snapshots.** A read at an older head keeps working while newer commits land: every object is immutable and read by
version, and a commit only adds delete markers.

**Garbage** is an object that no head a reader could be on refers to:
1. **The committer, exactly.** A commit that lands deletes `dead`. An attempt that is discarded, doomed, loses its race
   or fails deletes `made`. A commit whose outcome is unknown (the compare-and-swap errored, so the head may have
   landed) deletes neither. Deletes go as DeleteObjects, 1,000 to a request. They start at the commit and are joined at
   the end of the response body: no added latency if the body outlasts them, one trip at most if not, and the Lambda
   environment isn't frozen under them, as it can be under the detached task they run in today.
2. **The lifecycle rule,** which the bucket has: a deleted object is a noncurrent version, and goes a day later.
3. **The sweep,** last of the stages, for what the committer can't know (below).

*Why it is safe for readers.* A reader holds a head it read at most 300 s ago (`TOTAL`) and reads objects by version id.
A delete adds a marker and leaves that version readable, and the lifecycle waits a day.

*What leaks.* A trap, a timeout, a lost race and a failed commit are all seen by the host, which deletes. What it can't
delete is a current object of an attempt whose host died mid-attempt (killed, or out of memory), at most what a turn
could upload in `TOTAL`, and of a commit whose outcome was unknown.

*The sweep* is a mark-and-sweep with a grace period, run by serve with the app's own credentials:
- It lists the app's names, and for each walks the tree from its head, lists `values/<name>/` (as 16 listings at once,
  one for each first digit of an id, so that a name of millions of objects takes less than a run), and deletes what is
  older than the grace period (an hour at least) and unreachable. No later head can refer to such an object, as no
  attempt lasts an hour, which the build asserts against the limits of a turn. Only a key a tree could have made is
  ever deleted, and a head is read fresh.
- It needs `s3:ListBucket` on the app's `names/*` and `values/*` only, by an `s3:prefix` condition, in the router's
  policy (`src/route.rs`), `infra/aws/main.tf`, `docker/router-policy.json` and the module's test, with assertions.
  `names/` is there because a missing head can't be told from a refused read by a GET, and can be by the listing.
- It runs once a day for each app, from the Scheduler through the router and into serve, as a retry does, and never
  always-on. It deletes nothing from a name unless that name's walk completed. A run is bounded (240 s), and resumes
  where the last stopped. It logs what it reclaimed. A request to the events function's `cron` alias, with
  `{"app", "sweep": true}`, runs it on demand.
- The design of the trigger, and the arguments for its safety, are in `docs/decisions.md`.

**Limits.** All are policy: the representation has none of its own.

| Limit | Value | Why |
|---|---|---|
| Block | 256 KiB | a request costs the same at any size, so large; but a write rewrites one |
| Node | 64 KiB | the root is in the head, which every turn reads; a write rewrites one leaf |
| Inline | 4 KiB | a value or file this small costs no request of its own |
| A key's value | 1 MiB | as now (`VALUE_MAX`); one object |
| A file | 16 GiB | as much as the name holds (`DATA_MAX`); reads and writes stream |
| A name's data | 16 GiB | 64 Ki blocks, in under a thousand leaves |
| A name's entries | 4 Mi | three levels |
| Name, path, target | 255, 4,096, 4,096 bytes | POSIX's `NAME_MAX` and `PATH_MAX` |
| Symlink follows | 40 | Linux's |
| Held and copied, per turn | 8 MiB | memory, per turn: spilled when passed |
| Descriptors | 256 | the engine's `RESOURCES`; one more is `insufficient-memory` |

These two are the numbers to revisit once they are measured: the block, node and inline sizes, and the 16 GiB and 4 Mi
budgets. A turn has no cap on its changes: it spills.

**Security:**
- A descriptor is an inode number in the name's tree. No host path, file descriptor or object key is the guest's.
  Names are validated (UTF-8, no `/` or NUL, not `.` or `..`, at most 255 bytes), and live inside nodes.
- Path walks can't leave the tree (above), and a hard link can't cross trees, as there is one.
- Objects are under `apps/<app>/values/<name>/`, written and read with the app's own credentials. The name passes
  `is_name`, so it holds no `/`, and an id is random: nothing the guest controls forms a key. A link read back is
  checked before it is used, so a changed head or node can't point outside the prefix. The sweep's `ListBucket` on
  `names/` and `values/` is the only IAM change, and lists no other prefix.
- Reads are checked against the link's hash and length. A mismatch is an error, and the object, version and detail are
  logged and never given to the guest.
- Nothing is content-addressed, so there is no deduplication, within an app or between apps, and no way to learn from a
  write's speed that another tenant holds the same file.
- The guest sees the stock interfaces except the file system, linked by name one at a time, with no shadowing: a
  component can only reach our file system, never wasmtime-wasi's, and never the host's.
- A snapshot is read-only, and every size above is capped, so a guest can't take the host's memory or requests beyond a
  turn's limits.

**Structure and size.** One core, two thin bindings, each with its own `bindgen!`: wasmtime-wasi's `Descriptor` wraps an
OS file through cap-std, and its generated host traits fix that type. The WIT is vendored verbatim from wasmtime-wasi
49.0.2 (`wit/filesystem-p2` is 0.2.12, `wit/filesystem-p3` is 0.3.0). The stock interfaces are linked one at a time,
without the file system: for 0.3, `cli`, `clocks`, `random` and `sockets` `add_to_linker`; for 0.2, each interface's
`add_to_linker`, with our `bindgen!` sharing the stock `wasi:io` stream resources through `with:`.

| File | What | Lines |
|---|---|---|
| `src/tree.rs` | nodes, links, edits, spill, merge, scans, the cache, garbage lists | 1,080 (built) |
| `src/name.rs`, `src/tric.rs`, `src/store.rs` | the turn over the tree; the joined deletes | +70 (built) |
| `src/fs.rs` | the core: descriptors, the gate, orphans, read-ahead, and the paths over the operations | 790 (built) |
| `src/fs/ops.rs` | inodes, blocks, directories, rename, errors, on the tree | 850 (built) |
| `src/fs/p2.rs` | 0.2: the descriptor's methods, streams, preopens | 560 (built) |
| `src/fs/p3.rs` | 0.3: the same, async, with `stream` and `future` | 650 (built) |
| `src/sweep.rs`, and the trigger | listing, walk, delete, the cursor; the schedule, the router, serve | 480 (built) |

The tree came to 1,080 lines, against an estimate of 800, because spilling, merging, the cache and the log are in it.
The core and the 0.2 binding came to 2,200 against 1,250: the gate that keeps a stream's write from being lost to the
answer, the orphans, a mapping of every failure to an errno, read-ahead, and the streams that outlive their calls are
most of the difference. The 0.3 binding came to 650 against 600, its `stream` and `future` plumbing (a producer for a
read, one for a directory listing and a consumer for a write) being about a third of it. The sweep came to about 480
against 300, the walk and its cap, the shape of the keys it may delete, the listing by digit, the cursor, the router's
and serve's part, and the reconciling of an app's schedules, which gave deploy its first tests, being the difference.
The whole is about 4,400 lines of runtime code, on the 4,289 in `src` at the start. Tests come to well over two
thousand more. The tree and `name.rs` came first and alone: the key tests' assertions hold on them, which tests the
tree before any file exists.

**Stages.** Each is committed on its own and passes the whole gate:
- (a) the tree, with `wasi:keyvalue` on it (done);
- (b) the file system core and the 0.2 binding, with the testsuite's wasip1 modules and the end-to-end file cases
  (done);
- (c) the 0.3 binding, with its 14 components (done);
- (d) the sweep, with its trigger (done).

**Conformance:**
- **The WebAssembly testsuite** (`wasi-testsuite`, pinned by commit). The toolchain image fetches it as a tarball
  checked by sha256 (`docker/build.Dockerfile`), with the preview 1 adapter of Wasmtime's 49.0.2 release, also by
  hash: neither is a Cargo dependency. Its prebuilt wasm32-wasip1 modules that name a directory to preopen (42 in Rust
  and 7 in C) are made into components with `wasm-tools component new --adapt`, and run against the 0.2 binding. Its
  14 0.3 `filesystem-*` components run against the 0.3 binding. All 63 pass.
- **The harness** is a `#[cfg(test)]` module in the binary (`src/fs/conformance.rs`), using the production linker, host
  and limits, and a turn over the memory store, which holds the test's own `fs-tests.dir`; the turn is committed
  after the test. Wasmtime's runner expects every one of these to pass on Linux, and so does the harness: a `FAILS`
  list names each that does not, and why, and a test that is listed and passes fails the run. It is skipped without
  `TRIC_CONFORMANCE`, the directory of the fetched files.
- **Our own tests:** the tree against a `BTreeMap`, over random operations with tiny nodes so that it splits, merges and
  collapses, and grows to four levels, with what is in the store checked against what the tree names after every round;
  request counts with a counting `ObjectStore`, to show that a write puts only what it changed, and that a failed
  upload leaves nothing; a tampered object; links that try to leave the prefix; paths that try to escape.
- **e2e, on RustFS, through both bindings:** writes held until the answer; a 5xx discards; a lost race re-runs; a write
  after the answer fails; a snapshot is read-only; keys and files commit together; a file of several blocks updated in
  place; a directory of thousands of files.

**Alternatives, rejected** (reasons and sources in `docs/decisions.md`): Git's objects through `gix`; IPFS UnixFS with
sharded directories; JuiceFS; content-defined chunking; SlateDB and ZeroFS; a Lambda `/tmp` served by the stock
wasmtime-wasi; prolly trees; a file system component over `wasi:keyvalue`; separate trees for keys and files; an
overlay applied at the commit.

**Decided,** by the choices behind the design (each with its reason in `docs/decisions.md`):
1. **The sweep is built,** last, for the leaks above, with `s3:ListBucket` limited to the app's `names/` and `values/`.
2. **The numbers** above, to be revisited once measured.
3. **Only a turn's own name is mounted.** Mounting other names would need a path convention.
4. **A turn edits the tree and spills at its budget,** with no cap on its changes.
5. **Thin nodes merge,** as bbolt's do.
6. **JSON nodes,** not a binary codec, which would be a new crate.
7. **The deletes are joined at the end of the body,** which can add one trip to a body that is shorter than a delete.
8. **A name over 64 KiB reads its keys through the tree,** a GET more per level when cold, and not from its head.
9. **0.2 first and 0.3 after,** by stage.

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
  it only if the commit is pending in the head with the event's digest. A failure schedules the next try.
  - The window is 24 h from the first failure, as EventBridge's default retry policy has it, with exponential backoff
    (a minute, doubling to an hour) and jitter. `Retry-After` is a floor.
  - Any other 4xx is an answer, and is final. A delivered or dropped event deletes its object.
  - After the window, the router logs a warning with the app and the commit, deletes the object, and leaves `pending`
    to expire.
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
  1. Wait for the sender's commit: `If-None-Match` on the version the turn started from, up to the turn deadline.
  2. Drop the event unless its commit id and its digest are in `pending`.
  3. Send each request in order:
     - to the app's own origin: in-process, as a turn;
     - anywhere else: over the network, under the allow list.
  4. Clear the commit from `pending`.

  A 5xx, a 429 or a failed exchange answers 5xx, and the router retries as above. Any other answer is final.
- **It handles a `POST` carrying `Forwarded: for=_ws` as a socket's event,** which only the router sends: the
  socket's record, its id and the event. It runs the turn of it at the URL the socket opened at.
- **At a turn's answer, with background requests:** invoke the router's `outbox` alias first, then commit, recording
  the digest in `pending`.
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
  - the Scheduler role, which can invoke `events:cron` only;
  - the retry role, which can invoke `events:retry` only.
- **The bucket:** versioning, the lifecycle rules (noncurrent versions, delete markers, `outbox/`, `ws/`), and short
  log retention.
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
- `tric logs`;
- release history commands;
- previews and forks;
- `Accept-Datetime`;
- mounting any name but the turn's own;
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
5. **Forged outbox events.** An event naming app B is dropped unless its commit and digest are pending in B's head.
6. **Cron** reaches the app through `route` as a `POST` with `for=_cron`.
7. **Native code** is written under `native/<app>/…` with the app's credentials.
8. **The app semantics** pass through `route` → `serve`: names, turns, the outbox, self-fetch, middleware and
   outbound.
9. **Files**, through both `wasi:filesystem` 0.2 and 0.3:
   - the WebAssembly testsuite's file system tests pass, bar a skip-list that gives a reason for each;
   - a write is held until the answer; a 5xx, a trap or a lost race leaves nothing visible; a write after the answer
     fails;
   - keys and files commit together or not at all;
   - a path can't leave the tree by `..`, an absolute path or a symlink, and an altered object is `io`;
   - a write puts only the blocks and nodes it changes, and a read fetches only those it touches, as a counting store
     shows;
   - a name that never opens a file makes no request for one.

**`tofu validate`** passes, and the module shows:
- serve is per tenant, with no URL;
- serve's role has no S3 access;
- the app role trusts only the router;
- the outbox and retry aliases carry the async config and no destination;
- the router's S3 access is `ws/`, `outbox/` and a listing of `ws/channels/`, and it creates schedules in the retry
  group only, passing only the retry role, which can invoke `events:retry` only;
- lifecycle expires noncurrent versions and `outbox/`.

**The remote test, after the user says go:**
- an app answers through CloudFront, and streams;
- two apps run in different tenants, and neither one's credentials read the other's data;
- the outbox, its retries and cron work;
- a direct call to the router can't forge `_cron`, `_tric` or credentials;
- `for=` is the viewer's address.
