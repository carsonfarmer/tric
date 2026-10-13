//! Names: each name's state is one object, its head, which a turn changes with a compare-and-swap when it commits.
use crate::fs;
use crate::kv::{Error, KeyResponse, other};
use crate::outbox::{Event, Held, Sink};
use crate::store::{self, Store};
use crate::tree::{self, Full, Root, Tree};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use object_store::{PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{sync::RwLock, sync::watch, time::sleep, time::timeout_at};
use wasmtime::{Result, error::Context};

const NAME_MAX: usize = 128;
const KEY_MAX: usize = 256; // bytes
const VALUE_MAX: usize = 1 << 20;
pub const HEAD_MAX: usize = 1 << 20;
const HELD_MAX: usize = 1_000_000; // bytes of held requests, as JSON, so an event is within Lambda's 1 MB
const PAGE: usize = 1000; // keys per `list-keys`
/// How long a commit's put may take, with learning whether it landed when its answer is lost; past that, the commit
/// fails with [`Failed::Unsettled`]. A turn's runs, which begin before its deadline (`tric::DEADLINE`), the guest's
/// `ANSWER` and this fit in CloudFront's 60 s for an answer from the origin, so the client hears the 500, and not a 504
/// that it might try again.
pub const SETTLE: Duration = Duration::from_secs(30);
/// The header that makes a turn apply once, however often it is sent: see [`Idempotency`].
pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const IDEMPOTENCY_KEY_MAX: usize = 255; // bytes
/// A day, in milliseconds: the records a commit keeps in the tree, of its `Idempotency-Key` and of its delivery event,
/// are kept by the Unix day they were made on.
const DAY: u64 = 24 * 3600 * 1000;
/// The most expired records of each kind one commit deletes.
const PRUNE_MAX: usize = 1000;

/// Whether `s` is a name: 1 to 128 of `A-Za-z0-9._~:-`, starting with a letter or digit.
pub fn is_name(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b"._~:-".contains(&b);
    s.len() <= NAME_MAX && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) && s.bytes().all(ok)
}

/// The name a path addresses: what follows `/@`, up to the next `/`.
pub fn of(path: &str) -> Option<&str> {
    path.strip_prefix("/@").map(|rest| rest.split('/').next().unwrap_or_default())
}

/// A name's state: its tree, with the root in the head, and the commit that wrote it.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    #[serde(default, skip_serializing_if = "Root::is_empty")]
    tree: Root,
    /// The random id of the commit that wrote it, new to each: so no head's bytes are ever another's, and a put on a
    /// head's `ETag` in the store lands on that head itself or not at all.
    commit: String,
}

impl Head {
    /// The head's `ETag`: its commit's id, which changes only when a commit writes the name.
    fn etag(&self) -> String {
        format!("\"{}\"", self.commit)
    }

    /// Adds to `seen` the ids of the objects the head's tree is made of, with the name's own `app` and `name`, and
    /// gives how many nodes it read, or `None` if that would be over `max`: see [`Tree::mark`]. The tree is all a head
    /// names of its name's objects: its `commit` is the id of a commit, which names none.
    pub async fn mark(
        &self,
        store: &Store,
        app: &str,
        name: &str,
        seen: &mut HashSet<u128>,
        max: usize,
    ) -> Result<Option<usize>> {
        Ok(tree::open(store, app, name, &self.tree, tree::config())?.mark(seen, max).await?)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn path(app: &str, name: &str) -> Path {
    store::app(app, &["names", name])
}

/// The head at `path`, with its version. Each read and write of a head logs, at debug, the bucket's `ETag` of it and
/// what it took: the numbers that say whether a cache of heads in each environment would pay.
pub async fn read(store: &Store, path: &Path) -> Result<Option<(Head, UpdateVersion)>> {
    let start = Instant::now();
    let read = store.json(path, HEAD_MAX as u64).await;
    let etag = read.as_ref().ok().and_then(Option::as_ref).and_then(|(_, v)| v.e_tag.as_deref());
    tracing::debug!(%path, etag, ms = start.elapsed().as_millis() as u64, "read");
    read
}

/// Writes `head` over the version `base`, or where there is none, if that is still the head: its new version, or
/// `None` if another write landed first and this one did not. An error may have landed, or may yet: see
/// [`Store::put`].
async fn put(store: &Store, path: &Path, head: &Head, base: Option<UpdateVersion>) -> Result<Option<UpdateVersion>> {
    let json = serde_json::to_vec(head)?;
    let (start, bytes, was) = (Instant::now(), json.len(), base.as_ref().and_then(|v| v.e_tag.clone()));
    let put = store.put(path, json.into(), base.map_or(PutMode::Create, PutMode::Update)).await;
    let etag = put.as_ref().ok().and_then(Option::as_ref).and_then(|v| v.e_tag.as_deref());
    tracing::debug!(%path, base = was, etag, bytes, ms = start.elapsed().as_millis() as u64, "put");
    put
}

/// `If-Match` and `If-None-Match`, which a turn evaluates against the head's `ETag` before it runs.
#[derive(Clone, Default)]
pub struct Conditions {
    if_match: Option<String>,
    if_none_match: Option<String>,
}

impl Conditions {
    pub fn of(headers: &HeaderMap) -> Self {
        let list = |name| {
            let all: Vec<_> = headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
            (!all.is_empty()).then(|| all.join(","))
        };
        Self { if_match: list(http::header::IF_MATCH), if_none_match: list(http::header::IF_NONE_MATCH) }
    }

    /// Whether they hold for the head with `etag`, or for no head: `If-Match` compares strongly, `If-None-Match`
    /// weakly.
    fn hold(&self, etag: Option<&str>) -> bool {
        let any = |list: &str, strong: bool| match list.trim() {
            "*" => etag.is_some(),
            list => etag.is_some_and(|e| tags(list).any(|(weak, t)| !(weak && strong) && t == e)),
        };
        self.if_match.as_deref().is_none_or(|l| any(l, true))
            && !self.if_none_match.as_deref().is_some_and(|l| any(l, false))
    }
}

/// The entity tags of a list, each with whether it is weak, quotes and all. A tag with a comma in it is split, so it
/// matches none of the head's, which have none.
fn tags(list: &str) -> impl Iterator<Item = (bool, &str)> {
    list.split(',').map(str::trim).map(|t| t.strip_prefix("W/").map_or((false, t), |t| (true, t)))
}

/// A turn's `Idempotency-Key`, by its SHA-256, and its request's fingerprint. A turn that commits records what it
/// answered under the key, and one sent again with the key is answered so, without running; one sent with another
/// request is refused.
#[derive(Clone)]
pub struct Idempotency {
    key: String,
    fingerprint: String,
}

impl Idempotency {
    /// The SHA-256 of the request's `Idempotency-Key`, if it has one, which is never kept or logged itself: one field,
    /// of 1 to 255 visible ASCII characters other than a comma, taken as sent; else 400. Lambda joins repeated fields
    /// with commas, so a key with none is one field wherever it came in.
    pub fn key(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
        let visible = |b: &u8| b.is_ascii_graphic() && *b != b',';
        let ok = |k: &HeaderValue| (1..=IDEMPOTENCY_KEY_MAX).contains(&k.len()) && k.as_bytes().iter().all(visible);
        match headers.get_all(IDEMPOTENCY_KEY).iter().collect::<Vec<_>>()[..] {
            [] => Ok(None),
            [key] if ok(key) => Ok(Some(store::hash(key.as_bytes()))),
            _ => Err(StatusCode::BAD_REQUEST),
        }
    }

    /// The key `key` of the request `method uri` with `body`, whose fingerprint is the SHA-256 of the method, a space,
    /// the path and query, a line feed and the body.
    pub fn of(key: String, method: &Method, uri: &Uri, body: &[u8]) -> Self {
        let target = uri.path_and_query().map_or("/", |p| p.as_str());
        let fingerprint = store::hash(&[method.as_str().as_bytes(), b" ", target.as_bytes(), b"\n", body].concat());
        Self { key, fingerprint }
    }
}

/// What a turn with an `Idempotency-Key` answered, which its commit keeps in the tree at `i/<day>/<key>`: its
/// request's fingerprint, its status and its `Location`, but not its body.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    fingerprint: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// The key in the tree of the record of the `Idempotency-Key` `key`, made on the Unix day `day`.
fn record_key(day: u64, key: &str) -> String {
    format!("i/{day:06}/{key}")
}

/// The record of the `Idempotency-Key` `key`, if it was made yesterday, today or tomorrow, as clocks differ.
async fn recorded(tree: &Tree, key: &str, today: u64) -> Result<Option<Record>> {
    for day in [today.saturating_sub(1), today, today + 1] {
        if let Some(item) = tree.get(&record_key(day, key)).await? {
            return Ok(Some(serde_json::from_slice(&tree.value(&item).await?)?));
        }
    }
    Ok(None)
}

/// The key in the tree of the record of a delivery event, by its SHA-256 `digest`, made on the Unix day `day`.
fn event_key(day: u64, digest: &str) -> String {
    format!("o/{day:06}/{digest}")
}

/// The first `PRUNE_MAX` records under `prefix`, `i/` or `o/`, up to the first made on the day `kept` or after.
async fn expired(tree: &Tree, prefix: &str, kept: u64) -> Result<Vec<String>> {
    let old = |k: &String| k.split('/').nth(1).and_then(|d| d.parse::<u64>().ok()).is_some_and(|d| d < kept);
    if !tree.scan(prefix, None, 1).await?.first().is_some_and(old) {
        return Ok(vec![]); // as there mostly are none, at the cost of one key
    }
    Ok(tree.scan(prefix, None, PRUNE_MAX).await?.into_iter().take_while(old).collect())
}

/// Whether the head of `name` holds the record of the delivery event with the SHA-256 `digest` made on the Unix day
/// `day`, which its commit writes, and the commits after it keep until two days after `day`; and the head's commit, if
/// there is a head.
pub async fn holds(store: &Store, app: &str, name: &str, day: u64, digest: &str) -> Result<(bool, Option<String>)> {
    let Some((head, _)) = read(store, &path(app, name)).await? else { return Ok((false, None)) };
    let tree = tree::open(store, app, name, &head.tree, tree::config())?;
    Ok((tree.get(&event_key(day, digest)).await?.is_some(), Some(head.commit)))
}

/// What a turn that does not run answers.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Refused(StatusCode),
    Replay(Record), // what the turn that committed with the same `Idempotency-Key` answered
}

/// What a turn changes, under its lock.
#[derive(Default)]
struct State {
    head: Head,                  // as read
    base: Option<UpdateVersion>, // the head's version, if there is a head
    answered: bool,
    sent: bool, // once the turn has sent an unsafe request before it answered, so it may not run again
    held: Vec<Held>,
    held_size: usize,
}

/// A request that may write one name: its writes are in its tree, which is committed or discarded once it answers. A
/// snapshot is a turn that has answered: it reads the name as it was, and writes nothing.
///
/// The locks are taken in the order the file system's gate (which a commit takes and lets go of at once), `tree`,
/// `state`; `state` is never held across an await.
pub struct Turn {
    store: Store,
    app: String,
    pub name: String,
    path: Path,
    state: Mutex<State>,
    tree: RwLock<Tree>,
    /// What the file system keeps: the descriptors open on it, and the gate its changes pass.
    pub fs: fs::State,
    settled: watch::Sender<Option<bool>>, // whether it committed, once it is known
    once: Option<Idempotency>,
}

/// How a commit went.
#[derive(Debug, PartialEq)]
pub enum Committed {
    Done(Option<String>), // with the head's `ETag`, if that is known
    Conflict,             // another turn changed the name first
}

/// Why a commit failed, where that decides how the turn answers: only a commit that failed before its put
/// ([`Failed::Unsent`]) answers 503, so a client may try again, as nothing landed; any other failure answers 500, as it
/// may have landed. An error carries at most one.
#[derive(Debug, PartialEq)]
pub enum Failed {
    /// The name's tree would be over its limits with the commit's records, of its `Idempotency-Key` and its delivery
    /// event. Running again would fail again, as for a trap, so it is never `Unsent`.
    TooLarge,
    /// The commit failed before its head was put, so nothing landed.
    Unsent,
    /// The put had no sure answer, and the head did not say that it landed: it was another commit's, which may have
    /// been written over the turn's, or it could not be read within `SETTLE`. It may have landed, so a client that tried
    /// again could run the turn twice.
    Unsettled,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Failed::TooLarge => write!(f, "the name would be over its limits"),
            Failed::Unsent => write!(f, "the commit failed before its head was put"),
            Failed::Unsettled => {
                write!(f, "the commit may have landed, and the name did not say whether it did")
            }
        }
    }
}

/// The key of `key` in the tree: the keys of the keyvalue store are under `k/`.
fn tree_key(key: &str) -> String {
    format!("k/{key}")
}

/// What the guest is told of a tree error: that a limit was hit, which it can act on, and else only that the name
/// failed, as the rest is the host's to know.
fn fail(e: tree::Error) -> Error {
    match e {
        tree::Error::Full(Full::Entries(n)) => other(format!("a name holds {n} keys or fewer")),
        tree::Error::Full(Full::Data(n)) => other(format!("a name holds {n} bytes or fewer")),
        e => {
            tracing::warn!("name: {:#}", wasmtime::Error::from(e));
            other("the name could not be read or written")
        }
    }
}

fn check(items: &[(String, Option<Bytes>)]) -> Result<(), Error> {
    for (k, v) in items {
        if !(1..=KEY_MAX).contains(&k.len()) {
            return Err(other(format!("a key is 1 to {KEY_MAX} bytes")));
        }
        if v.as_ref().is_some_and(|v| v.len() > VALUE_MAX) {
            return Err(other(format!("a value is {VALUE_MAX} bytes or less")));
        }
    }
    Ok(())
}

impl Turn {
    /// A turn on `name` with the head `read`, answered already if `answered`, with the `Idempotency-Key` `once`.
    fn new(
        store: &Store,
        app: &str,
        name: &str,
        read: Option<(Head, UpdateVersion)>,
        answered: bool,
        once: Option<Idempotency>,
    ) -> Result<Arc<Self>> {
        let (head, base) = read.map_or_else(Default::default, |(head, version)| (head, Some(version)));
        let tree = RwLock::new(tree::open(store, app, name, &head.tree, tree::config())?);
        let state = Mutex::new(State { head, base, answered, ..Default::default() });
        let (app, name, path, settled) = (app.into(), name.into(), path(app, name), watch::Sender::new(None));
        Ok(Arc::new(Self { store: store.clone(), app, name, path, state, tree, fs: Default::default(), settled, once }))
    }

    /// A snapshot of `name`: as it is now.
    pub async fn snap(store: &Store, app: &str, name: &str) -> Result<Arc<Self>> {
        Self::new(store, app, name, read(store, &path(app, name)).await?, true, None)
    }

    /// Opens a turn on `name`, if `conditions` hold; or answers without one. A turn whose `Idempotency-Key` `once`
    /// committed is answered as it was, at once, or with 422 if its request was another; else the turn is refused with
    /// 412 if the conditions do not hold.
    pub async fn open(
        store: &Store,
        app: &str,
        name: &str,
        conditions: &Conditions,
        once: Option<&Idempotency>,
    ) -> Result<Result<Arc<Self>, Answer>> {
        let turn = Self::new(store, app, name, read(store, &path(app, name)).await?, false, once.cloned())?;
        if let Some(once) = once
            && let Some(record) = recorded(&*turn.tree.read().await, &once.key, now() / DAY).await?
        {
            return Ok(Err(match record.fingerprint == once.fingerprint {
                true => Answer::Replay(record),
                false => Answer::Refused(StatusCode::UNPROCESSABLE_ENTITY),
            }));
        }
        if !conditions.hold(turn.etag().as_deref()) {
            return Ok(Err(Answer::Refused(StatusCode::PRECONDITION_FAILED)));
        }
        Ok(Ok(turn))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// The `ETag` of the head as the turn last read or wrote it; none if there is no head.
    pub fn etag(&self) -> Option<String> {
        let s = self.state();
        s.base.is_some().then(|| s.head.etag())
    }

    /// Readies an unsafe outbound request: before the answer, by marking the turn as one that may not run again, as
    /// what it sends stays sent; after it, by waiting for the commit.
    pub async fn before_unsafe(&self) -> Result<(), &'static str> {
        {
            let mut s = self.state();
            if !s.answered {
                s.sent = true;
                return Ok(());
            }
        }
        match self.settled.subscribe().wait_for(Option::is_some).await.map(|v| *v) {
            Ok(Some(true)) => Ok(()),
            _ => Err("the request's changes were discarded"),
        }
    }

    /// Whether the turn may run again: it has sent no unsafe request before it answered.
    pub fn repeatable(&self) -> bool {
        !self.state().sent
    }

    /// Marks the turn answered: no more writes or held requests.
    pub fn answer(&self) {
        self.state().answered = true;
    }

    pub fn is_open(&self) -> bool {
        !self.state().answered
    }

    /// The tree, which the file system reads and changes by its own rules.
    pub fn tree(&self) -> &RwLock<Tree> {
        &self.tree
    }

    /// Holds `held` for the outbox, unless the turn is answered or holds too much.
    pub fn hold(&self, held: Held) -> Result<(), String> {
        let n = serde_json::to_vec(&held).map_err(|e| e.to_string())?.len();
        let mut s = self.state();
        if s.answered {
            return Err("the request has answered".into());
        }
        if s.held_size + n > HELD_MAX {
            return Err(format!("a request holds {HELD_MAX} bytes of requests or less"));
        }
        s.held_size += n;
        s.held.push(held);
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>, Error> {
        let tree = self.tree.read().await;
        let Some(item) = tree.get(&tree_key(key)).await.map_err(fail)? else { return Ok(None) };
        Ok(Some(tree.value(&item).await.map_err(fail)?))
    }

    pub async fn exists(&self, key: &str) -> Result<bool, Error> {
        Ok(self.tree.read().await.get(&tree_key(key)).await.map_err(fail)?.is_some())
    }

    pub async fn list(&self, cursor: Option<String>) -> Result<KeyResponse, Error> {
        // One page: the first `PAGE` keys after `cursor`, which is the last key of the page before.
        let after = cursor.map(|c| tree_key(&c));
        let found = self.tree.read().await.scan("k/", after.as_deref(), PAGE).await.map_err(fail)?;
        let keys: Vec<String> = found.iter().filter_map(|k| k.strip_prefix("k/")).map(str::to_owned).collect();
        Ok(KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys })
    }

    /// The write lock on the tree, if the turn has not answered. Commit sets `answered` before it takes the lock, so a
    /// write that holds the lock and sees the turn open lands before the commit.
    async fn writable(&self) -> Result<tokio::sync::RwLockWriteGuard<'_, Tree>, Error> {
        let open = || (!self.state().answered).then_some(()).ok_or(Error::AccessDenied);
        open()?;
        let tree = self.tree.write().await;
        open()?;
        Ok(tree)
    }

    /// Writes, `None` deleting, all or none of them.
    pub async fn write(&self, items: Vec<(String, Option<Bytes>)>) -> Result<(), Error> {
        check(&items)?;
        let mut tree = self.writable().await?;
        let edits = items.into_iter().map(|(k, v)| (tree_key(&k), v)).collect();
        tree.write(edits).await.map_err(fail)
    }

    /// Adds `delta` to the counter at `key`, 0 if there is none: 8 bytes, little-endian, as Spin keeps one.
    pub async fn increment(&self, key: &str, delta: i64) -> Result<i64, Error> {
        check(&[(key.into(), None)])?;
        let mut tree = self.writable().await?;
        let key = tree_key(key);
        let now = match tree.get(&key).await.map_err(fail)? {
            None => 0,
            Some(item) if item.len() == 8 => {
                let value = tree.value(&item).await.map_err(fail)?;
                value.as_ref().try_into().map(i64::from_le_bytes).map_err(|_| other("not a counter"))?
            }
            Some(_) => return Err(other("not a counter")),
        };
        let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
        tree.write(vec![(key, Some(Bytes::copy_from_slice(&next.to_le_bytes())))]).await.map_err(fail)?;
        Ok(next)
    }

    /// Writes `value` at `key` if it still holds `seen`, and returns whether it did.
    pub async fn swap(&self, key: &str, seen: &Option<Bytes>, value: Bytes) -> Result<bool, Error> {
        check(&[(key.into(), Some(value.clone()))])?;
        let mut tree = self.writable().await?;
        let key = tree_key(key);
        let now = match tree.get(&key).await.map_err(fail)? {
            Some(item) => Some(tree.value(&item).await.map_err(fail)?),
            None => None,
        };
        if now != *seen {
            return Ok(false);
        }
        tree.write(vec![(key, Some(value))]).await.map_err(fail)?;
        Ok(true)
    }

    /// Commits the turn, asked at `host` and answered with `status` and `location`: its tree is uploaded, its held
    /// requests go to `sink` as a delivery event whose digest the tree records, and its head goes over the version it
    /// read, if that is still the head. A commit that puts a head deletes expired records, and a turn with a key always
    /// puts one, to record it, whatever it changed. The `ETag` it returns is the new head's, or the old one's when there
    /// was nothing to write. A commit that fails is discarded.
    pub async fn commit(
        &self,
        host: &str,
        sink: &Sink,
        status: StatusCode,
        location: Option<&HeaderValue>,
    ) -> Result<Committed> {
        let committed = self.try_commit(host, sink, status, location).await;
        if committed.is_err() {
            self.discard().await;
        }
        self.settled.send_replace(Some(matches!(committed, Ok(Committed::Done(_)))));
        committed
    }

    async fn try_commit(
        &self,
        host: &str,
        sink: &Sink,
        status: StatusCode,
        location: Option<&HeaderValue>,
    ) -> Result<Committed> {
        let (mut head, base, held) = {
            let s = &mut *self.state();
            s.answered = true;
            (s.head.clone(), s.base.clone(), std::mem::take(&mut s.held))
        };
        self.fs.drain().await; // so the file system's changes that were admitted land first
        let mut tree = self.tree.write().await; // after the writes that were in flight
        if !tree.edited() && held.is_empty() && self.once.is_none() {
            return Ok(Committed::Done(self.etag()));
        }
        let (commit, was) = (store::random(), base.is_some().then(|| head.commit.clone()));
        let ready = async {
            let mut kept = None;
            if tree.edited() {
                kept = self.fs.settle(&mut tree).await?;
            }
            // An `Idempotency-Key`'s record is read until the day after its own; a delivery event's until two days
            // after, past the window its delivery is tried in.
            let today = now() / DAY;
            let mut edits = expired(&tree, "i/", today.saturating_sub(1)).await?;
            edits.extend(expired(&tree, "o/", today.saturating_sub(2)).await?);
            let mut edits: Vec<_> = edits.into_iter().map(|k| (k, None)).collect();
            if let Some(once) = &self.once {
                let location = location.and_then(|l| l.to_str().ok()).map(Into::into);
                let record = Record { fingerprint: once.fingerprint.clone(), status: status.as_u16(), location };
                edits.push((record_key(today, &once.key), Some(serde_json::to_vec(&record)?.into())));
            }
            let mut event = None;
            if !held.is_empty() {
                let e = Event {
                    app: self.app.clone(),
                    host: host.into(),
                    name: self.name.clone(),
                    day: today,
                    base: was.clone(),
                    commit: commit.clone(),
                    requests: held,
                };
                let bytes = Bytes::from(serde_json::to_vec(&e)?);
                edits.push((event_key(today, &store::hash(&bytes)), Some(Bytes::new())));
                event = Some(bytes);
            }
            tree.write(edits).await.map_err(|e| match e {
                // A name too full for the records stays so until something is deleted: it is not tried again.
                tree::Error::Full(_) | tree::Error::TooLarge(_) => wasmtime::Error::from(e).context(Failed::TooLarge),
                e => e.into(),
            })?;
            if tree.edited() {
                head.tree = tree.finish().await?;
            }
            head.commit = commit;
            if let Some(event) = event {
                sink(event).await?; // before the head is put, so a commit that lands has handed it over
            }
            Ok::<_, wasmtime::Error>(kept)
        };
        let kept = match ready.await {
            Ok(kept) => kept,
            // Nothing landed, so the client may try again, unless it would fail again.
            Err(e) => {
                tree.reset();
                let again = e.downcast_ref::<Failed>().is_none(); // so not `TooLarge`
                return Err(if again { e.context(Failed::Unsent) } else { e });
            }
        };
        let landed = self.land(&head, base, was.as_deref()).await;
        match &landed {
            Ok(Committed::Done(_)) => {
                if let Some(kept) = kept {
                    self.fs.keep(kept); // under the write lock, so a read sees it from the first
                }
            }
            _ => tree.reset(),
        }
        landed
    }

    /// Puts `head` over `base`, the head the turn read, whose commit was `was`. A put with no sure answer may have
    /// landed, or may yet, so until `SETTLE` after the put began the head is read, and read again after a wait from
    /// 100 ms that doubles up to 2 s: while it is still the one the turn read, the commit is put again, as it was, which
    /// lands only if no put of it has; once it is the commit's, the commit is done. Once it is another commit's, which
    /// may be over the commit's or over `base`, or if it cannot be read in time, the commit fails with
    /// [`Failed::Unsettled`], and is neither done nor run again.
    async fn land(&self, head: &Head, base: Option<UpdateVersion>, was: Option<&str>) -> Result<Committed> {
        let until = tokio::time::Instant::now() + SETTLE;
        let lost = match timeout_at(until, put(&self.store, &self.path, head, base.clone())).await {
            Ok(Ok(Some(_))) => return Ok(Committed::Done(Some(head.etag()))),
            Ok(Ok(None)) => return Ok(Committed::Conflict),
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => format!("no answer within {SETTLE:?}"),
        };
        let (path, commit) = (&self.path, &head.commit);
        let learn = async {
            let mut wait = Duration::from_millis(100);
            loop {
                match read(&self.store, path).await {
                    Ok(now) if now.as_ref().map(|(h, _)| h.commit.as_str()) == was => {
                        match put(&self.store, path, head, base.clone()).await {
                            Ok(Some(_)) => return true,
                            Ok(None) => {} // something landed since, which the head says
                            Err(e) => tracing::info!(%path, commit, "commit: {e:#}"),
                        }
                    }
                    Ok(now) => return now.is_some_and(|(h, _)| h.commit == *commit),
                    Err(e) => tracing::info!(%path, commit, "commit: {e:#}"),
                }
                sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(2));
            }
        };
        let landed = timeout_at(until, learn).await.unwrap_or(false);
        tracing::warn!(%path, commit, landed, "commit: {lost}");
        landed.then(|| Committed::Done(Some(head.etag()))).context(Failed::Unsettled)
    }

    /// Discards the turn.
    pub async fn discard(&self) {
        {
            let s = &mut *self.state();
            s.answered = true;
            s.held.clear();
        }
        self.fs.drain().await;
        self.tree.write().await.reset();
        self.settled.send_replace(Some(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::{self, BoxStream};
    use futures_util::{StreamExt, TryStreamExt};
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    #[test]
    fn names() {
        for ok in ["a", "A9", "x.y~z:w-v_u", &"a".repeat(128)] {
            assert!(is_name(ok), "{ok}");
        }
        for bad in ["", ".a", "-a", "a/b", "a b", "a%20", "é", &"a".repeat(129)] {
            assert!(!is_name(bad), "{bad}");
        }
        assert_eq!(of("/@doc/edit?x"), Some("doc"));
        assert_eq!(of("/@doc"), Some("doc"));
        assert_eq!(of("/doc"), None);
    }

    #[test]
    fn conditions() {
        let c = |m: Option<&str>, n: Option<&str>| Conditions {
            if_match: m.map(Into::into),
            if_none_match: n.map(Into::into),
        };
        let e = Some("\"abc\"");
        assert!(c(Some("*"), None).hold(e));
        assert!(!c(Some("*"), None).hold(None));
        assert!(c(Some("\"x\", \"abc\""), None).hold(e));
        assert!(!c(Some("W/\"abc\""), None).hold(e), "If-Match compares strongly");
        assert!(!c(None, Some("*")).hold(e));
        assert!(c(None, Some("*")).hold(None));
        assert!(!c(None, Some("W/\"abc\"")).hold(e), "If-None-Match compares weakly");
        assert!(c(None, Some("\"x\"")).hold(e));
        assert!(c(None, None).hold(None));
    }

    /// A bucket of heads, each with an `ETag` that is the SHA-256 of its bytes, as S3's is their MD5. Its conditional
    /// puts are of heads, and go as its plan says, a step each, and as asked once the plan is done; the rest goes to
    /// `inner`.
    #[derive(Debug, Default)]
    struct Lossy {
        inner: InMemory,
        heads: Mutex<HashMap<Path, Bytes>>,
        plan: Mutex<VecDeque<(Put, bool)>>, // how a put goes, and whether another turn's commit lands on it after
        tries: AtomicUsize,                 // the conditional puts
        down: AtomicBool,                   // so no head can be read or put
    }

    /// How a planned put goes.
    #[derive(Clone, Copy, Debug)]
    enum Put {
        Lands, // and its answer is lost
        Fails, // with no answer, and does not land
        Late,  // the put before it, which had no answer, lands as this one is sent, so this one is refused
    }

    impl Lossy {
        /// Plans the next conditional put, after which another turn's commit lands if `other`.
        fn plan(&self, put: Put, other: bool) {
            self.plan.lock().unwrap().push_back((put, other));
        }

        /// Puts `bytes` at `at`, if `mode` allows.
        fn write(&self, at: &Path, bytes: Bytes, mode: &PutMode) -> object_store::Result<PutResult> {
            let mut heads = self.heads.lock().unwrap();
            let etag = heads.get(at).map(|bytes| store::hash(bytes));
            let holds = match mode {
                PutMode::Create => etag.is_none(),
                PutMode::Update(base) => etag.is_some() && etag == base.e_tag,
                PutMode::Overwrite => true,
            };
            if !holds {
                return Err(object_store::Error::Precondition { path: at.to_string(), source: "refused".into() });
            }
            let e_tag = Some(store::hash(&bytes));
            heads.insert(at.clone(), bytes);
            Ok(PutResult { e_tag, version: None, extensions: Default::default() })
        }

        /// Lands another turn's commit on the head at `at`, which changes nothing in its tree.
        fn other(&self, at: &Path) {
            let mut heads = self.heads.lock().unwrap();
            let mut head: Head = heads.get(at).map_or_else(Default::default, |b| serde_json::from_slice(b).unwrap());
            head.commit = store::random();
            heads.insert(at.clone(), serde_json::to_vec(&head).unwrap().into());
        }
    }

    /// The error of a request that had no answer.
    fn lost() -> object_store::Error {
        object_store::Error::Generic { store: "lossy", source: "the answer was lost".into() }
    }

    impl std::fmt::Display for Lossy {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("lossy")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Lossy {
        async fn put_opts(&self, at: &Path, payload: PutPayload, opts: PutOptions) -> object_store::Result<PutResult> {
            if matches!(opts.mode, PutMode::Overwrite) {
                return self.inner.put_opts(at, payload, opts).await;
            }
            self.tries.fetch_add(1, SeqCst);
            if self.down.load(SeqCst) {
                return Err(lost());
            }
            let bytes = Bytes::from(payload);
            let Some((put, other)) = self.plan.lock().unwrap().pop_front() else {
                return self.write(at, bytes, &opts.mode);
            };
            if !matches!(put, Put::Fails) {
                self.write(at, bytes, &opts.mode)?;
            }
            if other {
                self.other(at);
            }
            match put {
                Put::Late => Err(object_store::Error::Precondition { path: at.to_string(), source: "late".into() }),
                Put::Lands | Put::Fails => Err(lost()),
            }
        }

        async fn put_multipart_opts(
            &self,
            at: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(at, opts).await
        }

        async fn get_opts(&self, at: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
            let head = self.heads.lock().unwrap().get(at).cloned();
            let Some(bytes) = head else { return self.inner.get_opts(at, opts).await };
            if self.down.load(SeqCst) {
                return Err(lost());
            }
            let (size, e_tag) = (bytes.len() as u64, Some(store::hash(&bytes)));
            let meta =
                ObjectMeta { location: at.clone(), last_modified: Default::default(), size, e_tag, version: None };
            let payload = GetResultPayload::Stream(stream::once(async move { Ok(bytes) }).boxed());
            Ok(GetResult {
                payload,
                meta,
                range: 0..size,
                attributes: Default::default(),
                extensions: Default::default(),
            })
        }

        fn delete_stream(
            &self,
            _: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            unreachable!("a turn deletes nothing, whatever becomes of it: the sweep is the only collector")
        }

        fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        fn list_with_offset(
            &self,
            prefix: Option<&Path>,
            at: &Path,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list_with_offset(prefix, at)
        }

        async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, opts).await
        }
    }

    /// A store through a new `Lossy`.
    fn fresh() -> (Store, Arc<Lossy>) {
        let lossy = Arc::new(Lossy::default());
        (Store { inner: lossy.clone(), once: None, versioned: true, cache: Arc::default() }, lossy)
    }

    /// A store through a `Lossy`, whose name `n` has one commit, of a value at `s`: the base of the turns.
    async fn based() -> (Store, Arc<Lossy>) {
        let (store, lossy) = fresh();
        let turn = writes(&store, "s").await.unwrap();
        assert!(matches!(commit(&turn).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        (store, lossy)
    }

    /// A turn on `n` that writes at `key` a value too large to be inline, so it is an object.
    async fn writes(store: &Store, key: &str) -> Result<Arc<Turn>, Answer> {
        let turn = Turn::open(store, "a", "n", &Default::default(), None).await.unwrap()?;
        turn.write(vec![(key.into(), Some(Bytes::from(vec![7; 5000])))]).await.unwrap();
        Ok(turn)
    }

    /// Commits `turn`.
    async fn commit(turn: &Turn) -> Result<Committed> {
        let sink: Sink = Arc::new(|_| Box::pin(async { Ok(()) }));
        turn.commit("h", &sink, StatusCode::OK, None).await
    }

    /// How many objects the name's tree has in the store.
    async fn objects(store: &Store) -> usize {
        store.objects(&store::app("a", &["values", "n"]), None).try_collect::<Vec<_>>().await.unwrap().len()
    }

    /// The value at `key`, as a turn would read it now.
    async fn value(store: &Store, key: &str) -> Option<Bytes> {
        Turn::snap(store, "a", "n").await.unwrap().get(key).await.unwrap()
    }

    /// The head of `n` now.
    async fn head_now(store: &Store) -> Head {
        read(store, &path("a", "n")).await.unwrap().unwrap().0
    }

    /// What a commit that wrote the head now answers.
    async fn done_now(store: &Store) -> Committed {
        Committed::Done(Some(head_now(store).await.etag()))
    }

    #[tokio::test]
    async fn a_commit_that_landed_but_lost_its_answer_is_done_and_keeps_what_it_made() {
        let (store, lossy) = based().await;
        lossy.plan(Put::Lands, false);
        let turn = writes(&store, "k").await.unwrap();
        let done = commit(&turn).await.unwrap();
        assert_eq!(lossy.tries.load(SeqCst), 1, "not put again, as the head says it landed");
        assert_eq!(done, done_now(&store).await, "not a conflict, so the turn does not run again");
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));
        assert_eq!(objects(&store).await, 2, "the base's and the turn's");
    }

    #[tokio::test]
    async fn a_commit_that_did_not_land_is_put_again() {
        // Nothing else changed the name, so it is put again, and lands.
        let (store, lossy) = based().await;
        for _ in 0..3 {
            lossy.plan(Put::Fails, false);
        }
        let turn = writes(&store, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), done_now(&store).await);
        assert_eq!((lossy.tries.load(SeqCst), objects(&store).await), (4, 2));

        // A try landed after all, as it was put again, so that was refused: the head says it landed.
        let (store, lossy) = based().await;
        for _ in 0..3 {
            lossy.plan(Put::Fails, false);
        }
        lossy.plan(Put::Late, false);
        let turn = writes(&store, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), done_now(&store).await);
        assert_eq!(lossy.tries.load(SeqCst), 4);
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));
    }

    #[tokio::test]
    async fn the_first_commit_on_a_name_learns_whether_it_landed() {
        let (store, lossy) = fresh();
        lossy.plan(Put::Lands, false);
        let turn = writes(&store, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), done_now(&store).await);
        assert_eq!(lossy.tries.load(SeqCst), 1);

        let (store, lossy) = fresh();
        for _ in 0..3 {
            lossy.plan(Put::Fails, false);
        }
        let turn = writes(&store, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), done_now(&store).await);
        assert_eq!(lossy.tries.load(SeqCst), 4);
    }

    #[tokio::test]
    async fn a_commit_whose_answer_was_lost_as_another_landed_is_unsettled() {
        // The head is another commit's, which may be over the turn's or over its base: whether the turn landed is not
        // known, so it neither answers as done nor runs again.
        for (put, landed) in [(Put::Lands, true), (Put::Fails, false)] {
            let (store, lossy) = based().await;
            lossy.plan(put, true);
            let e = commit(&writes(&store, "k").await.unwrap()).await.unwrap_err();
            assert_eq!(e.downcast_ref(), Some(&Failed::Unsettled), "{put:?}: {e:#}");
            assert_eq!(value(&store, "k").await.is_some(), landed, "{put:?}");
        }
        let (store, lossy) = fresh();
        lossy.plan(Put::Fails, true);
        let e = commit(&writes(&store, "k").await.unwrap()).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Unsettled), "another's first commit landed: {e:#}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_that_cannot_reach_the_bucket_is_unsettled_and_not_tried_again() {
        let (store, lossy) = based().await;
        let turn = writes(&store, "k").await.unwrap();
        lossy.down.store(true, SeqCst);
        let start = tokio::time::Instant::now();
        let e = commit(&turn).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Unsettled), "{e:#}");
        assert!(start.elapsed() >= SETTLE, "it asked for as long as it may");
        let res = crate::tric::failed(&e);
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(res.headers().get(http::header::RETRY_AFTER).is_none(), "so the client does not try again");
    }

    #[tokio::test]
    async fn a_commit_that_conflicts_keeps_what_it_made_for_the_sweep() {
        let (store, lossy) = based().await;
        let (first, second) = (writes(&store, "k").await.unwrap(), writes(&store, "l").await.unwrap());
        assert!(matches!(commit(&first).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        assert_eq!(commit(&second).await.unwrap(), Committed::Conflict);
        assert_eq!(lossy.tries.load(SeqCst), 1, "refused at once, so it did not land");
        assert_eq!((value(&store, "k").await.is_some(), value(&store, "l").await), (true, None));
        assert_eq!(objects(&store).await, 3, "the base's, the first's, and the second's for the sweep");
    }

    #[tokio::test]
    async fn a_commit_that_failed_before_its_put_may_be_tried_again() {
        // Its event is not handed over, so nothing lands.
        let (store, _) = based().await;
        let turn = writes(&store, "k").await.unwrap();
        let req = http::Request::builder().method("POST").uri("https://h/x").body(Default::default()).unwrap();
        turn.hold(Held::of(req).await.unwrap()).unwrap();
        let sink: Sink = Arc::new(|_| Box::pin(async { wasmtime::bail!("the sink failed") }));
        let e = turn.commit("h", &sink, StatusCode::OK, None).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Unsent), "{e:#}");
        assert_eq!(value(&store, "k").await, None, "it did not land");
        let res = crate::tric::failed(&e);
        assert_eq!(
            (res.status(), res.headers().get(http::header::RETRY_AFTER).is_some()),
            (StatusCode::SERVICE_UNAVAILABLE, true)
        );
    }

    #[tokio::test]
    async fn a_turn_that_sent_an_unsafe_request_may_not_run_again() {
        // Before it answers, the request goes at once, and the turn may not run again, as it would send it again.
        let (store, _) = based().await;
        let turn = writes(&store, "k").await.unwrap();
        assert!(turn.repeatable());
        assert_eq!(turn.before_unsafe().await, Ok(()));
        assert!(!turn.repeatable());
        assert!(matches!(commit(&writes(&store, "l").await.unwrap()).await.unwrap(), Committed::Done(Some(_))));
        assert_eq!(commit(&turn).await.unwrap(), Committed::Conflict, "so it answers 503, and what it wrote is gone");
        assert_eq!(value(&store, "k").await, None);

        // After it answers, the request waits for the commit, and goes only if that is done.
        for (done, sent) in [(true, Ok(())), (false, Err("the request's changes were discarded"))] {
            let turn = writes(&store, "m").await.unwrap();
            turn.answer();
            let unsafe_ = tokio::spawn({
                let turn = turn.clone();
                async move { turn.before_unsafe().await }
            });
            match done {
                true => assert!(matches!(commit(&turn).await.unwrap(), Committed::Done(Some(_)))),
                false => turn.discard().await,
            }
            assert_eq!(unsafe_.await.unwrap(), sent);
            assert!(turn.repeatable(), "it sent nothing before it answered");
        }
    }

    /// A request held for the outbox.
    async fn held() -> Held {
        Held::of(http::Request::builder().method("POST").uri("https://h/x").body(Default::default()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_commit_records_its_event_by_its_digest_and_hands_it_over_before_its_put() {
        let (store, _) = based().await;
        let events = Arc::new(Mutex::new(vec![]));
        let sink: Sink = Arc::new({
            let events = events.clone();
            move |bytes| {
                events.lock().unwrap().push(bytes);
                Box::pin(async { Ok(()) })
            }
        });
        let base = head_now(&store).await.commit;
        let (first, second) = (writes(&store, "k").await.unwrap(), writes(&store, "l").await.unwrap());
        for turn in [&first, &second] {
            turn.hold(held().await).unwrap();
        }
        assert!(matches!(first.commit("h", &sink, StatusCode::OK, None).await.unwrap(), Committed::Done(Some(_))));
        assert_eq!(second.commit("h", &sink, StatusCode::OK, None).await.unwrap(), Committed::Conflict);
        let events = events.lock().unwrap().clone();
        let day = serde_json::from_slice::<Event>(&events[0]).unwrap().day;
        let holds = async |bytes: &[u8]| holds(&store, "a", "n", day, &store::hash(bytes)).await.unwrap();
        let head = Some(head_now(&store).await.commit);
        assert_eq!(holds(&events[0]).await, (true, head.clone()), "the commit that landed");
        assert_eq!(holds(&events[1]).await, (false, head.clone()), "the one that did not, which never will");
        assert_eq!(holds(b"other").await, (false, head), "another event");
        let event = serde_json::from_slice::<Event>(&events[1]).unwrap();
        assert_eq!(event.base, Some(base), "the commit both wrote over");
    }

    #[tokio::test]
    async fn a_commit_that_would_put_the_name_over_its_limits_does_not_run_again() {
        // Its tree is full, so it has no room for the commit's records: of its key, or of its event.
        for event in [false, true] {
            let store = Store::memory();
            let full = serde_json::json!({ "tree": { "entries": tree::ENTRIES_MAX }, "commit": "c" });
            put(&store, &path("a", "n"), &serde_json::from_value(full).unwrap(), None).await.unwrap().unwrap();
            let turn = match event {
                false => keyed(&store, &once("1")).await.unwrap(),
                true => Turn::open(&store, "a", "n", &Default::default(), None).await.unwrap().unwrap(),
            };
            if event {
                turn.hold(held().await).unwrap();
            }
            let e = commit(&turn).await.unwrap_err();
            assert_eq!(e.downcast_ref(), Some(&Failed::TooLarge), "{e:#}");
            let res = crate::tric::failed(&e);
            assert_eq!(
                res.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "though nothing landed, as it would fail again"
            );
            assert!(res.headers().get(http::header::RETRY_AFTER).is_none());
        }
    }

    #[test]
    fn idempotency_keys() {
        let key = |values: &[&[u8]]| {
            let mut headers = HeaderMap::new();
            for v in values {
                headers.append(IDEMPOTENCY_KEY, HeaderValue::from_bytes(v).unwrap());
            }
            Idempotency::key(&headers)
        };
        assert_eq!(key(&[]), Ok(None));
        assert_eq!(key(&[b"k"]), Ok(Some(store::hash(b"k"))));
        assert_eq!(key(&[b"\"k\""]), Ok(Some(store::hash(b"\"k\""))), "taken as sent, quotes and all");
        assert_eq!(key(&[&[b'a'; 255]]), Ok(Some(store::hash(&[b'a'; 255]))));
        for bad in [&b""[..], &[b'a'; 256][..], "café".as_bytes(), &b"a b"[..], &b"a\tb"[..], &b"a,b"[..]] {
            assert_eq!(key(&[bad]), Err(StatusCode::BAD_REQUEST), "{bad:?}");
        }
        assert_eq!(key(&[b"k", b"k"]), Err(StatusCode::BAD_REQUEST), "one field");
    }

    #[test]
    fn fingerprints() {
        let of = |method: Method, uri: &'static str, body: &str| {
            Idempotency::of("k".into(), &method, &Uri::from_static(uri), body.as_bytes()).fingerprint
        };
        let one = of(Method::POST, "/@n/x?y=1", "b");
        assert_eq!(one, store::hash(b"POST /@n/x?y=1\nb"));
        assert_eq!(of(Method::POST, "https://h/@n/x?y=1", "b"), one, "the target is in origin form");
        for other in
            [of(Method::PUT, "/@n/x?y=1", "b"), of(Method::POST, "/@n/x?y=2", "b"), of(Method::POST, "/@n/x?y=1", "c")]
        {
            assert_ne!(other, one);
        }
    }

    /// The `Idempotency-Key` `k` of a `POST` to `/@n` of `body`.
    fn once(body: &str) -> Idempotency {
        Idempotency::of(store::hash(b"k"), &Method::POST, &Uri::from_static("/@n"), body.as_bytes())
    }

    /// A turn on `n` with `once`, if one is opened.
    async fn keyed(store: &Store, once: &Idempotency) -> Result<Arc<Turn>, Answer> {
        Turn::open(store, "a", "n", &Default::default(), Some(once)).await.unwrap()
    }

    /// Commits `turn`, as answered with `status` and `location`.
    async fn answered(turn: &Turn, status: u16, location: Option<&'static str>) -> Committed {
        let sink: Sink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let (status, location) = (StatusCode::from_u16(status).unwrap(), location.map(HeaderValue::from_static));
        turn.commit("h", &sink, status, location.as_ref()).await.unwrap()
    }

    #[tokio::test]
    async fn a_turn_whose_key_committed_is_answered_as_it_was_without_running() {
        let store = Store::memory();
        let turn = keyed(&store, &once("1")).await.unwrap();
        turn.increment("c", 1).await.unwrap();
        assert!(matches!(answered(&turn, 201, Some("/@n/1")).await, Committed::Done(Some(_))));
        let record = Record { fingerprint: once("1").fingerprint, status: 201, location: Some("/@n/1".into()) };
        assert_eq!(keyed(&store, &once("1")).await.err(), Some(Answer::Replay(record)));
        let other = Answer::Refused(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(keyed(&store, &once("2")).await.err(), Some(other), "the key, with another request");

        // Before the conditions, so a replay is never refused for what changed.
        let conditions = Conditions { if_match: Some("\"other\"".into()), if_none_match: None };
        let replay = Turn::open(&store, "a", "n", &conditions, Some(&once("1"))).await.unwrap();
        assert!(matches!(replay, Err(Answer::Replay(_))));
        assert_eq!(value(&store, "c").await, Some(Bytes::copy_from_slice(&1i64.to_le_bytes())), "it ran once");
    }

    #[tokio::test]
    async fn a_turn_with_a_key_records_it_whatever_it_changed() {
        // So a retry is answered as the first was, even if what the first found has changed since; a 4xx as it is.
        let store = Store::memory();
        let turn = keyed(&store, &once("1")).await.unwrap();
        assert!(matches!(answered(&turn, 409, None).await, Committed::Done(Some(_))), "a head, to hold the record");
        let replay = keyed(&store, &once("1")).await.err();
        assert!(matches!(replay, Some(Answer::Replay(Record { status: 409, location: None, .. }))), "{replay:?}");
    }

    #[tokio::test]
    async fn a_discarded_turn_records_nothing() {
        // As for a 5xx, or a trap.
        let store = Store::memory();
        let turn = keyed(&store, &once("1")).await.unwrap();
        turn.increment("c", 1).await.unwrap();
        turn.discard().await;
        keyed(&store, &once("1")).await.expect("so it runs again");
    }

    #[tokio::test]
    async fn a_record_is_found_from_the_day_before_its_own_to_the_day_after() {
        let store = Store::memory();
        let mut tree = tree::open(&store, "a", "n", &Root::default(), tree::config()).unwrap();
        let record = Record { fingerprint: "f".into(), status: 200, location: None };
        tree.write(vec![(record_key(100, "k"), Some(serde_json::to_vec(&record).unwrap().into()))]).await.unwrap();
        for (today, found) in [(98, false), (99, true), (100, true), (101, true), (102, false)] {
            assert_eq!(recorded(&tree, "k", today).await.unwrap().is_some(), found, "on day {today}");
        }
        assert_eq!(recorded(&tree, "l", 100).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_records_are_deleted_a_thousand_at_a_time_up_to_the_first_that_is_not() {
        let store = Store::memory();
        let mut tree = tree::open(&store, "a", "n", &Root::default(), tree::config()).unwrap();
        let days = [(40, 600), (50, 600), (98, 5), (99, 5), (100, 5)];
        let record = |(day, i)| (record_key(day, &format!("{i:064x}")), Some(Bytes::from_static(b"{}")));
        tree.write(days.iter().flat_map(|&(day, n)| (0..n).map(move |i| record((day, i)))).collect()).await.unwrap();
        tree.write(vec![(event_key(10, "e"), Some(Bytes::new()))]).await.unwrap();
        for n in [PRUNE_MAX, 205, 0] {
            let expired = expired(&tree, "i/", 99).await.unwrap();
            assert_eq!(expired.len(), n);
            assert!(expired.iter().all(|k| k.as_str() < record_key(99, "").as_str()), "none from day 99 on");
            tree.write(expired.into_iter().map(|k| (k, None)).collect()).await.unwrap();
        }
        assert_eq!(tree.scan("i/", None, 100).await.unwrap().len(), 10, "day 99's and day 100's are kept");
        assert_eq!(expired(&tree, "o/", 99).await.unwrap(), [event_key(10, "e")], "and the records of another kind");
    }

    #[tokio::test]
    async fn a_commit_that_puts_a_head_deletes_the_expired_records() {
        // A key's record is kept to yesterday, and an event's to the day before.
        let store = Store::memory();
        let today = now() / DAY;
        let mut tree = tree::open(&store, "a", "n", &Root::default(), tree::config()).unwrap();
        let mut records: Vec<_> = [today - 2, today - 1].map(|day| (record_key(day, "k"), Some(Bytes::new()))).into();
        records.extend([today - 3, today - 2].map(|day| (event_key(day, "e"), Some(Bytes::new()))));
        tree.write(records).await.unwrap();
        let head = Head { tree: tree.finish().await.unwrap(), commit: store::random() };
        put(&store, &path("a", "n"), &head, None).await.unwrap().unwrap();
        commit(&writes(&store, "k").await.unwrap()).await.unwrap();
        let snap = Turn::snap(&store, "a", "n").await.unwrap();
        let tree = snap.tree().read().await;
        assert_eq!(tree.scan("i/", None, 10).await.unwrap(), [record_key(today - 1, "k")]);
        assert_eq!(tree.scan("o/", None, 10).await.unwrap(), [event_key(today - 2, "e")]);
    }
}
