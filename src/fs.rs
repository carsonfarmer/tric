//! The file system the guest sees: a name's tree, as directories and files, under a preopened `/`. Which turn's tree it
//! is, is the request's: the turn on the name it addresses, to read and write until it answers, and a snapshot of the
//! name if it only reads. A [`Handle`] is a descriptor on it, and [`p2`] and [`p3`] serve `wasi:filesystem` of 0.2 and
//! of 0.3 with them.
//!
//! Descriptors outlive the calls that made them, and so do the streams a descriptor makes, so the rules for the turn
//! are these:
//! * The tree is behind the turn's lock. A read takes it shared, for the length of the read, and a change takes it
//!   exclusive, for the length of the change, and only while the turn is open.
//! * Before a change takes the tree it is admitted, by the gate: a shared lock that is held until the change is done.
//!   The commit takes the gate exclusive, and at once lets go, after it has marked the turn answered and before it
//!   takes the tree. So it waits for every change that was admitted before the answer, which then lands before the
//!   commit, and a change after it is refused with `read-only`. A stream's write is admitted when the guest makes it,
//!   which is before it runs, so a write that the stream has taken is not lost to the answer. The order of the locks is
//!   the gate, then the tree: a change never waits for the gate while it holds the tree.
//! * A write that was admitted is never cancelled: a change to the tree that is cut off half way leaves it unfit to
//!   commit. So a stream's writes are tasks of their own, which a closed stream leaves to finish.
//! * A read holds the tree shared for as long as it is under way, and nothing outlives it that holds the tree. What
//!   a stream reads is cancelled by closing the stream, and by the guest cancelling its read of it: the read is
//!   dropped, and the stream goes on from where it was. A stream that reads on from where it was reads ahead, for the
//!   cache only, in a task that is cancelled with it: see [`Ahead`].
//! * A file that is unlinked while it is open stays in the tree, with no links, until the last descriptor on it is
//!   closed, when it is removed; or, if the turn commits first, until then in a copy of the tree that the descriptors
//!   read, while the commit leaves it out.
//! * What descriptors read after the answer is the tree as the commit left it, or as it was if the turn was discarded:
//!   so a file that was made in a turn that was discarded is no more, and reads fail with `io`.
#[cfg(test)]
mod conformance;
mod ops;
pub mod p2;
pub mod p3;

use crate::engine::Host;
use crate::name::Turn;
use crate::tree::Tree;
use bytes::Bytes;
use futures_util::future::BoxFuture;
pub use ops::{At, Errno, Kind, Res, Stat, Time};
use ops::{Loc, PAGE, ROOT};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{OwnedRwLockReadGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::task::{JoinError, JoinHandle};
use wasmtime::Result;
use wasmtime::component::{Resource, ResourceTableError};

/// The most bytes a read returns.
pub const READ_MAX: u64 = 1 << 20;
/// The most bytes a stream's write takes at once.
pub const WRITE_CAP: usize = 256 << 10;
/// How far ahead of a stream that reads on from where it was the blocks are fetched, for the cache.
const AHEAD: u64 = 1 << 20;

/// How a call of a binding fails: with an error of the file system, which the guest is told, or with a trap, which ends
/// the guest.
#[derive(Debug)]
pub enum Fault {
    Errno(Errno),
    Trap(wasmtime::Error),
}

impl From<Errno> for Fault {
    fn from(e: Errno) -> Self {
        Self::Errno(e)
    }
}

impl From<ResourceTableError> for Fault {
    fn from(e: ResourceTableError) -> Self {
        Self::Trap(e.into())
    }
}

impl From<wasmtime::Error> for Fault {
    fn from(e: wasmtime::Error) -> Self {
        Self::Trap(e)
    }
}

impl Host {
    /// The descriptor, which is cloned out of the table so that nothing of the table is held across an await.
    fn fd(&self, fd: &Resource<Handle>) -> Result<Handle, Fault> {
        Ok(self.table.get(fd)?.clone())
    }

    /// The file a stream writes, if the turn can be written.
    fn writer(&self, fd: &Resource<Handle>) -> Result<Handle, Fault> {
        let file = self.fd(fd)?;
        file.writing()?;
        if !file.mutable() {
            return Err(Errno::ReadOnly.into());
        }
        Ok(file)
    }

    /// Adds a resource to the table, if there is room: the guest is told there is not, as it would be of memory.
    fn add<T: Send + 'static>(&mut self, resource: T) -> Result<Resource<T>, Fault> {
        self.table.push(resource).map_err(|e| match e {
            ResourceTableError::Full => Errno::InsufficientMemory.into(),
            e => e.into(),
        })
    }

    /// The preopened directories: `/` if the request addresses a name, and none if it does not.
    async fn preopen(&mut self) -> Result<Vec<(Resource<Handle>, String)>> {
        let ctx = self.ctx.clone();
        let Some(turn) = ctx.mount().await? else { return Ok(Vec::new()) };
        Ok(vec![(self.table.push(Handle::root(&turn))?, "/".into())])
    }
}

/// What a write task found, as the error of the call it was for: all of `len` bytes written is `Ok`, and fewer, which
/// is the name filling part way, is `insufficient-space`.
fn wrote(done: Result<Res<usize>, JoinError>, len: usize) -> Res<()> {
    match done {
        Ok(Ok(n)) if n == len => Ok(()),
        Ok(Ok(_)) => Err(Errno::InsufficientSpace),
        Ok(Err(e)) => Err(e),
        Err(e) => {
            tracing::warn!("fs: a write failed to finish: {e}");
            Err(Errno::Io)
        }
    }
}

/// An admitted change, for as long as it is held.
type Permit = OwnedRwLockReadGuard<()>;

/// What a turn keeps of its file system: which inodes are open, and which of those were unlinked.
#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
    gate: Arc<RwLock<()>>,
}

#[derive(Default)]
struct Inner {
    /// The descriptors, and streams, open on each inode.
    open: HashMap<u64, usize>,
    /// The inodes that were unlinked while open, which have no links, and are in the tree until they are closed.
    orphans: HashSet<u64>,
    /// The orphans that were open when the turn committed, and the tree they are read from.
    kept: Option<Kept>,
}

/// The tree as a commit was made, for the descriptors that were open on files it removed: the commit leaves them out.
pub struct Kept {
    fork: Arc<Tree>,
    inos: HashSet<u64>,
}

impl State {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn opened(&self, ino: u64) {
        *self.inner().open.entry(ino).or_default() += 1;
    }

    /// A descriptor on `ino` is closed. The last on an orphan removes it, if the turn is open to remove it: else the
    /// commit will, or has.
    fn closed(&self, turn: &Arc<Turn>, ino: u64) {
        let orphan = {
            let mut inner = self.inner();
            let Some(n) = inner.open.get_mut(&ino) else { return };
            *n -= 1;
            if *n > 0 {
                return;
            }
            inner.open.remove(&ino);
            if let Some(kept) = &mut inner.kept {
                kept.inos.remove(&ino);
                if kept.inos.is_empty() {
                    inner.kept = None;
                }
            }
            inner.orphans.contains(&ino)
        };
        if let (true, true, Ok(runtime)) = (orphan, turn.is_open(), tokio::runtime::Handle::try_current()) {
            runtime.spawn(remove(turn.clone(), ino));
        }
    }

    /// Whether `ino` is open, and so stays as an orphan: decided as the tree is changed, so no one opens it between.
    fn orphan(&self, ino: u64) -> bool {
        let mut inner = self.inner();
        let open = inner.open.contains_key(&ino);
        if open {
            inner.orphans.insert(ino);
        }
        open
    }

    /// Whether `ino` was an orphan with no one left to read it, which is no more.
    fn release(&self, ino: u64) -> bool {
        let mut inner = self.inner();
        !inner.open.contains_key(&ino) && inner.orphans.remove(&ino)
    }

    /// The tree to read `ino` from, if it was an orphan when the turn committed.
    fn kept(&self, ino: u64) -> Option<Arc<Tree>> {
        self.inner().kept.as_ref().filter(|k| k.inos.contains(&ino)).map(|k| k.fork.clone())
    }

    /// Admits a change, for as long as the permit is held. Fails once the turn has answered. The answer is checked
    /// after the permit is held, for the commit marks the turn answered before it drains the gate: a permit that sees
    /// the turn open is one the commit waits for.
    async fn admit(&self, turn: &Turn) -> Res<Permit> {
        if !turn.is_open() {
            return Err(Errno::ReadOnly);
        }
        let permit = self.gate.clone().read_owned().await;
        if !turn.is_open() {
            return Err(Errno::ReadOnly); // the answer came as it waited
        }
        Ok(permit)
    }

    /// [`State::admit`] without waiting, for a call that cannot. It fails if a commit waits on the gate, and so the
    /// turn has answered.
    fn try_admit(&self, turn: &Turn) -> Res<Permit> {
        let permit = self.gate.clone().try_read_owned().map_err(|_| Errno::ReadOnly)?;
        if !turn.is_open() {
            return Err(Errno::ReadOnly);
        }
        Ok(permit)
    }

    /// Waits for the changes that were admitted. The turn must be marked answered, or more are admitted behind it.
    pub async fn drain(&self) {
        drop(self.gate.write().await);
    }

    /// Readies the turn's tree for a commit: removes the orphans, which the commit leaves out. Returns the tree they
    /// were in, if descriptors are still open on any, for [`State::keep`] once the commit has landed.
    pub async fn settle(&self, tree: &mut Tree) -> Result<Option<Kept>> {
        let (orphans, open) = {
            let mut inner = self.inner();
            let orphans = std::mem::take(&mut inner.orphans);
            let open: HashSet<u64> = orphans.iter().filter(|i| inner.open.contains_key(i)).copied().collect();
            (orphans, open)
        };
        let kept = (!open.is_empty()).then(|| Kept { fork: Arc::new(tree.fork()), inos: open });
        for ino in orphans {
            ops::purge(tree, ino).await?;
        }
        Ok(kept)
    }

    /// The commit landed: the descriptors still open on its orphans read them from `kept`.
    pub fn keep(&self, mut kept: Kept) {
        let mut inner = self.inner();
        kept.inos.retain(|i| inner.open.contains_key(i)); // some may have closed since
        inner.kept = (!kept.inos.is_empty()).then_some(kept);
    }
}

/// The tree, to change: held with the permit that admitted the change.
struct Edit<'a> {
    tree: RwLockWriteGuard<'a, Tree>,
    _permit: Permit,
}

impl<'a> Edit<'a> {
    async fn with(turn: &'a Turn, permit: Permit) -> Self {
        Self { tree: turn.tree().write().await, _permit: permit }
    }
}

impl Deref for Edit<'_> {
    type Target = Tree;

    fn deref(&self) -> &Tree {
        &self.tree
    }
}

impl DerefMut for Edit<'_> {
    fn deref_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }
}

/// The tree of `turn`, to change, if the turn is open.
async fn edit(turn: &Turn) -> Res<Edit<'_>> {
    let permit = turn.fs.admit(turn).await?;
    Ok(Edit::with(turn, permit).await)
}

/// The tree, to read.
enum View<'a> {
    Live(RwLockReadGuard<'a, Tree>),
    Kept(Arc<Tree>),
}

impl Deref for View<'_> {
    type Target = Tree;

    fn deref(&self) -> &Tree {
        match self {
            Self::Live(tree) => tree,
            Self::Kept(tree) => tree,
        }
    }
}

/// Removes an orphan that was closed, if the turn is still open to.
async fn remove(turn: Arc<Turn>, ino: u64) {
    let Ok(mut tree) = edit(&turn).await else { return };
    if turn.fs.release(ino) {
        _ = ops::purge(&mut tree, ino).await; // a failure poisons the tree, and the turn's commit fails
    }
}

/// Removes `gone`, an inode that has no links left, unless it is open: it is then an orphan until it is closed.
async fn reclaim(turn: &Turn, tree: &mut Tree, gone: Option<u64>) -> Res<()> {
    match gone {
        Some(ino) if !turn.fs.orphan(ino) => ops::purge(tree, ino).await,
        _ => Ok(()),
    }
}

/// An inode that is open. Dropping the last of them on an orphan removes it.
struct Pin {
    turn: Arc<Turn>,
    ino: u64,
}

impl Pin {
    /// Opens `ino`, which must be done with the tree locked, so that an unlink sees it.
    fn new(turn: &Arc<Turn>, ino: u64) -> Arc<Self> {
        turn.fs.opened(ino);
        Arc::new(Self { turn: turn.clone(), ino })
    }

    /// The tree to read this inode from: the live one, or if the turn committed without it, the one it was in.
    async fn view(&self) -> View<'_> {
        let live = self.turn.tree().read().await; // first: the commit sets `kept` while it holds the write lock
        match self.turn.fs.kept(self.ino) {
            Some(fork) => View::Kept(fork),
            None => View::Live(live),
        }
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.turn.fs.closed(&self.turn, self.ino);
    }
}

/// How a path is opened.
#[derive(Clone, Copy, Default)]
pub struct Mode {
    pub follow: bool,
    pub create: bool,
    pub directory: bool,
    pub exclusive: bool,
    pub truncate: bool,
    pub read: bool,
    pub write: bool,
    /// For a directory: that it be opened to change its entries, which `get-flags` reports back.
    pub mutate: bool,
}

/// What `open` found.
enum Found {
    Existing(u64, Kind),
    Missing { parent: u64, name: String },
}

fn resolved(loc: Loc, m: &Mode) -> Res<Found> {
    let changes = m.create || m.truncate || m.write;
    match loc {
        Loc::Dir(_) if m.create && m.exclusive => Err(Errno::Exist),
        Loc::Dir(_) if changes => Err(Errno::IsDirectory),
        Loc::Dir(ino) => Ok(Found::Existing(ino, Kind::Dir)),
        Loc::Entry { at: None, .. } if !m.create => Err(Errno::NoEntry),
        Loc::Entry { at: None, slash: true, .. } => Err(Errno::IsDirectory),
        Loc::Entry { parent, name, at: None, .. } => Ok(Found::Missing { parent, name }),
        Loc::Entry { at: Some(_), .. } if m.create && m.exclusive => Err(Errno::Exist),
        Loc::Entry { at: Some(d), slash, .. } => match d.kind() {
            Kind::Link => Err(if m.directory { Errno::NotDirectory } else { Errno::Loop }),
            Kind::Dir if changes => Err(Errno::IsDirectory),
            Kind::Dir => Ok(Found::Existing(d.ino(), Kind::Dir)),
            Kind::File if m.directory || slash => Err(Errno::NotDirectory),
            Kind::File => Ok(Found::Existing(d.ino(), Kind::File)),
        },
    }
}

/// A descriptor: an open file or directory, with the rights it was opened with. The paths it takes lead down from it,
/// and never up: the preopened root is the top of what the guest can name.
#[derive(Clone)]
pub struct Handle {
    pin: Arc<Pin>,
    kind: Kind,
    read: bool,
    write: bool,
    /// For a directory: whether it was opened to change its entries. Only what `get-flags` says: what the turn allows
    /// is what decides, as the adapter of WASI 0.1 asks for no more than reading of a directory it opens, and as
    /// Wasmtime's own host does not hold a directory to it either.
    mutate: bool,
}

impl Handle {
    /// The root, as the preopen, which is read, and changed by whoever may change the turn.
    pub fn root(turn: &Arc<Turn>) -> Self {
        Self { pin: Pin::new(turn, ROOT), kind: Kind::Dir, read: true, write: false, mutate: true }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn readable(&self) -> bool {
        self.read
    }

    pub fn writable(&self) -> bool {
        self.write
    }

    /// Whether the directory can have its entries changed: if the turn is open.
    pub fn mutable(&self) -> bool {
        self.pin.turn.is_open()
    }

    /// Whether `get-flags` says `mutate-directory`: if the directory was opened to change its entries, and still can
    /// be.
    pub fn mutates(&self) -> bool {
        self.mutate && self.mutable()
    }

    fn turn(&self) -> &Arc<Turn> {
        &self.pin.turn
    }

    fn ino(&self) -> u64 {
        self.pin.ino
    }

    fn dir(&self) -> Res<()> {
        (self.kind == Kind::Dir).then_some(()).ok_or(Errno::NotDirectory)
    }

    /// A descriptor on `ino`, opened by `mode`, which must be done with the tree locked.
    fn child(&self, ino: u64, kind: Kind, m: &Mode) -> Self {
        let is_file = kind == Kind::File;
        let pin = Pin::new(self.turn(), ino);
        Self { pin, kind, read: !is_file || m.read, write: is_file && m.write, mutate: !is_file && m.mutate }
    }

    /// Opens `path`, below this directory.
    pub async fn open(&self, path: &str, m: Mode) -> Res<Self> {
        self.dir()?;
        if m.directory && (m.create || m.exclusive || m.truncate) {
            return Err(Errno::Invalid);
        }
        if m.create || m.truncate {
            let mut tree = edit(self.turn()).await?;
            let (ino, kind) = match resolved(ops::locate(&tree, self.ino(), path, m.follow).await?, &m)? {
                Found::Existing(ino, kind) => {
                    if m.truncate && kind == Kind::File {
                        ops::set_size(&mut tree, ino, 0).await?;
                    }
                    (ino, kind)
                }
                Found::Missing { parent, name } => {
                    (ops::create(&mut tree, parent, &name, Kind::File, None).await?, Kind::File)
                }
            };
            return Ok(self.child(ino, kind, &m));
        }
        let tree = self.pin.view().await;
        let Found::Existing(ino, kind) = resolved(ops::locate(&tree, self.ino(), path, m.follow).await?, &m)? else {
            return Err(Errno::NoEntry);
        };
        if m.write && !self.mutable() {
            return Err(Errno::ReadOnly);
        }
        Ok(self.child(ino, kind, &m))
    }

    pub async fn stat(&self) -> Res<Stat> {
        let tree = self.pin.view().await;
        ops::stat(&tree, self.ino()).await
    }

    pub async fn stat_at(&self, path: &str, follow: bool) -> Res<Stat> {
        self.dir()?;
        let tree = self.pin.view().await;
        let (ino, _) = ops::lookup(&tree, self.ino(), path, follow).await?;
        ops::stat(&tree, ino).await
    }

    /// Opens `path` as `open-at` asks, with `mode` holding the flags of the descriptor as the guest gave them. `mutate`
    /// of it is the right to change the directory entries, which a turn that has answered does not give, and `sync` is
    /// that a write be durable when it returns, which only a commit makes it.
    pub async fn open_as(&self, path: &str, mut mode: Mode, sync: bool) -> Res<Self> {
        if sync {
            return Err(Errno::Unsupported);
        }
        if mode.mutate && !self.mutable() {
            return Err(Errno::ReadOnly);
        }
        // A descriptor that is not asked to write reads, and one that makes a file or empties it writes.
        mode.read |= !mode.write;
        mode.write |= mode.create || mode.truncate;
        self.open(path, mode).await
    }

    /// Sets the size of the file.
    pub async fn set_size(&self, size: u64) -> Res<()> {
        self.writing()?;
        let mut tree = edit(self.turn()).await?;
        ops::set_size(&mut tree, self.ino(), size).await
    }

    pub async fn set_times(&self, atime: Time, mtime: Time) -> Res<()> {
        let mut tree = edit(self.turn()).await?;
        ops::set_times(&mut tree, self.ino(), atime, mtime).await
    }

    pub async fn set_times_at(&self, path: &str, follow: bool, atime: Time, mtime: Time) -> Res<()> {
        self.dir()?;
        let mut tree = edit(self.turn()).await?;
        let (ino, _) = ops::lookup(&tree, self.ino(), path, follow).await?;
        ops::set_times(&mut tree, ino, atime, mtime).await
    }

    /// The file, which a read or write needs this to be.
    fn file(&self) -> Res<()> {
        match self.kind {
            Kind::File => Ok(()),
            Kind::Dir => Err(Errno::IsDirectory),
            Kind::Link => Err(Errno::BadDescriptor),
        }
    }

    /// A file that was opened to read.
    pub fn reading(&self) -> Res<()> {
        self.file()?;
        self.read.then_some(()).ok_or(Errno::BadDescriptor)
    }

    /// A file that was opened to write.
    pub fn writing(&self) -> Res<()> {
        self.file()?;
        self.write.then_some(()).ok_or(Errno::BadDescriptor)
    }

    /// Up to `len` bytes from `offset`, and whether that was the end of the file: fewer than were asked for, or
    /// than `READ_MAX`.
    pub async fn read(&self, offset: u64, len: u64) -> Res<(Bytes, bool)> {
        self.reading()?;
        if offset > i64::MAX as u64 {
            return Err(Errno::Invalid); // as `pread` does: an offset is signed
        }
        let len = len.min(READ_MAX);
        let tree = self.pin.view().await;
        let data = ops::read_at(&tree, self.ino(), offset, len).await?;
        let end = (data.len() as u64) < len;
        Ok((data, end))
    }

    /// Fetches the blocks of a read of `len` bytes from `offset`, for the cache, and lets them go.
    async fn warm(&self, offset: u64, len: u64) -> Res<()> {
        self.reading()?;
        let tree = self.pin.view().await;
        ops::warm(&tree, self.ino(), offset, len.min(READ_MAX)).await
    }

    /// Writes `data`, and returns how much of it: fewer than all if the name filled.
    pub async fn write(&self, at: At, data: &[u8]) -> Res<usize> {
        self.writing()?;
        let mut tree = edit(self.turn()).await?;
        ops::write_at(&mut tree, self.ino(), at, data).await
    }

    /// Starts a write for a stream, which is admitted now and runs as a task of its own, which nothing cancels. It
    /// returns how much was written, like [`Handle::write`].
    pub fn write_task(&self, at: At, data: Bytes) -> Res<JoinHandle<Res<usize>>> {
        self.writing()?;
        let (pin, turn) = (self.pin.clone(), self.turn().clone());
        let permit = turn.fs.try_admit(&turn)?;
        Ok(tokio::spawn(async move {
            let mut tree = Edit::with(&turn, permit).await;
            ops::write_at(&mut tree, pin.ino, at, &data).await // `pin` is held until it is done
        }))
    }

    /// The entries of the directory, which are listed a page at a time.
    pub fn entries(&self) -> Res<Entries> {
        self.dir()?;
        Ok(Entries { dir: self.clone(), after: None, page: VecDeque::new(), done: false })
    }

    pub async fn mkdir_at(&self, path: &str) -> Res<()> {
        self.dir()?;
        let mut tree = edit(self.turn()).await?;
        let (parent, name, at, _) = ops::entry(&tree, self.ino(), path, false, Errno::Exist).await?;
        if at.is_some() {
            return Err(Errno::Exist);
        }
        ops::create(&mut tree, parent, &name, Kind::Dir, None).await?;
        Ok(())
    }

    /// Makes `path` a symlink to `target`.
    pub async fn symlink_at(&self, path: &str, target: &str) -> Res<()> {
        self.dir()?;
        let mut tree = edit(self.turn()).await?;
        let (parent, name, at, slash) = ops::entry(&tree, self.ino(), path, false, Errno::Exist).await?;
        if at.is_some() {
            return Err(Errno::Exist);
        }
        if slash {
            return Err(Errno::NoEntry);
        }
        ops::symlink(&mut tree, parent, &name, target).await?;
        Ok(())
    }

    pub async fn readlink_at(&self, path: &str) -> Res<String> {
        self.dir()?;
        let tree = self.pin.view().await;
        match ops::locate(&tree, self.ino(), path, false).await? {
            Loc::Dir(_) => Err(Errno::Invalid),
            Loc::Entry { at: None, .. } => Err(Errno::NoEntry),
            Loc::Entry { at: Some(d), .. } if d.kind() == Kind::Link => ops::readlink(&tree, d.ino()).await,
            Loc::Entry { .. } => Err(Errno::Invalid),
        }
    }

    pub async fn unlink_at(&self, path: &str) -> Res<()> {
        self.dir()?;
        let mut tree = edit(self.turn()).await?;
        let (parent, name, at, slash) = ops::entry(&tree, self.ino(), path, false, Errno::IsDirectory).await?;
        if slash && at.is_some_and(|d| d.kind() != Kind::Dir) {
            return Err(Errno::NotDirectory);
        }
        let gone = ops::unlink(&mut tree, parent, &name).await?;
        reclaim(self.turn(), &mut tree, gone).await
    }

    pub async fn rmdir_at(&self, path: &str) -> Res<()> {
        self.dir()?;
        let mut tree = edit(self.turn()).await?;
        let (parent, name, _, _) = ops::entry(&tree, self.ino(), path, false, Errno::Invalid).await?;
        let gone = ops::rmdir(&mut tree, parent, &name).await?;
        reclaim(self.turn(), &mut tree, gone).await
    }

    /// Moves `from`, below this directory, to `to`, below `to_dir`.
    pub async fn rename_at(&self, from: &str, to_dir: &Self, to: &str) -> Res<()> {
        self.dir()?;
        to_dir.dir()?;
        if !Arc::ptr_eq(self.turn(), to_dir.turn()) {
            return Err(Errno::Invalid);
        }
        let mut tree = edit(self.turn()).await?;
        let (from_dir, from, _, from_slash) = ops::entry(&tree, self.ino(), from, false, Errno::Busy).await?;
        let (to_dir, to, _, to_slash) = ops::entry(&tree, to_dir.ino(), to, false, Errno::Busy).await?;
        let gone = ops::rename(&mut tree, (from_dir, &from), (to_dir, &to), (from_slash, to_slash)).await?;
        reclaim(self.turn(), &mut tree, gone).await
    }

    /// Makes `to`, below `to_dir`, another name for `path` below this directory, which is not followed if it is a
    /// symlink.
    pub async fn link_at(&self, path: &str, to_dir: &Self, to: &str) -> Res<()> {
        self.dir()?;
        to_dir.dir()?;
        if !Arc::ptr_eq(self.turn(), to_dir.turn()) {
            return Err(Errno::Invalid);
        }
        let mut tree = edit(self.turn()).await?;
        let (ino, _) = ops::lookup(&tree, self.ino(), path, false).await?;
        let (parent, name, _, slash) = ops::entry(&tree, to_dir.ino(), to, false, Errno::Exist).await?;
        if slash {
            return Err(Errno::NoEntry);
        }
        ops::link(&mut tree, ino, parent, &name).await
    }

    /// Whether `other` is the same file or directory.
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.turn(), other.turn()) && self.ino() == other.ino()
    }

    /// Something that tells one file from another and is the same for one file: its inode.
    pub fn hash(&self) -> (u64, u64) {
        (self.ino(), 0)
    }

    pub async fn hash_at(&self, path: &str, follow: bool) -> Res<(u64, u64)> {
        self.dir()?;
        let tree = self.pin.view().await;
        Ok((ops::lookup(&tree, self.ino(), path, follow).await?.0, 0))
    }
}

/// The entries of a directory, in order, as the tree has them when each page is read.
pub struct Entries {
    dir: Handle,
    after: Option<String>,
    page: VecDeque<(String, Kind)>,
    done: bool,
}

/// A page of the entries of a directory.
pub type Page = Vec<(String, Kind)>;

impl Entries {
    /// Reads the page that follows the entries taken so far, if there is one. It holds the tree shared while it reads
    /// and changes nothing, so it can be dropped at any point, and read again.
    pub fn more(&self) -> Option<BoxFuture<'static, Res<Page>>> {
        if !self.page.is_empty() || self.done {
            return None;
        }
        let (dir, after) = (self.dir.clone(), self.after.clone());
        Some(Box::pin(async move {
            let tree = dir.pin.view().await;
            ops::readdir(&tree, dir.ino(), after.as_deref(), PAGE).await
        }))
    }

    /// Takes the page that [`Entries::more`] read.
    pub fn fill(&mut self, page: Page) {
        self.done = page.len() < PAGE;
        self.after = page.last().map(|(name, _)| name.clone());
        self.page = page.into();
    }

    /// The next entry of the page taken, if any is left.
    pub fn pop(&mut self) -> Option<(String, Kind)> {
        self.page.pop_front()
    }

    pub async fn next(&mut self) -> Res<Option<(String, Kind)>> {
        if let Some(more) = self.more() {
            self.fill(more.await?);
        }
        Ok(self.pop())
    }
}

/// A task that is aborted when it is dropped: a read that holds the tree shared and nothing else, which closing the
/// stream it is for cancels.
pub struct Task<T>(pub JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The read-ahead of a stream that reads a file. When a read lands where the one before it ended, the stream is
/// reading on, and the blocks after it, up to [`AHEAD`] bytes, are fetched for the cache, so that the stream does not
/// wait for the store at each block. They are fetched as a read does, verified against their links, and the task that
/// does it is cancelled with the stream. A read of a file that only gets its first bytes reads nothing it did not
/// ask for.
#[derive(Default)]
pub struct Ahead {
    /// Where the last read that landed ended.
    end: Option<u64>,
    /// Where the blocks that were asked for end.
    asked: u64,
    task: Option<Task<()>>,
}

impl Ahead {
    /// Notes that a read of `len` bytes from `offset` landed.
    pub fn landed(&mut self, file: &Handle, offset: u64, len: u64) {
        let end = offset.saturating_add(len);
        let on = self.end.replace(end) == Some(offset);
        if !on || len == 0 || self.task.as_ref().is_some_and(|task| !task.0.is_finished()) {
            return;
        }
        let (from, to) = (self.asked.max(end), end.saturating_add(AHEAD));
        if from >= to {
            return;
        }
        self.asked = to;
        let file = file.clone();
        // Nothing here is told of a failure: the read that comes to those blocks will tell it.
        self.task = Some(Task(tokio::spawn(async move {
            let _ = file.warm(from, to - from).await;
        })));
    }
}

#[cfg(test)]
impl Ahead {
    /// Waits for the blocks that are being fetched to be in the cache.
    pub async fn settled(&mut self) {
        if let Some(mut task) = self.task.take() {
            let _ = (&mut task.0).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::Committed;
    use crate::outbox::Sink;
    use crate::store::Store;
    use std::time::{Duration, Instant};

    pub(super) async fn turn(store: &Store) -> Arc<Turn> {
        Turn::open(store, "a", "n", false, &Default::default(), Instant::now()).await.unwrap().ok().unwrap()
    }

    pub(super) async fn commit(turn: &Turn) {
        let sink: Sink = Arc::new(|_| Box::pin(async { Ok(()) }));
        assert!(matches!(turn.commit("h", &sink).await.unwrap(), Committed::Done(_)));
    }

    /// To read a file or a directory, following symlinks.
    pub(super) fn read() -> Mode {
        Mode { follow: true, read: true, ..Default::default() }
    }

    /// To read and write a file, which is made, or emptied.
    pub(super) fn make() -> Mode {
        Mode { follow: true, create: true, truncate: true, read: true, write: true, ..Default::default() }
    }

    fn dir() -> Mode {
        Mode { follow: true, directory: true, read: true, ..Default::default() }
    }

    /// Makes the file `path` below `dir` hold `data`, and returns it, open to read and write.
    pub(super) async fn put(dir: &Handle, path: &str, data: &[u8]) -> Handle {
        let file = dir.open(path, make()).await.unwrap();
        assert_eq!(file.write(At::Offset(0), data).await, Ok(data.len()));
        file
    }

    pub(super) async fn text(dir: &Handle, path: &str) -> Res<Bytes> {
        Ok(dir.open(path, read()).await?.read(0, 1 << 20).await?.0)
    }

    /// Whether the tree holds nothing of the inode `ino`: not it, and not its blocks.
    async fn gone(turn: &Turn, ino: u64) -> bool {
        turn.tree().read().await.scan(&format!("f/{ino}"), None, 10).await.unwrap().is_empty()
    }

    /// Waits for the removal of an orphan that was closed, which is a task of its own.
    async fn removed(turn: &Turn, ino: u64) {
        for _ in 0..200 {
            if gone(turn, ino).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("inode {ino} was not removed");
    }

    /// What is in the tree, and what the guest sees of the two things in it that a path could reach.
    async fn contents(turn: &Turn, root: &Handle) -> (Vec<String>, String, String) {
        let keys = turn.tree().read().await.scan("", None, usize::MAX).await.unwrap();
        let (secret, sub) = (root.stat_at("secret", true).await.unwrap(), root.stat_at("sub", true).await.unwrap());
        (keys, format!("{secret:?}"), format!("{sub:?}"))
    }

    /// Every way to name `path` from `from`, which holds `own`, a file: each is refused.
    async fn refused(from: &Handle, own: &str, path: &str) {
        let (h, no) = (from, Some(Errno::NotPermitted));
        assert_eq!(h.open(path, read()).await.err(), no, "open {path}");
        assert_eq!(h.open(path, make()).await.err(), no, "create {path}");
        assert_eq!(h.open(path, dir()).await.err(), no, "open directory {path}");
        assert_eq!(h.stat_at(path, true).await.err(), no, "stat {path}");
        assert_eq!(h.stat_at(path, false).await.err(), no, "lstat {path}");
        assert_eq!(h.hash_at(path, false).await.err(), no, "hash {path}");
        assert_eq!(h.set_times_at(path, false, Time::Now, Time::Now).await.err(), no, "set times {path}");
        assert_eq!(h.readlink_at(path).await.err(), no, "readlink {path}");
        assert_eq!(h.mkdir_at(path).await.err(), no, "mkdir {path}");
        assert_eq!(h.symlink_at(path, "x").await.err(), no, "symlink {path}");
        assert_eq!(h.unlink_at(path).await.err(), no, "unlink {path}");
        assert_eq!(h.rmdir_at(path).await.err(), no, "rmdir {path}");
        assert_eq!(h.rename_at(path, h, "moved").await.err(), no, "rename from {path}");
        assert_eq!(h.rename_at(own, h, path).await.err(), no, "rename to {path}");
        assert_eq!(h.link_at(path, h, "linked").await.err(), no, "link from {path}");
        assert_eq!(h.link_at(own, h, path).await.err(), no, "link to {path}");
    }

    /// A symlink that is followed leads nowhere either, by whatever way it is followed.
    async fn refused_when_followed(from: &Handle, path: &str) {
        let (h, no) = (from, Some(Errno::NotPermitted));
        assert_eq!(h.open(path, read()).await.err(), no, "open {path}");
        assert_eq!(h.open(path, make()).await.err(), no, "create {path}");
        assert_eq!(h.stat_at(path, true).await.err(), no, "stat {path}");
        assert_eq!(h.hash_at(path, true).await.err(), no, "hash {path}");
        assert_eq!(h.set_times_at(path, true, Time::Now, Time::Now).await.err(), no, "set times {path}");
    }

    #[tokio::test]
    async fn handles_stay_below_the_root() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        root.mkdir_at("sub").await.unwrap();
        root.mkdir_at("sub/deeper").await.unwrap();
        put(&root, "secret", b"outside").await;
        put(&root, "sub/inside", b"in").await;
        root.symlink_at("sub/up", "../secret").await.unwrap();
        root.symlink_at("sub/high", "../../secret").await.unwrap();
        assert_eq!(root.symlink_at("sub/abs", "/secret").await, Err(Errno::NotPermitted), "refused when made");
        // Inserted as a damaged tree could hold it, to see that following one still leads nowhere.
        let sub = root.open("sub", dir()).await.unwrap();
        let mut tree = turn.tree().write().await;
        ops::create(&mut tree, sub.ino(), "abs", Kind::Link, Some("/secret")).await.unwrap();
        drop(tree);
        let before = contents(&turn, &root).await;

        // From a directory the guest opened, and from the root, nothing leads above it.
        let above = ["..", "../secret", "../sub/inside", "deeper/../..", "./../secret", "/secret", "/", "//secret"];
        for path in above.into_iter().chain(["up/x", "high/x", "abs/x", "deeper/../up/x"]) {
            refused(&sub, "inside", path).await;
        }
        for path in ["..", "../sub", "sub/../..", "./../secret", "/secret", "/", "sub/high/x", "sub/abs/x"] {
            refused(&root, "secret", path).await;
        }
        for path in ["up", "high", "abs", "deeper/../up"] {
            refused_when_followed(&sub, path).await;
        }
        for path in ["sub/high", "sub/abs"] {
            refused_when_followed(&root, path).await;
        }
        // The symlinks are in `sub`, and are named, not followed, by these.
        assert_eq!(sub.readlink_at("up").await.as_deref(), Ok("../secret"));
        assert_eq!(sub.readlink_at("high").await.as_deref(), Ok("../../secret"));
        assert!(sub.stat_at("up", false).await.is_ok());
        // From the root, a symlink may lead up out of `sub` and into the root, and no further.
        assert_eq!(text(&root, "sub/up").await.as_deref(), Ok(&b"outside"[..]));
        assert_eq!(text(&root, "secret").await.as_deref(), Ok(&b"outside"[..]));
        assert_eq!(contents(&turn, &root).await, before, "none of it changed anything");
    }

    /// A file opened to write, and not to read.
    fn write_only() -> Mode {
        Mode { follow: true, write: true, ..Default::default() }
    }

    #[tokio::test]
    async fn a_descriptor_does_what_it_was_opened_for() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        put(&root, "f", b"abc").await;
        root.mkdir_at("d").await.unwrap();
        root.symlink_at("l", "f").await.unwrap();
        let bad = Some(Errno::BadDescriptor);

        let reader = root.open("f", read()).await.unwrap();
        assert_eq!(reader.write(At::End, b"x").await, Err(Errno::BadDescriptor));
        assert_eq!(reader.set_size(0).await, Err(Errno::BadDescriptor));
        assert_eq!(reader.read(0, 10).await.unwrap(), (Bytes::from_static(b"abc"), true));
        // An offset is signed, as `pread` has it: past the largest is invalid, and not the end of the file.
        assert_eq!(reader.read(i64::MAX as u64, 1).await.unwrap(), (Bytes::new(), true));
        assert_eq!(reader.read(i64::MAX as u64 + 1, 1).await.err(), Some(Errno::Invalid));
        let writer = root.open("f", write_only()).await.unwrap();
        assert_eq!(writer.read(0, 1).await.err(), bad);
        assert_eq!(writer.write(At::End, b"d").await, Ok(1));
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"abcd"[..]));
        // A file is not a directory, and a directory is not a file.
        let dir = root.open("d", read()).await.unwrap();
        assert_eq!(dir.read(0, 1).await.err(), Some(Errno::IsDirectory));
        assert_eq!(dir.write(At::End, b"x").await, Err(Errno::IsDirectory));
        assert_eq!(root.open("d", write_only()).await.err(), Some(Errno::IsDirectory));
        assert_eq!(reader.open("x", read()).await.err(), Some(Errno::NotDirectory));
        assert_eq!(root.open("f", self::dir()).await.err(), Some(Errno::NotDirectory));
        assert_eq!(root.open("f/", read()).await.err(), Some(Errno::NotDirectory));
        // Neither is a symlink that is not followed.
        assert_eq!(root.open("l", Mode { read: true, ..Default::default() }).await.err(), Some(Errno::Loop));
        let excl = Mode { create: true, exclusive: true, ..make() };
        assert_eq!(root.open("f", excl).await.err(), Some(Errno::Exist));
        assert_eq!(root.open("d", Mode { create: true, ..self::dir() }).await.err(), Some(Errno::Invalid));
        // All of the above changed nothing.
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"abcd"[..]));
    }

    #[tokio::test]
    async fn a_directory_says_it_can_be_changed_only_if_it_was_opened_to_and_the_turn_can() {
        let turn = turn(&Store::memory()).await;
        let root = Handle::root(&turn);
        root.mkdir_at("d").await.unwrap();
        let (plain, changing) = (Mode { directory: true, ..read() }, Mode { mutate: true, ..self::dir() });
        let (d, e) = (root.open("d", plain).await.unwrap(), root.open("d", changing).await.unwrap());
        assert!(root.mutates(), "the preopen is opened to change it");
        assert!(!d.mutates() && e.mutates());
        // It is only what is said, as Wasmtime's host has it: a directory that is not said to be changed can be.
        assert!(d.mkdir_at("x").await.is_ok());
        // A turn that has answered changes nothing, and the descriptors say so.
        turn.answer();
        assert!(!root.mutates() && !e.mutates());
        assert_eq!(root.open_as("d", Mode { mutate: true, ..self::dir() }, false).await.err(), Some(Errno::ReadOnly));
        assert!(root.open_as("d", plain, false).await.is_ok());
    }

    #[tokio::test]
    async fn a_file_unlinked_while_open_is_there_until_it_is_closed() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        let file = put(&root, "f", b"abc").await;
        let (ino, twin) = (file.ino(), root.open("f", read()).await.unwrap());
        root.unlink_at("f").await.unwrap();

        assert_eq!(root.stat_at("f", false).await.err(), Some(Errno::NoEntry));
        assert_eq!(file.stat().await.unwrap().links, 0);
        assert_eq!(file.write(At::End, b"def").await, Ok(3));
        assert_eq!(twin.read(0, 10).await.unwrap().0, &b"abcdef"[..]);
        // The name is free for another file, which is not this one.
        let other = put(&root, "f", b"new").await;
        assert_ne!(other.ino(), ino);
        // It goes with the last descriptor, and not before.
        drop(file);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!gone(&turn, ino).await, "a descriptor is still open");
        assert_eq!(twin.read(0, 10).await.unwrap().0, &b"abcdef"[..]);
        drop(twin);
        removed(&turn, ino).await;
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"new"[..]));
    }

    #[tokio::test]
    async fn a_directory_or_a_replaced_file_that_is_open_is_an_orphan_too() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        root.mkdir_at("d").await.unwrap();
        let d = root.open("d", dir()).await.unwrap();
        root.rmdir_at("d").await.unwrap();
        assert_eq!(d.stat().await.unwrap().links, 0);
        assert_eq!(d.mkdir_at("x").await, Err(Errno::NoEntry), "nothing is made in a directory that is gone");
        assert_eq!(d.open("x", make()).await.err(), Some(Errno::NoEntry));
        let ino = d.ino();
        drop(d);
        removed(&turn, ino).await;

        let (old, new) = (put(&root, "old", b"old").await, put(&root, "new", b"new").await);
        let displaced = new.ino();
        root.rename_at("old", &root, "new").await.unwrap();
        assert_eq!(new.stat().await.unwrap().links, 0);
        assert_eq!(new.read(0, 10).await.unwrap().0, &b"new"[..]);
        assert_eq!(text(&root, "new").await.as_deref(), Ok(&b"old"[..]));
        assert_eq!(old.stat().await.unwrap().links, 1);
        drop(new);
        removed(&turn, displaced).await;
    }

    #[tokio::test]
    async fn a_commit_leaves_out_what_is_unlinked_and_open() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect(); // two blocks
        let (kept, file, shut) =
            (put(&root, "kept", b"kept").await, put(&root, "file", &big).await, put(&root, "shut", b"shut").await);
        let (ino, shut_ino) = (file.ino(), shut.ino());
        root.unlink_at("file").await.unwrap();
        root.unlink_at("shut").await.unwrap();
        drop(shut);
        commit(&first).await;

        // The turn has answered, and what was open reads as it did.
        assert_eq!(file.read(0, 1 << 20).await.unwrap().0, &big[..]);
        assert_eq!(kept.read(0, 10).await.unwrap().0, &b"kept"[..]);
        assert_eq!(file.write(At::End, b"x").await, Err(Errno::ReadOnly));
        assert!(first.fs.kept(ino).is_some());
        // What the next turn finds is the commit, without them or their blocks.
        let second = turn(&store).await;
        let next = Handle::root(&second);
        assert_eq!(text(&next, "kept").await.as_deref(), Ok(&b"kept"[..]));
        assert_eq!(next.stat_at("file", false).await.err(), Some(Errno::NoEntry));
        assert!(gone(&second, ino).await && gone(&second, shut_ino).await);
        // The copy of the tree it was read from goes with the last descriptor.
        drop(file);
        assert!(first.fs.kept(ino).is_none());
        assert_eq!(kept.read(0, 10).await.unwrap().0, &b"kept"[..]);
    }

    #[tokio::test]
    async fn descriptors_on_a_discarded_turn_find_nothing() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        let file = put(&root, "f", b"abc").await;
        turn.discard().await;
        // The file is not in the tree that is left, which the host explains, not the guest.
        assert_eq!(file.read(0, 10).await.err(), Some(Errno::Io));
        assert_eq!(file.write(At::End, b"x").await, Err(Errno::ReadOnly));
        assert_eq!(root.stat_at("f", false).await.err(), Some(Errno::NoEntry));
        assert_eq!(root.mkdir_at("d").await, Err(Errno::ReadOnly));
        let snap = Turn::snap(&store, "a", "n").await.unwrap();
        assert_eq!(Handle::root(&snap).stat_at("f", false).await.err(), Some(Errno::NoEntry));
    }

    #[tokio::test]
    async fn nothing_changes_after_the_answer() {
        let store = Store::memory();
        let turn = turn(&store).await;
        let root = Handle::root(&turn);
        let file = put(&root, "f", b"abc").await;
        root.mkdir_at("d").await.unwrap();
        assert!(root.mutable());
        turn.answer();
        assert!(!root.mutable());

        let changes = [
            file.write(At::End, b"x").await.map(drop),
            file.set_size(0).await,
            file.set_times(Time::Now, Time::Now).await,
            root.set_times_at("f", true, Time::Now, Time::Now).await,
            root.mkdir_at("e").await,
            root.symlink_at("l", "f").await,
            root.unlink_at("f").await,
            root.rmdir_at("d").await,
            root.rename_at("f", &root, "g").await,
            root.link_at("f", &root, "g").await,
            root.open("new", make()).await.map(drop),
            root.open("f", write_only()).await.map(drop),
            root.open("f", Mode { truncate: true, ..write_only() }).await.map(drop),
        ];
        assert!(changes.iter().all(|c| *c == Err(Errno::ReadOnly)), "{changes:?}");
        assert_eq!(file.write_task(At::End, Bytes::from_static(b"x")).err(), Some(Errno::ReadOnly));
        // Reads go on, to the commit and after it, and what it commits is what was there at the answer.
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"abc"[..]));
        commit(&turn).await;
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"abc"[..]));
        assert_eq!(file.write(At::End, b"x").await, Err(Errno::ReadOnly));
        let next = Handle::root(&self::turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"abc"[..]));
        assert!(next.stat_at("d", false).await.is_ok());
    }

    #[tokio::test]
    async fn a_snapshot_reads_and_does_not_change() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        put(&root, "f", b"abc").await;
        root.mkdir_at("d").await.unwrap();
        commit(&first).await;

        let snap = Turn::snap(&store, "a", "n").await.unwrap();
        let root = Handle::root(&snap);
        assert!(!root.mutable());
        assert_eq!(text(&root, "f").await.as_deref(), Ok(&b"abc"[..]));
        let mut names = vec![];
        let mut entries = root.entries().unwrap();
        while let Some((name, kind)) = entries.next().await.unwrap() {
            names.push((name, kind));
        }
        assert_eq!(names, [("d".to_owned(), Kind::Dir), ("f".to_owned(), Kind::File)]);
        assert_eq!(root.mkdir_at("e").await, Err(Errno::ReadOnly));
        assert_eq!(root.open("g", make()).await.err(), Some(Errno::ReadOnly));
        assert_eq!(root.open("f", write_only()).await.err(), Some(Errno::ReadOnly));
        assert_eq!(root.unlink_at("f").await, Err(Errno::ReadOnly));
    }

    #[tokio::test]
    async fn a_write_that_was_admitted_lands_before_the_commit() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"").await;
        let task = file.write_task(At::Offset(0), Bytes::from_static(b"admitted")).unwrap();
        commit(&first).await; // waits for the write, which has not run
        assert_eq!(task.await.unwrap(), Ok(8));
        assert_eq!(file.write_task(At::End, Bytes::from_static(b"x")).err(), Some(Errno::ReadOnly));
        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"admitted"[..]));
    }
}
