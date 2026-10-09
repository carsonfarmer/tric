//! Names: each name's state is one object, its head, which a turn changes with a compare-and-swap when it commits.
use crate::engine::ANSWER;
use crate::kv::{Error, KeyResponse, other};
use crate::outbox::{Event, Held, Sink};
use crate::store::{self, Store};
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use object_store::{PutMode, UpdateVersion, path::Path};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::{sync::watch, time::sleep};
use wasmtime::{Result, ensure};

const NAME_MAX: usize = 128;
const KEY_MAX: usize = 256; // bytes
const VALUE_MAX: usize = 1 << 20;
const INLINE_MAX: usize = 1 << 10; // a larger value is an object of its own
const HEAD_MAX: usize = 1 << 20;
const HELD_MAX: usize = 1_000_000; // bytes of held requests, as JSON, so an event is within Lambda's 1 MB
const PAGE: usize = 1000; // keys per `list-keys`
/// How long a turn waits for others' claims, and reruns, before it gives up with a 429.
pub const BUSY: Duration = Duration::from_secs(5);
/// A claim outlives the deadline of the turn that took it, by the time a commit may take.
const CLAIM_TTL: Duration = ANSWER.saturating_add(Duration::from_secs(5));
/// How long a commit stays pending: Lambda's longest wait for an event.
const PENDING_TTL: Duration = Duration::from_secs(6 * 3600);

/// Whether `s` is a name: 1 to 128 of `A-Za-z0-9._~:-`, starting with a letter or digit.
pub fn is_name(s: &str) -> bool {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b"._~:-".contains(&b);
    s.len() <= NAME_MAX && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) && s.bytes().all(ok)
}

/// The name a path addresses: what follows `/@`, up to the next `/`.
pub fn of(path: &str) -> Option<&str> {
    path.strip_prefix("/@").map(|rest| rest.split('/').next().unwrap_or_default())
}

/// A name's state.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    values: BTreeMap<String, Value>,
    /// The commits whose background requests are not yet delivered.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending: BTreeMap<String, Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<Claim>,
}

/// A commit's delivery event, by its SHA-256, and when the commit landed, in Unix milliseconds.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub digest: String,
    at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Value {
    Data(String), // base64
    /// A value too large to keep in the head: the object `apps/<app>/values/<key>` at `version`, in a versioned bucket.
    Object {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
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

fn value_path(app: &str, key: &str) -> Path {
    store::app(app, &["values", key])
}

/// The head at `path`, with its version.
pub async fn read(store: &Store, path: &Path) -> Result<Option<(Head, UpdateVersion)>> {
    store.json(path, HEAD_MAX as u64).await
}

/// Writes `head` over the version `base`, or where there is none, if that is still the head: its new version, or
/// `None` if another write landed first.
async fn put(store: &Store, path: &Path, head: &Head, base: Option<UpdateVersion>) -> Result<Option<UpdateVersion>> {
    let json = serde_json::to_vec(head)?;
    ensure!(json.len() <= HEAD_MAX, TooLarge);
    store.put(path, json.into(), base.map_or(PutMode::Create, PutMode::Update)).await
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
    writes: BTreeMap<String, Option<Bytes>>,
    answered: bool,
    claim: Option<String>, // this turn's, once taken
    doomed: bool,          // a claim failed, so the commit will
    held: Vec<Held>,
    held_size: usize,
}

/// A request that may write one name: its writes are buffered, and committed or discarded once it answers. A snapshot
/// is a turn that has answered: it reads the name as it was, and writes nothing.
pub struct Turn {
    store: Store,
    app: String,
    pub name: String,
    path: Path,
    state: Mutex<State>,
    claiming: tokio::sync::Mutex<()>,
    settled: watch::Sender<Option<bool>>, // whether it committed, once it is known
}

/// How a commit went.
pub enum Committed {
    Done(Option<String>), // with the head's `ETag`
    Conflict,             // another turn changed the name first
}

/// Why a commit failed when its head would be over `HEAD_MAX`: the app wrote too much, so, as for a trap, it answers 500.
#[derive(Debug)]
pub struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the name would be over {HEAD_MAX} bytes")
    }
}

impl State {
    fn write(&mut self, items: Vec<(String, Option<Bytes>)>) -> Result<(), Error> {
        if self.answered {
            return Err(Error::AccessDenied);
        }
        for (k, v) in &items {
            if !(1..=KEY_MAX).contains(&k.len()) {
                return Err(other(format!("a key is 1 to {KEY_MAX} bytes")));
            }
            if v.as_ref().is_some_and(|v| v.len() > VALUE_MAX) {
                return Err(other(format!("a value is {VALUE_MAX} bytes or less")));
            }
        }
        self.writes.extend(items);
        Ok(())
    }
}

impl Turn {
    /// A turn on `name` with the head `read`, answered already if `answered`.
    fn new(store: &Store, app: &str, name: &str, read: Option<(Head, UpdateVersion)>, answered: bool) -> Arc<Self> {
        let (head, base) = read.map_or_else(Default::default, |(head, version)| (head, Some(version)));
        let state = Mutex::new(State { head, base, answered, ..Default::default() });
        let (app, name, path, settled) = (app.into(), name.into(), path(app, name), watch::Sender::new(None));
        Arc::new(Self { store: store.clone(), app, name, path, state, claiming: Default::default(), settled })
    }

    /// A snapshot of `name`: as it is now, without waiting for anyone's claim.
    pub async fn snap(store: &Store, app: &str, name: &str) -> Result<Arc<Self>> {
        Ok(Self::new(store, app, name, read(store, &path(app, name)).await?, true))
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
            let turn = Self::new(store, app, name, read(store, &path(app, name)).await?, false);
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
            let put = put(&turn.store, &turn.path, &head, base).await;
            let mut s = turn.state();
            match put {
                Ok(Some(version)) => {
                    (s.head.claim, s.base, s.claim) = (head.claim, Some(version), Some(id));
                    true
                }
                Ok(None) | Err(_) => {
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
        let v = {
            let s = self.state();
            match s.writes.get(key) {
                Some(w) => return Ok(w.clone()),
                None => s.head.values.get(key).cloned(),
            }
        };
        match v {
            None => Ok(None),
            Some(Value::Data(b64)) => Ok(Some(B64.decode(b64).map_err(other)?.into())),
            Some(Value::Object { key, version }) => {
                let got = self.store.get(&value_path(&self.app, &key), version, VALUE_MAX as u64).await;
                let got = got.map_err(|e| other(format!("{e:#}")))?.ok_or_else(|| other("the value is missing"))?;
                Ok(Some(got.0))
            }
        }
    }

    pub fn exists(&self, key: &str) -> bool {
        let s = self.state();
        s.writes.get(key).map_or_else(|| s.head.values.contains_key(key), Option::is_some)
    }

    pub fn list(&self, cursor: Option<String>) -> KeyResponse {
        let s = self.state();
        let mut keys: BTreeSet<&str> = s.head.values.keys().map(String::as_str).collect();
        for (k, w) in &s.writes {
            match w {
                Some(_) => keys.insert(k),
                None => keys.remove(k.as_str()),
            };
        }
        // One page: the first `PAGE` keys after `cursor`, which is the last key of the page before.
        let keys: Vec<String> = keys
            .into_iter()
            .filter(|k| cursor.as_deref().is_none_or(|c| *k > c))
            .take(PAGE)
            .map(str::to_owned)
            .collect();
        KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys }
    }

    /// Buffers writes, `None` deleting, all or none of them.
    pub fn write(&self, items: Vec<(String, Option<Bytes>)>) -> Result<(), Error> {
        self.state().write(items)
    }

    /// Adds `delta` to the counter at `key`, 0 if there is none: 8 bytes, little-endian, as Spin keeps one.
    pub fn increment(&self, key: &str, delta: i64) -> Result<i64, Error> {
        let mut s = self.state();
        let now = match s.writes.get(key) {
            Some(v) => v.clone(),
            None => match s.head.values.get(key) {
                Some(Value::Data(b64)) => Some(B64.decode(b64).map_err(other)?.into()),
                Some(Value::Object { .. }) => return Err(other("not a counter")),
                None => None,
            },
        };
        let now =
            now.map_or(Ok(0), |v| v.as_ref().try_into().map(i64::from_le_bytes)).map_err(|_| other("not a counter"))?;
        let next = now.checked_add(delta).ok_or_else(|| other("overflow"))?;
        s.write(vec![(key.into(), Some(Bytes::copy_from_slice(&next.to_le_bytes())))])?;
        Ok(next)
    }

    /// Writes `value` at `key` if it still holds `seen`, and returns whether it did. A key the turn has not written
    /// holds what it did when the turn opened, which is what `seen` was read from.
    pub fn swap(&self, key: &str, seen: &Option<Bytes>, value: Bytes) -> Result<bool, Error> {
        let mut s = self.state();
        if s.writes.get(key).is_some_and(|w| w != seen) {
            return Ok(false);
        }
        s.write(vec![(key.into(), Some(value))])?;
        Ok(true)
    }

    /// Commits the turn, asked at `host`: its large values go to objects of their own, its held requests to `sink` as a
    /// delivery event whose digest the head keeps in `pending`, and its head over the version it read, if that is still
    /// the head. The `ETag` it returns is the new head's, or the old one's when there was nothing to write. A commit
    /// that fails is discarded.
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
        let (mut head, base, writes, held, claimed, doomed) = {
            let s = &mut *self.state();
            s.answered = true;
            let (writes, held) = (std::mem::take(&mut s.writes), std::mem::take(&mut s.held));
            (s.head.clone(), s.base.clone(), writes, held, s.claim.is_some(), s.doomed)
        };
        if doomed {
            return Ok(Committed::Conflict);
        }
        if writes.is_empty() && held.is_empty() && !claimed {
            return Ok(Committed::Done(base.as_ref().and_then(etag)));
        }
        let (mut made, mut gone) = (vec![], vec![]);
        let put = async {
            for (key, w) in writes {
                let old = match w {
                    Some(v) if v.len() > INLINE_MAX => {
                        let k = store::random();
                        let path = value_path(&self.app, &k);
                        let version = self.store.put(&path, v, PutMode::Overwrite).await?.and_then(|v| v.version);
                        made.push(path);
                        head.values.insert(key, Value::Object { key: k, version })
                    }
                    Some(v) => head.values.insert(key, Value::Data(B64.encode(v))),
                    None => head.values.remove(&key),
                };
                if let Some(Value::Object { key: k, .. }) = old {
                    gone.push(value_path(&self.app, &k));
                }
            }
            let now = now();
            head.pending.retain(|_, p| now.saturating_sub(p.at) < PENDING_TTL.as_millis() as u64);
            head.claim = None;
            if !held.is_empty() {
                let commit = store::random();
                let event = Event {
                    app: self.app.clone(),
                    host: host.into(),
                    name: self.name.clone(),
                    base: base.as_ref().and_then(|v| v.e_tag.clone()),
                    commit: commit.clone(),
                    requests: held,
                };
                let bytes = Bytes::from(serde_json::to_vec(&event)?);
                head.pending.insert(commit, Pending { digest: store::hash(&bytes), at: now });
                ensure!(serde_json::to_vec(&head)?.len() <= HEAD_MAX, TooLarge); // or the event would be sent in vain
                sink(bytes).await?;
            }
            put(&self.store, &self.path, &head, base.clone()).await
        };
        let put = put.await;
        let cleanup = if let Ok(Some(_)) = put { gone } else { made };
        let store = self.store.clone();
        tokio::spawn(async move {
            for path in cleanup {
                _ = store.delete(&path).await;
            }
        });
        Ok(put?.map_or(Committed::Conflict, |version| Committed::Done(etag(&version))))
    }

    /// Discards the turn, and its claim if it took one.
    pub async fn discard(&self) {
        let _one = self.claiming.lock().await;
        let (mut head, base, claimed) = {
            let s = &mut *self.state();
            s.answered = true; // so the claim timer takes no claim after
            s.writes.clear();
            s.held.clear();
            (s.head.clone(), s.base.clone(), s.claim.take())
        };
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
    use http_body_util::{BodyExt, Empty};

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
    async fn too_large() {
        let store = Store::memory();
        let turn =
            Turn::open(&store, "a", "n", false, &Default::default(), Instant::now()).await.unwrap().ok().unwrap();
        let v = Bytes::from(vec![0; INLINE_MAX]);
        turn.write((0..800).map(|k| (k.to_string(), Some(v.clone()))).collect()).unwrap();
        let body = Empty::new().map_err(|n| match n {}).boxed_unsync();
        turn.hold(Held::of(http::Request::new(body)).await.ok().unwrap()).unwrap();
        let sink: Sink = Arc::new(|_| panic!("an event of a commit that fails is not sent"));
        assert!(turn.commit("h", &sink).await.err().unwrap().is::<TooLarge>());
        assert!(read(&store, &path("a", "n")).await.unwrap().is_none());
    }
}
