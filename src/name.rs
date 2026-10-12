//! Names: each name's state is one object, its head, which a turn changes with a compare-and-swap when it commits.
use crate::engine::ANSWER;
use crate::fs;
use crate::kv::{Error, KeyResponse, other};
use crate::outbox::{Event, Held, Sink, WINDOW};
use crate::store::{self, Store};
use crate::tree::{self, Full, Root, Tree};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use object_store::{PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{sync::RwLock, sync::watch, time::sleep, time::timeout_at};
use wasmtime::{Result, ensure, error::Context};

const NAME_MAX: usize = 128;
const KEY_MAX: usize = 256; // bytes
const VALUE_MAX: usize = 1 << 20;
pub const HEAD_MAX: usize = 1 << 20;
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
/// How long a commit's put may take, with learning whether it landed when its answer is lost; past that, the commit
/// fails with [`Failed::Unsettled`]. The guest's `ANSWER`, the claims it waits for and this fit in CloudFront's 60 s for an
/// answer from the origin, so the client hears the 500, and not a 504 that it might try again.
pub const SETTLE: Duration = Duration::from_secs(30);
/// The header that makes a turn apply once, however often it is sent: see [`Idempotency`].
pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const IDEMPOTENCY_KEY_MAX: usize = 255; // bytes
/// A day, in milliseconds: the records of `Idempotency-Key`s are kept by the Unix day they were made on.
const DAY: u64 = 24 * 3600 * 1000;
/// The most expired records of `Idempotency-Key`s one commit deletes.
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

/// A name's state: its tree, with the root in the head, the commits pending, the claim, the commit that wrote it and
/// the version it was written over.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    #[serde(default, skip_serializing_if = "Root::is_empty")]
    tree: Root,
    /// The commits whose background requests are not yet delivered.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending: BTreeMap<String, Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
    /// The random id of the last commit, which a claim, its release and `settle` keep; none before the first.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    commit: String,
    /// The version id of the head this one was written over, which `put` sets; none for the first head, or where the
    /// bucket keeps no versions. So a head's versions are a chain, and no head's bytes are ever another's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    over: Option<String>,
}

impl Head {
    /// Adds to `seen` the ids of the objects the head's tree is made of, with the name's own `app` and `name`, and
    /// gives how many nodes it read, or `None` if that would be over `max`: see [`Tree::mark`]. The tree is all a head
    /// names of its name's objects: its `pending` holds digests of events, its claim a lease, `commit` the id of a
    /// commit and `over` a version of the head itself, and none of them names an object.
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

/// Writes `head` over the version `base`, or where there is none, if that is still the head, with `over` set to
/// `base`'s version id: its new version, or `None` if another write landed first and this one did not. An error other
/// than [`Failed::TooLarge`] may have landed, or may yet: see [`Store::put`].
async fn put(
    store: &Store,
    path: &Path,
    head: &mut Head,
    base: Option<UpdateVersion>,
) -> Result<Option<UpdateVersion>> {
    head.over = base.as_ref().and_then(|v| v.version.clone());
    let json = serde_json::to_vec(head)?;
    ensure!(json.len() <= HEAD_MAX, Failed::TooLarge);
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

/// The first `PRUNE_MAX` records of `Idempotency-Key`s, up to the first made yesterday or after, which no lookup reads.
async fn expired(tree: &Tree, today: u64) -> Result<Vec<String>> {
    let old = |k: &String| k.split('/').nth(1).and_then(|d| d.parse::<u64>().ok()).is_some_and(|d| d + 1 < today);
    if !tree.scan("i/", None, 1).await?.first().is_some_and(old) {
        return Ok(vec![]); // as there mostly are none, at the cost of one key
    }
    Ok(tree.scan("i/", None, PRUNE_MAX).await?.into_iter().take_while(old).collect())
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
    head: Head,                  // as read, or as this turn's claim wrote it
    base: Option<UpdateVersion>, // the head's version, if there is a head
    answered: bool,
    claimed: bool, // once this turn's claim is taken
    doomed: bool,  // a claim failed, so the commit will
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
    once: Option<Idempotency>,
}

/// How a commit went.
#[derive(Debug, PartialEq)]
pub enum Committed {
    Done(Option<String>), // with the head's `ETag`, if that is known
    Conflict,             // another turn changed the name first
}

/// What became of the commit `commit`, whose head `head` was put over `base` with no sure answer: what [`fate`] says,
/// asked again while it cannot say yet, after a wait from 100 ms that doubles up to 2 s; `None` if it cannot say.
async fn learn(
    store: &Store,
    path: &Path,
    head: &mut Head,
    base: Option<&UpdateVersion>,
    commit: &str,
) -> Option<Committed> {
    let mut wait = Duration::from_millis(100);
    loop {
        match fate(store, path, head, base, commit).await {
            Ok(fate) => return fate,
            Err(e) => tracing::info!(%path, commit, "commit: {e:#}"),
        }
        sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(2));
    }
}

/// What became of the commit `commit`, whose head `head` was put over `base` with no sure answer, by the head as it is
/// now: done, with the head's `ETag` if the head is still the one the commit wrote, or a conflict, as another write
/// landed on its base, so it never will; `None` if the head cannot say, and an error if it cannot say yet.
///
/// Each head names the commit that wrote it, and the version it was written over, so no head's bytes, and so no head's
/// `ETag`, are ever another's, and a put on `base`'s `ETag` lands on `base` itself or not at all. So a head's versions
/// are a chain, and only the one written over `base` can be the commit's:
/// - if the head is still `base`, or there is still none where there was none, nothing has landed on it, and the commit
///   is put again, as it was, which lands only if the first put has not; if that is refused, something has landed
///   since, so the head is asked again;
/// - if the head names the commit, it landed;
/// - else the commit landed if the version written over `base` names it, and never will if that version does not.
///
/// The chain is read back from the head, one version at a time, so with no versions, a head that is gone or a version
/// that has expired, whether the commit landed is unknown.
async fn fate(
    store: &Store,
    path: &Path,
    head: &mut Head,
    base: Option<&UpdateVersion>,
    commit: &str,
) -> Result<Option<Committed>> {
    let read = read(store, path).await?;
    let untouched = match (&read, base) {
        (None, None) => true,
        (Some((_, version)), Some(base)) => same(base, version),
        _ => false,
    };
    if untouched {
        let put = put(store, path, head, base.cloned()).await?;
        return put.map(|v| Some(Committed::Done(etag(&v)))).context("put again, and refused: something landed since");
    }
    let Some((mut now, version)) = read else { return Ok(None) };
    let over = base.and_then(|b| b.version.as_deref());
    if now.commit == commit {
        // A commit's own head is the one its answer would describe: one written over it has others' changes.
        let own = now.over.as_deref() == over && now.claim.is_none();
        return Ok(Some(Committed::Done(if own { etag(&version) } else { None })));
    }
    if version.version.is_none() || base.is_some_and(|b| b.version.is_none()) {
        return Ok(None);
    }
    loop {
        if now.over.as_deref() == over {
            return Ok(Some(if now.commit == commit { Committed::Done(None) } else { Committed::Conflict }));
        }
        let Some(before) = now.over.as_deref() else { return Ok(None) };
        let Some(bytes) = store.version(path, before, HEAD_MAX as u64).await? else { return Ok(None) };
        now = serde_json::from_slice(&bytes)?;
    }
}

/// Whether `a` and `b` are the same version of a head: by their version ids, and where the bucket keeps none, by their
/// `ETag`s.
fn same(a: &UpdateVersion, b: &UpdateVersion) -> bool {
    match (&a.version, &b.version) {
        (None, None) => a.e_tag.is_some() && a.e_tag == b.e_tag,
        (a, b) => a == b,
    }
}

/// Why a commit failed, where that decides how the turn answers: only an unclaimed turn's commit that failed before
/// its put ([`Failed::Unsent`]) answers 503, so a client may try again, as nothing landed; any other failure answers
/// 500, as it may have landed, or did what the turn claimed the name for, which running again would do again. An error
/// carries at most one.
#[derive(Debug, PartialEq)]
pub enum Failed {
    /// The name would be over its limits: its head over `HEAD_MAX`, or its tree too full for the record of an
    /// `Idempotency-Key`. The head is the root of the tree, at most `NODE_MAX`, and the pending commits, at most
    /// `PENDING_MAX` of them, so that is a backstop. Running again would fail again, as for a trap, so it is never
    /// `Unsent`.
    TooLarge,
    /// An unclaimed turn's commit failed before its head was put, so nothing landed.
    Unsent,
    /// The put had no sure answer, and the head did not say within `SETTLE` whether it landed. It may have, so a
    /// client that tried again could run the turn twice.
    Unsettled,
    /// A claimed turn's claim ran out, and another write landed on its base first. The turn did what it claimed the
    /// name for, which cannot be undone, so it does not run again.
    Lapsed,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Failed::TooLarge => write!(f, "the name would be over its limits"),
            Failed::Unsent => write!(f, "the commit failed before its head was put"),
            Failed::Unsettled => {
                write!(f, "the commit may have landed, and the name did not say within {SETTLE:?} whether it did")
            }
            Failed::Lapsed => write!(f, "the turn's claim ran out, and another write landed on the name first"),
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
        let (claiming, fs) = (Default::default(), Default::default());
        Ok(Arc::new(Self { store: store.clone(), app, name, path, state, tree, fs, claiming, settled, once }))
    }

    /// A snapshot of `name`: as it is now, without waiting for anyone's claim.
    pub async fn snap(store: &Store, app: &str, name: &str) -> Result<Arc<Self>> {
        Self::new(store, app, name, read(store, &path(app, name)).await?, true, None)
    }

    /// Opens a turn on `name`, once no one else's claim is live, if `conditions` hold, and claimed if `claim`; or
    /// answers without one. A turn whose `Idempotency-Key` `once` committed is answered as it was, at once, or with 422
    /// if its request was another; else the turn is refused with 412 if the conditions do not hold, or with 429 if
    /// others still hold the name, or keep changing it, at `until`.
    pub async fn open(
        store: &Store,
        app: &str,
        name: &str,
        claim: bool,
        conditions: &Conditions,
        once: Option<&Idempotency>,
        until: Instant,
    ) -> Result<Result<Arc<Self>, Answer>> {
        loop {
            let turn = Self::new(store, app, name, read(store, &path(app, name)).await?, false, once.cloned())?;
            if let Some(once) = once
                && let Some(record) = recorded(&*turn.tree.read().await, &once.key, now() / DAY).await?
            {
                return Ok(Err(match record.fingerprint == once.fingerprint {
                    true => Answer::Replay(record),
                    false => Answer::Refused(StatusCode::UNPROCESSABLE_ENTITY),
                }));
            }
            let free = !turn.state().head.claim.as_ref().is_some_and(Claim::live);
            if free && !conditions.hold(turn.etag().as_deref()) {
                return Ok(Err(Answer::Refused(StatusCode::PRECONDITION_FAILED)));
            }
            if free && (!claim || turn.claim().await) {
                return Ok(Ok(turn));
            }
            if Instant::now() >= until {
                return Ok(Err(Answer::Refused(StatusCode::TOO_MANY_REQUESTS)));
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
                if s.claimed || s.doomed || s.answered {
                    return s.claimed;
                }
                (s.head.clone(), s.base.clone())
            };
            let id = store::random();
            head.claim = Some(Claim { id: id.clone(), until: now() + CLAIM_TTL.as_millis() as u64 });
            let landed = match put(&turn.store, &turn.path, &mut head, base).await {
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
                    (s.head.claim, s.base, s.claimed) = (head.claim, Some(version), true);
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
    /// requests go to `sink` as a delivery event whose digest the head keeps in `pending`, and its head goes over the
    /// version it read, if that is still the head. A commit that puts a head deletes expired records of
    /// `Idempotency-Key`s, and a turn with a key always puts one, to record it, whatever it changed. The `ETag` it
    /// returns is the new head's, or the old one's when there was nothing to write. A commit that fails is discarded.
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
        let _one = self.claiming.lock().await; // so a claim in flight lands first
        let (mut head, base, held, claimed, doomed) = {
            let s = &mut *self.state();
            s.answered = true;
            let held = std::mem::take(&mut s.held);
            (s.head.clone(), s.base.clone(), held, s.claimed, s.doomed)
        };
        self.fs.drain().await; // so the file system's changes that were admitted land first
        let mut tree = self.tree.write().await; // after the writes that were in flight
        if doomed {
            tree.reset();
            return Ok(Committed::Conflict);
        }
        if !tree.edited() && held.is_empty() && !claimed && self.once.is_none() {
            return Ok(Committed::Done(base.as_ref().and_then(etag)));
        }
        let commit = store::random();
        let ready = async {
            let mut kept = None;
            if tree.edited() {
                kept = self.fs.settle(&mut tree).await?;
            }
            let now = now();
            let mut edits: Vec<_> = expired(&tree, now / DAY).await?.into_iter().map(|k| (k, None)).collect();
            if let Some(once) = &self.once {
                let location = location.and_then(|l| l.to_str().ok()).map(Into::into);
                let record = Record { fingerprint: once.fingerprint.clone(), status: status.as_u16(), location };
                edits.push((record_key(now / DAY, &once.key), Some(serde_json::to_vec(&record)?.into())));
            }
            tree.write(edits).await.map_err(|e| match e {
                // A name too full for the record stays so until something is deleted: it is not tried again.
                tree::Error::Full(_) | tree::Error::TooLarge(_) => wasmtime::Error::from(e).context(Failed::TooLarge),
                e => e.into(),
            })?;
            if tree.edited() {
                head.tree = tree.finish().await?;
            }
            head.pending.retain(|_, p| p.live(now));
            head.claim = None;
            head.commit = commit.clone();
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
                // Before the event is handed over, or it would be sent in vain.
                ensure!(serde_json::to_vec(&head)?.len() <= HEAD_MAX, Failed::TooLarge);
                sink(bytes).await?;
            }
            Ok::<_, wasmtime::Error>(kept)
        };
        let kept = match ready.await {
            Ok(kept) => kept,
            // Nothing landed, so an unclaimed turn may run again, unless it would fail again; a claimed one did what it
            // claimed the name for.
            Err(e) => {
                tree.reset();
                let again = !claimed && e.downcast_ref::<Failed>().is_none(); // so not `TooLarge`
                return Err(if again { e.context(Failed::Unsent) } else { e });
            }
        };
        let landed = match self.land(&mut head, base, &commit).await {
            // A claimed turn sent what it claimed the name for, so it runs once at most, whatever became of its claim.
            Ok(Committed::Conflict) if claimed => Err(wasmtime::Error::msg(Failed::Lapsed)),
            landed => landed,
        };
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

    /// Puts `head`, the commit `commit`'s, over `base`. A put with no sure answer may have landed, or may yet, so the
    /// head's history says which, as [`learn`] reads it, until `SETTLE` after the put began: a commit that landed is
    /// done, and one that never will is a conflict; else it fails with [`Failed::Unsettled`], and is neither done nor
    /// run again.
    async fn land(&self, head: &mut Head, base: Option<UpdateVersion>, commit: &str) -> Result<Committed> {
        let until = tokio::time::Instant::now() + SETTLE;
        let lost = match timeout_at(until, put(&self.store, &self.path, head, base.clone())).await {
            Ok(Ok(Some(version))) => return Ok(Committed::Done(etag(&version))),
            Ok(Ok(None)) => return Ok(Committed::Conflict),
            Ok(Err(e)) if e.downcast_ref() == Some(&Failed::TooLarge) => return Err(e), // so it did not go
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => format!("no answer within {SETTLE:?}"),
        };
        let fate = timeout_at(until, learn(&self.store, &self.path, head, base.as_ref(), commit)).await;
        let fate = fate.ok().flatten();
        tracing::warn!(path = %self.path, commit, ?fate, "commit: {lost}");
        fate.context(Failed::Unsettled)
    }

    /// Discards the turn, and its claim if it took one.
    pub async fn discard(&self) {
        let _one = self.claiming.lock().await;
        let (mut head, base, claimed) = {
            let s = &mut *self.state();
            s.answered = true; // so the claim timer takes no claim after
            s.held.clear();
            (s.head.clone(), s.base.clone(), std::mem::take(&mut s.claimed))
        };
        self.fs.drain().await;
        self.tree.write().await.reset();
        self.settled.send_replace(Some(false));
        if claimed {
            head.claim = None;
            _ = put(&self.store, &self.path, &mut head, base).await; // if it fails, the claim expires
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
        if let Ok(Some(_)) = put(store, &path, &mut head, Some(version)).await {
            return;
        }
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

    #[tokio::test]
    async fn a_name_with_too_many_commits_pending_is_backlogged() {
        for (live, backlogged) in [(PENDING_MAX, true), (PENDING_MAX - 1, false)] {
            let (store, mut head) = (Store::memory(), Head::default());
            for i in 0..=PENDING_MAX {
                let at = if i < live { now() } else { 0 }; // the rest have expired, and do not count
                head.pending.insert(store::random(), Pending { digest: store::hash(&[]), at });
            }
            assert!(serde_json::to_vec(&head).unwrap().len() < HEAD_MAX / 4, "the head stays far under its limit");
            put(&store, &path("a", "n"), &mut head, None).await.unwrap().unwrap();
            let turn = Turn::open(&store, "a", "n", false, &Default::default(), None, Instant::now()).await;
            assert_eq!(turn.unwrap().ok().unwrap().backlogged(), backlogged, "{live} live");
        }
    }

    #[tokio::test]
    async fn too_large() {
        let (store, mut head) = (Store::memory(), Head::default());
        for _ in 0..10_000 {
            head.pending.insert(store::random(), Pending { digest: store::hash(&[]), at: now() });
        }
        let put = put(&store, &path("a", "n"), &mut head, None).await;
        assert_eq!(put.err().unwrap().downcast_ref(), Some(&Failed::TooLarge));
        assert!(read(&store, &path("a", "n")).await.unwrap().is_none());
    }

    /// A versioned bucket, which keeps every version of a head, as S3 does, each with an `ETag` that is the SHA-256 of
    /// its bytes, as S3's is their MD5. Its conditional puts are of heads, and go as its plan says, a step each, and as
    /// asked once the plan is done; the rest goes to `inner`.
    #[derive(Debug, Default)]
    struct Lossy {
        inner: InMemory,
        heads: Mutex<HashMap<Path, Vec<(String, Bytes)>>>, // each head's versions, with their ids, the latest last
        plan: Mutex<VecDeque<(Put, &'static [Then])>>,     // how a put goes, and the writes that land on it after
        tries: AtomicUsize,                                // the conditional puts
        down: AtomicBool,                                  // so no head can be read or put
    }

    /// How a planned put goes.
    #[derive(Clone, Copy, Debug)]
    enum Put {
        Lands, // and its answer is lost
        Fails, // with no answer, and does not land
        Late,  // the put before it, which had no answer, lands as this one is sent, so this one is refused
    }

    /// What lands on a head after a planned put.
    #[derive(Clone, Copy, Debug)]
    enum Then {
        Commit,  // another turn's commit
        Claim,   // another turn's claim, which keeps the head's commit
        Release, // the claim let go, which keeps the head as it was but for the claim
        Expire,  // not a write: every version but the latest expires
    }

    impl Lossy {
        /// Plans the next conditional put.
        fn plan(&self, put: Put, then: &'static [Then]) {
            self.plan.lock().unwrap().push_back((put, then));
        }

        /// Puts `bytes` at `at` as a new version, if `mode` allows.
        fn write(&self, at: &Path, bytes: Bytes, mode: &PutMode) -> object_store::Result<PutResult> {
            let mut heads = self.heads.lock().unwrap();
            let versions = heads.entry(at.clone()).or_default();
            let etag = versions.last().map(|(_, bytes)| store::hash(bytes));
            let holds = match mode {
                PutMode::Create => etag.is_none(),
                PutMode::Update(base) => etag.is_some() && etag == base.e_tag,
                PutMode::Overwrite => true,
            };
            if !holds {
                return Err(object_store::Error::Precondition { path: at.to_string(), source: "refused".into() });
            }
            let id = store::random();
            versions.push((id.clone(), bytes.clone()));
            Ok(PutResult { e_tag: Some(store::hash(&bytes)), version: Some(id), extensions: Default::default() })
        }

        /// Lands `then` on the head at `at`.
        fn other(&self, at: &Path, then: Then) {
            let mut heads = self.heads.lock().unwrap();
            let versions = heads.entry(at.clone()).or_default();
            let mut head = Head::default();
            if let Some((id, bytes)) = versions.last() {
                head = serde_json::from_slice(bytes).unwrap();
                head.over = Some(id.clone());
            }
            match then {
                Then::Commit => (head.claim, head.commit) = (None, store::random()),
                Then::Claim => head.claim = Some(Claim { id: store::random(), until: now() + 60_000 }),
                Then::Release => head.claim = None,
                Then::Expire => {
                    versions.drain(..versions.len().saturating_sub(1));
                    return;
                }
            }
            versions.push((store::random(), serde_json::to_vec(&head).unwrap().into()));
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
            let Some((put, thens)) = self.plan.lock().unwrap().pop_front() else {
                return self.write(at, bytes, &opts.mode);
            };
            if !matches!(put, Put::Fails) {
                self.write(at, bytes, &opts.mode)?;
            }
            for &then in thens {
                self.other(at, then);
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
            let versions = self.heads.lock().unwrap().get(at).cloned();
            let Some(versions) = versions else { return self.inner.get_opts(at, opts).await };
            if self.down.load(SeqCst) {
                return Err(lost());
            }
            // The latest version, or the one asked for.
            let found = versions.into_iter().rfind(|(id, _)| opts.version.as_ref().is_none_or(|v| v == id));
            let Some((id, bytes)) = found else {
                return Err(object_store::Error::NotFound { path: at.to_string(), source: "no such version".into() });
            };
            let size = bytes.len() as u64;
            let (e_tag, version) = (Some(store::hash(&bytes)), Some(id));
            let meta = ObjectMeta { location: at.clone(), last_modified: Default::default(), size, e_tag, version };
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

    /// A versioned store through a new `Lossy`.
    fn fresh() -> (Store, Arc<Lossy>) {
        let lossy = Arc::new(Lossy::default());
        (Store { inner: lossy.clone(), once: None, versioned: true, cache: Arc::default() }, lossy)
    }

    /// A versioned store through a `Lossy`, whose name `n` has one commit, of a value at `s`: the base of the turns.
    async fn based() -> (Store, Arc<Lossy>) {
        let (store, lossy) = fresh();
        let turn = writes(&store, false, "s").await.unwrap();
        assert!(matches!(commit(&turn).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        (store, lossy)
    }

    /// A turn on `n`, claimed if `claim`, that writes at `key` a value too large to be inline, so it is an object.
    async fn writes(store: &Store, claim: bool, key: &str) -> Result<Arc<Turn>, Answer> {
        let turn = Turn::open(store, "a", "n", claim, &Default::default(), None, Instant::now()).await.unwrap()?;
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

    /// The head's `ETag` now.
    async fn etag_now(store: &Store) -> Option<String> {
        etag(&read(store, &path("a", "n")).await.unwrap().unwrap().1)
    }

    #[tokio::test]
    async fn a_commit_that_landed_but_lost_its_answer_is_done_and_keeps_what_it_made() {
        let (store, lossy) = based().await;
        lossy.plan(Put::Lands, &[]);
        let turn = writes(&store, false, "k").await.unwrap();
        let done = commit(&turn).await.unwrap();
        assert_eq!(lossy.tries.load(SeqCst), 2, "tried again, and refused as it had landed");
        assert_eq!(done, Committed::Done(etag_now(&store).await), "not a conflict, so the turn does not run again");
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));
        assert_eq!(objects(&store).await, 2, "the base's and the turn's");

        // Others wrote on it before the head was read, so the head is not its own: done, with no ETag. The head's
        // versions say so, however far back the commit's is.
        let others: [&'static [Then]; 3] = [&[Then::Commit], &[Then::Claim], &[Then::Claim, Then::Commit]];
        for then in others {
            let (store, lossy) = based().await;
            lossy.plan(Put::Lands, then);
            let turn = writes(&store, false, "k").await.unwrap();
            assert_eq!(commit(&turn).await.unwrap(), Committed::Done(None), "{then:?}");
            assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])), "{then:?}");
            assert_eq!(objects(&store).await, 2, "{then:?}");
        }
    }

    #[tokio::test]
    async fn a_commit_that_did_not_land_is_put_again_or_is_a_conflict() {
        // Nothing else changed the name, so it is put again, and lands.
        let (store, lossy) = based().await;
        for _ in 0..store::TRIES {
            lossy.plan(Put::Fails, &[]);
        }
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert_eq!((lossy.tries.load(SeqCst), objects(&store).await), (store::TRIES as usize + 1, 2));

        // A try landed after all, as it was put again, so that was refused: the head says it landed.
        let (store, lossy) = based().await;
        for _ in 0..store::TRIES {
            lossy.plan(Put::Fails, &[]);
        }
        lossy.plan(Put::Late, &[]);
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert_eq!(lossy.tries.load(SeqCst), store::TRIES as usize + 1);
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])));

        // Another write landed on its base instead, so it never will: a conflict, and what it made is left for the
        // sweep. A claim taken and let go is such a write, though the name is as it was.
        let others: [&'static [Then]; 3] = [&[Then::Commit], &[Then::Claim], &[Then::Claim, Then::Release]];
        for then in others {
            let (store, lossy) = based().await;
            lossy.plan(Put::Fails, then);
            let turn = writes(&store, false, "k").await.unwrap();
            assert_eq!(commit(&turn).await.unwrap(), Committed::Conflict, "{then:?}");
            assert_eq!((value(&store, "k").await, objects(&store).await), (None, 2), "{then:?}");
        }
    }

    #[tokio::test]
    async fn the_first_commit_on_a_name_learns_whether_it_landed() {
        let (store, lossy) = fresh();
        lossy.plan(Put::Lands, &[]);
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert_eq!(lossy.tries.load(SeqCst), 2);

        let (store, lossy) = fresh();
        for _ in 0..store::TRIES {
            lossy.plan(Put::Fails, &[]);
        }
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert_eq!(lossy.tries.load(SeqCst), store::TRIES as usize + 1);

        let (store, lossy) = fresh();
        lossy.plan(Put::Fails, &[Then::Commit]);
        let turn = writes(&store, false, "k").await.unwrap();
        assert_eq!(commit(&turn).await.unwrap(), Committed::Conflict, "another's first commit landed instead");
    }

    #[tokio::test]
    async fn a_commit_whose_head_cannot_say_whether_it_landed_is_unsettled() {
        // It landed, and another commit on it, and the version it wrote has expired.
        let (store, lossy) = based().await;
        lossy.plan(Put::Lands, &[Then::Commit, Then::Expire]);
        let turn = writes(&store, false, "k").await.unwrap();
        let e = commit(&turn).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Unsettled), "{e:#}");
        assert_eq!(value(&store, "k").await, Some(Bytes::from(vec![7; 5000])), "so it may not run again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_that_cannot_reach_the_bucket_is_unsettled_and_not_tried_again() {
        let (store, lossy) = based().await;
        let turn = writes(&store, false, "k").await.unwrap();
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
        let (first, second) = (writes(&store, false, "k").await.unwrap(), writes(&store, false, "l").await.unwrap());
        assert!(matches!(commit(&first).await.unwrap(), Committed::Done(Some(_))));
        lossy.tries.store(0, SeqCst);
        assert_eq!(commit(&second).await.unwrap(), Committed::Conflict);
        assert_eq!(lossy.tries.load(SeqCst), 1, "refused at once, so it did not land");
        assert_eq!((value(&store, "k").await.is_some(), value(&store, "l").await), (true, None));
        assert_eq!(objects(&store).await, 3, "the base's, the first's, and the second's for the sweep");
    }

    #[tokio::test]
    async fn a_claimed_turn_whose_claim_ran_out_does_not_run_again() {
        // Its put has no answer, and another commit lands on its claim, as it would once the claim had run out.
        let (store, lossy) = based().await;
        let turn = writes(&store, true, "k").await.unwrap();
        lossy.plan(Put::Fails, &[Then::Commit]);
        let e = commit(&turn).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Lapsed), "{e:#}");
        assert_eq!(value(&store, "k").await, None, "it did not land");
        let res = crate::tric::failed(&e);
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(res.headers().get(http::header::RETRY_AFTER).is_none(), "so the client does not try again");

        // Another commit landed on its claim before its put, which is refused at once.
        let (store, _) = based().await;
        let turn = writes(&store, true, "k").await.unwrap();
        let (mut other, version) = read(&store, &path("a", "n")).await.unwrap().unwrap();
        (other.claim, other.commit) = (None, store::random());
        put(&store, &path("a", "n"), &mut other, Some(version)).await.unwrap().unwrap();
        let e = commit(&turn).await.unwrap_err();
        assert_eq!(e.downcast_ref(), Some(&Failed::Lapsed), "{e:#}");
    }

    #[tokio::test]
    async fn a_claimed_turn_whose_commit_failed_before_its_put_does_not_run_again() {
        // Its event is not handed over, so nothing lands: an unclaimed turn may run again, and a claimed one may not.
        for claim in [false, true] {
            let (store, _) = based().await;
            let turn = writes(&store, claim, "k").await.unwrap();
            let req = http::Request::builder().method("POST").uri("https://h/x").body(Default::default()).unwrap();
            turn.hold(Held::of(req).await.unwrap()).unwrap();
            let sink: Sink = Arc::new(|_| Box::pin(async { wasmtime::bail!("the sink failed") }));
            let e = turn.commit("h", &sink, StatusCode::OK, None).await.unwrap_err();
            assert_eq!(e.downcast_ref() == Some(&Failed::Unsent), !claim, "{e:#}");
            assert_eq!(value(&store, "k").await, None, "it did not land");
            let res = crate::tric::failed(&e);
            let answer = (res.status(), res.headers().get(http::header::RETRY_AFTER).is_some());
            match claim {
                false => assert_eq!(answer, (StatusCode::SERVICE_UNAVAILABLE, true)),
                true => assert_eq!(answer, (StatusCode::INTERNAL_SERVER_ERROR, false), "so it does not run again"),
            }
            assert!(read(&store, &path("a", "n")).await.unwrap().unwrap().0.claim.is_none(), "and its claim is let go");
        }
    }

    #[tokio::test]
    async fn a_commit_that_would_put_the_name_over_its_limits_does_not_run_again() {
        // Its head is over `HEAD_MAX` once it is the commit's: as it is put, or, with an event, before that is sent.
        for event in [false, true] {
            let (store, mut head) = (Store::memory(), Head::default());
            head.pending.insert("p".into(), Pending { digest: String::new(), at: now() });
            let size = serde_json::to_vec(&head).unwrap().len();
            head.pending.get_mut("p").unwrap().digest = "0".repeat(HEAD_MAX - size - 10);
            put(&store, &path("a", "n"), &mut head, None).await.unwrap().unwrap();
            let turn = writes(&store, false, "k").await.unwrap();
            if event {
                let req = http::Request::builder().method("POST").uri("https://h/x").body(Default::default()).unwrap();
                turn.hold(Held::of(req).await.unwrap()).unwrap();
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

    #[tokio::test]
    async fn a_claim_that_landed_but_lost_its_answer_is_held() {
        let (store, lossy) = based().await;
        lossy.plan(Put::Lands, &[]);
        let turn = writes(&store, true, "k").await.expect("the claim is the turn's, not another's");
        assert!(turn.state().claimed && !turn.doomed());
        assert_eq!(commit(&turn).await.unwrap(), Committed::Done(etag_now(&store).await));
        assert!(read(&store, &path("a", "n")).await.unwrap().unwrap().0.claim.is_none());
    }

    #[tokio::test]
    async fn a_claim_let_go_leaves_a_head_unlike_the_one_it_was_taken_on() {
        let (store, _) = based().await;
        let at = path("a", "n");
        let (before, base) = store.get(&at, HEAD_MAX as u64).await.unwrap().unwrap();
        let turn = writes(&store, true, "k").await.unwrap();
        assert!(turn.state().claimed);
        turn.discard().await;
        let (after, version) = store.get(&at, HEAD_MAX as u64).await.unwrap().unwrap();
        let (was, is): (Head, Head) =
            (serde_json::from_slice(&before).unwrap(), serde_json::from_slice(&after).unwrap());
        assert!(is.claim.is_none() && is.commit == was.commit, "the name is as it was");
        assert_ne!(after, before, "but not its bytes");
        assert_ne!(version.e_tag, base.e_tag, "nor its ETag");
        let put = put(&store, &at, &mut Head::default(), Some(base)).await.unwrap();
        assert!(put.is_none(), "so a put on the head it was taken on, as of a commit put again, is refused");
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

    /// A turn on `n`, claimed if `claim`, with `once`, if one is opened at once.
    async fn keyed(store: &Store, claim: bool, once: &Idempotency) -> Result<Arc<Turn>, Answer> {
        Turn::open(store, "a", "n", claim, &Default::default(), Some(once), Instant::now()).await.unwrap()
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
        let turn = keyed(&store, false, &once("1")).await.unwrap();
        turn.increment("c", 1).await.unwrap();
        assert!(matches!(answered(&turn, 201, Some("/@n/1")).await, Committed::Done(Some(_))));
        let record = Record { fingerprint: once("1").fingerprint, status: 201, location: Some("/@n/1".into()) };
        assert_eq!(keyed(&store, false, &once("1")).await.err(), Some(Answer::Replay(record)));
        let other = Answer::Refused(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(keyed(&store, false, &once("2")).await.err(), Some(other), "the key, with another request");

        // Before anyone's claim, and the conditions, so a replay never waits, and is never refused for what changed.
        let claimed = writes(&store, true, "k").await.unwrap();
        let conditions = Conditions { if_match: Some("\"other\"".into()), if_none_match: None };
        let replay = Turn::open(&store, "a", "n", false, &conditions, Some(&once("1")), Instant::now()).await.unwrap();
        assert!(matches!(replay, Err(Answer::Replay(_))));
        claimed.discard().await;
        assert_eq!(value(&store, "c").await, Some(Bytes::copy_from_slice(&1i64.to_le_bytes())), "it ran once");
    }

    #[tokio::test]
    async fn a_turn_with_a_key_records_it_whatever_it_changed() {
        // So a retry is answered as the first was, even if what the first found has changed since; a 4xx as it is.
        let store = Store::memory();
        let turn = keyed(&store, false, &once("1")).await.unwrap();
        assert!(matches!(answered(&turn, 409, None).await, Committed::Done(Some(_))), "a head, to hold the record");
        let replay = keyed(&store, false, &once("1")).await.err();
        assert!(matches!(replay, Some(Answer::Replay(Record { status: 409, location: None, .. }))), "{replay:?}");
    }

    #[tokio::test]
    async fn a_discarded_turn_records_nothing() {
        // As for a 5xx, or a trap.
        let store = Store::memory();
        let turn = keyed(&store, false, &once("1")).await.unwrap();
        turn.increment("c", 1).await.unwrap();
        turn.discard().await;
        keyed(&store, false, &once("1")).await.expect("so it runs again");
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
        for n in [PRUNE_MAX, 205, 0] {
            let expired = expired(&tree, 100).await.unwrap();
            assert_eq!(expired.len(), n);
            assert!(expired.iter().all(|k| k.as_str() < record_key(99, "").as_str()), "none from yesterday on");
            tree.write(expired.into_iter().map(|k| (k, None)).collect()).await.unwrap();
        }
        assert_eq!(tree.scan("i/", None, 100).await.unwrap().len(), 10, "yesterday's and today's are kept");
    }

    #[tokio::test]
    async fn a_commit_that_puts_a_head_deletes_the_records_made_before_yesterday() {
        let store = Store::memory();
        let today = now() / DAY;
        let mut tree = tree::open(&store, "a", "n", &Root::default(), tree::config()).unwrap();
        let records = [today - 2, today - 1].map(|day| (record_key(day, "k"), Some(Bytes::from_static(b"{}"))));
        tree.write(records.into()).await.unwrap();
        let mut head = Head { tree: tree.finish().await.unwrap(), ..Default::default() };
        put(&store, &path("a", "n"), &mut head, None).await.unwrap().unwrap();
        commit(&writes(&store, false, "k").await.unwrap()).await.unwrap();
        let snap = Turn::snap(&store, "a", "n").await.unwrap();
        assert_eq!(snap.tree().read().await.scan("i/", None, 10).await.unwrap(), [record_key(today - 1, "k")]);
    }
}
