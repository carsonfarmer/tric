//! The file system on a name's tree. Everything is a key: an inode is `f/<ino>`, as JSON; the block `index` of a file's
//! data is `f/<ino>/<index>`, in 16 hex digits; a directory entry is `d/<dir>/<name>`, as JSON. Inode numbers are
//! decimal, count up from the root's, 1, and are not reused. A block that is not there is a hole, and reads as zeros,
//! as does the rest of a block stored shorter than `BLOCK`; no block holds bytes past the end of its file.
//!
//! Each operation is one batch of edits, which the tree applies all or none, except a write of more than `WRITE_MAX`
//! bytes, which is one for each `WRITE_MAX`, and the deletes of a file's blocks, which are one for each `PAGE`.
//!
//! A path never leaves the directory it is resolved from: `..` stops at it, and so do symlinks, which may not be
//! absolute.
use crate::tree::{DATA_MAX, Full, IN_FLIGHT, Item, Tree};
use bytes::{Bytes, BytesMut};
use futures_util::stream::{self, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque, hash_map::Entry};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// The root directory's inode.
pub const ROOT: u64 = 1;
/// The bytes of a file one key holds, at most.
pub const BLOCK: u64 = 256 << 10;
/// The most bytes a file is: all a name may hold.
pub const FILE_MAX: u64 = DATA_MAX;
const NAME_MAX: usize = 255;
const PATH_MAX: usize = 4096;
const TARGET_MAX: usize = 1024;
/// The symlinks one path follows, at most, before it is a loop.
const FOLLOW_MAX: usize = 40;
/// The entries of a directory, or the blocks of a file, in one batch or one read of the tree.
pub const PAGE: usize = 1000;
/// The bytes one batch of a write holds, which the tree keeps in memory until they are uploaded.
const WRITE_MAX: usize = 4 << 20;

/// Why an operation failed, as the file system's error codes name it. What the host knows of a failure is logged, and
/// the guest is told one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    BadDescriptor,
    Busy,
    Exist,
    FileTooLarge,
    Invalid,
    Io,
    IsDirectory,
    Loop,
    NameTooLong,
    NoEntry,
    InsufficientMemory,
    InsufficientSpace,
    NotDirectory,
    NotEmpty,
    Overflow,
    Unsupported,
    NotPermitted,
    ReadOnly,
    TooManyLinks,
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Self::BadDescriptor => "bad descriptor",
            Self::Busy => "busy",
            Self::Exist => "exists",
            Self::FileTooLarge => "file too large",
            Self::Invalid => "invalid argument",
            Self::Io => "i/o error",
            Self::IsDirectory => "is a directory",
            Self::Loop => "too many levels of symbolic links",
            Self::NameTooLong => "name too long",
            Self::NoEntry => "no such file or directory",
            Self::InsufficientMemory => "out of memory",
            Self::InsufficientSpace => "no space left",
            Self::NotDirectory => "not a directory",
            Self::NotEmpty => "directory not empty",
            Self::Overflow => "value too large",
            Self::Unsupported => "unsupported",
            Self::NotPermitted => "not permitted",
            Self::ReadOnly => "read-only",
            Self::TooManyLinks => "too many links",
        })
    }
}

impl std::error::Error for Errno {}

pub type Res<T> = Result<T, Errno>;

/// What the guest is told of a tree error: that the name is full, which it can act on, and else only that the i/o
/// failed, as the rest is the host's to know.
pub fn fail(e: wasmtime::Error) -> Errno {
    if e.downcast_ref::<Full>().is_some() {
        return Errno::InsufficientSpace;
    }
    tracing::warn!("name: {e:#}");
    Errno::Io
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    File,
    Link,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// An inode. Times are Unix nanoseconds; `atime` is set, never by a read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inode {
    k: Kind,
    /// The links to it: the entries that name it, which for a directory is 1. A file that is open and unlinked is at 0
    /// until it is closed.
    n: u32,
    /// The size of a file, or the length of a symlink's target.
    #[serde(default, skip_serializing_if = "is_zero")]
    s: u64,
    a: u64,
    m: u64,
    c: u64,
    /// A symlink's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t: Option<String>,
    /// A directory's parent, which a rename checks against moving a directory into itself.
    #[serde(default, skip_serializing_if = "is_zero")]
    p: u64,
}

impl Inode {
    /// The root, which is not in the tree until something changes it.
    fn root() -> Self {
        Self { k: Kind::Dir, n: 1, s: 0, a: 0, m: 0, c: 0, t: None, p: 0 }
    }
}

/// A directory entry: the inode it names, and what that is, so a listing need not read each.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dirent {
    i: u64,
    k: Kind,
}

impl Dirent {
    pub fn ino(&self) -> u64 {
        self.i
    }

    pub fn kind(&self) -> Kind {
        self.k
    }
}

/// What an inode says of itself.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub kind: Kind,
    pub links: u64,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

/// A time to set.
#[derive(Clone, Copy, Debug)]
pub enum Time {
    Keep,
    Now,
    At(u64),
}

impl Time {
    /// The time `seconds` and `nanoseconds` after the epoch, if the tree can hold it.
    pub fn at(seconds: u64, nanoseconds: u32) -> Res<Self> {
        if nanoseconds >= 1_000_000_000 {
            return Err(Errno::Invalid);
        }
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|n| n.checked_add(nanoseconds.into()))
            .map(Self::At)
            .ok_or(Errno::Overflow)
    }
}

/// Where a write goes.
#[derive(Clone, Copy, Debug)]
pub enum At {
    Offset(u64),
    End,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
}

fn inode_key(ino: u64) -> String {
    format!("f/{ino}")
}

fn block_key(ino: u64, index: u64) -> String {
    format!("f/{ino}/{index:016x}")
}

fn blocks_prefix(ino: u64) -> String {
    format!("f/{ino}/")
}

fn dirent_key(dir: u64, name: &str) -> String {
    format!("d/{dir}/{name}")
}

fn dir_prefix(dir: u64) -> String {
    format!("d/{dir}/")
}

/// The JSON at `key`, if there is any.
async fn load<T: DeserializeOwned>(tree: &Tree, key: &str) -> Res<Option<T>> {
    let Some(item) = tree.get(key).await.map_err(fail)? else { return Ok(None) };
    let bytes = tree.value(&item).await.map_err(fail)?;
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        tracing::warn!("name: {key}: {e}");
        Errno::Io
    })
}

fn encode<T: Serialize>(tree: &Tree, value: &T) -> Res<Item> {
    let json = serde_json::to_vec(value).map_err(|e| fail(e.into()))?;
    Ok(tree.item(json.into()))
}

/// The inode `ino`. One that a name or a descriptor leads to and that is not there is the host's to explain: the
/// turn was discarded, or the tree is damaged.
pub async fn fetch(tree: &Tree, ino: u64) -> Res<Inode> {
    match load(tree, &inode_key(ino)).await? {
        Some(inode) => Ok(inode),
        None if ino == ROOT => Ok(Inode::root()),
        None => {
            tracing::warn!("name: inode {ino} is missing");
            Err(Errno::Io)
        }
    }
}

pub async fn stat(tree: &Tree, ino: u64) -> Res<Stat> {
    let i = fetch(tree, ino).await?;
    Ok(Stat { kind: i.k, links: u64::from(i.n), size: i.s, atime: i.a, mtime: i.m, ctime: i.c })
}

async fn dirent(tree: &Tree, dir: u64, name: &str) -> Res<Option<Dirent>> {
    load(tree, &dirent_key(dir, name)).await
}

/// What a path leads to.
#[derive(Debug)]
pub enum Loc {
    /// A directory the path ends at by `.` or `..`, or that it started at.
    Dir(u64),
    /// An entry of `parent`, if there is one at `at`. `slash` is whether the path ended in one, so that it asks for
    /// a directory.
    Entry { parent: u64, name: String, at: Option<Dirent>, slash: bool },
}

/// Resolves `path` from the directory `start`, which it never leaves: a `..` above it, and an absolute path or
/// symlink, are `NotPermitted`. A symlink is followed in the middle of a path, and at the end if `follow` is set or the
/// path ends in a slash.
pub async fn locate(tree: &Tree, start: u64, path: &str, follow: bool) -> Res<Loc> {
    if path.is_empty() {
        return Err(Errno::NoEntry);
    }
    if path.len() > PATH_MAX {
        return Err(Errno::NameTooLong);
    }
    if path.contains('\0') {
        return Err(Errno::Invalid);
    }
    if path.starts_with('/') {
        return Err(Errno::NotPermitted);
    }
    let parts = |s: &str| s.split('/').filter(|c| !c.is_empty()).map(str::to_owned).collect::<VecDeque<_>>();
    let (mut dirs, mut todo, mut slash, mut follows) = (vec![start], parts(path), path.ends_with('/'), 0);
    while let Some(part) = todo.pop_front() {
        let last = todo.is_empty();
        let top = *dirs.last().unwrap_or(&start);
        match part.as_str() {
            "." => {}
            ".." => {
                if dirs.len() == 1 {
                    return Err(Errno::NotPermitted);
                }
                dirs.pop();
            }
            name if name.len() > NAME_MAX => return Err(Errno::NameTooLong),
            name => {
                let at = dirent(tree, top, name).await?;
                match at {
                    Some(Dirent { k: Kind::Link, i }) if !last || follow || slash => {
                        follows += 1;
                        if follows > FOLLOW_MAX {
                            return Err(Errno::Loop);
                        }
                        let target = fetch(tree, i).await?.t.unwrap_or_default();
                        if target.starts_with('/') {
                            return Err(Errno::NotPermitted);
                        }
                        if target.is_empty() {
                            return Err(Errno::NoEntry);
                        }
                        slash |= last && target.ends_with('/');
                        for part in parts(&target).into_iter().rev() {
                            todo.push_front(part);
                        }
                    }
                    _ if last => return Ok(Loc::Entry { parent: top, name: part, at, slash }),
                    Some(Dirent { k: Kind::Dir, i }) => dirs.push(i),
                    Some(_) => return Err(Errno::NotDirectory),
                    None => return Err(Errno::NoEntry),
                }
                continue;
            }
        }
        if last {
            return Ok(Loc::Dir(*dirs.last().unwrap_or(&start)));
        }
    }
    Err(Errno::NoEntry) // not reached: a path has a part, and a symlink's target has one
}

/// The inode and kind `path` leads to, which must be there.
pub async fn lookup(tree: &Tree, start: u64, path: &str, follow: bool) -> Res<(u64, Kind)> {
    match locate(tree, start, path, follow).await? {
        Loc::Dir(ino) => Ok((ino, Kind::Dir)),
        Loc::Entry { at: None, .. } => Err(Errno::NoEntry),
        Loc::Entry { at: Some(d), slash, .. } if slash && d.k != Kind::Dir => Err(Errno::NotDirectory),
        Loc::Entry { at: Some(d), .. } => Ok((d.i, d.k)),
    }
}

/// An entry that a path leads to, or may lead to; `dir` is the error for a path that ends in a directory itself.
pub async fn entry(
    tree: &Tree,
    start: u64,
    path: &str,
    follow: bool,
    dir: Errno,
) -> Res<(u64, String, Option<Dirent>, bool)> {
    match locate(tree, start, path, follow).await? {
        Loc::Dir(_) => Err(dir),
        Loc::Entry { parent, name, at, slash } => Ok((parent, name, at, slash)),
    }
}

/// A batch of changes to the tree: the inodes it reads and changes are held until it is applied, with everything else
/// it writes, in one write of the tree.
struct Tx<'a> {
    tree: &'a mut Tree,
    now: u64,
    inodes: HashMap<u64, Slot>,
    edits: BTreeMap<String, Option<Item>>,
}

struct Slot {
    inode: Option<Inode>,
    dirty: bool,
}

impl<'a> Tx<'a> {
    fn new(tree: &'a mut Tree) -> Self {
        Self { tree, now: now(), inodes: HashMap::new(), edits: BTreeMap::new() }
    }

    async fn slot(&mut self, ino: u64) -> Res<&mut Slot> {
        Ok(match self.inodes.entry(ino) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => {
                let inode = load(self.tree, &inode_key(ino)).await?.or_else(|| (ino == ROOT).then(Inode::root));
                v.insert(Slot { inode, dirty: false })
            }
        })
    }

    /// The inode `ino`, to change.
    async fn edit(&mut self, ino: u64) -> Res<&mut Inode> {
        let slot = self.slot(ino).await?;
        slot.dirty = true;
        slot.inode.as_mut().ok_or_else(|| {
            tracing::warn!("name: inode {ino} is missing");
            Errno::Io
        })
    }

    /// The inode `ino`, to read.
    async fn get(&mut self, ino: u64) -> Res<Inode> {
        let slot = self.slot(ino).await?;
        slot.inode.clone().ok_or_else(|| {
            tracing::warn!("name: inode {ino} is missing");
            Errno::Io
        })
    }

    fn add(&mut self, ino: u64, inode: Inode) {
        self.inodes.insert(ino, Slot { inode: Some(inode), dirty: true });
    }

    /// Sets the change and modification times of the directory `ino` to now.
    async fn touch(&mut self, ino: u64) -> Res<()> {
        let now = self.now;
        let dir = self.edit(ino).await?;
        (dir.m, dir.c) = (now, now);
        Ok(())
    }

    /// Takes a link from `ino`; it is the inode of an entry that was removed. Returns it if that was its last, when it
    /// is for the caller to remove, now or once it is closed.
    async fn unlink(&mut self, ino: u64) -> Res<Option<u64>> {
        let now = self.now;
        let inode = self.edit(ino).await?;
        inode.n = if inode.k == Kind::Dir { 0 } else { inode.n.saturating_sub(1) };
        inode.c = now;
        Ok((inode.n == 0).then_some(ino))
    }

    fn put(&mut self, key: String, item: Item) {
        self.edits.insert(key, Some(item));
    }

    fn put_dirent(&mut self, dir: u64, name: &str, at: Dirent) -> Res<()> {
        let item = encode(self.tree, &at)?;
        self.put(dirent_key(dir, name), item);
        Ok(())
    }

    fn remove_dirent(&mut self, dir: u64, name: &str) {
        self.edits.insert(dirent_key(dir, name), None);
    }

    /// Writes it all.
    async fn finish(self) -> Res<()> {
        let Self { tree, inodes, mut edits, .. } = self;
        for (ino, slot) in inodes.into_iter().filter(|(_, s)| s.dirty) {
            let item = slot.inode.as_ref().map(|i| encode(tree, i)).transpose()?;
            edits.insert(inode_key(ino), item);
        }
        if edits.is_empty() {
            return Ok(());
        }
        tree.write(edits.into_iter().collect()).await.map_err(fail)
    }
}

fn check_name(name: &str) -> Res<()> {
    match name {
        "" | "." | ".." => Err(Errno::Invalid),
        n if n.len() > NAME_MAX => Err(Errno::NameTooLong),
        n if n.contains(['/', '\0']) => Err(Errno::Invalid),
        _ => Ok(()),
    }
}

/// A new file, directory or symlink (with its `target`) named `name` in the directory `parent`, which must not have
/// one. Returns its inode.
pub async fn create(tree: &mut Tree, parent: u64, name: &str, kind: Kind, target: Option<&str>) -> Res<u64> {
    check_name(name)?;
    let dir = fetch(tree, parent).await?;
    if dir.k != Kind::Dir {
        return Err(Errno::NotDirectory);
    }
    if dir.n == 0 {
        return Err(Errno::NoEntry); // it was removed while open
    }
    if dirent(tree, parent, name).await?.is_some() {
        return Err(Errno::Exist);
    }
    let ino = tree.alloc();
    let mut tx = Tx::new(tree);
    let now = tx.now;
    let mut inode = Inode { k: kind, n: 1, s: 0, a: now, m: now, c: now, t: None, p: 0 };
    match (kind, target) {
        (Kind::Dir, _) => inode.p = parent,
        (Kind::Link, Some(t)) => (inode.s, inode.t) = (t.len() as u64, Some(t.to_owned())),
        _ => {}
    }
    tx.add(ino, inode);
    tx.put_dirent(parent, name, Dirent { i: ino, k: kind })?;
    tx.touch(parent).await?;
    tx.finish().await?;
    Ok(ino)
}

/// A symlink to `target`, named `name` in `parent`. The target is relative, as an absolute path leads out of the
/// directory the guest was given (the WIT says `not-permitted`): `locate` refuses one it finds all the same.
pub async fn symlink(tree: &mut Tree, parent: u64, name: &str, target: &str) -> Res<u64> {
    if target.len() > TARGET_MAX {
        return Err(Errno::NameTooLong);
    }
    if target.is_empty() {
        return Err(Errno::NoEntry);
    }
    if target.starts_with('/') {
        return Err(Errno::NotPermitted);
    }
    create(tree, parent, name, Kind::Link, Some(target)).await
}

pub async fn readlink(tree: &Tree, ino: u64) -> Res<String> {
    let i = fetch(tree, ino).await?;
    match i.k {
        Kind::Link => Ok(i.t.unwrap_or_default()),
        _ => Err(Errno::Invalid),
    }
}

/// The bytes of block `index` of `ino`, as stored.
async fn block(tree: &Tree, ino: u64, index: u64) -> Res<Option<Bytes>> {
    let Some(item) = tree.get(&block_key(ino, index)).await.map_err(fail)? else { return Ok(None) };
    Ok(Some(tree.value(&item).await.map_err(fail)?))
}

/// The stored blocks of the file `ino` that `offset..end` is in, in order, and `None` for those that are not stored.
/// The links are looked up one by one, which costs the nodes only once, and the blocks are fetched [`IN_FLIGHT`] at a
/// time, and checked against their links, as `tree.value` does.
async fn blocks<'a>(
    tree: &'a Tree,
    ino: u64,
    offset: u64,
    end: u64,
) -> Res<impl stream::Stream<Item = Res<Option<Bytes>>> + 'a> {
    let mut links = Vec::new();
    for index in offset / BLOCK..=(end - 1) / BLOCK {
        links.push(tree.get(&block_key(ino, index)).await.map_err(fail)?);
    }
    let value = move |item: Option<Item>| async move {
        match item {
            Some(item) => Ok(Some(tree.value(&item).await.map_err(fail)?)),
            None => Ok(None),
        }
    };
    Ok(stream::iter(links).map(value).buffered(IN_FLIGHT))
}

/// The size of the file `ino`, and where a read of `len` bytes from `offset` ends: at its end, if that is first.
async fn extent(tree: &Tree, ino: u64, offset: u64, len: u64) -> Res<u64> {
    let i = fetch(tree, ino).await?;
    match i.k {
        Kind::Dir => return Err(Errno::IsDirectory),
        Kind::Link => return Err(Errno::BadDescriptor),
        Kind::File => {}
    }
    Ok(offset.saturating_add(len).min(i.s))
}

/// Up to `len` bytes of the file `ino` from `offset`: fewer at its end, and none past it. The blocks are fetched
/// together, up to [`IN_FLIGHT`] of them, and put in order.
pub async fn read_at(tree: &Tree, ino: u64, offset: u64, len: u64) -> Res<Bytes> {
    let end = extent(tree, ino, offset, len).await?;
    if offset >= end {
        return Ok(Bytes::new());
    }
    let mut out = BytesMut::with_capacity((end - offset) as usize);
    let mut index = offset / BLOCK;
    let mut blocks = std::pin::pin!(blocks(tree, ino, offset, end).await?);
    while let Some(stored) = blocks.next().await {
        let stored = stored?.unwrap_or_default();
        let start = index * BLOCK;
        let (from, to) = ((offset.max(start) - start) as usize, (end.min(start + BLOCK) - start) as usize);
        out.extend_from_slice(stored.get(from.min(stored.len())..to.min(stored.len())).unwrap_or_default());
        out.resize(out.len() + to.saturating_sub(stored.len().max(from)), 0); // what is not stored reads as zeros
        index += 1;
    }
    Ok(out.freeze())
}

/// Fetches the blocks of the file `ino` that a read of `len` bytes from `offset` would, for the cache, and lets them
/// go. They are fetched, and checked, as a read does it.
pub async fn warm(tree: &Tree, ino: u64, offset: u64, len: u64) -> Res<()> {
    let end = extent(tree, ino, offset, len).await?;
    if offset < end {
        let mut blocks = std::pin::pin!(blocks(tree, ino, offset, end).await?);
        while let Some(stored) = blocks.next().await {
            stored?;
        }
    }
    Ok(())
}

/// Writes `data` to the file `ino`, at `at`, and returns how much: all of it, or the part that fit if the name filled.
pub async fn write_at(tree: &mut Tree, ino: u64, mut at: At, data: &[u8]) -> Res<usize> {
    let mut done = 0;
    while done < data.len() {
        let chunk = &data[done..data.len().min(done + WRITE_MAX)];
        match write_chunk(tree, ino, at, chunk).await {
            Ok(end) => {
                done += chunk.len();
                at = At::Offset(end);
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break, // a short write, as the disk filling is
        }
    }
    Ok(done)
}

/// Writes `data` to the file `ino` in one batch, and returns where it ends.
async fn write_chunk(tree: &mut Tree, ino: u64, at: At, data: &[u8]) -> Res<u64> {
    let mut tx = Tx::new(tree);
    let size = {
        let i = tx.get(ino).await?;
        match i.k {
            Kind::Dir => return Err(Errno::IsDirectory),
            Kind::Link => return Err(Errno::BadDescriptor),
            Kind::File => i.s,
        }
    };
    let start = match at {
        At::Offset(o) => o,
        At::End => size,
    };
    let end = start.checked_add(data.len() as u64).filter(|e| *e <= FILE_MAX).ok_or(Errno::FileTooLarge)?;
    for index in start / BLOCK..=(end - 1) / BLOCK {
        let base = index * BLOCK;
        let (from, to) = ((start.max(base) - base) as usize, (end.min(base + BLOCK) - base) as usize);
        let piece = &data[(start.max(base) - start) as usize..(end.min(base + BLOCK) - start) as usize];
        let bytes = if piece.len() as u64 == BLOCK {
            Bytes::copy_from_slice(piece)
        } else {
            let stored = block(tx.tree, ino, index).await?.unwrap_or_default();
            let mut buf = BytesMut::zeroed(stored.len().max(to));
            buf[..stored.len()].copy_from_slice(&stored);
            buf[from..to].copy_from_slice(piece);
            buf.freeze()
        };
        let item = tx.tree.item(bytes);
        tx.put(block_key(ino, index), item);
    }
    let now = tx.now;
    let inode = tx.edit(ino).await?;
    (inode.s, inode.m, inode.c) = (inode.s.max(end), now, now);
    tx.finish().await?;
    Ok(end)
}

/// Sets the size of the file `ino`: a bigger one reads zeros where it was not written, and a smaller one drops what is
/// past it.
pub async fn set_size(tree: &mut Tree, ino: u64, size: u64) -> Res<()> {
    if size > FILE_MAX {
        return Err(Errno::FileTooLarge);
    }
    let mut tx = Tx::new(tree);
    let was = {
        let i = tx.get(ino).await?;
        match i.k {
            Kind::Dir => return Err(Errno::IsDirectory),
            Kind::Link => return Err(Errno::BadDescriptor),
            Kind::File => i.s,
        }
    };
    let now = tx.now;
    let inode = tx.edit(ino).await?;
    (inode.s, inode.m, inode.c) = (size, now, now);
    let keep = size.div_ceil(BLOCK); // the blocks that hold any of it
    if size < was && !size.is_multiple_of(BLOCK) {
        let last = keep - 1;
        if let Some(stored) = block(tx.tree, ino, last).await?.filter(|b| b.len() as u64 > size % BLOCK) {
            let item = tx.tree.item(stored.slice(..(size % BLOCK) as usize));
            tx.put(block_key(ino, last), item);
        }
    }
    tx.finish().await?;
    if size < was {
        drop_blocks(tree, ino, keep).await?;
    }
    Ok(())
}

/// Deletes the blocks of `ino` from `index` on, a page at a time.
async fn drop_blocks(tree: &mut Tree, ino: u64, index: u64) -> Res<()> {
    let prefix = blocks_prefix(ino);
    let after = index.checked_sub(1).map(|i| block_key(ino, i));
    loop {
        let keys = tree.scan(&prefix, after.as_deref(), PAGE).await.map_err(fail)?;
        if keys.is_empty() {
            return Ok(());
        }
        tree.write(keys.into_iter().map(|k| (k, None)).collect()).await.map_err(fail)?;
    }
}

/// Removes the inode `ino`, which has no links, with its blocks.
pub async fn purge(tree: &mut Tree, ino: u64) -> Res<()> {
    match load::<Inode>(tree, &inode_key(ino)).await? {
        Some(i) if i.n == 0 => {}
        _ => return Ok(()), // it is gone, or was linked again
    }
    drop_blocks(tree, ino, 0).await?;
    tree.write(vec![(inode_key(ino), None)]).await.map_err(fail)
}

/// Removes the file or symlink named `name` in `parent`. Returns its inode if that was its last link.
pub async fn unlink(tree: &mut Tree, parent: u64, name: &str) -> Res<Option<u64>> {
    let at = dirent(tree, parent, name).await?.ok_or(Errno::NoEntry)?;
    if at.k == Kind::Dir {
        return Err(Errno::IsDirectory);
    }
    let mut tx = Tx::new(tree);
    tx.remove_dirent(parent, name);
    let gone = tx.unlink(at.i).await?;
    tx.touch(parent).await?;
    tx.finish().await?;
    Ok(gone)
}

/// Removes the empty directory named `name` in `parent`. Returns its inode, which has no links now.
pub async fn rmdir(tree: &mut Tree, parent: u64, name: &str) -> Res<Option<u64>> {
    let at = dirent(tree, parent, name).await?.ok_or(Errno::NoEntry)?;
    if at.k != Kind::Dir {
        return Err(Errno::NotDirectory);
    }
    if !tree.scan(&dir_prefix(at.i), None, 1).await.map_err(fail)?.is_empty() {
        return Err(Errno::NotEmpty);
    }
    let mut tx = Tx::new(tree);
    tx.remove_dirent(parent, name);
    let gone = tx.unlink(at.i).await?;
    tx.touch(parent).await?;
    tx.finish().await?;
    Ok(gone)
}

/// Moves `from` in `from_dir` to `to` in `to_dir`, over what is there if it can be. A trailing slash on either asks
/// for a directory. Returns the inode that it replaced, if that had no more links.
pub async fn rename(
    tree: &mut Tree,
    (from_dir, from): (u64, &str),
    (to_dir, to): (u64, &str),
    slashes: (bool, bool),
) -> Res<Option<u64>> {
    check_name(to)?;
    let src = dirent(tree, from_dir, from).await?.ok_or(Errno::NoEntry)?;
    let dst = dirent(tree, to_dir, to).await?;
    if src.k != Kind::Dir && (slashes.0 || slashes.1) {
        return Err(Errno::NotDirectory);
    }
    let target = fetch(tree, to_dir).await?;
    if target.n == 0 {
        return Err(Errno::NoEntry); // the directory was removed while open
    }
    if dst.is_some_and(|d| d.i == src.i) {
        return Ok(None); // the same file under two names: nothing to do
    }
    if src.k == Kind::Dir {
        // A directory does not move into itself, or what is below it.
        let mut at = to_dir;
        while at != ROOT && at != 0 {
            if at == src.i {
                return Err(Errno::Invalid);
            }
            at = fetch(tree, at).await?.p;
        }
    }
    match (src.k, dst) {
        (Kind::Dir, Some(Dirent { k: Kind::Dir, i })) => {
            if !tree.scan(&dir_prefix(i), None, 1).await.map_err(fail)?.is_empty() {
                return Err(Errno::NotEmpty);
            }
        }
        (Kind::Dir, Some(_)) => return Err(Errno::NotDirectory),
        (_, Some(Dirent { k: Kind::Dir, .. })) => return Err(Errno::IsDirectory),
        _ => {}
    }
    let mut tx = Tx::new(tree);
    tx.remove_dirent(from_dir, from);
    tx.put_dirent(to_dir, to, src)?;
    let gone = match dst {
        Some(d) => tx.unlink(d.i).await?,
        None => None,
    };
    let now = tx.now;
    let moved = tx.edit(src.i).await?;
    moved.c = now;
    if src.k == Kind::Dir {
        moved.p = to_dir;
    }
    tx.touch(from_dir).await?;
    tx.touch(to_dir).await?;
    tx.finish().await?;
    Ok(gone)
}

/// Another name, `name` in `parent`, for the file or symlink `ino`.
pub async fn link(tree: &mut Tree, ino: u64, parent: u64, name: &str) -> Res<()> {
    check_name(name)?;
    let target = fetch(tree, parent).await?;
    if target.n == 0 {
        return Err(Errno::NoEntry);
    }
    if dirent(tree, parent, name).await?.is_some() {
        return Err(Errno::Exist);
    }
    let mut tx = Tx::new(tree);
    let now = tx.now;
    let inode = tx.edit(ino).await?;
    if inode.k == Kind::Dir {
        return Err(Errno::NotPermitted);
    }
    inode.n = inode.n.checked_add(1).ok_or(Errno::TooManyLinks)?;
    inode.c = now;
    let kind = inode.k;
    tx.put_dirent(parent, name, Dirent { i: ino, k: kind })?;
    tx.touch(parent).await?;
    tx.finish().await
}

/// Sets the access and modification times of `ino`.
pub async fn set_times(tree: &mut Tree, ino: u64, atime: Time, mtime: Time) -> Res<()> {
    let mut tx = Tx::new(tree);
    let now = tx.now;
    let inode = tx.edit(ino).await?;
    let pick = |t: Time, was: u64| match t {
        Time::Keep => was,
        Time::Now => now,
        Time::At(ns) => ns,
    };
    (inode.a, inode.m, inode.c) = (pick(atime, inode.a), pick(mtime, inode.m), now);
    tx.finish().await
}

/// The entries of the directory `dir` that follow `after`, a name, in order: at most `limit`.
pub async fn readdir(tree: &Tree, dir: u64, after: Option<&str>, limit: usize) -> Res<Vec<(String, Kind)>> {
    let prefix = dir_prefix(dir);
    let after = after.map(|n| format!("{prefix}{n}"));
    let mut out = vec![];
    for (key, item) in tree.entries(&prefix, after.as_deref(), limit).await.map_err(fail)? {
        let bytes = tree.value(&item).await.map_err(fail)?;
        let at: Dirent = serde_json::from_slice(&bytes).map_err(|e| {
            tracing::warn!("name: {key}: {e}");
            Errno::Io
        })?;
        out.push((key[prefix.len()..].to_owned(), at.k));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::tree::counting::counting;
    use crate::tree::{Limits, Shape};
    use futures_util::TryStreamExt;
    use object_store::{ObjectStore, ObjectStoreExt};
    use std::sync::Arc;
    use std::sync::atomic::Ordering::SeqCst;

    fn tree(limits: Limits) -> Tree {
        Tree::open(&Store::memory(), "a", "n", &Shape::default(), limits).unwrap()
    }

    #[tokio::test]
    async fn files_hold_what_is_written() {
        let mut t = tree(Limits::default());
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        assert_eq!(write_at(&mut t, ino, At::Offset(0), b"hello").await, Ok(5));
        assert_eq!(write_at(&mut t, ino, At::End, b" world").await, Ok(6));
        assert_eq!(read_at(&t, ino, 0, 100).await.unwrap(), &b"hello world"[..]);
        assert_eq!(read_at(&t, ino, 6, 3).await.unwrap(), &b"wor"[..]);
        assert!(read_at(&t, ino, 11, 3).await.unwrap().is_empty());
        assert_eq!(stat(&t, ino).await.unwrap().size, 11);
        // A hole reads as zeros, across blocks, and so does what a bigger size adds.
        let far = BLOCK * 2 + 10;
        write_at(&mut t, ino, At::Offset(far), b"x").await.unwrap();
        let got = read_at(&t, ino, 0, far + 1).await.unwrap();
        assert_eq!(got.len() as u64, far + 1);
        assert_eq!(&got[..11], b"hello world");
        assert!(got[11..far as usize].iter().all(|b| *b == 0));
        assert_eq!(&got[far as usize..], b"x");
        set_size(&mut t, ino, far + 100).await.unwrap();
        assert!(read_at(&t, ino, far + 1, 99).await.unwrap().iter().all(|b| *b == 0));
        // Shrinking drops the rest, and growing again does not bring it back.
        set_size(&mut t, ino, 3).await.unwrap();
        set_size(&mut t, ino, 8).await.unwrap();
        assert_eq!(read_at(&t, ino, 0, 100).await.unwrap(), &b"hel\0\0\0\0\0"[..]);
        let keys = t.scan(&format!("f/{ino}/"), None, 100).await.unwrap();
        assert_eq!(keys, [block_key(ino, 0)], "the blocks past the size are gone");
    }

    #[tokio::test]
    async fn a_write_across_blocks_and_in_the_middle_of_one() {
        let mut t = tree(Limits::default());
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        let data: Vec<u8> = (0..BLOCK * 2 + 100).map(|i| (i % 251) as u8).collect();
        assert_eq!(write_at(&mut t, ino, At::Offset(0), &data).await, Ok(data.len()));
        assert_eq!(write_at(&mut t, ino, At::Offset(BLOCK - 2), b"abcd").await, Ok(4));
        let got = read_at(&t, ino, 0, data.len() as u64).await.unwrap();
        let mut want = data.clone();
        want[BLOCK as usize - 2..BLOCK as usize + 2].copy_from_slice(b"abcd");
        assert!(got[..] == want[..]);
        let at = BLOCK + 5;
        assert_eq!(read_at(&t, ino, at, 3).await.unwrap(), &want[at as usize..at as usize + 3]);
    }

    #[tokio::test]
    async fn a_name_that_fills_takes_part_of_a_write_and_then_none() {
        let limits = Limits { data: 3 * WRITE_MAX as u64 + 1000, ..Limits::default() };
        let mut t = tree(limits);
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        let data = vec![7u8; 4 * WRITE_MAX];
        assert_eq!(write_at(&mut t, ino, At::Offset(0), &data).await, Ok(3 * WRITE_MAX), "the part that fits");
        assert_eq!(write_at(&mut t, ino, At::End, &data).await, Err(Errno::InsufficientSpace));
        assert_eq!(write_at(&mut t, ino, At::End, b"x").await, Ok(1), "a refusal is not a failure of the tree");
    }

    #[tokio::test]
    async fn paths_stay_below_where_they_start() {
        let mut t = tree(Limits::default());
        let sub = create(&mut t, ROOT, "sub", Kind::Dir, None).await.unwrap();
        create(&mut t, sub, "in", Kind::File, None).await.unwrap();
        create(&mut t, ROOT, "out", Kind::File, None).await.unwrap();
        symlink(&mut t, sub, "up", "../out").await.unwrap();
        assert_eq!(symlink(&mut t, sub, "abs", "/out").await, Err(Errno::NotPermitted), "refused when made");
        // Inserted as a damaged tree could hold it, to see that following one still leads nowhere.
        create(&mut t, sub, "abs", Kind::Link, Some("/out")).await.unwrap();
        symlink(&mut t, sub, "high", "../../out").await.unwrap();
        symlink(&mut t, sub, "self", "self").await.unwrap();
        symlink(&mut t, sub, "dot", ".").await.unwrap();
        symlink(&mut t, ROOT, "deep", "sub/dot/dot/dot/in").await.unwrap();
        let from = |dir, path: &'static str| {
            let t = &t;
            async move { lookup(t, dir, path, true).await.map(|(i, _)| i) }
        };
        assert!(from(sub, "in").await.is_ok());
        assert_eq!(from(sub, "../in").await, Err(Errno::NotPermitted), "a descriptor cannot name its parent");
        assert_eq!(from(sub, "..").await, Err(Errno::NotPermitted));
        assert_eq!(from(sub, "/in").await, Err(Errno::NotPermitted));
        assert_eq!(from(sub, "./a/../../in").await, Err(Errno::NoEntry), "a missing part is missing first");
        assert_eq!(from(sub, "dot/../in").await, Err(Errno::NotPermitted));
        assert_eq!(from(sub, "abs").await, Err(Errno::NotPermitted), "an absolute symlink leads nowhere");
        assert_eq!(from(sub, "self").await, Err(Errno::Loop));
        // A symlink's `..` is relative to the directory it is in, so it can lead up to the root from `sub`...
        assert!(from(ROOT, "sub/up").await.is_ok());
        // ...but not out of the root, nor out of a directory it was opened from.
        assert_eq!(from(ROOT, "sub/high").await, Err(Errno::NotPermitted));
        assert_eq!(from(sub, "up").await, Err(Errno::NotPermitted), "from `sub`, `../out` is above the start");
        assert!(from(ROOT, "deep").await.is_ok(), "a symlink may lead down through itself");
        assert_eq!(from(ROOT, "../sub").await, Err(Errno::NotPermitted));
        assert_eq!(from(ROOT, "sub/../..").await, Err(Errno::NotPermitted));
    }

    #[tokio::test]
    async fn forty_symlinks_are_followed_and_no_more() {
        let mut t = tree(Limits::default());
        create(&mut t, ROOT, "end", Kind::File, None).await.unwrap();
        symlink(&mut t, ROOT, "l0", "end").await.unwrap();
        for i in 1..45 {
            symlink(&mut t, ROOT, &format!("l{i}"), &format!("l{}", i - 1)).await.unwrap();
        }
        assert!(lookup(&t, ROOT, "l39", true).await.is_ok(), "40 links: l39 is the 40th");
        assert_eq!(lookup(&t, ROOT, "l40", true).await.map(|_| ()), Err(Errno::Loop));
        assert_eq!(lookup(&t, ROOT, "l44/", false).await.map(|_| ()), Err(Errno::Loop));
    }

    #[tokio::test]
    async fn directories_and_names() {
        let mut t = tree(Limits::default());
        let a = create(&mut t, ROOT, "a", Kind::Dir, None).await.unwrap();
        let b = create(&mut t, a, "b", Kind::Dir, None).await.unwrap();
        let f = create(&mut t, b, "f", Kind::File, None).await.unwrap();
        assert_eq!(create(&mut t, a, "b", Kind::File, None).await, Err(Errno::Exist));
        assert_eq!(rmdir(&mut t, ROOT, "a").await, Err(Errno::NotEmpty));
        assert_eq!(rename(&mut t, (ROOT, "a"), (b, "a"), (false, false)).await, Err(Errno::Invalid), "into itself");
        assert_eq!(rename(&mut t, (ROOT, "a"), (a, "x"), (false, false)).await, Err(Errno::Invalid));
        assert_eq!(rename(&mut t, (b, "f"), (a, "b"), (false, false)).await, Err(Errno::IsDirectory));
        assert_eq!(rename(&mut t, (a, "b"), (b, "f"), (false, false)).await, Err(Errno::Invalid));
        rename(&mut t, (b, "f"), (ROOT, "g"), (false, false)).await.unwrap();
        assert_eq!(lookup(&t, ROOT, "g", true).await.unwrap(), (f, Kind::File));
        assert_eq!(lookup(&t, ROOT, "a/b/f", true).await.map(|_| ()), Err(Errno::NoEntry));
        assert_eq!(lookup(&t, ROOT, "g/", true).await.map(|_| ()), Err(Errno::NotDirectory));
        assert_eq!(unlink(&mut t, ROOT, "a").await, Err(Errno::IsDirectory));
        assert_eq!(rmdir(&mut t, a, "b").await, Ok(Some(b)));
        assert_eq!(stat(&t, b).await.unwrap().links, 0, "until it is purged");
        assert_eq!(create(&mut t, b, "x", Kind::File, None).await, Err(Errno::NoEntry), "a removed directory is empty");
        purge(&mut t, b).await.unwrap();
        let names = readdir(&t, ROOT, None, 100).await.unwrap().into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names, ["a", "g"]);
        // Past the limits of a name.
        assert_eq!(create(&mut t, ROOT, &"x".repeat(256), Kind::File, None).await, Err(Errno::NameTooLong));
        assert_eq!(create(&mut t, ROOT, "a/b", Kind::File, None).await, Err(Errno::Invalid));
    }

    #[tokio::test]
    async fn links_count_and_the_last_one_leaves() {
        let mut t = tree(Limits::default());
        let f = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        write_at(&mut t, f, At::Offset(0), &vec![1u8; BLOCK as usize + 1]).await.unwrap();
        link(&mut t, f, ROOT, "g").await.unwrap();
        assert_eq!(link(&mut t, f, ROOT, "g").await, Err(Errno::Exist));
        let d = create(&mut t, ROOT, "d", Kind::Dir, None).await.unwrap();
        assert_eq!(link(&mut t, d, ROOT, "e").await, Err(Errno::NotPermitted));
        assert_eq!(stat(&t, f).await.unwrap().links, 2);
        assert_eq!(unlink(&mut t, ROOT, "f").await, Ok(None));
        assert_eq!(unlink(&mut t, ROOT, "f").await, Err(Errno::NoEntry));
        assert_eq!(read_at(&t, f, 0, 1).await.unwrap(), &[1u8][..]);
        assert_eq!(unlink(&mut t, ROOT, "g").await, Ok(Some(f)));
        purge(&mut t, f).await.unwrap();
        assert!(t.scan(&format!("f/{f}"), None, 10).await.unwrap().is_empty(), "no inode and no blocks remain");
        // Renaming over a file takes its last link too.
        let x = create(&mut t, ROOT, "x", Kind::File, None).await.unwrap();
        let y = create(&mut t, ROOT, "y", Kind::File, None).await.unwrap();
        assert_eq!(rename(&mut t, (ROOT, "x"), (ROOT, "y"), (false, false)).await, Ok(Some(y)));
        assert_eq!(lookup(&t, ROOT, "y", true).await.unwrap().0, x);
        assert_eq!(rename(&mut t, (ROOT, "y"), (ROOT, "y"), (false, false)).await, Ok(None));
    }

    #[tokio::test]
    async fn a_directory_of_thousands_is_listed_in_pages() {
        let mut t = tree(Limits { node: 4000, inline: 512, budget: 1 << 16, ..Limits::default() });
        for i in 0..2500 {
            create(&mut t, ROOT, &format!("f{i:05}"), Kind::File, None).await.unwrap();
        }
        let (mut after, mut all) = (None::<String>, vec![]);
        loop {
            let page = readdir(&t, ROOT, after.as_deref(), PAGE).await.unwrap();
            let done = page.len() < PAGE;
            after = page.last().map(|(n, _)| n.clone());
            all.extend(page.into_iter().map(|(n, _)| n));
            if done {
                break;
            }
        }
        assert_eq!(all.len(), 2500);
        assert!(all.windows(2).all(|w| w[0] < w[1]) && all[0] == "f00000" && all[2499] == "f02499");
    }

    #[tokio::test]
    async fn a_store_that_fails_is_io_to_the_guest_and_the_tree_cannot_commit() {
        let (store, counts) = counting();
        let mut t =
            Tree::open(&store, "a", "n", &Shape::default(), Limits { budget: 4096, ..Limits::default() }).unwrap();
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        counts.allow.store(counts.puts.load(SeqCst), SeqCst); // no more puts
        // The data does not fit in memory, so it is uploaded, and that fails: the guest is told nothing of why.
        assert_eq!(write_at(&mut t, ino, At::Offset(0), &[1; 20_000]).await, Err(Errno::Io));
        counts.allow.store(usize::MAX, SeqCst);
        assert_eq!(create(&mut t, ROOT, "g", Kind::File, None).await, Err(Errno::Io), "the tree takes no more");
        assert_eq!(unlink(&mut t, ROOT, "f").await, Err(Errno::Io));
        assert!(t.finish().await.is_err(), "and it is not committed");
    }

    #[tokio::test]
    async fn what_the_store_lost_is_io_to_the_guest() {
        let (store, _) = counting();
        let mut t = Tree::open(&store, "a", "n", &Shape::default(), Limits::default()).unwrap();
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        write_at(&mut t, ino, At::Offset(0), &vec![1; 300_000]).await.unwrap();
        let shape = t.finish().await.unwrap();
        let paths: Vec<_> = store.inner.list(None).map_ok(|m| m.location).try_collect().await.unwrap();
        assert!(paths.len() >= 2, "the blocks are objects");
        for path in paths {
            store.inner.delete(&path).await.unwrap();
        }
        let forgetful = Store { cache: Arc::default(), ..store }; // or the blocks are cached
        let t = Tree::open(&forgetful, "a", "n", &shape, Limits::default()).unwrap();
        assert_eq!(read_at(&t, ino, 0, 100).await.err(), Some(Errno::Io));
        assert_eq!(stat(&t, ino).await.unwrap().size, 300_000, "what is in the nodes is still there");
    }

    /// A file of `blocks` whole blocks and a tail that is small enough to sit in its node, in a tree that is committed,
    /// and a store to read it back from: its cache is cold, except for the nodes.
    async fn stored(blocks: u64) -> (Tree, u64, Store, Arc<crate::tree::counting::Counts>, Vec<u8>) {
        let (store, counts) = counting();
        let mut t = Tree::open(&store, "a", "n", &Shape::default(), Limits::default()).unwrap();
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        let data: Vec<u8> = (0..(blocks * BLOCK + 100)).map(|i| (i % 251) as u8).collect();
        write_at(&mut t, ino, At::Offset(0), &data).await.unwrap();
        let shape = t.finish().await.unwrap();
        let cold = Store { cache: Arc::default(), ..store };
        let t = Tree::open(&cold, "a", "n", &shape, Limits::default()).unwrap();
        t.get(&block_key(ino, 0)).await.unwrap().unwrap(); // the nodes
        counts.gets.store(0, SeqCst);
        (t, ino, cold, counts, data)
    }

    #[tokio::test]
    async fn the_blocks_of_a_read_are_fetched_together_and_put_in_order() {
        let (t, ino, _, counts, data) = stored(3).await;
        counts.delay.store(20, SeqCst);
        // Three blocks that are objects, and a tail that is not: a fetch for each of them, all under way at once.
        let got = read_at(&t, ino, 1000, data.len() as u64).await.unwrap();
        assert_eq!(got, &data[1000..]);
        assert_eq!(counts.gets.load(SeqCst), 3);
        assert_eq!(counts.peak.load(SeqCst), 3);

        // Past the bound they are not all at once, and each is fetched once.
        let (t, ino, _, counts, data) = stored(IN_FLIGHT as u64 * 2).await;
        counts.delay.store(20, SeqCst);
        assert_eq!(read_at(&t, ino, 0, data.len() as u64).await.unwrap(), &data[..]);
        assert_eq!(counts.gets.load(SeqCst), IN_FLIGHT * 2);
        assert_eq!(counts.peak.load(SeqCst), IN_FLIGHT);
    }

    #[tokio::test]
    async fn blocks_that_are_not_stored_read_as_zeros_among_those_that_are() {
        let mut t = tree(Limits::default());
        let ino = create(&mut t, ROOT, "f", Kind::File, None).await.unwrap();
        let (block, end) = (BLOCK as usize, BLOCK * 3 + 5);
        write_at(&mut t, ino, At::Offset(0), &vec![7; block + 10]).await.unwrap(); // a block, and a bit of the next
        write_at(&mut t, ino, At::Offset(end), b"end").await.unwrap(); // and a hole of two blocks, and some
        let mut want = vec![7; block + 10];
        want.resize(end as usize, 0);
        want.extend_from_slice(b"end");
        assert_eq!(read_at(&t, ino, 0, BLOCK * 4).await.unwrap(), &want[..]);
        assert_eq!(read_at(&t, ino, 100, BLOCK * 4).await.unwrap(), &want[100..]);
        assert_eq!(read_at(&t, ino, BLOCK + 5, 20).await.unwrap(), &want[block + 5..block + 25]);
    }

    #[tokio::test]
    async fn blocks_that_are_warmed_are_in_the_cache_and_not_fetched_again() {
        let (t, ino, _, counts, data) = stored(3).await;
        warm(&t, ino, 0, BLOCK * 2).await.unwrap();
        assert_eq!(counts.gets.load(SeqCst), 2);
        assert_eq!(read_at(&t, ino, 0, BLOCK * 2).await.unwrap(), &data[..BLOCK as usize * 2]);
        assert_eq!(counts.gets.load(SeqCst), 2, "a read of what was warmed asks nothing of the store");
        // Past the end there is nothing to warm, and in a directory it is refused, as a read is.
        warm(&t, ino, BLOCK * 9, BLOCK).await.unwrap();
        assert_eq!(counts.gets.load(SeqCst), 2);
        assert_eq!(warm(&t, ROOT, 0, 1).await, Err(Errno::IsDirectory));
    }

    #[tokio::test]
    async fn blocks_that_are_warmed_are_checked_as_a_read_checks_them() {
        let (t, ino, store, _, data) = stored(2).await;
        let metas: Vec<_> = store.inner.list(None).try_collect().await.unwrap();
        let blocks: Vec<_> = metas.into_iter().filter(|m| m.size == BLOCK).map(|m| m.location).collect();
        assert_eq!(blocks.len(), 2, "the two whole blocks are objects of their own");
        let kept = store.inner.get(&blocks[0]).await.unwrap().bytes().await.unwrap();
        store.inner.put(&blocks[0], vec![0u8; BLOCK as usize].into()).await.unwrap(); // not what the link says
        assert_eq!(warm(&t, ino, 0, BLOCK * 2).await, Err(Errno::Io));
        assert_eq!(read_at(&t, ino, 0, BLOCK * 2).await.err(), Some(Errno::Io));
        // What failed the check was not kept: the block is what it should be, and it reads.
        store.inner.put(&blocks[0], kept.into()).await.unwrap();
        assert_eq!(read_at(&t, ino, 0, BLOCK * 2).await.unwrap(), &data[..BLOCK as usize * 2]);
    }
}
