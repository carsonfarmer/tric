//! Names: each name's state is one object, its head, which a turn changes with a compare-and-swap when it commits.
use crate::engine::ANSWER;
use crate::kv::{Error, KeyResponse, other};
use crate::outbox::{Event, Held, Sink};
use crate::store::{self, Store};
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use http::HeaderMap;
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
const OBJECT_LEN: usize = 200; // the most a reference to a value object takes in a head
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
    Object(Obj),
}

/// A value too large to keep in the head: the object `apps/<app>/values/<key>` at `version`, in a versioned bucket.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Obj {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
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
    ensure!(json.len() <= HEAD_MAX, "{path} would be over {HEAD_MAX} bytes");
    store.put(path, json.into(), store::over(base)).await
}

/// The `ETag` of a head's version: a strong one, whatever the store says.
fn etag(version: &UpdateVersion) -> Option<String> {
    version.e_tag.as_ref().map(|e| format!("\"{}\"", e.trim_matches('"')))
}

fn key_ok(key: &str) -> Result<(), Error> {
    match key.len() {
        1..=KEY_MAX => Ok(()),
        _ => Err(other(format!("a key is 1 to {KEY_MAX} bytes"))),
    }
}

/// Loads a value, if there is one.
async fn load(store: &Store, app: &str, v: Option<&Value>) -> Result<Option<Bytes>, Error> {
    match v {
        None => Ok(None),
        Some(Value::Data(b64)) => Ok(Some(B64.decode(b64).map_err(other)?.into())),
        Some(Value::Object(o)) => {
            let got = store.get(&value_path(app, &o.key), o.version.clone(), VALUE_MAX as u64).await;
            let got = got.map_err(|e| other(format!("{e:#}")))?.ok_or_else(|| other("the value is missing"))?;
            Ok(Some(got.0))
        }
    }
}

/// One page of `keys`, which are in order: the first `PAGE` after `cursor`, which is the last key of the page before.
fn page<'a>(keys: impl Iterator<Item = &'a str>, cursor: Option<String>) -> KeyResponse {
    let keys: Vec<String> =
        keys.filter(|k| cursor.as_deref().is_none_or(|c| *k > c)).take(PAGE).map(str::to_owned).collect();
    KeyResponse { cursor: (keys.len() == PAGE).then(|| keys[PAGE - 1].clone()), keys }
}

/// A name as a request read it, once.
pub struct Snap {
    store: Store,
    app: String,
    head: Head,
    pub etag: Option<String>, // none when there is no head
}

impl Snap {
    pub async fn read(store: &Store, app: &str, name: &str) -> Result<Self> {
        let (head, etag) = match read(store, &path(app, name)).await? {
            Some((head, version)) => (head, etag(&version)),
            None => Default::default(),
        };
        Ok(Self { store: store.clone(), app: app.into(), head, etag })
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>, Error> {
        load(&self.store, &self.app, self.head.values.get(key)).await
    }

    pub fn exists(&self, key: &str) -> bool {
        self.head.values.contains_key(key)
    }

    pub fn list(&self, cursor: Option<String>) -> KeyResponse {
        page(self.head.values.keys().map(String::as_str), cursor)
    }
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

/// The entity tags of a list, each with whether it is weak, quotes and all.
fn tags(list: &str) -> impl Iterator<Item = (bool, &str)> {
    let mut rest = list;
    std::iter::from_fn(move || {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let weak = rest.starts_with("W/");
        rest = rest.strip_prefix("W/").unwrap_or(rest);
        let end = rest.strip_prefix('"')?.find('"')? + 2;
        let (tag, after) = rest.split_at(end);
        rest = after;
        Some((weak, tag))
    })
}

/// Why a turn did not open.
pub enum Refused {
    Busy,         // others held the name, or kept changing it, for `BUSY`
    Precondition, // its conditions failed
}

/// What a turn changes, under its lock.
struct State {
    head: Head,                  // as read, or as this turn's claim wrote it
    base: Option<UpdateVersion>, // the head's version, if there is a head
    writes: BTreeMap<String, Option<Bytes>>,
    size: usize, // of the head's JSON with the writes, projected
    answered: bool,
    claim: Option<String>, // this turn's, once taken
    doomed: bool,          // a claim failed, so the commit will
    held: Vec<Held>,
    held_size: usize,
}

/// A request that may write one name: its writes are buffered, and committed or discarded once it answers.
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

/// The JSON length of an entry of `values`: the key, a colon, the value and a comma.
fn entry_len(key: &str, value: usize) -> usize {
    serde_json::to_string(key).map_or(0, |k| k.len()) + value + 2
}

fn value_len(v: &Bytes) -> usize {
    match v.len() {
        n if n <= INLINE_MAX => 11 + n.div_ceil(3) * 4, // `{"data":""}` and the base64
        _ => OBJECT_LEN,
    }
}

impl State {
    fn write(&mut self, items: Vec<(String, Option<Bytes>)>) -> Result<(), Error> {
        for (k, v) in &items {
            key_ok(k)?;
            if v.as_ref().is_some_and(|v| v.len() > VALUE_MAX) {
                return Err(other(format!("a value is {VALUE_MAX} bytes or less")));
            }
        }
        if self.answered {
            return Err(other("the name is read-only after the response"));
        }
        let (writes, size) = (self.writes.clone(), self.size);
        for (k, v) in items {
            self.size = self.size - self.entry(&k) + v.as_ref().map_or(0, |v| entry_len(&k, value_len(v)));
            self.writes.insert(k, v);
            if self.size > HEAD_MAX {
                (self.writes, self.size) = (writes, size);
                return Err(other(format!("the name would be over {HEAD_MAX} bytes")));
            }
        }
        Ok(())
    }

    /// The length the entry for `key` takes now.
    fn entry(&self, key: &str) -> usize {
        match self.writes.get(key) {
            Some(Some(v)) => entry_len(key, value_len(v)),
            Some(None) => 0,
            None => {
                self.head.values.get(key).map_or(0, |v| entry_len(key, serde_json::to_string(v).map_or(0, |j| j.len())))
            }
        }
    }
}

impl Turn {
    /// Opens a turn on `name`, once no one else's claim is live, if `conditions` hold, and claimed if `claim`; or
    /// refuses it at `until`.
    pub async fn open(
        store: &Store,
        app: &str,
        name: &str,
        claim: bool,
        conditions: &Conditions,
        until: Instant,
    ) -> Result<Result<Arc<Self>, Refused>> {
        let path = path(app, name);
        loop {
            let (head, base) = match read(store, &path).await? {
                Some((head, version)) => (head, Some(version)),
                None => (Head::default(), None),
            };
            if !head.claim.as_ref().is_some_and(Claim::live) {
                if !conditions.hold(base.as_ref().and_then(etag).as_deref()) {
                    return Ok(Err(Refused::Precondition));
                }
                let size = serde_json::to_vec(&head)?.len();
                let (writes, held) = Default::default();
                let state =
                    State { head, base, writes, size, answered: false, claim: None, doomed: false, held, held_size: 0 };
                let (app, name, settled) = (app.into(), name.into(), watch::Sender::new(None));
                let (state, claiming) = (Mutex::new(state), Default::default());
                let turn =
                    Arc::new(Self { store: store.clone(), app, name, path: path.clone(), state, claiming, settled });
                if !claim || turn.claim().await {
                    return Ok(Ok(turn));
                }
            }
            if Instant::now() >= until {
                return Ok(Err(Refused::Busy));
            }
            sleep(Duration::from_millis(rand::random_range(25..=50))).await;
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Claims the name, so that no other turn commits before this one, and returns whether it holds the claim. One that
    /// fails dooms the turn: something else changed the name, so its commit will fail. Once the turn has answered there
    /// is nothing left to claim for. The claim is a task of its own, so a caller that goes away leaves no claim the
    /// turn does not know of.
    pub async fn claim(self: &Arc<Self>) -> bool {
        let turn = self.clone();
        tokio::spawn(async move { turn.take_claim().await }).await.unwrap_or(false)
    }

    async fn take_claim(&self) -> bool {
        let _one = self.claiming.lock().await;
        let (mut head, base) = {
            let s = self.state();
            if s.claim.is_some() || s.doomed || s.answered {
                return s.claim.is_some();
            }
            (s.head.clone(), s.base.clone())
        };
        let id = store::random();
        head.claim = Some(Claim { id: id.clone(), until: now() + CLAIM_TTL.as_millis() as u64 });
        let put = put(&self.store, &self.path, &head, base).await;
        let mut s = self.state();
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
        load(&self.store, &self.app, v.as_ref()).await
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
        page(keys.into_iter(), cursor)
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
                Some(Value::Object(_)) => return Err(other("not a counter")),
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
    /// the head. The `ETag` it returns is the new head's, or the old one's when there was nothing to write.
    pub async fn commit(&self, host: &str, sink: &Sink) -> Result<Committed> {
        let _one = self.claiming.lock().await; // so a claim in flight lands first
        let (mut head, base, writes, held, claimed, doomed) = {
            let s = &mut *self.state();
            s.answered = true;
            let (writes, held) = (std::mem::take(&mut s.writes), std::mem::take(&mut s.held));
            (s.head.clone(), s.base.clone(), writes, held, s.claim.is_some(), s.doomed)
        };
        if doomed {
            self.settled.send_replace(Some(false));
            return Ok(Committed::Conflict);
        }
        if writes.is_empty() && held.is_empty() && !claimed {
            self.settled.send_replace(Some(true));
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
                        head.values.insert(key, Value::Object(Obj { key: k, version }))
                    }
                    Some(v) => head.values.insert(key, Value::Data(B64.encode(v))),
                    None => head.values.remove(&key),
                };
                if let Some(Value::Object(o)) = old {
                    gone.push(value_path(&self.app, &o.key));
                }
            }
            let now = now();
            head.pending.retain(|_, p| now.saturating_sub(p.at) < PENDING_TTL.as_millis() as u64);
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
                let digest = store::hash(&bytes);
                sink(bytes).await?;
                head.pending.insert(commit, Pending { digest, at: now });
            }
            head.claim = None;
            put(&self.store, &self.path, &head, base.clone()).await
        };
        let put = put.await;
        let cleanup = match &put {
            Ok(Some(_)) => std::mem::take(&mut gone),
            _ => std::mem::take(&mut made),
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            for path in cleanup {
                _ = store.delete(&path).await;
            }
        });
        match put {
            Ok(Some(version)) => {
                self.settled.send_replace(Some(true));
                Ok(Committed::Done(etag(&version)))
            }
            Ok(None) => {
                self.settled.send_replace(Some(false));
                Ok(Committed::Conflict)
            }
            Err(e) => {
                drop(_one);
                self.discard().await;
                Err(e)
            }
        }
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

    #[test]
    fn head_json() {
        let mut head = Head::default();
        assert_eq!(serde_json::to_string(&head).unwrap(), "{}");
        head.values.insert("k".into(), Value::Data(B64.encode("v")));
        head.values.insert("big".into(), Value::Object(Obj { key: "0".repeat(32), version: Some("v1".into()) }));
        head.pending.insert("c".into(), Pending { digest: "d".into(), at: 1 });
        let json = serde_json::to_string(&head).unwrap();
        assert_eq!(
            json,
            r#"{"values":{"big":{"object":{"key":"00000000000000000000000000000000","version":"v1"}},"k":{"data":"dg=="}},"pending":{"c":{"digest":"d","at":1}}}"#
        );
        assert!(serde_json::from_str::<Head>(r#"{"values":{},"x":1}"#).is_err());
        // what the projection counts for an inline value is what it takes
        assert_eq!(value_len(&Bytes::from("v")), r#"{"data":"dg=="}"#.len());
    }
}
