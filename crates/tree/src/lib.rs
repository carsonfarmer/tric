//! A copy-on-write B+tree in an object store, as LMDB and bbolt keep theirs in a file.
//!
//! A tree maps string keys to byte values. Its root is a small [`Root`], which the caller keeps where it keeps its own
//! state, such as an object it swaps by compare-and-swap. Every other node, and every value too large to sit in a node,
//! is an immutable object directly under a prefix the caller gives, at a random 128-bit id: see [`object`]. A parent
//! names each object by a link of its id, its version if the store keeps versions, its SHA-256 and its length, and
//! every read is checked against its link.
//!
//! A [`Tree`] is opened at a root, written, and finished, which uploads what changed and gives the new root:
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), tric_tree::Error> {
//! use object_store::{memory::InMemory, path::Path};
//! use std::sync::Arc;
//! use tric_tree::{Cache, Config, Root, Tree};
//!
//! let (store, cache) = (Arc::new(InMemory::new()), Arc::new(Cache::default()));
//! let prefix = Path::from("trees/one");
//!
//! let mut tree = Tree::open(store.clone(), prefix.clone(), cache.clone(), &Root::default(), Config::default())?;
//! tree.write(vec![("a".into(), Some("small".into())), ("b".into(), Some(vec![7u8; 10_000].into()))]).await?;
//! let root = tree.finish().await?; // for the caller to keep, or to swap in where it keeps it
//!
//! let tree = Tree::open(store, prefix, cache, &root, Config::default())?;
//! let item = tree.get("b").await?.expect("b is there");
//! assert_eq!(tree.value(&item).await?.len(), 10_000);
//! assert_eq!(tree.scan("", None, 10).await?, ["a", "b"]);
//! # Ok(())
//! # }
//! ```
//!
//! # What it guarantees
//!
//! - **A read is verified.** An object that is missing, over the size the config allows, or not what its link says is
//!   [`Error::Corrupt`], and never data.
//! - **A finished root never changes.** Objects are only added, at new random ids: none is overwritten or deleted, so
//!   a root reads the same for as long as its objects are kept.
//! - **A write is all or none.** One refused as [`Error::Full`] or [`Error::TooLarge`] changes nothing. One that fails
//!   otherwise, as when the store fails, or that is dropped half way, leaves the tree [`Error::Poisoned`] until
//!   [`Tree::reset`].
//! - **Memory is bounded.** A node an edit changes is a copy in memory, and a large value is held until it is uploaded;
//!   past [`Config::budget`], the values and then the nodes are uploaded early, as LMDB spills dirty pages.
//! - **The tree stays balanced.** A node over its size splits, a node left under a quarter full merges with a
//!   neighbour, and a root of one child gives way to it, as in bbolt.
//!
//! # The caller's part
//!
//! - **The root, and its swap.** [`Tree::finish`] gives a root, and the tree keeps it nowhere. Where writers race, the
//!   caller swaps it in by compare-and-swap, and when that fails, it calls [`Tree::reset`] and tries again.
//! - **Collection.** The tree deletes nothing: what an edit replaces, and what a writer that lost its race or crashed
//!   uploaded, stays in the store. A collector lists the prefix, marks what every root it keeps reaches with
//!   [`Tree::mark`], and deletes only the ids that none reaches and that are older than the longest a writer takes
//!   from its first upload to its swap, as until then a writer's objects are reached by no root.
//! - **The store.** Any [`ObjectStore`]. In one that keeps versions, a link holds the version it wrote, and a read asks
//!   for it.
//! - **The config.** A tree is opened with a [`Config`] whose `node` and `blob` are no smaller than those it was
//!   written with, as a larger object is refused as corrupt. A `node` of 1 KiB or more holds enough to be a tree.
//! - **The cache.** A [`Cache`] of what was read, which the trees that are to share it share through an `Arc`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use base64::{Engine as _, prelude::BASE64_STANDARD as B64};
use bytes::Bytes;
use futures_util::future::{BoxFuture, try_join_all};
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{GetOptions, ObjectStore, PutMode, path::Path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tokio::sync::Semaphore;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// The longest version id a link may hold: S3 and RustFS give 32 and 36 characters.
const VERSION_MAX: usize = 64;
/// What a link is as JSON, at most, when its version id is not yet known.
const LINK_SIZE: usize = 144 + VERSION_MAX;
/// What a node is as JSON beyond its entries.
const OVERHEAD: usize = 16;
/// The bytes of nodes, as JSON, that a default cache keeps.
const NODES: usize = 16 << 20;
/// The bytes of values that a default cache keeps.
const VALUES: usize = 32 << 20;

/// How many uploads a tree has under way at once, and how many nodes a mark reads at once.
pub const IN_FLIGHT: usize = 16;

/// What bounds a tree, and how it uses the store. A tree is read with a `node` and a `blob` no smaller than those it
/// was written with.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The most a node is as JSON, in bytes: 64 KiB by default. A key and its value take a quarter of one at most.
    pub node: usize,
    /// The most a value is, to sit in its node: 4 KiB by default. A larger one is an object of its own.
    pub inline: usize,
    /// The most a tree holds in memory that is not yet uploaded, of the nodes it changed and the values it holds: 8 MiB
    /// by default.
    pub budget: usize,
    /// The most keys a tree holds: no bound by default.
    pub entries: u64,
    /// The most bytes of values a tree holds: no bound by default.
    pub data: u64,
    /// The most a value is: 1 MiB by default.
    pub blob: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { node: 64 << 10, inline: 4 << 10, budget: 8 << 20, entries: u64::MAX, data: u64::MAX, blob: 1 << 20 }
    }
}

/// Why a tree failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A write was refused, and changed nothing: the tree would hold more than its config allows.
    Full(Full),
    /// A write was refused, and changed nothing: a value or a key is over what the config allows. A node or a root over
    /// its size, when a tree is finished, is this too.
    TooLarge(String),
    /// An object is missing, or is not what its link says, or a link is malformed: the tree is not whole.
    Corrupt(String),
    /// The store failed.
    Store(object_store::Error),
    /// A change failed half way, or was dropped, and the tree is not to be finished: see [`Tree::reset`].
    Poisoned,
}

/// What a tree would hold more of than its config allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Full {
    /// The most keys a tree holds, which is [`Config::entries`].
    Entries(u64),
    /// The most bytes of values a tree holds, which is [`Config::data`].
    Data(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(Full::Entries(n)) => write!(f, "a tree holds {n} keys or fewer"),
            Self::Full(Full::Data(n)) => write!(f, "a tree holds {n} bytes of values or fewer"),
            Self::TooLarge(why) | Self::Corrupt(why) => f.write_str(why),
            Self::Store(e) => e.fmt(f),
            Self::Poisoned => f.write_str("an earlier change to the tree failed"),
        }
    }
}

impl std::error::Error for Error {
    /// The store's error is shown as this one, so what it has below it is the source.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => e.source(),
            _ => None,
        }
    }
}

impl From<object_store::Error> for Error {
    fn from(e: object_store::Error) -> Self {
        Self::Store(e)
    }
}

/// The result of what a tree does.
pub type Result<T, E = Error> = std::result::Result<T, E>;

fn corrupt<T>(why: String) -> Result<T> {
    Err(Error::Corrupt(why))
}

/// The SHA-256 of `bytes`, in hex.
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 128 random bits, in hex: a new object's id.
fn random() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// An object in the tree: where it is, which of its versions, and what it must hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    id: String, // 128 random bits, in hex
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v: Option<String>, // the version, if the store keeps them
    h: String,  // the SHA-256, in hex
    n: u64,     // the length
}

/// Whether `s` is `n` lowercase hex digits.
fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An object's id as a number, if it is one: 128 bits, as it is 32 lowercase hex digits, as every id a tree gives is.
/// A collector compares the ids it lists with those [`Tree::mark`] gives as numbers, in 16 bytes where the string takes
/// 56 or more.
pub fn number(id: &str) -> Option<u128> {
    hex(id, 32).then(|| u128::from_str_radix(id, 16).ok()).flatten()
}

/// Where the object `id` of the tree under `prefix` is: every one is directly under its prefix.
pub fn object(prefix: &Path, id: &str) -> Path {
    prefix.clone().join(id)
}

impl Link {
    /// Whether the link is well-formed, so that the path it leads to is under the tree's prefix and nowhere else.
    fn check(&self) -> Result<()> {
        let plain =
            |v: &str| v.len() <= VERSION_MAX && v.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
        match hex(&self.id, 32) && hex(&self.h, 64) && self.v.as_deref().is_none_or(plain) {
            true => Ok(()),
            false => corrupt("a link is malformed".into()),
        }
    }

    /// The id as a number: see [`number`].
    fn number(&self) -> Result<u128> {
        self.check()?;
        number(&self.id).map_or_else(|| corrupt("a link is malformed".into()), Ok)
    }

    /// What the link is as JSON, at most.
    fn wire(&self) -> usize {
        48 + self.id.len() + self.v.as_ref().map_or(0, String::len) + self.h.len()
    }
}

/// What a key holds, as its node holds it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Value {
    /// A value of `inline` bytes or less, in base64.
    Inline(String),
    /// A larger value, which is an object.
    Object(Link),
    /// A larger value that is not yet uploaded. It is only ever in memory, and counts against the budget.
    #[serde(skip)]
    Held(Bytes),
}

impl Value {
    fn of(bytes: Bytes, inline: usize) -> Self {
        match bytes.len() <= inline {
            true => Self::Inline(B64.encode(&bytes)),
            false => Self::Held(bytes),
        }
    }

    fn len(&self) -> u64 {
        match self {
            Self::Inline(s) => {
                (s.len() / 4 * 3).saturating_sub(s.bytes().rev().take_while(|b| *b == b'=').count()) as u64
            }
            Self::Object(link) => link.n,
            Self::Held(bytes) => bytes.len() as u64,
        }
    }

    /// What the value is as JSON, at most.
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

/// What a key holds, as a read finds it: the value, or a link to the object that holds it. [`Tree::value`] reads the
/// value.
#[derive(Clone, Debug)]
pub struct Item(Value);

impl Item {
    /// The length of the value, which is known without reading it.
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A tree as its caller keeps it: the root node, and the totals the config holds it to. It is small, at most a node,
/// and serialises to `{}` when it is empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    #[serde(default, skip_serializing_if = "Body::is_empty")]
    root: Body,
    #[serde(default, skip_serializing_if = "is_zero")]
    bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    entries: u64,
}

impl Root {
    /// Whether it is the root of a tree that holds nothing: such a root is the default one, and need not be kept.
    pub fn is_empty(&self) -> bool {
        self.root.is_empty() && self.bytes == 0 && self.entries == 0
    }
}

/// A node in memory. `size` is its JSON, at most, and `held` the bytes of values in it not yet uploaded.
#[derive(Clone, Debug)]
struct Node {
    body: Body,
    size: usize,
    held: usize,
}

/// What a node holds, as its object and the root hold it too. A child is its least key and its link: a node still in
/// memory has none, and is not written.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Body {
    Leaf(Vec<(String, Value)>),
    /// Each child under the least key it may hold, which the first child's is not held to.
    Branch(Vec<Child>),
}

impl Default for Body {
    fn default() -> Self {
        Self::Leaf(vec![])
    }
}

impl Body {
    fn is_empty(&self) -> bool {
        match self {
            Self::Leaf(es) => es.is_empty(),
            Self::Branch(kids) => kids.is_empty(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(from = "(String, Link)")]
struct Child {
    key: String,
    at: At,
}

impl From<(String, Link)> for Child {
    fn from((key, link): (String, Link)) -> Self {
        Self { key, at: At::Clean(link) }
    }
}

impl Serialize for Child {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.at {
            At::Clean(link) => (&self.key, link).serialize(s),
            At::Dirty(_) => Err(serde::ser::Error::custom("a node still has a child in memory")),
        }
    }
}

#[derive(Clone, Debug)]
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

fn entry(key: &str, value: &Value) -> usize {
    json_len(key) + value.wire() + 4
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

    fn of(body: Body) -> Result<Self> {
        if matches!(&body, Body::Branch(kids) if kids.is_empty()) {
            return corrupt("a branch has no children".into());
        }
        Ok(Self::measured(body))
    }

    /// The node as its object holds it, once all its children are links.
    fn json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.body).expect("a node whose children are links is JSON")
    }

    fn measure(&mut self) {
        (self.size, self.held) = match &self.body {
            Body::Leaf(es) => {
                (OVERHEAD + es.iter().map(|(k, v)| entry(k, v)).sum::<usize>(), es.iter().map(|(_, v)| v.held()).sum())
            }
            Body::Branch(kids) => (OVERHEAD + kids.iter().map(child).sum::<usize>(), 0),
        };
    }

    fn is_empty(&self) -> bool {
        self.body.is_empty()
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
                Body::Leaf(es.split_off(cut(es.iter().map(|(k, v)| entry(k, v)), self.size, es.len())))
            }
            Body::Branch(kids) if kids.len() > 1 => {
                Body::Branch(kids.split_off(cut(kids.iter().map(child), self.size, kids.len())))
            }
            _ => return corrupt(format!("a node of one entry is over {} bytes", self.size)),
        };
        self.measure();
        Ok(Self::measured(body))
    }

    /// Adds the entries of `right`, the node next to this one on its right.
    fn append(&mut self, right: Self) -> Result<()> {
        match (&mut self.body, right.body) {
            (Body::Leaf(a), Body::Leaf(b)) => a.extend(b),
            (Body::Branch(a), Body::Branch(b)) => a.extend(b),
            _ => return corrupt("the tree is not as deep on one side".into()),
        }
        self.measure();
        Ok(())
    }
}

/// The objects read, for the trees that share it: nodes, and the values too large to sit in one. An object never
/// changes, and is kept by its full path, its version and the hash it was checked against, so a hit is what its link
/// says, and trees under any prefix, of any store, may share one. Reads that come to an object that another read is
/// fetching, as two reads of one value at once do, wait for that fetch and have what it checked, instead of fetching
/// it again; when the one that fetches is cancelled, the next in turn does.
pub struct Cache {
    nodes: quick_cache::sync::Cache<Key, Arc<Node>, Size>,
    values: quick_cache::sync::Cache<Key, Bytes, Size>,
}

/// An object's path, version and hash.
type Key = (Path, Option<String>, String);

/// What an entry weighs in the cache: its bytes, or the most a node is thought to be as JSON.
#[derive(Clone)]
struct Size;

impl quick_cache::Weighter<Key, Arc<Node>> for Size {
    fn weight(&self, _: &Key, node: &Arc<Node>) -> u64 {
        node.size as u64
    }
}

impl quick_cache::Weighter<Key, Bytes> for Size {
    fn weight(&self, _: &Key, bytes: &Bytes) -> u64 {
        (bytes.len() as u64).max(1) // an entry that weighs nothing is never forgotten
    }
}

/// A cache of up to `bytes`, in one shard: in several, each would have a share of the bound, and keep nothing over its
/// share. Its tables are sized for objects of about 16 KiB, a quarter of a default node.
fn pool<V: Clone>(bytes: usize) -> quick_cache::sync::Cache<Key, V, Size>
where
    Size: quick_cache::Weighter<Key, V>,
{
    let options = quick_cache::OptionsBuilder::new()
        .shards(1)
        .estimated_items_capacity(bytes / (16 << 10) + 1)
        .weight_capacity(bytes as u64)
        .build()
        .expect("both capacities are set");
    quick_cache::sync::Cache::with_options(options, Size, Default::default(), Default::default())
}

impl Cache {
    /// A cache that keeps up to `nodes` bytes of nodes, as JSON, and `values` bytes of values. Past either, it forgets
    /// what was used least, as `quick_cache` judges it.
    pub fn new(nodes: usize, values: usize) -> Self {
        Self { nodes: pool(nodes), values: pool(values) }
    }
}

impl Default for Cache {
    /// A cache of 16 MiB of nodes and 32 MiB of values.
    fn default() -> Self {
        Self::new(NODES, VALUES)
    }
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cache")
            .field("nodes", &(self.nodes.weight(), self.nodes.capacity()))
            .field("values", &(self.values.weight(), self.values.capacity()))
            .finish_non_exhaustive()
    }
}

/// Where a tree's objects are, and how to read them.
struct Reader {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
    cache: Arc<Cache>,
    config: Config,
    /// The objects the tree fetched and uploaded, for the debug log, which is written when the tree is gone.
    gets: AtomicU64,
    puts: AtomicU64,
}

impl Reader {
    fn path(&self, id: &str) -> Path {
        object(&self.prefix, id)
    }

    fn key(&self, link: &Link) -> Key {
        (self.path(&link.id), link.v.clone(), link.h.clone())
    }

    /// The object `link` names, which must be `max` bytes or less, and be what the link says.
    async fn fetch(&self, link: &Link, max: u64) -> Result<Bytes> {
        link.check()?;
        let path = self.path(&link.id);
        if link.n > max {
            return corrupt(format!("{path} is over {max} bytes"));
        }
        let got = match self.store.get_opts(&path, GetOptions { version: link.v.clone(), ..Default::default() }).await {
            Ok(got) => got,
            Err(object_store::Error::NotFound { .. }) => return corrupt(format!("{path} is missing")),
            Err(e) => return Err(e.into()),
        };
        if got.meta.size > max {
            return corrupt(format!("{path} is over {max} bytes"));
        }
        let bytes = got.bytes().await?;
        self.gets.fetch_add(1, Relaxed);
        if bytes.len() as u64 != link.n || hash(&bytes) != link.h {
            return corrupt(format!("{path} is not what its link says"));
        }
        Ok(bytes)
    }

    async fn node(&self, link: &Link) -> Result<Arc<Node>> {
        self.load(link, true).await
    }

    /// The node `link` names, from the cache if it is there, else read and checked against the link, and then kept if
    /// `keep`. A mark passes `false`, so as not to push out what reads are using.
    async fn load(&self, link: &Link, keep: bool) -> Result<Arc<Node>> {
        link.check()?;
        let key = self.key(link);
        let read = async {
            let bytes = self.fetch(link, self.config.node as u64).await?;
            match serde_json::from_slice(&bytes) {
                Ok(body) => Ok(Arc::new(Node::of(body)?)),
                Err(e) => corrupt(format!("{}: {e}", key.0)),
            }
        };
        if keep {
            return self.cache.nodes.get_or_insert_async(&key, read).await;
        }
        match self.cache.nodes.peek(&key) {
            Some(node) => Ok(node),
            None => read.await,
        }
    }

    /// The value `value` holds.
    async fn value(&self, value: &Value) -> Result<Bytes> {
        match value {
            Value::Inline(s) => B64.decode(s).map(Bytes::from).or_else(|e| corrupt(format!("a value in a node: {e}"))),
            Value::Object(link) => self.large_value(link).await,
            Value::Held(bytes) => Ok(bytes.clone()),
        }
    }

    /// The large value `link` names, which is an object of its own.
    async fn large_value(&self, link: &Link) -> Result<Bytes> {
        link.check()?;
        self.cache.values.get_or_insert_async(&self.key(link), self.fetch(link, self.config.blob)).await
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let (gets, puts) = (*self.gets.get_mut(), *self.puts.get_mut());
        if gets + puts > 0 {
            let prefix = &self.prefix;
            tracing::debug!(%prefix, gets, puts, "tree");
        }
    }
}

/// The value at `key`, below `node`.
fn find<'a>(r: &'a Reader, node: &'a Node, key: &'a str) -> BoxFuture<'a, Result<Option<Value>>> {
    Box::pin(async move {
        match &node.body {
            Body::Leaf(es) => Ok(es.binary_search_by(|(k, _)| k.as_str().cmp(key)).ok().map(|i| es[i].1.clone())),
            Body::Branch(kids) => match &kids[slot(kids, key)].at {
                At::Dirty(below) => find(r, below, key).await,
                At::Clean(link) => find(r, &*r.node(link).await?, key).await,
            },
        }
    })
}

/// Adds to `out` the keys below `node` that start with `prefix`, and follow `after` if there is one, up to `limit`.
fn walk<'a>(
    r: &'a Reader,
    node: &'a Node,
    (prefix, after, limit): (&'a str, Option<&'a str>, usize),
    out: &'a mut Vec<String>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        match &node.body {
            Body::Leaf(es) => {
                let before = |k: &str| k < prefix || after.is_some_and(|a| k <= a);
                for (k, _) in &es[es.partition_point(|(k, _)| before(k))..] {
                    if out.len() >= limit || !k.starts_with(prefix) {
                        break;
                    }
                    out.push(k.clone());
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
                        At::Dirty(below) => walk(r, below, (prefix, after, limit), &mut *out).await?,
                        At::Clean(link) => {
                            let node = r.node(link).await?;
                            walk(r, &node, (prefix, after, limit), &mut *out).await?
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

/// Adds to `seen` the objects `node` names, and those below it that are in memory, and to `next` the links of the
/// nodes among them that were not in `seen`.
fn refs(node: &Node, seen: &mut HashSet<u128>, next: &mut Vec<Link>) -> Result<()> {
    match &node.body {
        Body::Leaf(es) => {
            for (_, value) in es {
                if let Value::Object(link) = value {
                    seen.insert(link.number()?);
                }
            }
        }
        Body::Branch(kids) => {
            for kid in kids {
                match &kid.at {
                    At::Clean(link) => {
                        if seen.insert(link.number()?) {
                            next.push(link.clone());
                        }
                    }
                    At::Dirty(below) => refs(below, seen, next)?,
                }
            }
        }
    }
    Ok(())
}

/// The node `at` leads to, in memory and ready to change: a link is loaded, and copied.
async fn dirty<'a>(r: &Reader, at: &'a mut At) -> Result<&'a mut Node> {
    if let At::Clean(link) = at {
        let loaded = r.node(link).await?;
        *at = At::Dirty(Box::new((*loaded).clone()));
    }
    match at {
        At::Dirty(node) => Ok(node),
        At::Clean(_) => unreachable!("a node was just loaded"),
    }
}

/// Merges what is under a quarter full below `node`, as bbolt does: a child with a neighbour, and with the sides shared
/// out again if they do not fit in one. A child that is empty is dropped.
fn fix<'a>(r: &'a Reader, node: &'a mut Node) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let Body::Branch(kids) = &mut node.body else { return Ok(()) };
        for kid in kids.iter_mut() {
            if let At::Dirty(below) = &mut kid.at {
                fix(r, below).await?;
            }
        }
        kids.retain(|k| !matches!(&k.at, At::Dirty(n) if n.is_empty()));
        let (thin, mut i) = (r.config.node / 4, 0);
        while i < kids.len() {
            if kids.len() < 2 || !matches!(&kids[i].at, At::Dirty(n) if n.size < thin) {
                i += 1;
                continue;
            }
            let (a, b) = if i > 0 { (i - 1, i) } else { (0, 1) };
            dirty(r, &mut kids[a].at).await?;
            dirty(r, &mut kids[b].at).await?;
            let least = kids[b].key.clone();
            let At::Dirty(mut right) = kids.remove(b).at else { unreachable!("a node was just loaded") };
            if let Body::Branch(firsts) = &mut right.body {
                firsts[0].key = least; // the first child's key is a bound now, as it was the parent's
            }
            let At::Dirty(left) = &mut kids[a].at else { unreachable!("a node was just loaded") };
            left.append(*right)?;
            i = a;
            if left.size > r.config.node {
                let right = left.split_off()?;
                kids.insert(a + 1, Child { key: right.first_key().to_owned(), at: At::Dirty(Box::new(right)) });
                i = a + 2;
            }
        }
        node.measure();
        Ok(())
    })
}

/// What a tree holds besides its nodes.
#[derive(Clone, Debug, Default)]
struct Work {
    bytes: u64,
    entries: u64,
}

impl Work {
    /// Sets `key` to `value` below `node`, or removes it if `value` is none. A node that grows past its size is split,
    /// and its second half, with the least key of it, is for the parent to take.
    fn edit<'a>(
        &'a mut self,
        r: &'a Reader,
        node: &'a mut Node,
        key: &'a str,
        value: Option<Value>,
    ) -> BoxFuture<'a, Result<Option<(String, Node)>>> {
        Box::pin(async move {
            match &mut node.body {
                Body::Leaf(es) => match (es.binary_search_by(|(k, _)| k.as_str().cmp(key)), value) {
                    (Ok(i), Some(new)) => {
                        node.size = node.size + new.wire() - es[i].1.wire();
                        node.held = node.held + new.held() - es[i].1.held();
                        self.bytes = self.bytes + new.len() - es[i].1.len();
                        es[i].1 = new;
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
                    }
                    (Err(_), None) => {}
                },
                Body::Branch(kids) => {
                    let i = slot(kids, key);
                    let link = if let At::Clean(link) = &kids[i].at { Some(link.wire()) } else { None };
                    let below = dirty(r, &mut kids[i].at).await?;
                    node.size = node.size + link.map_or(0, |_| LINK_SIZE) - link.unwrap_or(0);
                    if let Some((least, right)) = self.edit(r, below, key, value).await? {
                        node.size += json_len(&least) + LINK_SIZE + 4;
                        kids.insert(i + 1, Child { key: least, at: At::Dirty(Box::new(right)) });
                    }
                }
            }
            if node.size <= r.config.node {
                return Ok(None);
            }
            let right = node.split_off()?;
            Ok(Some((right.first_key().to_owned(), right)))
        })
    }
}

/// An upload of what a tree holds, and of the nodes it changed.
struct Flush<'a> {
    reader: &'a Reader,
    permits: Semaphore,
    nodes: bool, // the nodes too, and not only the values
}

impl<'a> Flush<'a> {
    fn new(reader: &'a Reader, nodes: bool) -> Self {
        Self { reader, permits: Semaphore::new(IN_FLIGHT), nodes }
    }

    /// Uploads an object at a new id, and returns the link.
    async fn upload(&self, bytes: Bytes) -> Result<Link> {
        let (id, h) = (random(), hash(&bytes));
        let path = self.reader.path(&id);
        let _permit = self.permits.acquire().await.expect("the semaphore is never closed");
        let put = self.reader.store.put_opts(&path, bytes.clone().into(), PutMode::Overwrite.into()).await?;
        self.reader.puts.fetch_add(1, Relaxed);
        let link = Link { id, v: put.version, h, n: bytes.len() as u64 };
        link.check()?;
        Ok(link)
    }

    /// Uploads the values held in `node`, and, if `nodes`, the nodes below it that are in memory, so each is a link.
    /// None of them is empty, as [`fix`] dropped those. The node itself is for the caller to upload.
    fn node(&'a self, node: &'a mut Node) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match &mut node.body {
                Body::Leaf(es) => {
                    let held = es.iter_mut().filter(|(_, v)| matches!(v, Value::Held(_)));
                    try_join_all(held.map(|(_, value)| async move {
                        let Value::Held(bytes) = &*value else { return Ok::<_, Error>(()) };
                        let link = self.upload(bytes.clone()).await?;
                        self.reader.cache.values.insert(self.reader.key(&link), bytes.clone()); // for the next read
                        *value = Value::Object(link);
                        Ok(())
                    }))
                    .await?;
                }
                Body::Branch(kids) => {
                    try_join_all(kids.iter_mut().map(|kid| self.child(kid))).await?;
                }
            }
            node.measure();
            Ok(())
        })
    }

    async fn child(&self, kid: &mut Child) -> Result<()> {
        let At::Dirty(node) = &mut kid.at else { return Ok(()) };
        self.node(node).await?;
        if !self.nodes {
            return Ok(());
        }
        let (json, max) = (node.json(), self.reader.config.node);
        if json.len() > max {
            return Err(Error::TooLarge(format!("a node is over {max} bytes")));
        }
        let link = self.upload(json.into()).await?;
        let key = self.reader.key(&link);
        let At::Dirty(node) = std::mem::replace(&mut kid.at, At::Clean(link)) else { unreachable!("it is in memory") };
        self.reader.cache.nodes.insert(key, Arc::new(*node)); // for the next read
        Ok(())
    }
}

/// A tree, as one writer reads and edits it.
///
/// It is opened at a [`Root`], and a write changes it in memory, uploading early only what is over the budget.
/// [`Tree::finish`] uploads the rest and gives the new root, and [`Tree::reset`] goes back to the root it was opened
/// at. Reads take `&self`, and see the writes made so far.
pub struct Tree {
    reader: Reader,
    base: (Node, Work), // as opened: the root, and the totals
    root: Node,
    work: Work,
    poisoned: bool, // a change failed, or went away, half way, so the tree is not to be finished
    edited: bool,
}

impl Tree {
    /// The tree at `root`, whose objects are under `prefix` in `store`, and whose reads `cache` keeps. It reads
    /// nothing until it is used. A root whose node is malformed is [`Error::Corrupt`].
    pub fn open(
        store: Arc<dyn ObjectStore>,
        prefix: Path,
        cache: Arc<Cache>,
        root: &Root,
        config: Config,
    ) -> Result<Self> {
        let reader = Reader { store, prefix, cache, config, gets: 0.into(), puts: 0.into() };
        let node = Node::of(root.root.clone())?;
        let work = Work { bytes: root.bytes, entries: root.entries };
        let base = (node.clone(), work.clone());
        Ok(Self { reader, base, root: node, work, poisoned: false, edited: false })
    }

    /// The item at `key`.
    pub async fn get(&self, key: &str) -> Result<Option<Item>> {
        Ok(find(&self.reader, &self.root, key).await?.map(Item))
    }

    /// The value `item` holds, checked against its link if it is an object, and fetched once for the reads that come
    /// to it at once.
    pub async fn value(&self, item: &Item) -> Result<Bytes> {
        self.reader.value(&item.0).await
    }

    /// The first `limit` keys that start with `prefix` and follow `after`, if given, in order.
    pub async fn scan(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let mut keys = vec![];
        walk(&self.reader, &self.root, (prefix, after, limit), &mut keys).await?;
        Ok(keys)
    }

    /// Whether the tree was changed since it was opened or reset.
    pub fn edited(&self) -> bool {
        self.edited
    }

    /// Sets each key to its value, or removes it if the value is none, in order, all or none.
    ///
    /// A batch that would take the tree over [`Config::entries`] or [`Config::data`] is refused as [`Error::Full`],
    /// and one with a value over [`Config::blob`], or a key and value over a quarter of [`Config::node`], as
    /// [`Error::TooLarge`]: either changes nothing. A write that fails otherwise, or that is dropped half way, leaves
    /// the tree [`Error::Poisoned`].
    pub async fn write(&mut self, edits: Vec<(String, Option<Bytes>)>) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let config = self.reader.config;
        let edits: Vec<_> = edits.into_iter().map(|(k, v)| (k, v.map(|v| Value::of(v, config.inline)))).collect();
        for (key, value) in &edits {
            let Some(value) = value else { continue };
            if value.held() as u64 > config.blob {
                return Err(Error::TooLarge(format!("a value of {} bytes is over {}", value.len(), config.blob)));
            }
            if json_len(key) + 4 + value.wire().max(LINK_SIZE) > config.node / 4 {
                let (n, node) = (key.len(), config.node);
                return Err(Error::TooLarge(format!("a key of {n} bytes and its value are over a quarter of {node}")));
            }
        }
        let (adds, size) = edits
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .fold((0u64, 0u64), |(n, b), v| (n.saturating_add(1), b.saturating_add(v.len())));
        if self.work.entries.saturating_add(adds) > config.entries || self.work.bytes.saturating_add(size) > config.data
        {
            // Possibly over: count again, as the batch replaces and removes too.
            let (mut entries, mut bytes) = (i128::from(self.work.entries), i128::from(self.work.bytes));
            let mut seen: HashMap<&str, Option<u64>> = HashMap::new();
            for (key, value) in &edits {
                let was = match seen.get(key.as_str()) {
                    Some(was) => *was,
                    None => self.get(key).await?.map(|item| item.len()),
                };
                let now = value.as_ref().map(Value::len);
                entries += i128::from(now.is_some()) - i128::from(was.is_some());
                bytes += i128::from(now.unwrap_or(0)) - i128::from(was.unwrap_or(0));
                seen.insert(key, now);
            }
            if entries > i128::from(config.entries) {
                return Err(Error::Full(Full::Entries(config.entries)));
            }
            if bytes > i128::from(config.data) {
                return Err(Error::Full(Full::Data(config.data)));
            }
        }
        self.poisoned = true; // until it is applied, which a caller that goes away does not see
        let applied = self.apply(edits).await;
        self.poisoned = applied.is_err();
        applied
    }

    async fn apply(&mut self, edits: Vec<(String, Option<Value>)>) -> Result<()> {
        for (key, value) in edits {
            if value.is_none() && find(&self.reader, &self.root, &key).await?.is_none() {
                continue; // nothing to remove, so nothing to copy
            }
            self.edited = true;
            if let Some((least, right)) = self.work.edit(&self.reader, &mut self.root, &key, value).await? {
                let left = std::mem::replace(&mut self.root, Node::leaf());
                let first = Child { key: left.first_key().to_owned(), at: At::Dirty(Box::new(left)) };
                let second = Child { key: least, at: At::Dirty(Box::new(right)) };
                self.root = Node::measured(Body::Branch(vec![first, second]));
            }
        }
        let budget = self.reader.config.budget;
        if self.root.dirty() > budget {
            self.flush(false).await?; // the values first, which are the most
            if self.root.dirty() > budget {
                self.flush(true).await?;
            }
        }
        Ok(())
    }

    /// Uploads the values the tree holds, and, if `nodes`, the nodes it changed too, after it merges what is thin and
    /// drops what is empty, as at the commit, and gives up the levels of the root that have one child.
    async fn flush(&mut self, nodes: bool) -> Result<()> {
        let r = &self.reader;
        if nodes {
            fix(r, &mut self.root).await?;
            while let Body::Branch(kids) = &mut self.root.body {
                match kids.len() {
                    0 => self.root = Node::leaf(),
                    1 => {
                        let Some(Child { mut at, .. }) = kids.pop() else { break };
                        let only = dirty(r, &mut at).await?;
                        self.root = std::mem::replace(only, Node::leaf());
                    }
                    _ => break,
                }
            }
        }
        Flush::new(r, nodes).node(&mut self.root).await
    }

    /// Merges what is thin, uploads what changed, and gives the new root, for the caller to keep. The tree is then as
    /// that root is: if the caller's swap of it fails, [`Tree::reset`] goes back to the root the tree was opened at.
    pub async fn finish(&mut self) -> Result<Root> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.poisoned = true; // until it is done, as for a write
        let finished = self.seal().await;
        self.poisoned = finished.is_err();
        finished
    }

    async fn seal(&mut self) -> Result<Root> {
        self.flush(true).await?;
        let max = self.reader.config.node;
        if self.root.size > max {
            return Err(Error::TooLarge(format!("the root is over {max} bytes")));
        }
        let Work { bytes, entries } = self.work;
        Ok(Root { root: self.root.body.clone(), bytes, entries })
    }

    /// Goes back to the root the tree was opened at, as when the caller's swap of a finished root fails, or a write
    /// failed: what it uploaded since is left for the caller's collector. To go on from a root that was swapped in,
    /// open a tree at it.
    pub fn reset(&mut self) {
        (self.root, self.work) = self.base.clone();
        (self.poisoned, self.edited) = (false, false);
    }

    /// Adds to `seen` the ids of the objects the tree reaches below its root, as [`number`] gives them, its nodes and
    /// its values that are objects alike, and gives how many nodes it read, or `None` if that would be over `max`.
    ///
    /// The nodes are read level by level, [`IN_FLIGHT`] at once, each checked as any read is, and none is kept in the
    /// cache. An id already in `seen` is passed over with all below it, as what is below an object never changes, so
    /// one set passed to the marks of several roots has each object read once. A node that is missing or not what its
    /// link says is an error, so what is marked is of a tree that was whole. After `None` or an error, `seen` holds
    /// part of the tree, and is not to be used.
    pub async fn mark(&self, seen: &mut HashSet<u128>, max: usize) -> Result<Option<usize>> {
        let (mut next, mut nodes) = (vec![], 0);
        refs(&self.root, seen, &mut next)?;
        while !next.is_empty() {
            nodes += next.len();
            if nodes > max {
                return Ok(None);
            }
            let (r, links) = (&self.reader, std::mem::take(&mut next));
            let reads = stream::iter(links).map(|link| async move { r.load(&link, false).await });
            let mut reads = reads.buffer_unordered(IN_FLIGHT);
            while let Some(node) = reads.try_next().await? {
                refs(&node, seen, &mut next)?;
            }
        }
        Ok(Some(nodes))
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("prefix", &self.reader.prefix)
            .field("entries", &self.work.entries)
            .field("bytes", &self.work.bytes)
            .field("edited", &self.edited)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;

    /// A store, and the cache its trees share, as a process keeps them.
    #[derive(Clone)]
    struct Store {
        inner: Arc<dyn ObjectStore>,
        cache: Arc<Cache>,
    }

    fn memory() -> Store {
        Store { inner: Arc::new(InMemory::new()), cache: Arc::default() }
    }

    fn counting() -> (Store, Arc<testing::Counts>) {
        let (inner, counts) = testing::counting();
        (Store { inner, cache: Arc::default() }, counts)
    }

    fn prefix() -> Path {
        Path::from("t/n")
    }

    /// A config that makes a few keys a deep tree: a branch holds a few nodes, and a leaf as many values that are
    /// objects.
    fn tiny() -> Config {
        Config { node: 1400, inline: 8, budget: 2048, entries: 1000, data: 100_000, blob: 100 }
    }

    fn open(store: &Store, root: &Root, config: Config) -> Tree {
        Tree::open(store.inner.clone(), prefix(), store.cache.clone(), root, config).unwrap()
    }

    fn at(store: &Store, root: &Root) -> Tree {
        open(store, root, tiny())
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

    /// The ids of the objects `tree` reaches below its root: a mark from none, or `None` if it would read over `max`
    /// nodes.
    async fn reachable(tree: &Tree, max: usize) -> Result<Option<HashSet<u128>>> {
        let mut seen = HashSet::new();
        Ok(tree.mark(&mut seen, max).await?.map(|_| seen))
    }

    async fn set(tree: &mut Tree, key: &str, value: &[u8]) {
        tree.write(vec![(key.into(), Some(Bytes::copy_from_slice(value)))]).await.unwrap();
    }

    /// Finishes `tree`, as a writer whose swap lands does: the new root. Nothing is deleted.
    async fn commit(tree: &mut Tree) -> Root {
        tree.finish().await.unwrap()
    }

    /// What a tree holds, and that it is a well-formed one.
    #[derive(Default)]
    struct Audit {
        items: BTreeMap<String, Value>,
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
            let json = node.json();
            assert!(json.len() <= node.size, "a node is {} bytes, and was thought {}", json.len(), node.size);
            if level > 0 {
                assert!(json.len() <= r.config.node, "a node is {} bytes", json.len());
                assert!(!node.is_empty(), "an empty node");
            }
            match &node.body {
                Body::Leaf(es) => {
                    assert!(out.depth == 0 || out.depth == level + 1, "leaves are at different depths");
                    out.depth = level + 1;
                    assert!(es.windows(2).all(|w| w[0].0 < w[1].0), "keys are not in order");
                    for (k, value) in es {
                        assert!(
                            lo.is_none_or(|lo| k.as_str() >= lo) && hi.is_none_or(|hi| k.as_str() < hi),
                            "{k} strays"
                        );
                        assert!(!matches!(value, Value::Held(_)), "a value is still in memory");
                        if let Value::Object(link) = value {
                            assert!(out.ids.insert(link.id.clone()), "an object is named twice");
                        }
                        out.items.insert(k.clone(), value.clone());
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

    /// A reader of the objects under `prefix`, as a tree has one.
    fn reader(store: &Store, prefix: Path) -> Reader {
        let (store, cache) = (store.inner.clone(), store.cache.clone());
        Reader { store, prefix, cache, config: tiny(), gets: 0.into(), puts: 0.into() }
    }

    async fn audit(store: &Store, root: &Root) -> Audit {
        let node = Node::of(root.root.clone()).unwrap();
        let mut out = Audit::default();
        dig(&reader(store, prefix()), &node, 0, (None, None), &mut out).await;
        assert_eq!(out.items.len() as u64, root.entries);
        assert_eq!(out.items.values().map(Value::len).sum::<u64>(), root.bytes);
        out
    }

    /// The value at `key`, which is there.
    async fn text(tree: &Tree, key: &str) -> Vec<u8> {
        tree.value(&tree.get(key).await.unwrap().unwrap()).await.unwrap().to_vec()
    }

    /// The ids of the objects in the store.
    async fn stored(store: &Store) -> BTreeSet<String> {
        let all = store.inner.list(Some(&prefix()));
        all.map_ok(|m| m.location.filename().unwrap().to_owned()).try_collect().await.unwrap()
    }

    #[tokio::test]
    async fn matches_a_map() {
        let config = Config { entries: 5000, data: 1 << 20, ..tiny() };
        let (mut store, mut rng) = (memory(), Rng(0x5EED));
        let (mut model, mut root) = (BTreeMap::<String, Vec<u8>>::new(), Root::default());
        let mut before = BTreeSet::new(); // the objects stored after the round before
        let (mut deepest, mut gave_way) = (0, false);
        for round in 0..300 {
            if rng.below(2) == 0 {
                store.cache = Arc::default(); // a cold cache, as in another process
            }
            let mut tree = open(&store, &root, config);
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
                            edits.push((key, value.map(Bytes::from)));
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
            // It is finished and swapped in, or is finished and loses the race, or is thrown away as it is.
            let outcome = rng.below(10);
            let finished = if tree.edited() && outcome < 9 { Some(tree.finish().await.unwrap()) } else { None };
            if outcome < 7 {
                root = finished.unwrap_or(root);
                model = staged;
            } else {
                tree.reset();
                assert!(!tree.edited());
            }
            // It serves what is in the root it has now.
            for (k, v) in model.iter().take(20) {
                assert_eq!(text(&tree, k).await, v.as_slice(), "round {round}: {k}");
            }
            drop(tree);

            let audit = audit(&store, &root).await;
            gave_way |= audit.depth < deepest; // only a root that gives up a level makes a tree shallower
            deepest = deepest.max(audit.depth);
            assert_eq!(audit.items.keys().collect::<Vec<_>>(), model.keys().collect::<Vec<_>>(), "round {round}");
            let fresh = open(&store, &root, config);
            for (k, v) in &model {
                assert_eq!(text(&fresh, k).await, v.as_slice(), "{k}");
            }
            assert_eq!(fresh.scan("k/", None, usize::MAX).await.unwrap(), model.keys().cloned().collect::<Vec<_>>());
            assert_eq!(reachable(&fresh, usize::MAX).await.unwrap(), Some(numbers(&audit)), "round {round}");
            // Nothing was deleted, so nothing the root reaches is lost; what it does not reach is for a collector.
            let stored = stored(&store).await;
            let lost: Vec<_> = audit.ids.difference(&stored).collect();
            assert!(lost.is_empty(), "round {round}: {lost:?} are lost");
            assert!(before.is_subset(&stored), "round {round}: an object was deleted");
            before = stored;
        }
        assert!(deepest >= 4, "the tree was {deepest} deep");
        assert!(gave_way, "the root never gave up a level");
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
            for value in [Value::of(bytes.clone(), 16), Value::of(bytes, 0)] {
                assert_eq!(value.len(), n as u64);
                let wire = Value::Object(Link { id: "a".repeat(32), v: None, h: "b".repeat(64), n: 1 });
                assert!(serde_json::to_string(&wire).unwrap().len() <= wire.wire());
                if let Value::Inline(_) = value {
                    assert!(serde_json::to_string(&value).unwrap().len() <= value.wire());
                }
            }
        }
    }

    /// A tree of 400 keys, in the store, and its root.
    async fn grown(store: &Store) -> Root {
        let mut tree = at(store, &Root::default());
        for i in 0..400 {
            set(&mut tree, &format!("k/{i:03}"), &[i as u8; 5]).await;
        }
        commit(&mut tree).await
    }

    /// The ids that `audit` found, as the numbers `mark` gives.
    fn numbers(audit: &Audit) -> HashSet<u128> {
        audit.ids.iter().map(|id| number(id).unwrap()).collect()
    }

    #[tokio::test]
    async fn reachable_is_every_object_of_the_tree_read_once_and_kept_out_of_the_cache() {
        let (mut store, counts) = counting();
        let root = grown(&store).await;
        let audit = audit(&store, &root).await;
        let all = numbers(&audit);
        assert!(all.len() > 16, "{} nodes are too few to read at once", all.len());
        store.cache = Arc::default();
        let tree = at(&store, &root);

        // Every node below the root is read once, a few at once and no more than `IN_FLIGHT`, and none is kept.
        counts.gets.store(0, SeqCst);
        counts.delay.store(5, SeqCst);
        assert_eq!(reachable(&tree, usize::MAX).await.unwrap(), Some(all.clone()));
        assert_eq!(counts.gets.load(SeqCst), all.len());
        let peak = counts.peak.load(SeqCst);
        assert!((2..=IN_FLIGHT).contains(&peak), "{peak} at once");
        counts.delay.store(0, SeqCst);
        counts.gets.store(0, SeqCst);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), audit.depth - 1, "the walk left nothing in the cache");

        // Over `max` nodes it is not read at all past the level that went over, and says so.
        assert_eq!(reachable(&tree, all.len()).await.unwrap(), Some(all.clone()));
        counts.gets.store(0, SeqCst);
        assert_eq!(reachable(&tree, all.len() - 1).await.unwrap(), None);
        assert!(counts.gets.load(SeqCst) < all.len());
        assert_eq!(reachable(&tree, 0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reachable_fails_when_the_tree_is_not_whole() {
        for damage in ["missing", "changed", "emptied"] {
            let mut store = memory();
            let root = grown(&store).await;
            let audit = audit(&store, &root).await;
            // A node below the root, and one that is not a leaf if the tree has any: its loss costs the most.
            let id = audit.ids.iter().next().unwrap();
            let path = object(&prefix(), id);
            match damage {
                "missing" => store.inner.delete(&path).await.unwrap(),
                "changed" => drop(store.inner.put(&path, Bytes::from_static(b"{}").into()).await.unwrap()),
                _ => drop(store.inner.put(&path, Bytes::new().into()).await.unwrap()),
            }
            store.cache = Arc::default(); // a node in the cache was whole when it was put there
            let err = reachable(&at(&store, &root), usize::MAX).await.unwrap_err();
            assert!(matches!(err, Error::Corrupt(_)), "a {damage} node is no tree: {err}");
        }
    }

    #[tokio::test]
    async fn reachable_includes_values_that_are_objects() {
        let store = memory();
        let mut tree = at(&store, &Root::default());
        set(&mut tree, "small", b"abc").await;
        set(&mut tree, "big", &[7; 90]).await; // inline is 8, so this is an object of its own
        let root = commit(&mut tree).await;
        let audit = audit(&store, &root).await;
        assert_eq!(audit.ids.len(), 1, "a root of two keys has no node below it, and one value is an object");
        assert_eq!(reachable(&at(&store, &root), usize::MAX).await.unwrap(), Some(numbers(&audit)));
        assert_eq!(stored(&store).await, audit.ids, "the object is all there is");
    }

    #[tokio::test]
    async fn a_mark_shares_what_it_has_seen_across_roots() {
        let (mut store, counts) = counting();
        let first = grown(&store).await;
        let mut tree = at(&store, &first);
        set(&mut tree, "k/077", &[9; 90]).await; // an object of its own, which a mark does not read
        let second = commit(&mut tree).await;
        drop(tree);
        let (a, b) = (numbers(&audit(&store, &first).await), numbers(&audit(&store, &second).await));
        let new: HashSet<u128> = b.difference(&a).copied().collect();
        store.cache = Arc::default();
        counts.gets.store(0, SeqCst);

        // The second root shares most of the first's nodes, and those are not read again.
        let mut seen = HashSet::new();
        assert_eq!(at(&store, &first).mark(&mut seen, usize::MAX).await.unwrap(), Some(a.len()));
        assert_eq!(seen, a);
        assert_eq!(at(&store, &second).mark(&mut seen, usize::MAX).await.unwrap(), Some(new.len() - 1));
        assert_eq!(seen, a.union(&b).copied().collect());
        assert_eq!(counts.gets.load(SeqCst), a.len() + new.len() - 1, "each node was read once");
    }

    #[tokio::test]
    async fn costs_what_it_touches() {
        let (mut store, counts) = counting();
        let root = grown(&store).await;
        let levels = audit(&store, &root).await.depth;
        assert!(levels >= 3, "{levels} levels");
        let below = levels - 1; // the root is the caller's

        // A cold read is a GET for each node below the root; a warm one is none.
        store.cache = Arc::default();
        counts.gets.store(0, SeqCst);
        let tree = at(&store, &root);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), below);
        assert!(tree.get("k/077").await.unwrap().is_some());
        assert_eq!(counts.gets.load(SeqCst), below, "a node is read once");
        assert_eq!(tree.reader.gets.load(Relaxed) as usize, below, "the log counts what the store was asked");
        drop(tree);

        // A tree that wrote nothing writes nothing, and one that removes what is not there copies nothing.
        counts.puts.store(0, SeqCst);
        let mut tree = at(&store, &root);
        tree.write(vec![("k/zzz".into(), None)]).await.unwrap();
        assert!(!tree.edited());
        drop(tree);
        assert_eq!(counts.puts.load(SeqCst), 0);

        // One value changed is a copy of the nodes from it to the root, which the caller keeps.
        store.cache = Arc::default();
        counts.gets.store(0, SeqCst);
        let mut tree = at(&store, &root);
        set(&mut tree, "k/077", &[9; 5]).await;
        assert_eq!(counts.gets.load(SeqCst), below);
        let root = tree.finish().await.unwrap();
        assert_eq!(counts.puts.load(SeqCst), below);
        assert_eq!(tree.reader.puts.load(Relaxed) as usize, below);
        assert_eq!(audit(&store, &root).await.depth, levels);
        drop(tree);
        assert_eq!(counts.deletes.load(SeqCst), 0, "a tree deletes nothing, not even the nodes it replaced");
    }

    #[tokio::test]
    async fn shrinks_as_it_is_emptied() {
        let store = memory();
        let root = grown(&store).await;
        let full = audit(&store, &root).await;
        assert!(full.depth >= 3, "{} levels", full.depth);

        // Three keys of four go, from every leaf and none whole, so there are fewer nodes only if thin ones merged.
        let mut tree = at(&store, &root);
        tree.write((0..400).filter(|i| i % 4 > 0).map(|i| (format!("k/{i:03}"), None)).collect()).await.unwrap();
        let root = commit(&mut tree).await;
        let thinned = audit(&store, &root).await;
        assert_eq!(thinned.items.len(), 100);
        assert!(thinned.ids.len() < full.ids.len(), "{} nodes, of {}", thinned.ids.len(), full.ids.len());

        // All but one go, and the root gives up its levels for the leaf that holds it.
        tree.write((4..400).step_by(4).map(|i| (format!("k/{i:03}"), None)).collect()).await.unwrap();
        let root = commit(&mut tree).await;
        assert_eq!(audit(&store, &root).await.depth, 1);
        assert_eq!(text(&tree, "k/000").await, [0; 5]);
    }

    #[tokio::test]
    async fn a_failed_upload_loses_nothing() {
        let (store, counts) = counting();
        let root = grown(&store).await;
        let kept = audit(&store, &root).await.ids;
        assert!(stored(&store).await.is_superset(&kept));

        // A put fails after `allowed` more: at the first, the next, and so on, until the tree is finished.
        let mut failed = 0;
        for allowed in 0..200 {
            let mut tree = open(&store, &root, Config { budget: 1 << 20, ..tiny() });
            for i in 0..30 {
                set(&mut tree, &format!("k/{:03}", i * 13), &[1; 60]).await;
            }
            counts.allow.store(counts.puts.load(SeqCst) + allowed, SeqCst);
            let finished = tree.finish().await;
            if let Err(e) = &finished {
                failed += 1;
                assert!(matches!(e, Error::Store(_)), "{e}");
                assert!(matches!(tree.finish().await, Err(Error::Poisoned)), "a tree that failed is not finished");
                assert!(matches!(tree.write(vec![]).await, Err(Error::Poisoned)));
            }
            tree.reset();
            assert!(!tree.edited());
            assert_eq!(text(&tree, "k/013").await, [13; 5], "it is back at its root");
            counts.allow.store(usize::MAX, SeqCst);
            assert!(stored(&store).await.is_superset(&kept), "{allowed} uploads allowed");
            assert_eq!(audit(&store, &root).await.items.len(), 400);
            if let Ok(done) = finished {
                assert_eq!(text(&at(&store, &done), "k/013").await, [1; 60]);
                assert_eq!(audit(&store, &done).await.items.len(), 400);
                break;
            }
        }
        assert!(failed >= 30, "{failed} finishes failed");
        assert_eq!(counts.deletes.load(SeqCst), 0, "what a failed finish uploaded is left for a collector");
    }

    #[tokio::test]
    async fn spills_when_it_holds_too_much() {
        let (store, counts) = counting();
        let mut tree = at(&store, &Root::default());
        for i in 0..150 {
            set(&mut tree, &format!("k/{i:03}"), &[i as u8; 100]).await;
            assert!(tree.root.dirty() <= tiny().budget, "{} bytes held", tree.root.dirty());
        }
        assert!(counts.puts.load(SeqCst) > 0, "it spilled");
        assert_eq!(text(&tree, "k/000").await, [0; 100]);
        // Dropped, it deletes nothing: what it spilled is left for a collector.
        drop(tree);
        assert_eq!(stored(&store).await.len(), counts.puts.load(SeqCst));
        assert_eq!(counts.deletes.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn refuses_what_is_over_its_limits() {
        let store = memory();
        let mut tree = open(&store, &Root::default(), Config { blob: 1 << 10, ..tiny() });
        let edits: Vec<_> = (0..1001).map(|i| (format!("k/{i:04}"), Some(Bytes::from_static(b"v")))).collect();
        let full = tree.write(edits).await.unwrap_err();
        assert!(matches!(full, Error::Full(Full::Entries(1000))), "{full}");
        assert!(!tree.edited() && tree.root.is_empty(), "nothing was written");

        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(Bytes::from(vec![0; 120])))).collect();
        assert!(matches!(tree.write(edits).await, Err(Error::Full(Full::Data(100_000)))));
        assert!(!tree.edited());

        // A value over the most a value is, and a key too long for a node, are refused, and change nothing either.
        let big = vec![("k/a".into(), Some(Bytes::from_static(b"v"))), ("k/b".into(), Some(vec![0; 1025].into()))];
        assert!(matches!(tree.write(big).await, Err(Error::TooLarge(_))));
        let long = vec![("k/".repeat(200), Some(Bytes::from_static(b"v")))];
        assert!(matches!(tree.write(long).await, Err(Error::TooLarge(_))));
        assert!(!tree.edited());

        // What a batch replaces or removes does not count against it.
        for i in 0..900 {
            set(&mut tree, &format!("k/{i:04}"), &[0; 100]).await;
        }
        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(Bytes::from(vec![1; 100])))).collect();
        tree.write(edits).await.unwrap();
        let edits: Vec<_> = (0..900).map(|i| (format!("k/{i:04}"), Some(Bytes::from(vec![1; 50])))).collect();
        tree.write(edits).await.unwrap();
        set(&mut tree, "k/zzz", &[1; 100]).await;
        assert!(tree.finish().await.is_ok(), "it is not poisoned");
    }

    #[tokio::test]
    async fn is_not_fooled_by_a_changed_object() {
        let mut store = memory();
        let mut tree = at(&store, &Root::default());
        for i in 0..40 {
            set(&mut tree, &format!("k/{i:02}"), &[i as u8; 50]).await;
        }
        let root = commit(&mut tree).await;
        let seen = audit(&store, &root).await;

        let Body::Branch(kids) = &root.root else { panic!("a tree of one node") };
        let At::Clean(node) = &kids[0].at else { panic!("a node in memory") };
        let Some(Value::Object(blob)) = seen.items.values().next().cloned() else { panic!("no object") };
        for (link, what) in [(node.clone(), "a node"), (blob, "a value")] {
            let path = object(&prefix(), &link.id);
            let good = store.inner.get(&path).await.unwrap().bytes().await.unwrap();
            for bad in [vec![b' '; good.len()], good[1..].to_vec()] {
                store.inner.put(&path, bad.into()).await.unwrap();
                store.cache = Arc::default();
                let tree = at(&store, &root);
                let got = match tree.get("k/00").await {
                    Ok(Some(item)) => tree.value(&item).await.map(drop),
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                };
                let err = got.unwrap_err();
                assert!(
                    matches!(&err, Error::Corrupt(why) if why.contains("is not what its link says")),
                    "{what}: {err}"
                );
            }
            store.inner.put(&path, good.into()).await.unwrap();
        }
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
        let malformed = |e: &Error| matches!(e, Error::Corrupt(why) if why.contains("malformed"));
        for link in bads {
            assert!(link.check().is_err(), "{link:?}");
            let root = Root { root: Body::Branch(vec![(String::new(), link.clone()).into()]), ..Root::default() };
            let tree = at(&store, &root);
            let err = tree.get("k/a").await.unwrap_err();
            assert!(malformed(&err), "{err}");
            let err = tree.value(&Item(Value::Object(link))).await.unwrap_err();
            assert!(malformed(&err), "{err}");
        }
        assert_eq!(counts.gets.load(SeqCst), 0, "nothing was asked of the store");

        // The root's own words about a link are kept to as well.
        assert!(serde_json::from_str::<Link>(r#"{"id":"a","h":"b","n":1,"x":2}"#).is_err());
        assert!(serde_json::from_str::<Root>(r#"{"entries":1,"x":2}"#).is_err());
    }

    #[test]
    fn an_empty_tree_is_an_empty_root() {
        assert_eq!(serde_json::to_string(&Root::default()).unwrap(), "{}");
        assert!(Root::default().is_empty());
        let root: Root = serde_json::from_str("{}").unwrap();
        assert!(root.is_empty());
    }

    #[tokio::test]
    async fn lists_what_a_prefix_a_start_and_a_limit_pick_out() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Root::default());
        let mut model = BTreeMap::<String, Vec<u8>>::new();
        for prefix in ["a", "k", "k0", "z"] {
            for i in 0..120 {
                // A value sits in its node, or is an object of its own.
                let (key, value) = (format!("{prefix}/{i:03}"), format!("{prefix}/{i:03}").repeat(1 + i % 2));
                set(&mut tree, &key, value.as_bytes()).await;
                model.insert(key, value.into_bytes());
            }
        }
        let root = commit(&mut tree).await;
        drop(tree);
        let levels = audit(&store, &root).await.depth;
        assert!(levels >= 3, "{levels} levels");

        // A listing reads the nodes on its way to the first key, and no others.
        for (prefix, found) in [("k/", "k/000"), ("k/119", "k/119")] {
            store.cache = Arc::default();
            counts.gets.store(0, SeqCst);
            let tree = at(&store, &root);
            assert_eq!(tree.scan(prefix, None, 1).await.unwrap(), [found]);
            assert_eq!(counts.gets.load(SeqCst), levels - 1, "{prefix}");
        }

        let prefixes = ["", "a/", "k/", "k/1", "k/12", "k0/", "z/", "b", "zz", "k/119", "k/1190"];
        let afters =
            [None, Some(""), Some("a/050"), Some("k/"), Some("k/100"), Some("k/1005"), Some("k/999"), Some("zzz")];
        store.cache = Arc::default();
        let mut tree = at(&store, &root);
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
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn an_object_is_read_once_and_is_what_its_link_says_each_time() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Root::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        set(&mut tree, "k/b", &[2; 60]).await;
        let root = commit(&mut tree).await;
        drop(tree);

        // What a tree uploaded is kept for the next read, so a read of it is not a GET.
        counts.gets.store(0, SeqCst);
        assert_eq!(text(&at(&store, &root), "k/a").await, [1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 0);

        // What is not, is fetched once.
        store.cache = Arc::default();
        let tree = at(&store, &root);
        let item = tree.get("k/a").await.unwrap().unwrap();
        let Value::Object(link) = item.0.clone() else { panic!("a value in its node") };
        for _ in 0..3 {
            assert_eq!(tree.value(&item).await.unwrap().as_ref(), &[1; 60]);
        }
        assert_eq!(counts.gets.load(SeqCst), 1);

        // A link that names the same object, but another hash, is not answered from the cache: the store is asked, and
        // what it gives is not what that link says.
        let other = Link { h: hash(b"another"), ..link.clone() };
        let err = tree.value(&Item(Value::Object(other))).await.unwrap_err();
        assert!(matches!(&err, Error::Corrupt(why) if why.contains("is not what its link says")), "{err}");
        assert_eq!(counts.gets.load(SeqCst), 2);
        assert_eq!(tree.value(&item).await.unwrap().as_ref(), &[1; 60], "the cache has not been spoilt");
        assert_eq!(counts.gets.load(SeqCst), 2);

        // Nor is it answered for another prefix: that is another object, which is not there.
        for other in ["t/m", "u/n"] {
            let tree = Tree::open(store.inner.clone(), Path::from(other), store.cache.clone(), &root, tiny()).unwrap();
            let err = tree.value(&item).await.unwrap_err();
            assert!(matches!(&err, Error::Corrupt(why) if why.contains("is missing")), "{other}: {err}");
        }
        assert_eq!(counts.gets.load(SeqCst), 4);
    }

    #[tokio::test]
    async fn a_large_value_that_is_being_fetched_is_waited_for_by_the_reads_that_come_to_it() {
        let (mut store, counts) = counting();
        let mut tree = at(&store, &Root::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        let root = commit(&mut tree).await;
        drop(tree);
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &root));
        let item = tree.get("k/a").await.unwrap().unwrap();
        let read = |tree: &Arc<Tree>| {
            let (tree, item) = (tree.clone(), item.clone());
            tokio::spawn(async move { tree.value(&item).await })
        };
        counts.gets.store(0, SeqCst);
        counts.delay.store(50, SeqCst);

        // Eight reads at once are one fetch, and each has the value, which the cache then keeps.
        let reads: Vec<_> = (0..8).map(|_| read(&tree)).collect();
        for read in futures_util::future::join_all(reads).await {
            assert_eq!(read.unwrap().unwrap().as_ref(), &[1; 60]);
        }
        assert_eq!(counts.gets.load(SeqCst), 1);
        assert_eq!(tree.value(&item).await.unwrap().as_ref(), &[1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 1);

        // A read that is cancelled while it waits fetches nothing, and the one that fetches has the value to itself.
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &root));
        counts.gets.store(0, SeqCst);
        counts.delay.store(200, SeqCst);
        let leader = read(&tree);
        while counts.gets.load(SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let waiter = read(&tree);
        tokio::time::sleep(Duration::from_millis(20)).await; // it is waiting for the fetch
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(leader.await.unwrap().unwrap().as_ref(), &[1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 1, "the waiter that was cancelled fetched nothing");

        // The one that is fetching is cancelled: the next in turn fetches, and none of them is left waiting.
        store.cache = Arc::default();
        let tree = Arc::new(at(&store, &root));
        counts.gets.store(0, SeqCst);
        let first = read(&tree);
        while counts.gets.load(SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let second = read(&tree);
        tokio::time::sleep(Duration::from_millis(20)).await; // it is waiting for the fetch
        first.abort();
        assert_eq!(second.await.unwrap().unwrap().as_ref(), &[1; 60]);
        assert_eq!(counts.gets.load(SeqCst), 2, "the fetch that was cancelled, and the one that took its place");
    }

    #[tokio::test]
    async fn reads_that_are_cancelled_at_any_moment_of_a_fetch_leave_none_waiting() {
        let (store, counts) = counting();
        let mut tree = at(&store, &Root::default());
        set(&mut tree, "k/a", &[1; 60]).await;
        let root = commit(&mut tree).await;
        drop(tree);
        // That includes when the one that fetches lets go and wakes the next in turn: whichever way the drops of the
        // cancelled read and of what it waits on fall, the reads that are not cancelled have the value, and so does
        // one after them.
        for round in 0..4 {
            let store = Store { cache: Arc::default(), ..store.clone() };
            let tree = Arc::new(at(&store, &root));
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
            let rest = async {
                for read in reads {
                    if let Ok(got) = read.await {
                        assert_eq!(got.unwrap().as_ref(), &[1; 60]);
                    }
                }
                tree.value(&item).await.unwrap()
            };
            let last = tokio::time::timeout(Duration::from_secs(10), rest).await;
            assert_eq!(last.expect("a read was left waiting").as_ref(), &[1; 60], "round {round}");
        }
    }

    #[test]
    fn a_cache_entry_is_for_one_path_version_and_hash() {
        let store = memory();
        let link = Link { id: "a".repeat(32), v: Some("v1".into()), h: "h1".into(), n: 3 };
        let key = |prefix: &str, link: &Link| reader(&store, Path::from(prefix)).key(link);
        store.cache.values.insert(key("t/n", &link), Bytes::from_static(b"abc"));
        assert_eq!(store.cache.values.get(&key("t/n", &link)), Some(Bytes::from_static(b"abc")));
        for (prefix, other, what) in [
            ("t/n", Link { v: Some("v2".into()), ..link.clone() }, "another version"),
            ("t/n", Link { v: None, ..link.clone() }, "no version"),
            ("t/m", link.clone(), "another prefix"),
            ("u/n", link.clone(), "another prefix that ends the same"),
            ("t/n", Link { id: "b".repeat(32), ..link.clone() }, "another id"),
            ("t/n", Link { h: "h2".into(), ..link.clone() }, "another hash"),
        ] {
            assert_eq!(store.cache.values.get(&key(prefix, &other)), None, "{what}");
        }
    }

    #[test]
    fn a_cache_keeps_to_its_bounds() {
        let cache = Cache::new(1000, 1000);
        let key = |i: usize| (Path::from(format!("t/n/{i}")), None, String::new());
        for i in 0..100 {
            cache.values.insert(key(i), Bytes::from(vec![0; 50]));
        }
        assert!(cache.values.weight() <= 1000, "{} bytes", cache.values.weight());
        // One pool, so an object of most of it is kept, and the rest give way.
        cache.values.insert(key(100), Bytes::from(vec![0; 900]));
        assert!(cache.values.get(&key(100)).is_some());
        assert!(cache.values.weight() <= 1000, "{} bytes", cache.values.weight());
        let node = Arc::new(Node::of(Body::Leaf(vec![])).unwrap());
        for i in 0..100 {
            cache.nodes.insert(key(i), node.clone());
        }
        assert!(cache.nodes.weight() <= 1000 && cache.nodes.weight() >= node.size as u64);
    }
}
