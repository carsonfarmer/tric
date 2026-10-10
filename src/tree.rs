//! A name's data: one copy-on-write B+tree of keys and values, as LMDB and bbolt keep theirs. The root is in the name's
//! head; every other node, and every value too large to sit in a node, is an immutable object under
//! `apps/<app>/values/<name>/`, which its parent names by a [`Link`]: the object's id, its S3 version, its SHA-256 and
//! its length. A read follows links and checks each object against its link, so a changed object is an error, not data.
//!
//! A turn edits the tree directly. A node it touches becomes a copy in memory, and the rest stay links. When the
//! copies, and the large values held for upload, pass their budget, they are uploaded and become links again, as LMDB
//! spills dirty pages. The commit uploads what is left, bottom-up, and its caller swaps in the head that names the new
//! root. A node left under a quarter full merges with a neighbour, and a root with one child gives way to it, as in
//! bbolt.
//!
//! Every object the turn uploads is `made`, and every object its edits replace is `dead`. A commit that lands deletes
//! the dead, and one that does not deletes what was made, so nothing leaks but what a crash leaves behind.
use crate::store::{self, Store};
use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use futures_util::future::{BoxFuture, try_join_all};
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{PutMode, path::Path};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::{OwnedMutexGuard, Semaphore};
use wasmtime::{Result, bail, ensure, error::Context};

/// The most a node is as JSON.
pub const NODE_MAX: usize = 64 << 10;
/// The most a value is, to sit in its node. A larger one is an object of its own.
pub const INLINE_MAX: usize = 4 << 10;
/// The most a turn holds in memory that is not yet uploaded: nodes it changed, and values it has not uploaded.
pub const BUDGET: usize = 8 << 20;
/// The most keys a name holds.
pub const ENTRIES_MAX: u64 = 4 << 20;
/// The most bytes of values a name holds.
pub const DATA_MAX: u64 = 16 << 30;
/// The most a value that is an object of its own is.
pub const BLOB_MAX: u64 = 1 << 20;
/// The most of the nodes read, as a process keeps them for the next request, in bytes of JSON.
const NODES_MAX: usize = 16 << 20;
/// The most of the values and blocks read, kept in the same way.
const BLOCKS_MAX: usize = 32 << 20;
/// How many uploads a turn has in flight, and how many downloads one read does at once.
pub const IN_FLIGHT: usize = 16;
/// The longest S3 version id a link may hold: S3 and RustFS give 32 and 36 characters.
const VERSION_MAX: usize = 64;
/// What a link is as JSON, at most, when its version id is not yet known.
const LINK_SIZE: usize = 144 + VERSION_MAX;
/// What a node is as JSON beyond its entries.
const OVERHEAD: usize = 16;

/// What bounds a name's tree. The tests use tiny ones, so that a few keys make a deep tree.
#[derive(Clone, Copy)]
pub struct Limits {
    /// The most a node is as JSON.
    pub node: usize,
    /// The most a value is, to sit in its node.
    pub inline: usize,
    /// The most a turn holds in memory that is not yet uploaded.
    pub budget: usize,
    /// The most keys.
    pub entries: u64,
    /// The most bytes of values.
    pub data: u64,
    /// The most a value that is an object of its own is.
    pub blob: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            node: NODE_MAX,
            inline: INLINE_MAX,
            budget: BUDGET,
            entries: ENTRIES_MAX,
            data: DATA_MAX,
            blob: BLOB_MAX,
        }
    }
}

/// Why a write was refused: the name would hold more than its limits allow. The guest is told, as it can act on it.
#[derive(Debug)]
pub struct Full(String);

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An object in the tree: where it is, which of its versions, and what it must hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    id: String, // 128 random bits, in hex
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v: Option<String>, // the S3 version
    h: String,  // the SHA-256, in hex
    n: u64,     // the length
}

/// Whether `s` is `n` lowercase hex digits.
fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An object's id as a number, if it is one: 128 bits, as it is 32 lowercase hex digits. A key listed in the bucket is
/// compared with the ids a tree names as numbers, in 16 bytes where the string takes 56 or more.
pub fn number(id: &str) -> Option<u128> {
    hex(id, 32).then(|| u128::from_str_radix(id, 16).ok()).flatten()
}

/// Where the object `id` of a name's tree is: every one is directly under the name's own prefix.
pub fn object(app: &str, name: &str, id: &str) -> Path {
    store::app(app, &["values", name, id])
}

impl Link {
    /// Whether the link is well-formed, so that the path it leads to is under the name's own and nowhere else.
    fn check(&self) -> Result<()> {
        let plain =
            |v: &str| v.len() <= VERSION_MAX && v.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
        ensure!(hex(&self.id, 32) && hex(&self.h, 64) && self.v.as_deref().is_none_or(plain), "a link is malformed");
        Ok(())
    }

    /// The id as a number: see [`number`].
    fn number(&self) -> Result<u128> {
        self.check()?;
        number(&self.id).context("a link is malformed")
    }

    /// What the link is as JSON, at most.
    fn wire(&self) -> usize {
        48 + self.id.len() + self.v.as_ref().map_or(0, String::len) + self.h.len()
    }
}

/// What a key holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Item {
    /// A value of `INLINE_MAX` bytes or less, in base64.
    Inline(String),
    /// A larger value, which is an object.
    Object(Link),
    /// A larger value that is not yet uploaded. It is only ever in memory, and counts against the budget.
    #[serde(skip)]
    Held(Bytes),
}

impl Item {
    fn of(bytes: Bytes, inline: usize) -> Self {
        match bytes.len() <= inline {
            true => Self::Inline(B64.encode(&bytes)),
            false => Self::Held(bytes),
        }
    }

    /// The length of the value.
    pub fn len(&self) -> u64 {
        match self {
            Self::Inline(s) => {
                (s.len() / 4 * 3).saturating_sub(s.bytes().rev().take_while(|b| *b == b'=').count()) as u64
            }
            Self::Object(link) => link.n,
            Self::Held(bytes) => bytes.len() as u64,
        }
    }

    /// What the item is as JSON, at most.
    fn wire(&self) -> usize {
        match self {
            Self::Inline(s) => s.len() + 14,
            Self::Object(link) => link.wire() + 12,
            Self::Held(_) => LINK_SIZE + 12, // what it will be, once it is uploaded
        }
    }

    /// The bytes it holds in memory, for the budget.
    fn held(&self) -> usize {
        match self {
            Self::Held(bytes) => bytes.len(),
            _ => 0,
        }
    }
}

/// A node as the object, and the head's root, hold it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Wire {
    Leaf(Vec<(String, Item)>),
    /// Each child under the least key it may hold, which the first child's is not held to.
    Branch(Vec<(String, Link)>),
}

impl Default for Wire {
    fn default() -> Self {
        Self::Leaf(vec![])
    }
}

impl Wire {
    fn is_empty(&self) -> bool {
        match self {
            Self::Leaf(es) => es.is_empty(),
            Self::Branch(ks) => ks.is_empty(),
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A tree as a head holds it: the root, and the totals the limits are held to.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    #[serde(default, skip_serializing_if = "Wire::is_empty")]
    root: Wire,
    #[serde(default, skip_serializing_if = "is_zero")]
    bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    entries: u64,
    /// The last inode number the file system allocated, which no key uses; none have been allocated if it is 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    seq: u64,
}

impl Shape {
    pub fn is_empty(&self) -> bool {
        self.root.is_empty() && self.bytes == 0 && self.entries == 0 && self.seq == 0
    }
}

/// A node in memory. `size` is its JSON, at most, and `held` the bytes of values in it not yet uploaded.
#[derive(Clone)]
struct Node {
    body: Body,
    size: usize,
    held: usize,
}

#[derive(Clone)]
enum Body {
    Leaf(Vec<(String, Item)>),
    Branch(Vec<Child>),
}

#[derive(Clone)]
struct Child {
    key: String,
    at: At,
}

#[derive(Clone)]
enum At {
    Clean(Link),
    Dirty(Box<Node>),
}

/// What `s` is as a JSON string.
fn json_len(s: &str) -> usize {
    let one = |c: char| match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < ' ' => 6,
        c => c.len_utf8(),
    };
    2 + s.chars().map(one).sum::<usize>()
}

fn entry(key: &str, item: &Item) -> usize {
    json_len(key) + item.wire() + 4
}

fn child(kid: &Child) -> usize {
    json_len(&kid.key) + 4 + if let At::Clean(link) = &kid.at { link.wire() } else { LINK_SIZE }
}

/// Where to cut `len` entries of the sizes `sizes`, `total` as a node, so that each side has about half.
fn cut(sizes: impl Iterator<Item = usize>, total: usize, len: usize) -> usize {
    let mut seen = OVERHEAD;
    for (i, size) in sizes.enumerate() {
        seen += size;
        if seen >= total / 2 {
            return (i + 1).clamp(1, len - 1);
        }
    }
    len - 1
}

/// The child of `kids` that may hold `key`. The first child's key is not looked at, as keys below the second's are the
/// first's, and it can be more than keys that were added to it since.
fn slot(kids: &[Child], key: &str) -> usize {
    kids[1..].partition_point(|c| c.key.as_str() <= key)
}

impl Node {
    fn leaf() -> Self {
        Self { body: Body::Leaf(vec![]), size: OVERHEAD, held: 0 }
    }

    fn measured(body: Body) -> Self {
        let mut node = Self { body, size: 0, held: 0 };
        node.measure();
        node
    }

    fn of(wire: Wire) -> Result<Self> {
        Ok(Self::measured(match wire {
            Wire::Leaf(es) => Body::Leaf(es),
            Wire::Branch(ks) => {
                ensure!(!ks.is_empty(), "a branch has no children");
                Body::Branch(ks.into_iter().map(|(key, link)| Child { key, at: At::Clean(link) }).collect())
            }
        }))
    }

    /// The node as the head holds it, once all its children are links.
    fn wire(&self) -> Result<Wire> {
        Ok(match &self.body {
            Body::Leaf(es) => Wire::Leaf(es.clone()),
            Body::Branch(kids) => Wire::Branch(
                kids.iter()
                    .map(|k| match &k.at {
                        At::Clean(link) => Ok((k.key.clone(), link.clone())),
                        At::Dirty(_) => bail!("a node still has a child in memory"),
                    })
                    .collect::<Result<_>>()?,
            ),
        })
    }

    /// The node as the object holds it, once all its children are links.
    fn json(&self) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        #[serde(rename_all = "lowercase")]
        enum Ref<'a> {
            Leaf(&'a [(String, Item)]),
            Branch(Vec<(&'a str, &'a Link)>),
        }
        let wire = match &self.body {
            Body::Leaf(es) => Ref::Leaf(es),
            Body::Branch(kids) => Ref::Branch(
                kids.iter()
                    .map(|k| match &k.at {
                        At::Clean(link) => Ok((k.key.as_str(), link)),
                        At::Dirty(_) => bail!("a node still has a child in memory"),
                    })
                    .collect::<Result<_>>()?,
            ),
        };
        Ok(serde_json::to_vec(&wire)?)
    }

    fn measure(&mut self) {
        (self.size, self.held) = match &self.body {
            Body::Leaf(es) => {
                (OVERHEAD + es.iter().map(|(k, i)| entry(k, i)).sum::<usize>(), es.iter().map(|(_, i)| i.held()).sum())
            }
            Body::Branch(kids) => (OVERHEAD + kids.iter().map(child).sum::<usize>(), 0),
        };
    }

    fn is_empty(&self) -> bool {
        match &self.body {
            Body::Leaf(es) => es.is_empty(),
            Body::Branch(kids) => kids.is_empty(),
        }
    }

    fn first_key(&self) -> &str {
        match &self.body {
            Body::Leaf(es) => es.first().map_or("", |e| e.0.as_str()),
            Body::Branch(kids) => kids.first().map_or("", |k| k.key.as_str()),
        }
    }

    /// What the node and all it has in memory hold in memory.
    fn dirty(&self) -> usize {
        let below = match &self.body {
            Body::Leaf(_) => 0,
            Body::Branch(kids) => {
                kids.iter().filter_map(|k| if let At::Dirty(n) = &k.at { Some(n.dirty()) } else { None }).sum()
            }
        };
        self.size + self.held + below
    }

    /// Cuts off the second half of the node, which is over its size.
    fn split_off(&mut self) -> Result<Self> {
        let body = match &mut self.body {
            Body::Leaf(es) if es.len() > 1 => {
                Body::Leaf(es.split_off(cut(es.iter().map(|(k, i)| entry(k, i)), self.size, es.len())))
            }
            Body::Branch(kids) if kids.len() > 1 => {
                Body::Branch(kids.split_off(cut(kids.iter().map(child), self.size, kids.len())))
            }
            _ => bail!("a node of one entry is over its size"),
        };
        self.measure();
        Ok(Self::measured(body))
    }

    /// Adds the entries of `right`, the node next to this one on its right.
    fn append(&mut self, right: Self) -> Result<()> {
        match (&mut self.body, right.body) {
            (Body::Leaf(a), Body::Leaf(b)) => a.extend(b),
            (Body::Branch(a), Body::Branch(b)) => a.extend(b),
            _ => bail!("the tree is not as deep on one side"),
        }
        self.measure();
        Ok(())
    }
}

/// The objects read, for the next request in the same process: nodes, and the values and blocks too large to sit in
/// one. They are immutable, and named by the object's full path and version, which hold the app, the name and an id
/// nobody else gets, so a hit is what a read would fetch. A hit must also be what the link says, by its hash.
pub struct Cache {
    nodes: Mutex<Lru<Arc<Node>>>,
    blocks: Mutex<Lru<Bytes>>,
    /// The blocks that are being fetched: see [`Flight`].
    flights: Mutex<HashMap<CacheKey, Gate>>,
}

/// The gate of a block that is being fetched, and how many reads have come to it: the one that holds it, and those
/// that wait. The count is kept with the map locked, so it is exact, and the gate goes when it is back to none.
#[derive(Default)]
struct Gate {
    lock: Arc<tokio::sync::Mutex<()>>,
    comers: usize,
}

type CacheKey = (Path, Option<String>);

/// What a cache keeps of an object: the value, the hash it was checked against, its size, and when it was last used.
struct Used<V> {
    value: V,
    hash: String,
    size: usize,
    used: u64,
}

struct Lru<V> {
    entries: HashMap<CacheKey, Used<V>>,
    bytes: usize,
    tick: u64,
    max: usize,
}

impl<V: Clone> Lru<V> {
    fn new(max: usize) -> Mutex<Self> {
        Mutex::new(Self { entries: HashMap::new(), bytes: 0, tick: 0, max })
    }

    /// The value at `key`, if it was checked against `hash`.
    fn get(&mut self, key: &CacheKey, hash: &str) -> Option<V> {
        self.tick += 1;
        let hit = self.entries.get_mut(key).filter(|hit| hit.hash == hash)?;
        hit.used = self.tick;
        Some(hit.value.clone())
    }

    fn put(&mut self, key: CacheKey, value: V, hash: String, size: usize) {
        self.tick += 1;
        self.bytes += size;
        if let Some(old) = self.entries.insert(key, Used { value, hash, size, used: self.tick }) {
            self.bytes -= old.size;
        }
        if self.bytes > self.max {
            let mut by_use: Vec<_> = self.entries.iter().map(|(k, e)| (e.used, k.clone())).collect();
            by_use.sort();
            for (_, key) in by_use {
                if self.bytes <= self.max / 8 * 7 {
                    break;
                }
                if let Some(old) = self.entries.remove(&key) {
                    self.bytes -= old.size;
                }
            }
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self { nodes: Lru::new(NODES_MAX), blocks: Lru::new(BLOCKS_MAX), flights: Mutex::default() }
    }
}

impl Cache {
    /// The cache a process shares among its stores of S3.
    pub fn shared() -> Arc<Self> {
        static SHARED: LazyLock<Arc<Cache>> = LazyLock::new(Default::default);
        SHARED.clone()
    }

    /// Waits for its turn to fetch the block at `key`, and has it.
    async fn flight(&self, key: &CacheKey) -> Flight<'_> {
        let lock = {
            let mut flights = self.flights.lock().unwrap();
            let gate = flights.entry(key.clone()).or_default();
            gate.comers += 1;
            gate.lock.clone()
        };
        // From here on the count is the flight's to give back, wherever it is dropped: while it waits too.
        let mut flight = Flight { cache: self, key: key.clone(), held: None };
        flight.held = Some(lock.lock_owned().await);
        flight
    }
}

/// The right to fetch one block. Reads that come to a block that another read is fetching, which a stream that reads
/// ahead and the read that catches up with it both do, wait for it and then find it in the cache, instead of fetching
/// it again. A block is checked against its link by whoever fetches it, and by whoever finds it in the cache, so
/// waiting does not take anything on trust. When the one that holds it is cancelled, the next in turn fetches. A read
/// that is cancelled while it waits is counted out as well, so that the gate does not outlive the reads that come to
/// it: what is counted is the flights, and not who holds a reference to the gate, which depends on the order in which
/// a cancelled future drops what it holds.
struct Flight<'a> {
    cache: &'a Cache,
    key: CacheKey,
    held: Option<OwnedMutexGuard<()>>,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        drop(self.held.take());
        let mut flights = self.cache.flights.lock().unwrap();
        if let Some(gate) = flights.get_mut(&self.key) {
            gate.comers -= 1;
            if gate.comers == 0 {
                flights.remove(&self.key);
            }
        }
    }
}

/// What a turn did with the bucket, for the debug log, which is written when the last of its trees is gone.
#[derive(Default)]
struct Stats {
    gets: AtomicU64,
    hits: AtomicU64,
    read: AtomicU64,
    puts: AtomicU64,
    written: AtomicU64,
    depth: AtomicUsize,
    splits: AtomicU64,
    merges: AtomicU64,
    collapses: AtomicU64,
}

/// Where a name's objects are, and how to read them.
struct Reader {
    store: Store,
    app: String,
    name: String,
    limits: Limits,
    stats: Stats,
}

impl Reader {
    fn path(&self, id: &str) -> Path {
        object(&self.app, &self.name, id)
    }

    fn key(&self, link: &Link) -> CacheKey {
        (self.path(&link.id), link.v.clone())
    }

    /// The object `link` names, which must be `max` bytes or less, and be what the link says.
    async fn fetch(&self, link: &Link, max: u64) -> Result<Bytes> {
        link.check()?;
        ensure!(link.n <= max, "an object is over {max} bytes");
        let path = self.path(&link.id);
        let got = self.store.get(&path, link.v.clone(), max).await?;
        let (bytes, _) = got.with_context(|| format!("{path} is missing"))?;
        self.stats.gets.fetch_add(1, Relaxed);
        self.stats.read.fetch_add(bytes.len() as u64, Relaxed);
        ensure!(bytes.len() as u64 == link.n && store::hash(&bytes) == link.h, "{path} is not what its link says");
        Ok(bytes)
    }

    async fn node(&self, link: &Link) -> Result<Arc<Node>> {
        self.load(link, true).await
    }

    /// The node `link` names, from the cache if it is there, else read and checked against the link, and then kept if
    /// `keep`. A read of a whole name passes `false`, so as not to push out what requests are using.
    async fn load(&self, link: &Link, keep: bool) -> Result<Arc<Node>> {
        link.check()?;
        let key = self.key(link);
        if let Some(node) = self.store.cache.nodes.lock().unwrap().get(&key, &link.h) {
            self.stats.hits.fetch_add(1, Relaxed);
            return Ok(node);
        }
        let bytes = self.fetch(link, self.limits.node as u64).await?;
        let wire = serde_json::from_slice(&bytes).with_context(|| format!("{}", key.0))?;
        let node = Arc::new(Node::of(wire)?);
        if keep {
            self.store.cache.nodes.lock().unwrap().put(key, node.clone(), link.h.clone(), node.size);
        }
        Ok(node)
    }

    /// Keeps a value this turn uploaded, for the next request to read.
    fn keep(&self, link: &Link, bytes: &Bytes) {
        let blocks = &mut *self.store.cache.blocks.lock().unwrap();
        blocks.put(self.key(link), bytes.clone(), link.h.clone(), bytes.len());
    }

    /// The value `link` names, if the cache has it.
    fn kept(&self, key: &CacheKey, link: &Link) -> Option<Bytes> {
        let bytes = self.store.cache.blocks.lock().unwrap().get(key, &link.h)?;
        self.stats.hits.fetch_add(1, Relaxed);
        Some(bytes)
    }

    /// The value `item` holds.
    async fn value(&self, item: &Item) -> Result<Bytes> {
        match item {
            Item::Inline(s) => Ok(B64.decode(s)?.into()),
            Item::Object(link) => self.block(link).await,
            Item::Held(bytes) => Ok(bytes.clone()),
        }
    }

    /// The value `link` names, which is an object of its own.
    async fn block(&self, link: &Link) -> Result<Bytes> {
        link.check()?;
        let key = self.key(link);
        if let Some(bytes) = self.kept(&key, link) {
            return Ok(bytes);
        }
        let _flight = self.store.cache.flight(&key).await;
        if let Some(bytes) = self.kept(&key, link) {
            return Ok(bytes);
        }
        let bytes = self.fetch(link, self.limits.blob).await?;
        self.keep(link, &bytes);
        Ok(bytes)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let s = &self.stats;
        let (gets, hits, puts) = (s.gets.load(Relaxed), s.hits.load(Relaxed), s.puts.load(Relaxed));
        if gets + hits + puts > 0 {
            let (read, written, depth) = (s.read.load(Relaxed), s.written.load(Relaxed), s.depth.load(Relaxed));
            let (splits, merges) = (s.splits.load(Relaxed), s.merges.load(Relaxed));
            let (collapses, app, name) = (s.collapses.load(Relaxed), &self.app, &self.name);
            tracing::debug!(app, name, gets, hits, puts, read, written, depth, splits, merges, collapses, "tree");
        }
    }
}

/// The value at `key`, below `node`.
fn find<'a>(r: &'a Reader, node: &'a Node, key: &'a str, depth: usize) -> BoxFuture<'a, Result<Option<Item>>> {
    Box::pin(async move {
        r.stats.depth.fetch_max(depth, Relaxed);
        match &node.body {
            Body::Leaf(es) => Ok(es.binary_search_by(|(k, _)| k.as_str().cmp(key)).ok().map(|i| es[i].1.clone())),
            Body::Branch(kids) => match &kids[slot(kids, key)].at {
                At::Dirty(below) => find(r, below, key, depth + 1).await,
                At::Clean(link) => find(r, &*r.node(link).await?, key, depth + 1).await,
            },
        }
    })
}

/// Adds to `out` what `pick` makes of the entries below `node` whose keys start with `prefix`, and follow `after` if
/// there is one, up to `limit`.
fn walk<'a, T: Send>(
    r: &'a Reader,
    node: &'a Node,
    (prefix, after, limit): (&'a str, Option<&'a str>, usize),
    out: &'a mut Vec<T>,
    pick: fn(&str, &Item) -> T,
    depth: usize,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        r.stats.depth.fetch_max(depth, Relaxed);
        match &node.body {
            Body::Leaf(es) => {
                let before = |k: &str| k < prefix || after.is_some_and(|a| k <= a);
                for (k, item) in &es[es.partition_point(|(k, _)| before(k))..] {
                    if out.len() >= limit || !k.starts_with(prefix) {
                        break;
                    }
                    out.push(pick(k, item));
                }
            }
            Body::Branch(kids) => {
                let first = slot(kids, after.map_or(prefix, |a| a.max(prefix)));
                for (i, kid) in kids.iter().enumerate().skip(first) {
                    // The least key of a child, but the first, is a bound: past the prefix, so is all after.
                    let past = i > first && kid.key.as_str() > prefix && !kid.key.starts_with(prefix);
                    if out.len() >= limit || past {
                        break;
                    }
                    match &kid.at {
                        At::Dirty(below) => walk(r, below, (prefix, after, limit), &mut *out, pick, depth + 1).await?,
                        At::Clean(link) => {
                            let node = r.node(link).await?;
                            walk(r, &node, (prefix, after, limit), &mut *out, pick, depth + 1).await?
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

/// Adds to `ids` the objects `node` names, and to `next` the links of its children that are nodes.
fn refs(node: &Node, ids: &mut HashSet<u128>, next: &mut Vec<Link>) -> Result<()> {
    match &node.body {
        Body::Leaf(es) => {
            for (_, item) in es {
                if let Item::Object(link) = item {
                    ids.insert(link.number()?);
                }
            }
        }
        Body::Branch(kids) => {
            for kid in kids {
                let At::Clean(link) = &kid.at else { bail!("a node has a child in memory") };
                if ids.insert(link.number()?) {
                    next.push(link.clone());
                }
            }
        }
    }
    Ok(())
}

/// What editing a tree changes besides its nodes.
#[derive(Clone, Default)]
struct Work {
    bytes: u64,
    entries: u64,
    /// The last inode number allocated.
    seq: u64,
    /// The objects the edits have replaced or dropped, to delete once the commit lands.
    dead: Vec<Path>,
}

impl Work {
    /// The node `at` leads to, in memory and ready to change: a link is loaded, and the object it names is dead.
    async fn dirty<'a>(&mut self, r: &Reader, at: &'a mut At) -> Result<&'a mut Node> {
        if let At::Clean(link) = at {
            let loaded = r.node(link).await?;
            self.dead.push(r.path(&link.id));
            *at = At::Dirty(Box::new((*loaded).clone()));
        }
        match at {
            At::Dirty(node) => Ok(node),
            At::Clean(_) => bail!("a node was not loaded"),
        }
    }

    /// Sets `key` to `item` below `node`, or removes it if `item` is none. A node that grows past its size is split,
    /// and its second half, with the least key of it, is for the parent to take.
    fn edit<'a>(
        &'a mut self,
        r: &'a Reader,
        node: &'a mut Node,
        key: &'a str,
        item: Option<Item>,
        depth: usize,
    ) -> BoxFuture<'a, Result<Option<(String, Node)>>> {
        Box::pin(async move {
            r.stats.depth.fetch_max(depth, Relaxed);
            match &mut node.body {
                Body::Leaf(es) => match (es.binary_search_by(|(k, _)| k.as_str().cmp(key)), item) {
                    (Ok(i), Some(new)) => {
                        node.size = node.size + new.wire() - es[i].1.wire();
                        node.held = node.held + new.held() - es[i].1.held();
                        self.bytes = self.bytes + new.len() - es[i].1.len();
                        let old = std::mem::replace(&mut es[i].1, new);
                        self.gone(r, old);
                    }
                    (Err(i), Some(new)) => {
                        node.size += entry(key, &new);
                        node.held += new.held();
                        self.bytes += new.len();
                        self.entries += 1;
                        es.insert(i, (key.to_owned(), new));
                    }
                    (Ok(i), None) => {
                        let (k, old) = es.remove(i);
                        node.size -= entry(&k, &old);
                        node.held -= old.held();
                        self.bytes -= old.len();
                        self.entries -= 1;
                        self.gone(r, old);
                    }
                    (Err(_), None) => {}
                },
                Body::Branch(kids) => {
                    let i = slot(kids, key);
                    let link = if let At::Clean(link) = &kids[i].at { Some(link.wire()) } else { None };
                    let below = self.dirty(r, &mut kids[i].at).await?;
                    node.size = node.size + link.map_or(0, |_| LINK_SIZE) - link.unwrap_or(0);
                    if let Some((least, right)) = self.edit(r, below, key, item, depth + 1).await? {
                        node.size += json_len(&least) + LINK_SIZE + 4;
                        kids.insert(i + 1, Child { key: least, at: At::Dirty(Box::new(right)) });
                    }
                }
            }
            if node.size <= r.limits.node {
                return Ok(None);
            }
            r.stats.splits.fetch_add(1, Relaxed);
            let right = node.split_off()?;
            Ok(Some((right.first_key().to_owned(), right)))
        })
    }

    /// Notes that `item` is gone from the tree.
    fn gone(&mut self, r: &Reader, item: Item) {
        if let Item::Object(link) = item {
            self.dead.push(r.path(&link.id));
        }
    }

    /// Merges what is under a quarter full below `node`, as bbolt does: a child with a neighbour, and with the sides
    /// shared out again if they do not fit in one. A child that is empty is dropped.
    fn fix<'a>(&'a mut self, r: &'a Reader, node: &'a mut Node) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Body::Branch(kids) = &mut node.body else { return Ok(()) };
            for kid in kids.iter_mut() {
                if let At::Dirty(below) = &mut kid.at {
                    self.fix(r, below).await?;
                }
            }
            kids.retain(|k| !matches!(&k.at, At::Dirty(n) if n.is_empty()));
            let (thin, mut i) = (r.limits.node / 4, 0);
            while i < kids.len() {
                if kids.len() < 2 || !matches!(&kids[i].at, At::Dirty(n) if n.size < thin) {
                    i += 1;
                    continue;
                }
                let (a, b) = if i > 0 { (i - 1, i) } else { (0, 1) };
                self.dirty(r, &mut kids[a].at).await?;
                self.dirty(r, &mut kids[b].at).await?;
                let least = kids[b].key.clone();
                let At::Dirty(mut right) = kids.remove(b).at else { bail!("a node was not loaded") };
                if let Body::Branch(firsts) = &mut right.body {
                    firsts[0].key = least; // the first child's key is a bound now, as it was the parent's
                }
                let At::Dirty(left) = &mut kids[a].at else { bail!("a node was not loaded") };
                left.append(*right)?;
                r.stats.merges.fetch_add(1, Relaxed);
                i = a;
                if left.size > r.limits.node {
                    let right = left.split_off()?;
                    kids.insert(a + 1, Child { key: right.first_key().to_owned(), at: At::Dirty(Box::new(right)) });
                    i = a + 2;
                }
            }
            node.measure();
            Ok(())
        })
    }
}

/// An upload of what the turn holds, and of the nodes it changed.
struct Flush<'a> {
    reader: &'a Reader,
    made: &'a Mutex<Vec<Path>>,
    permits: Semaphore,
    nodes: bool, // the nodes too, and not only the values
}

impl Flush<'_> {
    /// Uploads a value, and returns the link.
    async fn upload(&self, bytes: Bytes) -> Result<Link> {
        let id = store::random();
        let path = self.reader.path(&id);
        self.made.lock().unwrap().push(path.clone()); // before the upload, as a failed one may have landed
        let h = store::hash(&bytes);
        let _permit = self.permits.acquire().await?;
        let v = self.reader.store.put(&path, bytes.clone(), PutMode::Overwrite).await?.and_then(|v| v.version);
        self.reader.stats.puts.fetch_add(1, Relaxed);
        self.reader.stats.written.fetch_add(bytes.len() as u64, Relaxed);
        let link = Link { id, v, h, n: bytes.len() as u64 };
        link.check()?;
        Ok(link)
    }

    /// Uploads the values held in `node`, and, if `nodes`, the nodes below it that are in memory, so each is a link.
    /// Those left empty are dropped. The node itself is for the caller to upload.
    fn node<'a>(&'a self, node: &'a mut Node) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match &mut node.body {
                Body::Leaf(es) => {
                    let held = es.iter_mut().filter(|(_, i)| matches!(i, Item::Held(_)));
                    try_join_all(held.map(|(_, item)| async move {
                        let Item::Held(bytes) = &*item else { return Ok::<_, wasmtime::Error>(()) };
                        let link = self.upload(bytes.clone()).await?;
                        self.reader.keep(&link, bytes); // for the next request
                        *item = Item::Object(link);
                        Ok(())
                    }))
                    .await?;
                }
                Body::Branch(kids) => {
                    try_join_all(kids.iter_mut().map(|kid| self.child(kid))).await?;
                    if self.nodes {
                        kids.retain(|k| !matches!(&k.at, At::Dirty(n) if n.is_empty()));
                    }
                }
            }
            node.measure();
            Ok(())
        })
    }

    async fn child(&self, kid: &mut Child) -> Result<()> {
        let At::Dirty(node) = &mut kid.at else { return Ok(()) };
        self.node(node).await?;
        if !self.nodes || node.is_empty() {
            return Ok(());
        }
        let json = node.json()?;
        ensure!(json.len() <= self.reader.limits.node, "a node is over {} bytes", self.reader.limits.node);
        let link = self.upload(json.into()).await?;
        let (key, hash) = (self.reader.key(&link), link.h.clone());
        let At::Dirty(node) = std::mem::replace(&mut kid.at, At::Clean(link)) else { bail!("a node was not loaded") };
        let size = node.size;
        self.reader.store.cache.nodes.lock().unwrap().put(key, Arc::new(*node), hash, size); // for the next request
        Ok(())
    }
}

/// Reads the values of items of a name, apart from its tree. An item is a value in memory, or names an object that is
/// never changed, by its version, and is checked against its hash when it is read, so what it gives does not depend on
/// the tree, and a read of it need not hold the tree for the length of a round trip to the store. The tree is for
/// finding the items, and that is the part that holds it.
#[derive(Clone)]
pub struct Values(Arc<Reader>);

impl Values {
    /// The value `item` holds, checked as [`Tree::value`] checks it, and fetched once for the reads that come to it.
    pub async fn get(&self, item: &Item) -> Result<Bytes> {
        self.0.value(item).await
    }
}

/// A name's tree, as one turn edits it.
pub struct Tree {
    reader: Arc<Reader>,
    base: (Node, Work), // as opened: the root, and the totals
    root: Node,
    work: Work,
    /// Every object uploaded since the tree was opened or reset.
    made: Mutex<Vec<Path>>,
    poisoned: bool, // an edit failed, or went away, half way, so the tree cannot be committed
    edited: bool,
    sealed: bool, // the tree was finished, so what was made may be in a head by now
}

impl Tree {
    pub fn open(store: &Store, app: &str, name: &str, shape: &Shape, limits: Limits) -> Result<Self> {
        let reader =
            Reader { store: store.clone(), app: app.into(), name: name.into(), limits, stats: Stats::default() };
        let root = Node::of(shape.root.clone())?;
        let work = Work { bytes: shape.bytes, entries: shape.entries, seq: shape.seq, ..Default::default() };
        let base = (root.clone(), work.clone());
        Ok(Self {
            reader: reader.into(),
            base,
            root,
            work,
            made: Default::default(),
            poisoned: false,
            edited: false,
            sealed: false,
        })
    }

    /// A copy of the tree as it is, to read: it shares the objects, and nothing it does is committed.
    pub fn fork(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            base: self.base.clone(),
            root: self.root.clone(),
            work: Work { dead: vec![], ..self.work.clone() },
            made: Default::default(),
            poisoned: false,
            edited: false,
            sealed: true, // there is nothing to delete, and it is never committed
        }
    }

    /// The item at `key`.
    pub async fn get(&self, key: &str) -> Result<Option<Item>> {
        find(&self.reader, &self.root, key, 1).await
    }

    /// The value `item` holds.
    pub async fn value(&self, item: &Item) -> Result<Bytes> {
        self.reader.value(item).await
    }

    /// A way to read the values of items that does not hold the tree: see [`Values`].
    pub fn values(&self) -> Values {
        Values(self.reader.clone())
    }

    /// `bytes` as an item: inline if it is small.
    pub fn item(&self, bytes: Bytes) -> Item {
        Item::of(bytes, self.reader.limits.inline)
    }

    /// The first `limit` keys that start with `prefix` and follow `after`, if given.
    pub async fn scan(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let mut keys = vec![];
        walk(&self.reader, &self.root, (prefix, after, limit), &mut keys, |k, _| k.to_owned(), 1).await?;
        Ok(keys)
    }

    /// The first `limit` entries whose keys start with `prefix` and follow `after`, if given.
    pub async fn entries(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<(String, Item)>> {
        let mut entries = vec![];
        let pick = |k: &str, item: &Item| (k.to_owned(), item.clone());
        walk(&self.reader, &self.root, (prefix, after, limit), &mut entries, pick, 1).await?;
        Ok(entries)
    }

    /// The ids of the objects the tree is made of below its root: its nodes, and the values that are objects of their
    /// own. They are found by reading every node, `IN_FLIGHT` at once and one level after another, each checked against
    /// its link as any read is, and kept out of the cache; one that is missing or is not what its link says is an
    /// error, so a set is of a tree that was whole. `None` if the tree has more than `max` nodes, which is not read.
    pub async fn reachable(&self, max: usize) -> Result<Option<HashSet<u128>>> {
        let (mut ids, mut next, mut nodes) = (HashSet::new(), vec![], 0);
        refs(&self.root, &mut ids, &mut next)?;
        while !next.is_empty() {
            nodes += next.len();
            if nodes > max {
                return Ok(None);
            }
            let (r, links) = (&*self.reader, std::mem::take(&mut next));
            let reads = stream::iter(links).map(|link| async move { r.load(&link, false).await });
            let mut reads = reads.buffer_unordered(IN_FLIGHT);
            while let Some(node) = reads.try_next().await? {
                refs(&node, &mut ids, &mut next)?;
            }
        }
        Ok(Some(ids))
    }

    /// A new inode number, for the file system: the root is 1, and the others count up, never reused.
    pub fn alloc(&mut self) -> u64 {
        self.work.seq = self.work.seq.max(1) + 1;
        self.work.seq
    }

    /// Whether the tree was changed.
    pub fn edited(&self) -> bool {
        self.edited
    }

    /// Sets and removes keys, all or none: a batch over the limits is refused with [`Full`], and changes nothing. One
    /// that fails otherwise, or that is dropped half way, leaves a tree that cannot be committed.
    pub async fn write(&mut self, edits: Vec<(String, Option<Item>)>) -> Result<()> {
        ensure!(!self.poisoned, "an earlier change to the name failed");
        let limits = self.reader.limits;
        let (adds, size) = edits.iter().filter_map(|(_, i)| i.as_ref()).fold((0, 0), |(n, b), i| (n + 1, b + i.len()));
        if self.work.entries + adds > limits.entries || self.work.bytes + size > limits.data {
            // Possibly over: count again, as the batch replaces and removes too.
            let (mut entries, mut bytes) = (self.work.entries as i64, self.work.bytes as i64);
            let mut seen: HashMap<&str, Option<u64>> = HashMap::new();
            for (key, item) in &edits {
                let was = match seen.get(key.as_str()) {
                    Some(was) => *was,
                    None => self.get(key).await?.map(|i| i.len()),
                };
                let now = item.as_ref().map(Item::len);
                entries += i64::from(now.is_some()) - i64::from(was.is_some());
                bytes += now.unwrap_or(0) as i64 - was.unwrap_or(0) as i64;
                seen.insert(key, now);
            }
            ensure!(entries <= limits.entries as i64, Full(format!("a name holds {} keys or fewer", limits.entries)));
            ensure!(bytes <= limits.data as i64, Full(format!("a name holds {} bytes or fewer", limits.data)));
        }
        self.poisoned = true; // until it is applied, which a caller that goes away does not see
        let applied = self.apply(edits).await;
        self.poisoned = applied.is_err();
        applied
    }

    async fn apply(&mut self, edits: Vec<(String, Option<Item>)>) -> Result<()> {
        for (key, item) in edits {
            if item.is_none() && self.get(&key).await?.is_none() {
                continue; // nothing to remove, so nothing to copy
            }
            self.edited = true;
            if let Some((least, right)) = self.work.edit(&self.reader, &mut self.root, &key, item, 1).await? {
                let left = std::mem::replace(&mut self.root, Node::leaf());
                let first = Child { key: left.first_key().to_owned(), at: At::Dirty(Box::new(left)) };
                let second = Child { key: least, at: At::Dirty(Box::new(right)) };
                self.root = Node::measured(Body::Branch(vec![first, second]));
            }
        }
        let limit = self.reader.limits.budget;
        if self.root.dirty() > limit {
            self.flush(false).await?; // the values first, which are the most
            if self.root.dirty() > limit {
                self.flush(true).await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self, nodes: bool) -> Result<()> {
        let flush = Flush { reader: &self.reader, made: &self.made, permits: Semaphore::new(IN_FLIGHT), nodes };
        flush.node(&mut self.root).await?;
        if matches!(&self.root.body, Body::Branch(kids) if kids.is_empty()) {
            self.root = Node::leaf();
        }
        Ok(())
    }

    /// Readies the tree for a head: merges what is thin, uploads what is changed, and returns the shape of it. The head
    /// must land before `landed`, or `lost` follows.
    pub async fn finish(&mut self) -> Result<Shape> {
        ensure!(!self.poisoned, "an earlier change to the name failed");
        self.poisoned = true; // until it is done, as for a write
        let finished = self.seal().await;
        self.poisoned = finished.is_err();
        finished
    }

    async fn seal(&mut self) -> Result<Shape> {
        let r = &*self.reader;
        self.work.fix(r, &mut self.root).await?;
        while let Body::Branch(kids) = &mut self.root.body {
            match kids.len() {
                0 => self.root = Node::leaf(),
                1 => {
                    let Some(Child { mut at, .. }) = kids.pop() else { break };
                    let only = self.work.dirty(r, &mut at).await?;
                    self.root = std::mem::replace(only, Node::leaf());
                    r.stats.collapses.fetch_add(1, Relaxed);
                }
                _ => break,
            }
        }
        let flush = Flush { reader: r, made: &self.made, permits: Semaphore::new(IN_FLIGHT), nodes: true };
        flush.node(&mut self.root).await?;
        self.sealed = true;
        ensure!(self.root.size <= r.limits.node, "the root is over {} bytes", r.limits.node);
        let Work { bytes, entries, seq, .. } = self.work;
        Ok(Shape { root: self.root.wire()?, bytes, entries, seq })
    }

    /// The head landed: what the tree replaced is no more, and the tree is as it was committed. Returns the objects to
    /// delete.
    pub fn landed(&mut self) -> Vec<Path> {
        self.made.get_mut().unwrap().clear();
        std::mem::take(&mut self.work.dead)
    }

    /// The head did not land, or the turn is discarded: the tree is as it was opened. Returns the objects to delete.
    pub fn lost(&mut self) -> Vec<Path> {
        (self.root, self.work) = self.base.clone();
        (self.poisoned, self.edited, self.sealed) = (false, false, false);
        std::mem::take(self.made.get_mut().unwrap())
    }

    /// How often the tree split, merged and gave up a level of its root.
    #[cfg(test)]
    fn rebalanced(&self) -> (u64, u64, u64) {
        let s = &self.reader.stats;
        (s.splits.load(Relaxed), s.merges.load(Relaxed), s.collapses.load(Relaxed))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // What a turn that went away made is no one's, unless it was committing, when it may be the head's.
        let made = std::mem::take(self.made.get_mut().unwrap());
        if let (false, false, Ok(runtime)) = (self.sealed, made.is_empty(), tokio::runtime::Handle::try_current()) {
            let store = self.reader.store.clone();
            runtime.spawn(async move { store.delete_many(made).await });
        }
    }
}

/// A store for tests that counts what is asked of it, and fails the puts past a count.
#[cfg(test)]
pub(crate) mod counting {
    use crate::store::Store;
    use futures_util::StreamExt;
    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions,
        PutOptions, PutPayload, PutResult,
    };
    use std::fmt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    #[derive(Debug, Default)]
    pub struct Counts {
        pub gets: AtomicUsize,
        pub puts: AtomicUsize,
        pub deletes: AtomicUsize,
        pub allow: AtomicUsize,  // the puts that succeed, in all
        pub delay: AtomicU64,    // how long a get takes, in milliseconds
        pub flight: AtomicUsize, // the gets under way
        pub peak: AtomicUsize,   // the most gets that were ever under way together
    }

    /// A store that counts what is asked of it, and fails the puts past `allow`.
    #[derive(Debug)]
    struct Counting {
        inner: InMemory,
        counts: Arc<Counts>,
    }

    impl fmt::Display for Counting {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("counting")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Counting {
        async fn put_opts(&self, at: &Path, payload: PutPayload, opts: PutOptions) -> object_store::Result<PutResult> {
            if self.counts.puts.fetch_add(1, SeqCst) >= self.counts.allow.load(SeqCst) {
                return Err(object_store::Error::Generic { store: "counting", source: "a put that fails".into() });
            }
            self.inner.put_opts(at, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            at: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(at, opts).await
        }

        async fn get_opts(&self, at: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
            self.counts.gets.fetch_add(1, SeqCst);
            let flight = self.counts.flight.fetch_add(1, SeqCst) + 1;
            self.counts.peak.fetch_max(flight, SeqCst);
            let delay = self.counts.delay.load(SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            let got = self.inner.get_opts(at, opts).await;
            self.counts.flight.fetch_sub(1, SeqCst);
            got
        }

        fn delete_stream(
            &self,
            paths: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            let counts = self.counts.clone();
            let counted = paths.inspect(move |_| {
                counts.deletes.fetch_add(1, SeqCst);
            });
            self.inner.delete_stream(counted.boxed())
        }

        fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, opts).await
        }
    }

    pub fn counting() -> (Store, Arc<Counts>) {
        let counts = Arc::new(Counts { allow: AtomicUsize::new(usize::MAX), ..Default::default() });
        let inner = Counting { inner: InMemory::new(), counts: counts.clone() };
        (Store { inner: Arc::new(inner), versioned: true, cache: Arc::default() }, counts)
    }
}

#[cfg(test)]
mod tests {
    use super::counting::counting;
    use super::*;
    use futures_util::TryStreamExt;
    use object_store::{ObjectStore, ObjectStoreExt};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;

    /// Limits that make a few keys a deep tree: a branch holds a few nodes, and a leaf as many values that are objects.
    fn tiny() -> Limits {
        Limits { node: 1400, inline: 8, budget: 2048, entries: 1000, data: 100_000, blob: 100 }
    }

    fn versioned() -> Store {
        Store { versioned: true, ..Store::memory() }
    }

    /// The same sequence from a seed, as a test needs.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A value: mostly one that stays in its node, and else one that is an object.
        fn value(&mut self) -> Vec<u8> {
            let n = if self.below(10) < 6 { self.below(9) } else { 9 + self.below(92) };
            (0..n).map(|_| self.below(256) as u8).collect()
        }
    }

    fn at(store: &Store, shape: &Shape) -> Tree {
        Tree::open(store, "a", "n", shape, tiny()).unwrap()
    }

    async fn set(tree: &mut Tree, key: &str, value: &[u8]) {
        let item = tree.item(Bytes::copy_from_slice(value));
        tree.write(vec![(key.into(), Some(item))]).await.unwrap();
    }

    /// Commits `tree`, as a turn does: the new shape, and the objects it replaced deleted.
    async fn commit(store: &Store, tree: &mut Tree) -> Shape {
        let shape = tree.finish().await.unwrap();
        store.delete_many(tree.landed()).await;
        shape
    }

    /// What a tree holds, and that it is a well-formed one.
    #[derive(Default)]
    struct Audit {
        items: BTreeMap<String, Item>,
        ids: BTreeSet<String>,
        depth: usize,
    }

    /// Checks `node`, at `level`, which may hold only keys from `lo` up to, not including, `hi`, and what is below it.
    fn dig<'a>(
        r: &'a Reader,
        node: &'a Node,
        level: usize,
        (lo, hi): (Option<&'a str>, Option<&'a str>),
        out: &'a mut Audit,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let json = node.json().unwrap();
            assert!(json.len() <= node.size, "a node is {} bytes, and was thought {}", json.len(), node.size);
            if level > 0 {
                assert!(json.len() <= r.limits.node, "a node is {} bytes", json.len());
                assert!(!node.is_empty(), "an empty node");
            }
            match &node.body {
                Body::Leaf(es) => {
                    assert!(out.depth == 0 || out.depth == level + 1, "leaves are at different depths");
                    out.depth = level + 1;
                    assert!(es.windows(2).all(|w| w[0].0 < w[1].0), "keys are not in order");
                    for (k, item) in es {
                        assert!(
                            lo.is_none_or(|lo| k.as_str() >= lo) && hi.is_none_or(|hi| k.as_str() < hi),
                            "{k} strays"
                        );
                        assert!(!matches!(item, Item::Held(_)), "a value is still in memory");
                        if let Item::Object(link) = item {
                            assert!(out.ids.insert(link.id.clone()), "an object is named twice");
                        }
                        out.items.insert(k.clone(), item.clone());
                    }
                }
                Body::Branch(kids) => {
                    let seps: Vec<_> = kids[1..].iter().map(|k| k.key.as_str()).collect();
                    assert!(seps.windows(2).all(|w| w[0] < w[1]), "separators are not in order: {seps:?}");
                    assert!(
                        seps.iter().all(|k| lo.is_none_or(|lo| *k >= lo) && hi.is_none_or(|hi| *k < hi)),
                        "{seps:?}"
                    );
                    assert!(level > 0 || kids.len() > 1, "a root has one child");
                    for (i, kid) in kids.iter().enumerate() {
                        let At::Clean(link) = &kid.at else { panic!("a node has a child in memory") };
                        assert!(out.ids.insert(link.id.clone()), "an object is named twice");
                        let child = r.node(link).await.unwrap();
                        let lo = if i == 0 { lo } else { Some(kid.key.as_str()) };
                        let hi = kids.get(i + 1).map(|k| k.key.as_str()).or(hi);
                        dig(r, &child, level + 1, (lo, hi), &mut *out).await;
                    }
                }
            }
        })
    }

    async fn audit(store: &Store, shape: &Shape) -> Audit {
        let r = Reader {
            store: store.clone(),
            app: "a".into(),
            name: "n".into(),
            limits: tiny(),
            stats: Default::default(),
        };
        let root = Node::of(shape.root.clone()).unwrap();
        let mut out = Audit::default();
        dig(&r, &root, 0, (None, None), &mut out).await;
        assert_eq!(out.items.len() as u64, shape.entries);
        assert_eq!(out.items.values().map(Item::len).sum::<u64>(), shape.bytes);
        out
    }

    /// The value at `key`, which is there.
    async fn text(tree: &Tree, key: &str) -> Vec<u8> {
        tree.value(&tree.get(key).await.unwrap().unwrap()).await.unwrap().to_vec()
    }

    /// Lets what a dropped tree spawned run.
    async fn settled() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// The ids of the objects in the store.
    async fn stored(store: &Store) -> BTreeSet<String> {
        let all = store.inner.list(Some(&store::app("a", &["values", "n"])));
        all.map_ok(|m| m.location.filename().unwrap().to_owned()).try_collect().await.unwrap()
    }

    #[tokio::test]
    async fn matches_a_map() {
        let limits = Limits { entries: 5000, data: 1 << 20, ..tiny() };
        let (mut store, mut rng) = (versioned(), Rng(0x5EED));
        let (mut model, mut shape) = (BTreeMap::<String, Vec<u8>>::new(), Shape::default());
        let (mut splits, mut merges, mut collapses, mut deepest) = (0, 0, 0, 0);
        for round in 0..300 {
            if rng.below(2) == 0 {
                store.cache = Arc::default(); // a cold cache, as on another instance
            }
            let mut tree = Tree::open(&store, "a", "n", &shape, limits).unwrap();
            let mut staged = model.clone();
            if round % 9 == 3 {
                // Thin it out, so that nodes merge and the root gives way.
                let most = 1 + rng.below(34);
                let keep = rng.below(most);
                let gone: Vec<_> = staged.keys().filter(|_| rng.below(40) >= keep).cloned().collect();
                for chunk in gone.chunks(25) {
                    tree.write(chunk.iter().map(|k| (k.clone(), None)).collect()).await.unwrap();
                }
                staged.retain(|k, _| !gone.contains(k));
            }
            for _ in 0..=rng.below(120) {
                let key = format!("k/{:04}", rng.below(3000));
                match rng.below(10) {
                    0..=3 => {
                        let value = rng.value();
                        set(&mut tree, &key, &value).await;
                        staged.insert(key, value);
                    }
                    4..=5 => {
                        let mut edits = vec![];
                        for _ in 0..=rng.below(5) {
                            let key = format!("k/{:04}", rng.below(3000));
                            let value = (rng.below(3) > 0).then(|| rng.value());
                            match &value {
                                Some(v) => staged.insert(key.clone(), v.clone()),
                                None => staged.remove(&key),
                            };
                            edits.push((key, value.map(|v| tree.item(v.into()))));
                        }
                        tree.write(edits).await.unwrap();
                    }
                    6 => {
                        let end = format!("k/{:04}", key[2..].parse::<u64>().unwrap() + rng.below(40));
                        let range: Vec<_> = staged.range(key.clone()..end).map(|(k, _)| k.clone()).collect();
                        tree.write(range.iter().map(|k| (k.clone(), None)).collect()).await.unwrap();
                        range.iter().for_each(|k| drop(staged.remove(k)));
                    }
                    7 => {
                        tree.write(vec![(key.clone(), None)]).await.unwrap();
                        staged.remove(&key);
                    }
                    8 => {
                        let got = match tree.get(&key).await.unwrap() {
                            Some(item) => Some(tree.value(&item).await.unwrap().to_vec()),
                            None => None,
                        };
                        assert_eq!(got, staged.get(&key).cloned(), "{key}");
                    }
                    _ => {
                        let (prefix, limit) = (format!("k/{}", rng.below(3)), 1 + rng.below(30) as usize);
                        let after = (rng.below(2) == 0).then(|| format!("k/{:04}", rng.below(3000)));
                        let want: Vec<_> = staged
                            .keys()
                            .filter(|k| k.starts_with(&prefix) && after.as_ref().is_none_or(|a| *k > a))
                            .take(limit)
                            .cloned()
                            .collect();
                        assert_eq!(tree.scan(&prefix, after.as_deref(), limit).await.unwrap(), want);
                    }
                }
            }
            // It commits, or is ready to and loses the race, or is thrown away as it is.
            let outcome = rng.below(10);
            let finished = if tree.edited() && outcome < 9 { Some(tree.finish().await.unwrap()) } else { None };
            let (a, b, c) = tree.rebalanced();
            (splits, merges, collapses) = (splits + a, merges + b, collapses + c);
            if outcome < 7 {
                shape = finished.unwrap_or(shape);
                store.delete_many(tree.landed()).await;
                model = staged;
                // It serves what it committed.
                for (k, v) in model.iter().take(20) {
                    let item = tree.get(k).await.unwrap().unwrap();
                    assert_eq!(tree.value(&item).await.unwrap().as_ref(), v.as_slice());
                }
            } else {
                store.delete_many(tree.lost()).await;
            }
            drop(tree);

            let audit = audit(&store, &shape).await;
            deepest = deepest.max(audit.depth);
            assert_eq!(audit.items.keys().collect::<Vec<_>>(), model.keys().collect::<Vec<_>>(), "round {round}");
            let fresh = Tree::open(&store, "a", "n", &shape, limits).unwrap();
            for (k, v) in &model {
                let item = fresh.get(k).await.unwrap().unwrap();
                assert_eq!(fresh.value(&item).await.unwrap().as_ref(), v.as_slice(), "{k}");
            }
            assert_eq!(fresh.scan("k/", None, usize::MAX).await.unwrap(), model.keys().cloned().collect::<Vec<_>>());
            let live: HashSet<u128> = audit.ids.iter().map(|id| number(id).unwrap()).collect();
            assert_eq!(fresh.reachable(usize::MAX).await.unwrap(), Some(live), "round {round}");
            let stored = stored(&store).await;
            let (leaked, lost): (Vec<_>, Vec<_>) =
                (stored.difference(&audit.ids).collect(), audit.ids.difference(&stored).collect());
            assert!(
                leaked.is_empty() && lost.is_empty(),
                "round {round}: {leaked:?} are leaked, and {lost:?} are lost"
            );
        }
        assert!(deepest >= 4, "the tree was {deepest} deep");
        assert!(splits > 0 && merges > 0 && collapses > 0, "{splits} splits, {merges} merges, {collapses} collapses");
    }

    #[test]
    fn json_is_measured() {
        for s in ["", "abc", "a\"b\\c", "\n\r\t\u{8}\u{c}", "\u{0}\u{1f}\u{7f}", "é€𝄞", "k/001"] {
            assert_eq!(json_len(s), serde_json::to_string(s).unwrap().len(), "{s:?}");
        }
        let link = Link { id: "a".repeat(32), v: Some("v".repeat(VERSION_MAX)), h: "b".repeat(64), n: u64::MAX };
        assert_eq!(link.wire(), serde_json::to_string(&link).unwrap().len());
        assert_eq!(LINK_SIZE, link.wire());
        for n in 0..40 {
            let bytes = Bytes::from(vec![7; n]);
            for item in [Item::of(bytes.clone(), 16), Item::of(bytes, 0)] {
                assert_eq!(item.len(), n as u64);
                let wire = Item::Object(Link { id: "a".repeat(32), v: None, h: "b".repeat(64), n: 1 });
                assert!(serde_json::to_string(&wire).unwrap().len() <= wire.wire());
                if let Item::Inline(_) = item {
                    assert!(serde_json::to_string(&item).unwrap().len() <= item.wire());
                }
            }
        }
    }

    /// A tree of 400 keys, in the store, and its shape.
    async fn grown(store: &Store) -> Shape {
        let mut tree = at(store, &Shape::default());
        for i in 0..400 {
            set(&mut tree, &format!("k/{i:03}"), &[i as u8; 5]).await;
        }
        commit(store, &mut tree).await
    }

    /// The ids that `audit` found, as the numbers `reachable` gives.
    fn numbers(audit: &Audit) -> HashSet<u128> {
        audit.ids.iter().map(|id| number(id).unwrap()).collect()
    }

    #[tokio::test]
    async fn reachable_is_every_object_of_the_tree_read_once_and_kept_out_of_the_cache() {
        let (mut store, counts) = counting();
        let shape = grown(&store).await;
        let audit = audit(&store, &shape).await;
        let all = numbers(&audit);
        assert!(all.len() > 16, "{} nodes are too few to read at once", all.len());
        store.cache = Arc::default();
        let tree = at(&store, &shape);

        // Every node below the root is read once, a few at once and no more than IN_FLIGHT, and none is kept.
        counts.gets.store(0, SeqCst);
        counts.delay.store(5, SeqCst);
        assert_eq!(tree.reachable(usize::MAX).await.unwrap(), Some(all.clone()));
        assert_eq!(counts.gets.load(SeqCst), all.len());
        let peak = counts.peak.load(SeqCst);
        assert!((2..=IN_FLIGHT).contains(&peak), "{peak} at once");
        counts.delay.store(0, SeqCst);
        counts.gets.store(0, SeqCst);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), audit.depth - 1, "the walk left nothing in the cache");

        // Over `max` nodes it is not read at all past the level that went over, and says so.
        assert_eq!(tree.reachable(all.len()).await.unwrap(), Some(all.clone()));
        counts.gets.store(0, SeqCst);
        assert_eq!(tree.reachable(all.len() - 1).await.unwrap(), None);
        assert!(counts.gets.load(SeqCst) < all.len());
        assert_eq!(tree.reachable(0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reachable_fails_when_the_tree_is_not_whole() {
        for damage in ["missing", "changed", "emptied"] {
            let mut store = versioned();
            let shape = grown(&store).await;
            let audit = audit(&store, &shape).await;
            // A node below the root, and one that is not a leaf if the tree has any: its loss costs the most.
            let id = audit.ids.iter().next().unwrap();
            let path = object("a", "n", id);
            match damage {
                "missing" => store.inner.delete(&path).await.unwrap(),
                "changed" => drop(store.put(&path, Bytes::from_static(b"{}"), PutMode::Overwrite).await.unwrap()),
                _ => drop(store.put(&path, Bytes::new(), PutMode::Overwrite).await.unwrap()),
            }
            store.cache = Arc::default(); // a node in the cache was whole when it was put there
            let fresh = Tree::open(&store, "a", "n", &shape, tiny()).unwrap();
            assert!(fresh.reachable(usize::MAX).await.is_err(), "a {damage} node is no tree");
        }
    }

    #[tokio::test]
    async fn reachable_includes_values_that_are_objects() {
        let store = versioned();
        let mut tree = at(&store, &Shape::default());
        set(&mut tree, "small", b"abc").await;
        set(&mut tree, "big", &[7; 90]).await; // inline is 8, so this is an object of its own
        let shape = commit(&store, &mut tree).await;
        let audit = audit(&store, &shape).await;
        assert_eq!(audit.ids.len(), 1, "a root of two keys has no node below it, and one value is an object");
        let fresh = Tree::open(&store, "a", "n", &shape, tiny()).unwrap();
        assert_eq!(fresh.reachable(usize::MAX).await.unwrap(), Some(numbers(&audit)));
        assert_eq!(stored(&store).await, audit.ids, "the object is all there is");
    }

    #[tokio::test]
    async fn costs_what_it_touches() {
        let (mut store, counts) = counting();
        let shape = grown(&store).await;
        let levels = audit(&store, &shape).await.depth;
        assert!(levels >= 3, "{levels} levels");
        let below = levels - 1; // the root is in the head

        // A cold read is a GET for each node below the root; a warm one is none.
        store.cache = Arc::default();
        counts.gets.store(0, SeqCst);
        let tree = at(&store, &shape);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), below);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), below, "a node is read once");
        assert_eq!(tree.reader.stats.hits.load(Relaxed) as usize, below);
        drop(tree);

        // A turn that wrote nothing writes nothing, and one that removes what is not there copies nothing.
        counts.puts.store(0, SeqCst);
        let mut tree = at(&store, &shape);
        tree.write(vec![("k/zzz".into(), None)]).await.unwrap();
        assert!(!tree.edited() && tree.work.dead.is_empty());
        drop(tree);
        assert_eq!(counts.puts.load(SeqCst), 0);

        // One value changed is a copy of the nodes from it to the root, which the head holds.
        store.cache = Arc::default();
        counts.gets.store(0, SeqCst);
        let mut tree = at(&store, &shape);
        set(&mut tree, "k/077", &[9; 5]).await;
        assert_eq!(counts.gets.load(SeqCst), below);
        let shape = tree.finish().await.unwrap();
        assert_eq!(counts.puts.load(SeqCst), below);
        assert_eq!(tree.landed().len(), below, "the nodes it replaced");
        assert_eq!(tree.reader.stats.depth.load(Relaxed), levels);
        let after = audit(&store, &shape).await;
        assert_eq!(after.depth, levels);
        drop(tree);
    }

    #[tokio::test]
    async fn a_failed_upload_leaves_nothing() {
        let (store, counts) = counting();
        let shape = grown(&store).await;
        let kept = audit(&store, &shape).await.ids;
        assert_eq!(stored(&store).await, kept);

        // A put fails after `allowed` more: at the first, the next, and so on, until the commit gets through.
        let mut failed = 0;
        for allowed in 0..200 {
            let mut tree = Tree::open(&store, "a", "n", &shape, Limits { budget: 1 << 20, ..tiny() }).unwrap();
            for i in 0..30 {
                set(&mut tree, &format!("k/{:03}", i * 13), &[1; 60]).await;
            }
            counts.allow.store(counts.puts.load(SeqCst) + allowed, SeqCst);
            let finished = tree.finish().await;
            if finished.is_err() {
                failed += 1;
                assert!(tree.finish().await.is_err(), "a tree that failed is not committed");
            }
            store.delete_many(tree.lost()).await;
            counts.allow.store(usize::MAX, SeqCst);
            assert_eq!(stored(&store).await, kept, "{allowed} uploads allowed");
            assert_eq!(audit(&store, &shape).await.items.len(), 400);
            if finished.is_ok() {
                break;
            }
        }
        assert!(failed >= 30, "{failed} commits failed");
    }

    #[tokio::test]
    async fn spills_when_it_holds_too_much() {
        let (store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        for i in 0..150 {
            set(&mut tree, &format!("k/{i:03}"), &[i as u8; 100]).await;
            assert!(tree.root.dirty() <= tiny().budget, "{} bytes held", tree.root.dirty());
        }
        assert!(counts.puts.load(SeqCst) > 0, "it spilled");
        let first = tree.get("k/000").await.unwrap().unwrap();
        assert_eq!(tree.value(&first).await.unwrap().as_ref(), &[0; 100]);
        // Dropped, it deletes what it made.
        let made = tree.made.lock().unwrap().len();
        assert_eq!(made, counts.puts.load(SeqCst));
        drop(tree);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(stored(&store).await.is_empty(), "{} left", stored(&store).await.len());
        assert_eq!(counts.deletes.load(SeqCst), made);
    }

    #[tokio::test]
    async fn refuses_what_is_over_its_limits() {
        let store = versioned();
        let mut tree = at(&store, &Shape::default());
        let edits: Vec<_> =
            (0..1001).map(|i| (format!("k/{i:04}"), Some(tree.item(Bytes::from_static(b"v"))))).collect();
        let full = tree.write(edits).await.unwrap_err();
        assert!(full.is::<Full>(), "{full:#}");
        assert!(!tree.edited() && tree.root.is_empty(), "nothing was written");

        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(tree.item(vec![0; 120].into())))).collect();
        assert!(tree.write(edits).await.unwrap_err().is::<Full>());
        assert!(!tree.edited());

        // What a batch replaces or removes does not count against it.
        for i in 0..900 {
            set(&mut tree, &format!("k/{i:04}"), &[0; 100]).await;
        }
        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(tree.item(vec![1; 100].into())))).collect();
        tree.write(edits).await.unwrap();
        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(tree.item(vec![1; 50].into())))).collect();
        tree.write(edits).await.unwrap();
        set(&mut tree, "k/zzz", &[1; 100]).await;
        assert!(tree.finish().await.is_ok(), "it is not poisoned");
    }

    #[tokio::test]
    async fn is_not_fooled_by_a_changed_object() {
        let mut store = versioned();
        let mut tree = at(&store, &Shape::default());
        for i in 0..40 {
            set(&mut tree, &format!("k/{i:02}"), &[i as u8; 50]).await;
        }
        let shape = commit(&store, &mut tree).await;
        let seen = audit(&store, &shape).await;
        let path = |id: &str| store::app("a", &["values", "n", id]);

        let Wire::Branch(kids) = &shape.root else { panic!("a tree of one node") };
        let node = &kids[0].1;
        let Some(Item::Object(blob)) = seen.items.values().next().cloned() else { panic!("no object") };
        for (link, what) in [(node.clone(), "a node"), (blob, "a value")] {
            let at = path(&link.id);
            let (good, _) = store.get(&at, None, 1 << 20).await.unwrap().unwrap();
            for bad in [vec![b' '; good.len()], good[1..].to_vec()] {
                store.put(&at, bad.into(), PutMode::Overwrite).await.unwrap();
                store.cache = Arc::default();
                let tree = at_shape(&store, &shape);
                let got = match tree.get("k/00").await {
                    Ok(Some(item)) => tree.value(&item).await.map(drop),
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                };
                let err = got.unwrap_err();
                assert!(format!("{err:#}").contains("is not what its link says"), "{what}: {err:#}");
            }
            store.put(&at, good, PutMode::Overwrite).await.unwrap();
        }
    }

    fn at_shape(store: &Store, shape: &Shape) -> Tree {
        at(store, shape)
    }

    #[tokio::test]
    async fn keeps_to_its_own_objects() {
        let (store, counts) = counting();
        let good = Link { id: "a".repeat(32), v: Some("3sL4kqtJlcpXroDTDmJ.rmSpXd".into()), h: "b".repeat(64), n: 5 };
        assert!(good.check().is_ok());
        let bad = |f: fn(&mut Link)| {
            let mut link = good.clone();
            f(&mut link);
            link
        };
        let bads = [
            bad(|l| l.id = "../names/n".into()),
            bad(|l| l.id = format!("{}/{}", "a".repeat(15), "a".repeat(16))),
            bad(|l| l.id = "A".repeat(32)),
            bad(|l| l.id = "a".repeat(31)),
            bad(|l| l.id = String::new()),
            bad(|l| l.h = "g".repeat(64)),
            bad(|l| l.v = Some("a\"b".into())),
            bad(|l| l.v = Some("a\\b".into())),
            bad(|l| l.v = Some("a b".into())),
            bad(|l| l.v = Some("v".repeat(VERSION_MAX + 1))),
        ];
        for link in bads {
            assert!(link.check().is_err(), "{link:?}");
            let shape = Shape { root: Wire::Branch(vec![(String::new(), link.clone())]), bytes: 0, entries: 0, seq: 0 };
            let tree = at(&store, &shape);
            let err = tree.get("k/a").await.unwrap_err();
            assert!(format!("{err:#}").contains("malformed"), "{err:#}");
            let err = tree.value(&Item::Object(link)).await.unwrap_err();
            assert!(format!("{err:#}").contains("malformed"), "{err:#}");
        }
        assert_eq!(counts.gets.load(SeqCst), 0, "nothing was asked of the store");

        // The head's own words about a link are kept to as well.
        assert!(serde_json::from_str::<Link>(r#"{"id":"a","h":"b","n":1,"x":2}"#).is_err());
        assert!(serde_json::from_str::<Shape>(r#"{"entries":1,"x":2}"#).is_err());
    }

    #[test]
    fn an_empty_tree_is_not_in_the_head() {
        assert_eq!(serde_json::to_string(&Shape::default()).unwrap(), "{}");
        assert!(Shape::default().is_empty());
        let shape: Shape = serde_json::from_str("{}").unwrap();
        assert!(shape.is_empty());
    }

    #[tokio::test]
    async fn lists_what_a_prefix_a_start_and_a_limit_pick_out() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        let mut model = BTreeMap::<String, Vec<u8>>::new();
        for prefix in ["a", "k", "k0", "z"] {
            for i in 0..120 {
                // A value sits in its node, or is an object of its own.
                let (key, value) = (format!("{prefix}/{i:03}"), format!("{prefix}/{i:03}").repeat(1 + i % 2));
                set(&mut tree, &key, value.as_bytes()).await;
                model.insert(key, value.into_bytes());
            }
        }
        let shape = commit(&store, &mut tree).await;
        drop(tree);
        let levels = audit(&store, &shape).await.depth;
        assert!(levels >= 3, "{levels} levels");

        // A listing reads the nodes on its way to the first key, and no others.
        for (prefix, found) in [("k/", "k/000"), ("k/119", "k/119")] {
            store.cache = Arc::default();
            counts.gets.store(0, SeqCst);
            let tree = at(&store, &shape);
            assert_eq!(tree.scan(prefix, None, 1).await.unwrap(), [found]);
            assert_eq!(counts.gets.load(SeqCst), levels - 1, "{prefix}");
        }

        let prefixes = ["", "a/", "k/", "k/1", "k/12", "k0/", "z/", "b", "zz", "k/119", "k/1190"];
        let afters =
            [None, Some(""), Some("a/050"), Some("k/"), Some("k/100"), Some("k/1005"), Some("k/999"), Some("zzz")];
        store.cache = Arc::default();
        let mut tree = at(&store, &shape);
        for round in 0..2 {
            if round == 1 {
                // With changes in memory: a whole range removed, and values added and replaced in the others.
                let gone: Vec<_> = model.keys().filter(|k| k.starts_with("k0/")).cloned().collect();
                tree.write(gone.iter().map(|k| (k.clone(), None)).collect()).await.unwrap();
                model.retain(|k, _| !k.starts_with("k0/"));
                for i in (0..120).step_by(7) {
                    let (old, new) = (format!("k/{i:03}"), format!("k/{i:03}~"));
                    tree.write(vec![(old.clone(), None)]).await.unwrap();
                    model.remove(&old);
                    set(&mut tree, &new, new.repeat(3).as_bytes()).await;
                    model.insert(new.clone(), new.repeat(3).into_bytes());
                }
                assert!(tree.edited());
            }
            for prefix in prefixes {
                for after in afters {
                    for limit in [0, 1, 7, 50, usize::MAX] {
                        let want: Vec<_> = model
                            .keys()
                            .filter(|k| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a))
                            .take(limit)
                            .cloned()
                            .collect();
                        let what = format!("round {round}: {prefix:?} after {after:?}, {limit}");
                        assert_eq!(tree.scan(prefix, after, limit).await.unwrap(), want, "{what}");
                        let entries = tree.entries(prefix, after, limit).await.unwrap();
                        assert_eq!(entries.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), want, "{what}");
                        for (key, item) in &entries {
                            assert_eq!(tree.value(item).await.unwrap().as_ref(), model[key].as_slice(), "{key}");
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_fork_is_a_tree_of_its_own() {
        let (store, counts) = counting();
        let shape = grown(&store).await;
        counts.deletes.store(0, SeqCst);
        let mut tree = at(&store, &shape);
        set(&mut tree, "k/000", b"was").await;
        set(&mut tree, "k/001", &[7; 90]).await; // a value to upload, held in the node

        // It has what the tree had, and then neither sees what the other does.
        let mut fork = tree.fork();
        assert_eq!(text(&fork, "k/000").await, b"was");
        assert_eq!(text(&fork, "k/001").await, [7; 90]);
        set(&mut tree, "k/000", b"tree").await;
        tree.write(vec![("k/200".into(), None)]).await.unwrap();
        set(&mut fork, "k/001", b"fork").await;
        fork.write(vec![("k/300".into(), None)]).await.unwrap();
        assert_eq!(text(&tree, "k/000").await, b"tree");
        assert_eq!(text(&fork, "k/000").await, b"was");
        assert_eq!(text(&tree, "k/001").await, [7; 90]);
        assert_eq!(text(&fork, "k/001").await, b"fork");
        assert!(tree.get("k/200").await.unwrap().is_none() && fork.get("k/200").await.unwrap().is_some());
        assert!(tree.get("k/300").await.unwrap().is_some() && fork.get("k/300").await.unwrap().is_none());

        // Whatever a tree uploaded before, a fork reads, and dropping it deletes none of it: that is for the tree.
        drop((fork, tree));
        settled().await;
        counts.deletes.store(0, SeqCst);
        let mut tree = at(&store, &shape);
        for i in 0..60 {
            set(&mut tree, &format!("k/{i:03}"), &[i as u8; 100]).await;
        }
        let made = tree.made.lock().unwrap().len();
        assert!(made > 0, "it spilled");
        let fork = tree.fork();
        for i in 0..60 {
            assert_eq!(text(&fork, &format!("k/{i:03}")).await, [i as u8; 100]);
        }
        drop(fork);
        settled().await;
        assert_eq!(counts.deletes.load(SeqCst), 0);
        drop(tree);
        settled().await;
        assert_eq!(counts.deletes.load(SeqCst), made);
    }

    #[tokio::test]
    async fn numbers_keep_counting_and_a_lost_turn_gives_back_its_own() {
        let store = versioned();
        let mut tree = at(&store, &Shape::default());
        assert_eq!((tree.alloc(), tree.alloc()), (2, 3), "the root is 1");
        set(&mut tree, "f/2", b"x").await;
        let shape = commit(&store, &mut tree).await;
        assert_eq!(shape.seq, 3);
        let shape: Shape = serde_json::from_str(&serde_json::to_string(&shape).unwrap()).unwrap();
        assert_eq!(shape.seq, 3);

        // A tree goes on from the shape, and a turn that is lost leaves its numbers to the next.
        let mut next = at(&store, &shape);
        assert_eq!((next.alloc(), next.alloc()), (4, 5));
        assert!(next.lost().is_empty());
        assert_eq!(next.alloc(), 4);

        // A name emptied has still used its numbers up, so its head is not empty.
        next.write(vec![("f/2".into(), None)]).await.unwrap();
        let emptied = next.finish().await.unwrap();
        assert!(emptied.root.is_empty() && emptied.entries == 0 && emptied.bytes == 0);
        assert!(!emptied.is_empty());
        assert_eq!(serde_json::to_string(&emptied).unwrap(), r#"{"seq":4}"#);
        assert_eq!(at(&store, &emptied).alloc(), 5);

        // One that never allocated starts at 2, and so does one whose root is the only inode.
        assert_eq!(at(&store, &Shape::default()).alloc(), 2);
        assert_eq!(at(&store, &Shape { seq: 1, ..Shape::default() }).alloc(), 2);
    }

    #[tokio::test]
    async fn an_object_is_read_once_and_is_what_its_link_says_each_time() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        set(&mut tree, "k/b", &[2; 60]).await;
        let shape = commit(&store, &mut tree).await;
        drop(tree);

        // What a turn uploaded is kept for the next request, so a read of it is not a GET.
        counts.gets.store(0, SeqCst);
        assert_eq!(text(&at(&store, &shape), "k/a").await, [1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 0);

        // What is not, is fetched once.
        store.cache = Arc::default();
        let tree = at(&store, &shape);
        let item = tree.get("k/a").await.unwrap().unwrap();
        let Item::Object(link) = item.clone() else { panic!("a value in its node") };
        for _ in 0..3 {
            assert_eq!(tree.value(&item).await.unwrap().as_ref(), &[1; 60]);
        }
        assert_eq!(counts.gets.load(SeqCst), 1);
        assert_eq!(tree.reader.stats.hits.load(Relaxed), 2);

        // A link that names the same object, but another hash, is not answered from the cache: the store is asked, and
        // what it gives is not what that link says.
        let other = Link { h: store::hash(b"another"), ..link.clone() };
        let err = tree.value(&Item::Object(other)).await.unwrap_err();
        assert!(format!("{err:#}").contains("is not what its link says"), "{err:#}");
        assert_eq!(counts.gets.load(SeqCst), 2);
        assert_eq!(tree.value(&item).await.unwrap().as_ref(), &[1; 60], "the cache has not been spoilt");
        assert_eq!(counts.gets.load(SeqCst), 2);

        // Nor is it answered for another app or name: those are other objects, which are not there.
        for (app, name) in [("b", "n"), ("a", "m")] {
            let tree = Tree::open(&store, app, name, &Shape::default(), tiny()).unwrap();
            let err = tree.value(&item).await.unwrap_err();
            assert!(format!("{err:#}").contains("is missing"), "{app}/{name}: {err:#}");
        }
        assert_eq!(counts.gets.load(SeqCst), 4);
    }

    #[tokio::test]
    async fn a_block_that_is_being_fetched_is_waited_for_by_the_reads_that_come_to_it() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        let shape = commit(&store, &mut tree).await;
        drop(tree);
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &shape));
        let item = tree.get("k/a").await.unwrap().unwrap();
        counts.gets.store(0, SeqCst);
        counts.delay.store(50, SeqCst);

        // Eight reads at once are one fetch, and each has the block.
        let reads = (0..8).map(|_| {
            let (tree, item) = (tree.clone(), item.clone());
            tokio::spawn(async move { tree.value(&item).await.unwrap() })
        });
        for read in futures_util::future::join_all(reads).await {
            assert_eq!(read.unwrap().as_ref(), &[1; 60]);
        }
        assert_eq!(counts.gets.load(SeqCst), 1);
        assert!(store.cache.flights.lock().unwrap().is_empty(), "the gate of a block is gone with the fetch");

        // The one that is fetching is cancelled: the next in turn fetches, and none of them is left waiting.
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &shape));
        counts.gets.store(0, SeqCst);
        counts.delay.store(200, SeqCst);
        let first = tokio::spawn({
            let (tree, item) = (tree.clone(), item.clone());
            async move { tree.value(&item).await }
        });
        while counts.flight.load(SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let second = tokio::spawn({
            let (tree, item) = (tree.clone(), item.clone());
            async move { tree.value(&item).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // it is waiting at the gate
        first.abort();
        assert_eq!(second.await.unwrap().as_ref(), &[1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 2, "the fetch that was cancelled, and the one that took its place");
        assert!(store.cache.flights.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_read_that_is_cancelled_at_the_gate_leaves_no_gate_behind() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        let shape = commit(&store, &mut tree).await;
        drop(tree);
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &shape));
        let item = tree.get("k/a").await.unwrap().unwrap();
        let comers = || store.cache.flights.lock().unwrap().values().map(|gate| gate.comers).sum::<usize>();
        let read = |tree: &Arc<Tree>| {
            let (tree, item) = (tree.clone(), item.clone());
            tokio::spawn(async move { tree.value(&item).await })
        };

        // The one that fetches has the block to itself, and the waiter that is cancelled while it does is counted out
        // at once, and not when the one that fetches is done: its gate is there until then, and no later.
        counts.delay.store(300, SeqCst);
        let leader = read(&tree);
        while comers() < 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let waiter = read(&tree);
        while comers() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await; // it is waiting at the gate
        }
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(comers(), 1, "the waiter went with its read");
        assert_eq!(leader.await.unwrap().unwrap().as_ref(), &[1; 60]);
        assert!(store.cache.flights.lock().unwrap().is_empty(), "nothing is left of the gate");
        assert_eq!(counts.gets.load(SeqCst), 1, "the waiter that was cancelled fetched nothing");
    }

    #[tokio::test]
    async fn reads_that_are_cancelled_at_any_moment_of_a_fetch_leave_no_gate_behind() {
        let (store, counts) = counting();
        let mut tree = at(&store, &Shape::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        let shape = commit(&store, &mut tree).await;
        drop(tree);
        // That includes when the one that fetches lets go of the gate and wakes the next in turn: whichever way the
        // drops of the cancelled read and of what it waits on fall, none of them leaves a gate behind.
        for round in 0..4 {
            let store = Store { cache: Arc::default(), ..store.clone() };
            let tree = Arc::new(at(&store, &shape));
            let item = tree.get("k/a").await.unwrap().unwrap();
            counts.delay.store(20, SeqCst);
            let reads: Vec<_> = (0..24)
                .map(|_| {
                    let (tree, item) = (tree.clone(), item.clone());
                    tokio::spawn(async move { tree.value(&item).await })
                })
                .collect();
            for (i, read) in reads.iter().enumerate().skip(1) {
                if i % 2 == round % 2 {
                    tokio::time::sleep(Duration::from_millis(i as u64 % 5)).await;
                    read.abort();
                }
            }
            for read in reads {
                if let Ok(got) = read.await {
                    assert_eq!(got.unwrap().as_ref(), &[1; 60]);
                }
            }
            assert!(store.cache.flights.lock().unwrap().is_empty(), "round {round}");
        }
    }

    #[test]
    fn a_cache_entry_is_for_one_path_version_and_hash() {
        let mut lru = Lru::<u32>::new(100).into_inner().unwrap();
        let key = |path: &str, version: Option<&str>| (Path::from(path), version.map(str::to_owned));
        lru.put(key("apps/a/values/n/x", Some("v1")), 7, "h1".into(), 10);
        assert_eq!(lru.get(&key("apps/a/values/n/x", Some("v1")), "h1"), Some(7));
        for (path, version, hash, what) in [
            ("apps/a/values/n/x", Some("v2"), "h1", "another version"),
            ("apps/a/values/n/x", None, "h1", "no version"),
            ("apps/b/values/n/x", Some("v1"), "h1", "another app"),
            ("apps/a/values/m/x", Some("v1"), "h1", "another name"),
            ("apps/a/values/n/y", Some("v1"), "h1", "another id"),
            ("apps/a/values/n/x", Some("v1"), "h2", "another hash"),
        ] {
            assert_eq!(lru.get(&key(path, version), hash), None, "{what}");
        }
    }

    #[test]
    fn a_cache_forgets_what_it_used_least() {
        let mut lru = Lru::<usize>::new(100).into_inner().unwrap();
        let key = |i: usize| (Path::from(format!("x/{i}")), None);
        for i in 0..10 {
            lru.put(key(i), i, "h".into(), 10);
        }
        assert_eq!(lru.bytes, 100);
        assert_eq!(lru.get(&key(0), "h"), Some(0)); // used again, so the others go first
        lru.put(key(10), 10, "h".into(), 10);
        let kept: Vec<_> = (0..=10).filter(|i| lru.get(&key(*i), "h").is_some()).collect();
        assert_eq!(kept, [0, 4, 5, 6, 7, 8, 9, 10], "down to seven eighths of the bound");
        assert_eq!(lru.bytes, 80);
        // An entry that is put again is not counted twice, and is the new one.
        lru.put(key(0), 11, "h2".into(), 10);
        assert_eq!(lru.bytes, 80);
        assert_eq!((lru.get(&key(0), "h"), lru.get(&key(0), "h2")), (None, Some(11)));
    }
}
