//! Names: each name's state is one object, its head, which a turn changes with a compare-and-swap when it commits.
use crate::engine::ANSWER;
use crate::fs;
use crate::kv::{Error, KeyResponse, other};
use crate::outbox::{Event, Held, Sink, WINDOW};
use crate::store::{self, Store};
use crate::tree::{Full, Limits, Shape, Tree};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use object_store::{PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{sync::RwLock, sync::watch, task::JoinHandle, time::sleep};
use wasmtime::{Result, ensure};

const NAME_MAX: usize = 128;
const KEY_MAX: usize = 256; // bytes
const VALUE_MAX: usize = 1 << 20;
const HEAD_MAX: usize = 1 << 20;
const HELD_MAX: usize = 1_000_000; // bytes of held requests, as JSON, so an event is within Lambda's 1 MB
const PAGE: usize = 1000; // keys per `list-keys`
/// How long a turn waits for others' claims, and reruns, before it gives up with a 429.
pub const BUSY: Duration = Duration::from_secs(5);
/// A claim outlives the deadline of the turn that took it, by the time a commit may take.
const CLAIM_TTL: Duration = ANSWER.saturating_add(Duration::from_secs(5));
/// How long a commit stays pending: the window its delivery is tried in, and an hour more, for an event that Lambda
/// was late to hand over.
const PENDING_TTL: Duration = WINDOW.saturating_add(Duration::from_secs(3600));
/// The most commits a name may have pending, at some 130 bytes each in its head. At that, a turn's background requests
/// are refused, so the head stays far under `HEAD_MAX`, and no pending commit is dropped before its time.
const PENDING_MAX: usize = 1000;
/// How many of the last commits a head names, at some 35 bytes each: enough that a commit whose answer was lost finds
/// itself among them though others landed on it while it read.
const COMMITS: usize = 8;

/// Whether `s` is a name: 1 to 128 of `A-Za-z0-9._~:-`, starting with a letter or digit.
pub fn is_name(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b"._~:-".contains(&b);
    s.len() <= NAME_MAX && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) && s.bytes().all(ok)
}

/// The name a path addresses: what follows `/@`, up to the next `/`.
pub fn of(path: &str) -> Option<&str> {
    path.strip_prefix("/@").map(|rest| rest.split('/').next().unwrap_or_default())
}

/// A name's state: its tree, with the root in the head, the commits pending, the claim and the last commits.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    #[serde(default, skip_serializing_if = "Shape::is_empty")]
    tree: Shape,
    /// The commits whose background requests are not yet delivered.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending: BTreeMap<String, Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
    /// The ids of the last `COMMITS` commits that landed, the newest last. Each is random, so no head after a commit
    /// is ever the same as one before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    commits: Vec<String>,
}

impl Head {
    /// The ids of the objects the head's tree is made of, with the name's own `app` and `name`: see
    /// [`Tree::reachable`]. The head is the one root of a name's objects: its `pending` holds digests of events and
    /// its claim a lease, and neither names an object.
    pub async fn reachable(&self, store: &Store, app: &str, name: &str, max: usize) -> Result<Option<HashSet<u128>>> {
        Tree::open(store, app, name, &self.tree, Limits::default())?.reachable(max).await
    }
}

/// A commit's delivery event, by its SHA-256, and when the commit landed, in Unix milliseconds.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub digest: String,
    at: u64,
}

impl Pending {
    fn live(&self, now: u64) -> bool {
        now.saturating_sub(self.at) < PENDING_TTL.as_millis() as u64
    }
}

/// A turn's lease on a name, until a Unix millisecond.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    id: String,
    until: u64,
}

impl Claim {
    fn live(&self) -> bool {
        self.until > now()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn path(app: &str, name: &str) -> Path {
    store::app(app, &["names", name])
}

/// The head at `path`, with its version. Each read and write of a head logs, at debug, its `ETag` and what it took:
/// the numbers that say whether a cache of heads in each environment would pay.
pub async fn read(store: &Store, path: &Path) -> Result<Option<(Head, UpdateVersion)>> {
    let start = Instant::now();
    let read = store.json(path, HEAD_MAX as u64).await;
    let etag = read.as_ref().ok().and_then(Option::as_ref).and_then(|(_, v)| v.e_tag.as_deref());
    tracing::debug!(%path, etag, ms = start.elapsed().as_millis() as u64, "read");
    read
}

/// Writes `head` over the version `base`, or where there is none, if that is still the head: its new version, or
/// `None` if another write landed first and this one did not. An error other than [`TooLarge`] may have landed, or may
/// yet: see [`Store::put`].
async fn put(store: &Store, path: &Path, head: &Head, base: Option<UpdateVersion>) -> Result<Option<UpdateVersion>> {
    let json = serde_json::to_vec(head)?;
    ensure!(json.len() <= HEAD_MAX, TooLarge);
    let (start, bytes, over) = (Instant::now(), json.len(), base.as_ref().and_then(|v| v.e_tag.clone()));
    let put = store.put(path, json.into(), base.map_or(PutMode::Create, PutMode::Update)).await;
    let etag = put.as_ref().ok().and_then(Option::as_ref).and_then(|v| v.e_tag.as_deref());
    tracing::debug!(%path, over, etag, bytes, ms = start.elapsed().as_millis() as u64, "put");
    put
}

/// The `ETag` of a head's version: a strong one, whatever the store says.
fn etag(version: &UpdateVersion) -> Option<String> {
    version.e_tag.as_ref().map(|e| format!("\"{}\"", e.trim_matches('"')))
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

/// What a turn changes, under its lock.
#[derive(Default)]
struct State {
    head: Head,                  // as read, or as this turn's claim wrote it
    base: Option<UpdateVersion>, // the head's version, if there is a head
    answered: bool,
    claim: Option<String>, // this turn's, once taken
    doomed: bool,          // a claim failed, so the commit will
    held: Vec<Held>,
    held_size: usize,
}

/// A request that may write one name: its writes are in its tree, which is committed or discarded once it answers. A
/// snapshot is a turn that has answered: it reads the name as it was, and writes nothing.
///
/// The locks are taken in the order `claiming`, the file system's gate (which a commit takes and lets go of at once),
/// `tree`, `state`; `state` is never held across an await.
pub struct Turn {
    store: Store,
    app: String,
    pub name: String,
    path: Path,
    state: Mutex<State>,
    tree: RwLock<Tree>,
    /// What the file system keeps: the descriptors open on it, and the gate its changes pass.
    pub fs: fs::State,
    claiming: tokio::sync::Mutex<()>,
    settled: watch::Sender<Option<bool>>, // whether it committed, once it is known
    reaping: Mutex<Vec<JoinHandle<()>>>,  // the deletes of objects no head names, which the response waits for
}

/// How a commit went.
#[derive(Debug, PartialEq)]
pub enum Committed {
    Done(Option<String>), // with the head's `ETag`, if that is known
    Conflict,             // another turn changed the name first
}

/// What became of a commit whose put had no sure answer, by the head as it is now.
#[derive(Debug, PartialEq)]
enum Fate {
    Landed(Option<String>), // with the head's `ETag`, if the commit is still the newest
    Unknown,
}

impl Head {
    /// The fate of the commit `commit`, if this is the head now, at `version`. It landed if this head names it, and
    /// else it may not have, or may yet.
    fn fate(&self, version: &UpdateVersion, commit: &str) -> Fate {
        let commits = &self.commits;
        match commits.iter().position(|c| c == commit) {
            // A commit's own head is the one its answer would have described: one on top has others' changes.
            Some(i) => Fate::Landed(if i + 1 == commits.len() { etag(version) } else { None }),
            None => Fate::Unknown,
        }
    }
}

/// Why a commit failed when its head would be over `HEAD_MAX`. The head is the root of the tree, at most `NODE_MAX`,
/// and the pending commits, at most `PENDING_MAX` of them, so this is a backstop; it answers 500, as for a trap.
#[derive(Debug)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the name would be over {HEAD_MAX} bytes")
    }
}

/// The key of `key` in the tree: the keys of the keyvalue store are under `k/`.
fn tree_key(key: &str) -> String {
    format!("k/{key}")
}

/// What the guest is told of a tree error: that a limit was hit, which it can act on, and else only that the name
/// failed, as the rest is the host's to know.
fn fail(e: wasmtime::Error) -> Error {
    if let Some(full) = e.downcast_ref::<Full>() {
        return other(full);
    }
    tracing::warn!("name: {e:#}");
    other("the name could not be read or written")
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
    /// A turn on `name` with the head `read`, answered already if `answered`.
    fn new(
        store: &Store,
        app: &str,
        name: &str,
        read: Option<(Head, UpdateVersion)>,
        answered: bool,
    ) -> Result<Arc<Self>> {
        let (head, base) = read.map_or_else(Default::default, |(head, version)| (head, Some(version)));
        let tree = RwLock::new(Tree::open(store, app, name, &head.tree, Limits::default())?);
        let state = Mutex::new(State { head, base, answered, ..Default::default() });
        let (app, name, path, settled) = (app.into(), name.into(), path(app, name), watch::Sender::new(None));
        let (claiming, reaping) = (Default::default(), Default::default());
        let fs = Default::default();
        Ok(Arc::new(Self { store: store.clone(), app, name, path, state, tree, fs, claiming, settled, reaping }))
    }

    /// A snapshot of `name`: as it is now, without waiting for anyone's claim.
    pub async fn snap(store: &Store, app: &str, name: &str) -> Result<Arc<Self>> {
        Self::new(store, app, name, read(store, &path(app, name)).await?, true)
    }

    /// Opens a turn on `name`, once no one else's claim is live, if `conditions` hold, and claimed if `claim`; or
    /// refuses it, with 412 if they do not, or with 429 if others still hold the name, or keep changing it, at `until`.
    pub async fn open(
        store: &Store,
        app: &str,
        name: &str,
        claim: bool,
        conditions: &Conditions,
        until: Instant,
    ) -> Result<Result<Arc<Self>, StatusCode>> {
        loop {
            let turn = Self::new(store, app, name, read(store, &path(app, name)).await?, false)?;
            let free = !turn.state().head.claim.as_ref().is_some_and(Claim::live);
            if free && !conditions.hold(turn.etag().as_deref()) {
                return Ok(Err(StatusCode::PRECONDITION_FAILED));
            }
            if free && (!claim || turn.claim().await) {
                return Ok(Ok(turn));
            }
            if Instant::now() >= until {
                return Ok(Err(StatusCode::TOO_MANY_REQUESTS));
            }
            sleep(Duration::from_millis(rand::random_range(25..=50))).await;
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// The `ETag` of the head as the turn last read or wrote it; none if there is no head.
    pub fn etag(&self) -> Option<String> {
        self.state().base.as_ref().and_then(etag)
    }

    /// Claims the name, so that no other turn commits before this one, and returns whether it holds the claim. One that
    /// fails dooms the turn: something else changed the name, so its commit will fail. Once the turn has answered there
    /// is nothing left to claim for. The claim is a task of its own, so a caller that goes away leaves no claim the
    /// turn does not know of.
    pub async fn claim(self: &Arc<Self>) -> bool {
        let turn = self.clone();
        let claim = async move {
            let _one = turn.claiming.lock().await;
            let (mut head, base) = {
                let s = turn.state();
                if s.claim.is_some() || s.doomed || s.answered {
                    return s.claim.is_some();
                }
                (s.head.clone(), s.base.clone())
            };
            let id = store::random();
            head.claim = Some(Claim { id: id.clone(), until: now() + CLAIM_TTL.as_millis() as u64 });
            let landed = match put(&turn.store, &turn.path, &head, base).await {
                Ok(landed) => landed,
                // It may have landed all the same: the claim is the turn's if the head holds it, and only the turn
                // changes a head that holds its live claim.
                Err(_) => match read(&turn.store, &turn.path).await {
                    Ok(Some((now, version))) if now.claim.as_ref().is_some_and(|c| c.id == id) => Some(version),
                    _ => None,
                },
            };
            let mut s = turn.state();
            match landed {
                Some(version) => {
                    (s.head.claim, s.base, s.claim) = (head.claim, Some(version), Some(id));
                    true
                }
                None => {
                    s.doomed = true;
                    false
                }
            }
        };
        tokio::spawn(claim).await.unwrap_or(false)
    }

    /// Readies an unsafe outbound request: before the answer, by claiming the name; after it, by waiting for the
    /// commit.
    pub async fn before_unsafe(self: &Arc<Self>) -> Result<(), &'static str> {
        if self.claim().await {
            return Ok(());
        }
        if !self.state().answered {
            return Err("another request changed the name first, so this one will run again");
        }
        match self.settled.subscribe().wait_for(Option::is_some).await.map(|v| *v) {
            Ok(Some(true)) => Ok(()),
            _ => Err("the request's changes were discarded"),
        }
    }

    /// Whether a claim failed, so the turn's commit will.
    pub fn doomed(&self) -> bool {
        self.state().doomed
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

    /// Whether the name has `PENDING_MAX` commits pending, so that the turn's commit would be one too many for its
    /// background requests.
    pub fn backlogged(&self) -> bool {
        let now = now();
        self.state().head.pending.values().filter(|p| p.live(now)).count() >= PENDING_MAX
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
        let edits = items.into_iter().map(|(k, v)| (tree_key(&k), v.map(|v| tree.item(v)))).collect();
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
        let item = tree.item(Bytes::copy_from_slice(&next.to_le_bytes()));
        tree.write(vec![(key, Some(item))]).await.map_err(fail)?;
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
        let item = tree.item(value);
        tree.write(vec![(key, Some(item))]).await.map_err(fail)?;
        Ok(true)
    }

    /// Deletes `paths`, which no head names, in the background; the response waits for it, in `reaped`.
    fn reap(&self, paths: Vec<Path>) {
        if paths.is_empty() {
            return;
        }
        let store = self.store.clone();
        self.reaping.lock().unwrap().push(tokio::spawn(async move {
            store.delete_many(paths).await;
        }));
    }

    /// The deletes the turn has started, to wait for before the response ends: a Lambda that is frozen does not finish
    /// them.
    pub fn reaped(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.reaping.lock().unwrap())
    }

    /// Commits the turn, asked at `host`: its tree is uploaded, its held requests go to `sink` as a delivery event
    /// whose digest the head keeps in `pending`, and its head goes over the version it read, if that is still the
    /// head. The `ETag` it returns is the new head's, or the old one's when there was nothing to write. A commit that
    /// fails is discarded.
    pub async fn commit(&self, host: &str, sink: &Sink) -> Result<Committed> {
        let committed = self.try_commit(host, sink).await;
        if committed.is_err() {
            self.discard().await;
        }
        self.settled.send_replace(Some(matches!(committed, Ok(Committed::Done(_)))));
        committed
    }

    async fn try_commit(&self, host: &str, sink: &Sink) -> Result<Committed> {
        let _one = self.claiming.lock().await; // so a claim in flight lands first
        let (mut head, base, held, claimed, doomed) = {
            let s = &mut *self.state();
            s.answered = true;
            let held = std::mem::take(&mut s.held);
            (s.head.clone(), s.base.clone(), held, s.claim.is_some(), s.doomed)
        };
        self.fs.drain().await; // so the file system's changes that were admitted land first
        let mut tree = self.tree.write().await; // after the writes that were in flight
        if doomed {
            self.reap(tree.lost());
            return Ok(Committed::Conflict);
        }
        if !tree.edited() && held.is_empty() && !claimed {
            return Ok(Committed::Done(base.as_ref().and_then(etag)));
        }
        let commit = store::random();
        let ready = async {
            let mut kept = None;
            if tree.edited() {
                kept = self.fs.settle(&mut tree).await?;
                head.tree = tree.finish().await?;
            }
            let now = now();
            head.pending.retain(|_, p| p.live(now));
            head.claim = None;
            head.commits.push(commit.clone());
            head.commits.drain(..head.commits.len().saturating_sub(COMMITS));
            if !held.is_empty() {
                let event = Event {
                    app: self.app.clone(),
                    host: host.into(),
                    name: self.name.clone(),
                    base: base.as_ref().and_then(|v| v.e_tag.clone()),
                    commit: commit.clone(),
                    requests: held,
                };
                let bytes = Bytes::from(serde_json::to_vec(&event)?);
                head.pending.insert(commit.clone(), Pending { digest: store::hash(&bytes), at: now });
                ensure!(serde_json::to_vec(&head)?.len() <= HEAD_MAX, TooLarge); // or the event would be sent in vain
                sink(bytes).await?;
            }
            Ok::<_, wasmtime::Error>(kept)
        };
        let kept = match ready.await {
            Ok(kept) => kept,
            Err(e) => {
                self.reap(tree.lost());
                return Err(e);
            }
        };
        let landed = match put(&self.store, &self.path, &head, base).await {
            Ok(Some(version)) => Some(etag(&version)),
            Ok(None) => None,
            // The head was too large, so it did not go.
            Err(e) if e.is::<TooLarge>() => {
                self.reap(tree.lost());
                return Err(e);
            }
            // Else it may have landed, or may yet: the head says which, if it can.
            Err(e) => {
                let fate = match read(&self.store, &self.path).await {
                    Ok(Some((now, version))) => now.fate(&version, &commit),
                    _ => Fate::Unknown,
                };
                tracing::warn!(path = %self.path, commit, ?fate, "commit: {e:#}");
                match fate {
                    Fate::Landed(etag) => Some(etag),
                    // If it lands, the head names what the tree made, so that is left for the sweep.
                    Fate::Unknown => {
                        tree.lost();
                        return Err(e);
                    }
                }
            }
        };
        match landed {
            Some(etag) => {
                self.reap(tree.landed());
                if let Some(kept) = kept {
                    self.fs.keep(kept); // under the write lock, so a read sees it from the first
                }
                Ok(Committed::Done(etag))
            }
            None => {
                self.reap(tree.lost());
                Ok(Committed::Conflict)
            }
        }
    }

    /// Discards the turn, and its claim if it took one.
    pub async fn discard(&self) {
        let _one = self.claiming.lock().await;
        let (mut head, base, claimed) = {
            let s = &mut *self.state();
            s.answered = true; // so the claim timer takes no claim after
            s.held.clear();
            (s.head.clone(), s.base.clone(), s.claim.take())
        };
        self.fs.drain().await;
        self.reap(self.tree.write().await.lost());
        self.settled.send_replace(Some(false));
        if claimed.is_some() {
            head.claim = None;
            _ = put(&self.store, &self.path, &head, base).await; // if it fails, the claim expires
        }
    }
}

/// Takes `commit` off the pending ones of `name`, as far as it can in a few tries. Never while the name is claimed, as
/// that would break the claim: the commit then stays until a later commit drops it, after `PENDING_TTL`.
pub async fn settle(store: &Store, app: &str, name: &str, commit: &str) {
    let path = path(app, name);
    for _ in 0..3 {
        let Ok(Some((mut head, version))) = read(store, &path).await else { return };
        if head.pending.remove(commit).is_none() || head.claim.as_ref().is_some_and(Claim::live) {
            return;
        }
        if let Ok(Some(_)) = put(store, &path, &head, Some(version)).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt,
        PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

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

    #[tokio::test]
    async fn a_name_with_too_many_commits_pending_is_backlogged() {
        for (live, backlogged) in [(PENDING_MAX, true), (PENDING_MAX - 1, false)] {
            let (store, mut head) = (Store::memory(), Head::default());
            for i in 0..=PENDING_MAX {
                let at = if i < live { now() } else { 0 }; // the rest have expired, and do not count
                head.pending.insert(store::random(), Pending { digest: store::hash(&[]), at });
            }
            assert!(serde_json::to_vec(&head).unwrap().len() < HEAD_MAX / 4, "the head stays far under its limit");
            put(&store, &path("a", "n"), &head, None).await.unwrap().unwrap();
            let turn = Turn::open(&store, "a", "n", false, &Default::default(), Instant::now()).await;
            assert_eq!(turn.unwrap().ok().unwrap().backlogged(), backlogged, "{live} live");
        }
    }

    #[tokio::test]
    async fn too_large() {
        let (store, mut head) = (Store::memory(), Head::default());
        for _ in 0..10_000 {
            head.pending.insert(store::random(), Pending { digest: store::hash(&[]), at: now() });
        }
        let put = put(&store, &path("a", "n"), &head, None).await;
        assert!(put.err().unwrap().is::<TooLarge>());
        assert!(read(&store, &path("a", "n")).await.unwrap().is_none());
    }

    /// A store whose conditional puts go as its plan says, a step each, and as asked once the plan is done. A step is
    /// whether the put lands and what lands on it after; then the put fails, as one whose answer was lost.
    #[derive(Debug, Default)]
    struct Lossy {
        inner: InMemory,
        plan: Mutex<VecDeque<(bool, Then)>>,
        tries: AtomicUsize, // the conditional puts
    }

    #[derive(Clone, Copy, Debug)]
    enum Then {
        Nothing,
        Commit, // another turn's commit, which lands on whatever the head is
        Claim,  // another turn's claim, which does too
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
            let Some((lands, then)) = self.plan.lock().unwrap().pop_front() else {
                return self.inner.put_opts(at, payload, opts).await;
            };
            if lands {
                self.inner.put_opts(at, payload, opts).await?;
            }
            let mut head: Head = match self.inner.get(at).await {
                Ok(got) => serde_json::from_slice(&got.bytes().await?).unwrap(),
                Err(_) => Head::default(),
            };
            match then {
                Then::Nothing => {}
                Then::Commit => {
                    head.claim = None;
                    head.commits.push(store::random());
                }
                Then::Claim => head.claim = Some(Claim { id: store::random(), until: now() + 60_000 }),
            }
            if !matches!(then, Then::Nothing) {
                self.inner.put(at, serde_json::to_vec(&head).unwrap().into()).await?;
            }
            Err(object_store::Error::Generic { store: "lossy", source: "the answer was lost".into() })
        }

        async fn put_multipart_opts(
            &self,
            at: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(at, opts).await
        }

        async fn get_opts(&self, at: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
            self.inner.get_opts(at, opts).await
        }

        fn delete_stream(
            &self,
            paths: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(paths)
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

    /// A versioned store through a `Lossy`, whose name `n` has one commit, of a value at `s`: the base of the turns.
    async fn based() -> (Store, Arc<Lossy>) {
        let lossy = Arc::new(Lossy::default());
        let store = Store { inner: lossy.clone(), versioned: true, cache: Arc::default() };
        let turn = writes(&store, false, "s").await.unwrap();
        assert!(matches!(commit(&turn).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        (store, lossy)
    }

    /// A turn on `n`, claimed if `claim`, that writes at `key` a value too large to be inline, so it is an object.
    async fn writes(store: &Store, claim: bool, key: &str) -> Result<Arc<Turn>, StatusCode> {
        let turn = Turn::open(store, "a", "n", claim, &Default::default(), Instant::now()).await.unwrap()?;
        turn.write(vec![(key.into(), Some(Bytes::from(vec![7; 5000])))]).await.unwrap();
        Ok(turn)
    }

    /// Commits `turn`, and waits for what it deletes.
    async fn commit(turn: &Turn) -> Result<Committed> {
        let sink: Sink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let committed = turn.commit("h", &sink).await;
        for reaping in turn.reaped() {
            reaping.await.unwrap();
        }
        committed
    }

    /// How many objects the name's tree has in the store.
    async fn objects(store: &Store) -> usize {
        store.objects(&store::app("a", &["values", "n"]), None).try_collect::<Vec<_>>().await.unwrap().len()
    }

    /// The value at `key`, as a turn would read it now.
    async fn value(store: &Store, key: &str) -> Option<Bytes> {
        Turn::snap(store, "a", "n").await.unwrap().get(key).await.unwrap()
    }

    /// The head's `ETag` now.
    async fn etag_now(store: &Store) -> Option<String> {
        etag(&read(store, &path("a", "n")).await.unwrap().unwrap().1)
    }

    #[tokio::test]
    async fn a_commit_that_landed_but_lost_its_answer_is_done_and_keeps_what_it_made() {
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((true, Then::Nothing));
        let turn = writes(&store, false, "k").await.unwrap();
        let done = commit(&turn).await.unwrap();
        assert_eq!(lossy.tries.load(SeqCst), 2, "tried again, and refused as it had landed");
        assert_eq!(done, Committed::Done(etag_now(&store).await), "not a conflict, so the turn does not run again");
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));
        assert_eq!(objects(&store).await, 2, "the base's and the turn's");

        // Another commit landed on it before the head was read: done, but with no ETag, as the head is not its own.
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((true, Then::Commit));
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(None));
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));
        assert_eq!(objects(&store).await, 2);
    }

    #[tokio::test]
    async fn a_commit_that_did_not_land_is_tried_again_or_settled_by_the_head() {
        // Nothing else changed the name, so it is tried again, and lands.
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((false, Then::Nothing));
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert_eq!((lossy.tries.load(SeqCst), objects(&store).await), (2, 2));

        // Another commit landed instead, but the head does not say that this one never will: the commit fails, and
        // deletes nothing it made.
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((false, Then::Commit));
        let turn = writes(&store, false, "k").await.unwrap();
        assert!(commit(&turn).await.is_err());
        assert_eq!((value(&store, "k").await, objects(&store).await), (None, 2));

        // Another took a claim, and committed nothing: the put may still land on the head once the claim is let go,
        // so the commit fails, and deletes nothing it made.
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((false, Then::Claim));
        let turn = writes(&store, false, "k").await.unwrap();
        assert!(commit(&turn).await.is_err());
        assert_eq!((value(&store, "k").await, objects(&store).await), (None, 2));

        // It never had an answer.
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().extend([(false, Then::Nothing); store::TRIES as usize]);
        let turn = writes(&store, false, "k").await.unwrap();
        assert!(commit(&turn).await.is_err());
        assert_eq!((lossy.tries.load(SeqCst), objects(&store).await), (store::TRIES as usize, 2));
    }

    #[tokio::test]
    async fn a_commit_that_conflicts_deletes_what_it_made_and_runs_again() {
        let (store, lossy) = based().await;
        let (first, second) = (writes(&store, false, "k").await.unwrap(), writes(&store, false, "l").await.unwrap());
        assert!(matches!(commit(&first).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        assert_eq!(commit(&second).await.unwrap(), Committed::Conflict);
        assert_eq!(lossy.tries.load(SeqCst), 1, "refused at once, so it did not land");
        assert_eq!((value(&store, "k").await.is_some(), value(&store, "l").await), (true, None));
        assert_eq!(objects(&store).await, 2, "the base's and the first's");
    }

    #[tokio::test]
    async fn a_claim_that_landed_but_lost_its_answer_is_held() {
        let (store, lossy) = based().await;
        lossy.plan.lock().unwrap().push_back((true, Then::Nothing));
        let turn = writes(&store, true, "k").await.expect("the claim is the turn's, not another's");
        assert!(turn.state().claim.is_some() && !turn.doomed());
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert!(read(&store, &path("a", "n")).await.unwrap().unwrap().0.claim.is_none());
    }

    #[test]
    fn fate() {
        let version = UpdateVersion { e_tag: Some("e".into()), version: None };
        let head = |commits: &[&str]| Head { commits: commits.iter().map(|&c| c.into()).collect(), ..Head::default() };
        let abc = head(&["a", "b", "c"]);
        assert_eq!(abc.fate(&version, "c"), Fate::Landed(Some("\"e\"".into())));
        assert_eq!(abc.fate(&version, "b"), Fate::Landed(None), "another landed on it");
        assert_eq!(abc.fate(&version, "x"), Fate::Unknown);
        assert_eq!(head(&[]).fate(&version, "x"), Fate::Unknown);
    }
}
