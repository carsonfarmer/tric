//! `wasi:filesystem@0.2` over [`Handle`]s: the descriptors, and the streams on them, that a guest of 0.2 uses, which is
//! also what a guest of `wasi:filesystem@0.1` gets through the adapter. The preopen is the one directory `/`, and only
//! for a request that addresses a name; any other has none.
//!
//! A stream is a state machine in the manner of the stock one, with a read or a write in flight as a task of its own.
//! A read holds the tree shared for its length only, so closing a stream cancels it. A write is admitted when the
//! stream takes it, and nothing cancels it: see the rules of [`super`].
use super::{Ahead, At, Entries, Errno, Fault, Handle, Kind, Mode, READ_MAX, Res, Stat, Task, Time, WRITE_CAP, wrote};
use crate::engine::Host;
use bytes::Bytes;
use std::mem;
use tokio::task::{JoinError, JoinHandle};
use wasmtime::component::{HasSelf, Linker, Resource};
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, InputStream, OutputStream, StreamError, StreamResult,
};

/// The bytes a stream reads ahead for a guest that polls it before it reads.
const READ_AHEAD: usize = 64 << 10;

wasmtime::component::bindgen!({
    path: "wit/filesystem-p2",
    world: "imports",
    imports: { default: async | trappable },
    trappable_error_type: { "wasi:filesystem/types.error-code" => Fault },
    with: {
        "wasi:io": wasmtime_wasi_io::bindings::wasi::io,
        "wasi:clocks": wasmtime_wasi::p2::bindings::clocks,
        "wasi:filesystem/types.descriptor": super::Handle,
        "wasi:filesystem/types.directory-entry-stream": super::Entries,
    },
});
use wasi::filesystem::preopens;
use wasi::filesystem::types::{
    self, Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};
use wasmtime_wasi::p2::bindings::clocks::wall_clock::Datetime;

/// Links `wasi:filesystem` and its preopens.
pub fn add_to_linker(linker: &mut Linker<Host>) -> wasmtime::Result<()> {
    types::add_to_linker::<Host, HasSelf<Host>>(linker, |h| h)?;
    preopens::add_to_linker::<Host, HasSelf<Host>>(linker, |h| h)
}

impl From<Errno> for ErrorCode {
    fn from(e: Errno) -> Self {
        match e {
            Errno::BadDescriptor => Self::BadDescriptor,
            Errno::Busy => Self::Busy,
            Errno::Exist => Self::Exist,
            Errno::FileTooLarge => Self::FileTooLarge,
            Errno::Invalid => Self::Invalid,
            Errno::Io => Self::Io,
            Errno::IsDirectory => Self::IsDirectory,
            Errno::Loop => Self::Loop,
            Errno::NameTooLong => Self::NameTooLong,
            Errno::NoEntry => Self::NoEntry,
            Errno::InsufficientMemory => Self::InsufficientMemory,
            Errno::InsufficientSpace => Self::InsufficientSpace,
            Errno::NotDirectory => Self::NotDirectory,
            Errno::NotEmpty => Self::NotEmpty,
            Errno::Overflow => Self::Overflow,
            Errno::Unsupported => Self::Unsupported,
            Errno::NotPermitted => Self::NotPermitted,
            Errno::ReadOnly => Self::ReadOnly,
            Errno::TooManyLinks => Self::TooManyLinks,
        }
    }
}

impl From<Kind> for DescriptorType {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Dir => Self::Directory,
            Kind::File => Self::RegularFile,
            Kind::Link => Self::SymbolicLink,
        }
    }
}

fn datetime(nanos: u64) -> Datetime {
    Datetime { seconds: nanos / 1_000_000_000, nanoseconds: (nanos % 1_000_000_000) as u32 }
}

impl From<Stat> for DescriptorStat {
    fn from(s: Stat) -> Self {
        Self {
            type_: s.kind.into(),
            link_count: s.links,
            size: s.size,
            data_access_timestamp: Some(datetime(s.atime)),
            data_modification_timestamp: Some(datetime(s.mtime)),
            status_change_timestamp: Some(datetime(s.ctime)),
        }
    }
}

fn time(t: NewTimestamp) -> Res<Time> {
    match t {
        NewTimestamp::NoChange => Ok(Time::Keep),
        NewTimestamp::Now => Ok(Time::Now),
        NewTimestamp::Timestamp(d) => Time::at(d.seconds, d.nanoseconds),
    }
}

impl preopens::Host for Host {
    async fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Handle>, String)>> {
        self.preopen().await
    }
}

impl types::Host for Host {
    fn convert_error_code(&mut self, err: Fault) -> wasmtime::Result<ErrorCode> {
        match err {
            Fault::Errno(e) => Ok(e.into()),
            Fault::Trap(e) => Err(e),
        }
    }

    async fn filesystem_error_code(&mut self, err: Resource<wasmtime::Error>) -> wasmtime::Result<Option<ErrorCode>> {
        Ok(self.table.get(&err)?.downcast_ref::<Errno>().map(|e| (*e).into()))
    }
}

impl types::HostDescriptor for Host {
    async fn read_via_stream(
        &mut self,
        fd: Resource<Handle>,
        offset: Filesize,
    ) -> Result<Resource<DynInputStream>, Fault> {
        let file = self.fd(&fd)?;
        file.reading()?;
        let stream: DynInputStream = Box::new(Input::new(file, offset));
        self.add(stream)
    }

    async fn write_via_stream(
        &mut self,
        fd: Resource<Handle>,
        offset: Filesize,
    ) -> Result<Resource<DynOutputStream>, Fault> {
        self.output(&fd, At::Offset(offset))
    }

    async fn append_via_stream(&mut self, fd: Resource<Handle>) -> Result<Resource<DynOutputStream>, Fault> {
        self.output(&fd, At::End)
    }

    async fn advise(
        &mut self,
        fd: Resource<Handle>,
        _offset: Filesize,
        _length: Filesize,
        _advice: Advice,
    ) -> Result<(), Fault> {
        match self.fd(&fd)?.kind() {
            Kind::File => Ok(()),
            _ => Err(Errno::BadDescriptor.into()),
        }
    }

    async fn sync_data(&mut self, fd: Resource<Handle>) -> Result<(), Fault> {
        self.fd(&fd).map(drop) // a commit is the only sync there is
    }

    async fn get_flags(&mut self, fd: Resource<Handle>) -> Result<DescriptorFlags, Fault> {
        let file = self.fd(&fd)?;
        let mut flags = DescriptorFlags::empty();
        if file.readable() {
            flags |= DescriptorFlags::READ;
        }
        if file.writable() {
            flags |= DescriptorFlags::WRITE;
        }
        if file.mutates() {
            flags |= DescriptorFlags::MUTATE_DIRECTORY;
        }
        Ok(flags)
    }

    async fn get_type(&mut self, fd: Resource<Handle>) -> Result<DescriptorType, Fault> {
        Ok(self.fd(&fd)?.kind().into())
    }

    async fn set_size(&mut self, fd: Resource<Handle>, size: Filesize) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.set_size(size).await?)
    }

    async fn set_times(&mut self, fd: Resource<Handle>, atime: NewTimestamp, mtime: NewTimestamp) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.set_times(time(atime)?, time(mtime)?).await?)
    }

    async fn read(
        &mut self,
        fd: Resource<Handle>,
        length: Filesize,
        offset: Filesize,
    ) -> Result<(Vec<u8>, bool), Fault> {
        let (data, end) = self.fd(&fd)?.read(offset, length).await?;
        Ok((data.into(), end))
    }

    async fn write(&mut self, fd: Resource<Handle>, buffer: Vec<u8>, offset: Filesize) -> Result<Filesize, Fault> {
        Ok(self.fd(&fd)?.write(At::Offset(offset), &buffer).await? as Filesize)
    }

    async fn read_directory(&mut self, fd: Resource<Handle>) -> Result<Resource<Entries>, Fault> {
        let entries = self.fd(&fd)?.entries()?;
        self.add(entries)
    }

    async fn sync(&mut self, fd: Resource<Handle>) -> Result<(), Fault> {
        self.fd(&fd).map(drop)
    }

    async fn create_directory_at(&mut self, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.mkdir_at(&path).await?)
    }

    async fn stat(&mut self, fd: Resource<Handle>) -> Result<DescriptorStat, Fault> {
        Ok(self.fd(&fd)?.stat().await?.into())
    }

    async fn stat_at(
        &mut self,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
    ) -> Result<DescriptorStat, Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        Ok(self.fd(&fd)?.stat_at(&path, follow).await?.into())
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
        atime: NewTimestamp,
        mtime: NewTimestamp,
    ) -> Result<(), Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        Ok(self.fd(&fd)?.set_times_at(&path, follow, time(atime)?, time(mtime)?).await?)
    }

    async fn link_at(
        &mut self,
        fd: Resource<Handle>,
        old_path_flags: PathFlags,
        old_path: String,
        new_fd: Resource<Handle>,
        new_path: String,
    ) -> Result<(), Fault> {
        let (old_dir, new_dir) = (self.fd(&fd)?, self.fd(&new_fd)?);
        if old_path_flags.contains(PathFlags::SYMLINK_FOLLOW) {
            return Err(Errno::Invalid.into()); // a link to what a symlink points to is not made
        }
        Ok(old_dir.link_at(&old_path, &new_dir, &new_path).await?)
    }

    async fn open_at(
        &mut self,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
        open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> Result<Resource<Handle>, Fault> {
        let dir = self.fd(&fd)?;
        let sync = DescriptorFlags::FILE_INTEGRITY_SYNC
            | DescriptorFlags::DATA_INTEGRITY_SYNC
            | DescriptorFlags::REQUESTED_WRITE_SYNC;
        let mode = Mode {
            follow: path_flags.contains(PathFlags::SYMLINK_FOLLOW),
            create: open_flags.contains(OpenFlags::CREATE),
            directory: open_flags.contains(OpenFlags::DIRECTORY),
            exclusive: open_flags.contains(OpenFlags::EXCLUSIVE),
            truncate: open_flags.contains(OpenFlags::TRUNCATE),
            read: flags.contains(DescriptorFlags::READ),
            write: flags.contains(DescriptorFlags::WRITE),
            mutate: flags.contains(DescriptorFlags::MUTATE_DIRECTORY),
        };
        let file = dir.open_as(&path, mode, flags.intersects(sync)).await?;
        self.add(file)
    }

    async fn readlink_at(&mut self, fd: Resource<Handle>, path: String) -> Result<String, Fault> {
        Ok(self.fd(&fd)?.readlink_at(&path).await?)
    }

    async fn remove_directory_at(&mut self, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.rmdir_at(&path).await?)
    }

    async fn rename_at(
        &mut self,
        fd: Resource<Handle>,
        old_path: String,
        new_fd: Resource<Handle>,
        new_path: String,
    ) -> Result<(), Fault> {
        let (old_dir, new_dir) = (self.fd(&fd)?, self.fd(&new_fd)?);
        Ok(old_dir.rename_at(&old_path, &new_dir, &new_path).await?)
    }

    async fn symlink_at(&mut self, fd: Resource<Handle>, old_path: String, new_path: String) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.symlink_at(&new_path, &old_path).await?) // `old_path` is what the link says
    }

    async fn unlink_file_at(&mut self, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(self.fd(&fd)?.unlink_at(&path).await?)
    }

    async fn is_same_object(&mut self, fd: Resource<Handle>, other: Resource<Handle>) -> wasmtime::Result<bool> {
        Ok(self.table.get(&fd)?.same(self.table.get(&other)?))
    }

    async fn metadata_hash(&mut self, fd: Resource<Handle>) -> Result<MetadataHashValue, Fault> {
        let (lower, upper) = self.fd(&fd)?.hash();
        Ok(MetadataHashValue { lower, upper })
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
    ) -> Result<MetadataHashValue, Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let (lower, upper) = self.fd(&fd)?.hash_at(&path, follow).await?;
        Ok(MetadataHashValue { lower, upper })
    }

    async fn drop(&mut self, fd: Resource<Handle>) -> wasmtime::Result<()> {
        Ok(self.table.delete(fd).map(drop)?)
    }
}

impl Host {
    /// A stream that writes the file from `at`: if the turn can be written.
    fn output(&mut self, fd: &Resource<Handle>, at: At) -> Result<Resource<DynOutputStream>, Fault> {
        let file = self.writer(fd)?;
        let stream: DynOutputStream = Box::new(Output { file, at, state: Write::Ready });
        self.add(stream)
    }
}

impl types::HostDirectoryEntryStream for Host {
    async fn read_directory_entry(&mut self, stream: Resource<Entries>) -> Result<Option<DirectoryEntry>, Fault> {
        let entries = self.table.get_mut(&stream)?;
        Ok(entries.next().await?.map(|(name, kind)| DirectoryEntry { type_: kind.into(), name }))
    }

    async fn drop(&mut self, stream: Resource<Entries>) -> wasmtime::Result<()> {
        Ok(self.table.delete(stream).map(drop)?)
    }
}

enum Read {
    /// Nothing is read, or held.
    Idle,
    /// A read is under way.
    Waiting(Task<Res<(Bytes, bool)>>),
    /// What a read found, which the guest has not taken.
    Data(Bytes),
    /// The read failed, which the guest is told once.
    Error(Errno),
    /// The end of the file, or the failure told.
    Closed,
}

/// A stream that reads a file from an offset.
struct Input {
    file: Handle,
    offset: u64,
    state: Read,
    ahead: Ahead,
}

impl Input {
    fn new(file: Handle, offset: u64) -> Self {
        Self { file, offset, state: Read::Idle, ahead: Ahead::default() }
    }

    fn start(&mut self, len: usize) {
        let (file, offset) = (self.file.clone(), self.offset);
        self.state = Read::Waiting(Task(tokio::spawn(async move { file.read(offset, len as u64).await })));
    }

    /// Waits for the read under way, if one is.
    async fn wait(&mut self) {
        if let Read::Waiting(task) = &mut self.state {
            let read = (&mut task.0).await;
            self.land(read);
        }
    }

    /// Takes what a read that started at the offset found.
    fn land(&mut self, read: Result<Res<(Bytes, bool)>, JoinError>) {
        self.state = match read {
            Ok(Ok((data, _))) if data.is_empty() => Read::Closed,
            Ok(Ok((data, _))) => {
                self.ahead.landed(&self.file, self.offset, data.len() as u64);
                Read::Data(data)
            }
            Ok(Err(e)) => Read::Error(e),
            Err(e) => {
                tracing::warn!("fs: a read failed to finish: {e}");
                Read::Error(Errno::Io)
            }
        };
    }
}

#[wasmtime_wasi_io::async_trait]
impl InputStream for Input {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        match &mut self.state {
            Read::Idle => {
                if size > 0 {
                    self.start(size.min(READ_MAX as usize));
                }
                Ok(Bytes::new())
            }
            Read::Waiting(_) => Ok(Bytes::new()),
            Read::Data(data) => {
                let chunk = data.split_to(data.len().min(size));
                if data.is_empty() {
                    self.state = Read::Idle;
                }
                self.offset += chunk.len() as u64;
                Ok(chunk)
            }
            Read::Error(e) => {
                let e = *e;
                self.state = Read::Closed;
                Err(StreamError::LastOperationFailed(e.into()))
            }
            Read::Closed => Err(StreamError::Closed),
        }
    }

    /// Reads at once, rather than as a task of its own to wait for.
    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        self.wait().await;
        if let (Read::Idle, true) = (&self.state, size > 0) {
            let read = self.file.read(self.offset, size.min(READ_MAX as usize) as u64).await;
            self.land(Ok(read));
        }
        self.read(size)
    }

    async fn cancel(&mut self) {
        self.state = Read::Closed;
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for Input {
    async fn ready(&mut self) {
        if let Read::Idle = self.state {
            self.start(READ_AHEAD);
        }
        self.wait().await;
    }
}

enum Write {
    Ready,
    /// A write is under way, of this many bytes.
    Waiting(JoinHandle<Res<usize>>, usize),
    /// The write failed, which the guest is told once.
    Error(Errno),
    Closed,
}

/// A stream that writes a file from an offset, or at its end.
struct Output {
    file: Handle,
    at: At,
    state: Write,
}

impl Output {
    /// The error that stopped the stream, which is told once, and then it is closed.
    fn failed(&mut self) -> StreamError {
        match mem::replace(&mut self.state, Write::Closed) {
            Write::Error(e) => StreamError::LastOperationFailed(e.into()),
            _ => StreamError::Closed,
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for Output {
    fn write(&mut self, buf: Bytes) -> StreamResult<()> {
        match self.state {
            Write::Ready => {}
            Write::Closed => return Err(StreamError::Closed),
            Write::Waiting(..) | Write::Error(_) => {
                return Err(StreamError::trap("write not permitted: check_write not called first"));
            }
        }
        if buf.len() > WRITE_CAP {
            return Err(StreamError::trap("cannot write more than check_write allows"));
        }
        let len = buf.len();
        if len > 0 {
            self.state = match self.file.write_task(self.at, buf) {
                Ok(task) => Write::Waiting(task, len),
                Err(e) => Write::Error(e), // told at the next call that can tell it
            };
        }
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        match self.state {
            Write::Ready | Write::Waiting(..) => Ok(()),
            Write::Error(_) | Write::Closed => Err(self.failed()),
        }
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        match self.state {
            Write::Ready => Ok(WRITE_CAP),
            Write::Waiting(..) => Ok(0),
            Write::Error(_) | Write::Closed => Err(self.failed()),
        }
    }

    /// Lets go of a write under way, which finishes by itself: it was admitted, and is not cancelled.
    async fn cancel(&mut self) {
        self.state = Write::Closed;
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for Output {
    async fn ready(&mut self) {
        if let Write::Waiting(task, len) = &mut self.state {
            let len = *len;
            self.state = match wrote(task.await, len) {
                Ok(()) => {
                    if let At::Offset(p) = &mut self.at {
                        *p += len as u64;
                    }
                    Write::Ready
                }
                Err(e) => Write::Error(e),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ops::BLOCK;
    use super::super::tests::{commit, put, read, text, turn};
    use super::*;
    use crate::store::Store;
    use crate::tree::counting::counting;
    use std::sync::Arc;
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    fn reader(file: &Handle, offset: u64) -> Input {
        Input::new(file.clone(), offset)
    }

    fn writer(file: &Handle, at: At) -> Output {
        Output { file: file.clone(), at, state: Write::Ready }
    }

    /// The error a stream tells, which is one of the file system's.
    fn told<T>(result: StreamResult<T>) -> Option<Errno> {
        match result {
            Err(StreamError::LastOperationFailed(e)) => e.downcast_ref::<Errno>().copied(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_stream_that_writes_outlives_the_answer() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"").await;
        let mut out = writer(&file, At::Offset(0));
        assert_eq!(out.check_write().unwrap(), WRITE_CAP);
        out.write(Bytes::from_static(b"hello")).unwrap();
        assert_eq!(out.check_write().unwrap(), 0, "the write is under way");

        // The commit waits for a write that was taken, and the stream finds it done.
        commit(&first).await;
        out.ready().await;
        assert_eq!(out.check_write().unwrap(), WRITE_CAP);
        // The next write comes after the answer: it is refused, once, and then the stream is closed.
        out.write(Bytes::from_static(b" world")).unwrap();
        assert_eq!(told(out.check_write()), Some(Errno::ReadOnly));
        assert!(matches!(out.check_write(), Err(StreamError::Closed)));
        assert!(matches!(out.write(Bytes::from_static(b"x")), Err(StreamError::Closed)));

        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"hello"[..]));
    }

    #[tokio::test]
    async fn a_stream_that_is_closed_leaves_its_write_to_finish() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"").await;
        let mut out = writer(&file, At::Offset(0));
        out.write(Bytes::from_static(b"kept")).unwrap();
        out.cancel().await;
        drop(out);
        commit(&first).await;
        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"kept"[..]));
    }

    #[tokio::test]
    async fn a_stream_that_reads_outlives_the_answer() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"0123456789").await;
        let mut input = reader(&file, 2);
        assert!(input.read(4).unwrap().is_empty(), "a read is started");
        commit(&first).await;

        input.ready().await;
        assert_eq!(input.read(4).unwrap(), &b"2345"[..]);
        assert_eq!(input.blocking_read(100).await.unwrap(), &b"6789"[..]);
        assert!(matches!(input.blocking_read(100).await, Err(StreamError::Closed)), "the end of the file");
    }

    #[tokio::test]
    async fn a_stream_that_reads_on_has_the_blocks_after_in_the_cache() {
        let (store, counts) = counting();
        let first = turn(&store).await;
        let block = BLOCK as usize;
        let big: Vec<u8> = (0..block * 12).map(|i| (i % 251) as u8).collect();
        put(&Handle::root(&first), "f", &big).await;
        commit(&first).await;

        // The next request is on another instance, which has read nothing.
        let cold = Store { cache: Arc::default(), ..store };
        let file = Handle::root(&turn(&cold).await).open("f", read()).await.unwrap();
        counts.gets.store(0, SeqCst);
        let mut input = reader(&file, 0);
        let gets = || counts.gets.load(SeqCst);

        // The first read is what it asked for, and no more is fetched: it may be the only one.
        assert_eq!(input.blocking_read(block).await.unwrap(), &big[..block]);
        input.ahead.settled().await;
        assert_eq!(gets(), 1);
        // The second carries on from it, and the blocks that follow are fetched, to a megabyte ahead.
        assert_eq!(input.blocking_read(block).await.unwrap(), &big[block..block * 2]);
        input.ahead.settled().await;
        assert_eq!(gets(), 2 + 4);
        // So the reads that come to those ask nothing of the store, and each brings the next block in.
        for i in 2..12 {
            assert_eq!(input.blocking_read(block).await.unwrap(), &big[i * block..(i + 1) * block]);
            input.ahead.settled().await;
            assert_eq!(gets(), (i + 5).min(12), "read {i}");
        }
        assert!(matches!(input.blocking_read(block).await, Err(StreamError::Closed)));
        assert_eq!(gets(), 12, "every block was fetched once, and none past the end");
    }

    #[tokio::test]
    async fn a_stream_that_is_closed_stops_reading_ahead() {
        let (store, counts) = counting();
        let first = turn(&store).await;
        let block = BLOCK as usize;
        put(&Handle::root(&first), "f", &vec![1; block * 12]).await;
        commit(&first).await;
        let cold = Store { cache: Arc::default(), ..store };
        let file = Handle::root(&turn(&cold).await).open("f", read()).await.unwrap();

        let mut input = reader(&file, 0);
        input.blocking_read(block).await.unwrap();
        file.read(block as u64, block as u64).await.unwrap(); // so the next read of the stream asks nothing
        counts.delay.store(60_000, SeqCst);
        input.blocking_read(block).await.unwrap();
        // The blocks after it are being fetched, by a task that has the file open.
        let waiting = || counts.flight.load(SeqCst) == 4 && Arc::strong_count(&file.pin) == 3;
        timeout(Duration::from_secs(5), async {
            while !waiting() {
                sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the read-ahead is under way");

        drop(input);
        let released = || Arc::strong_count(&file.pin) == 1;
        timeout(Duration::from_secs(5), async {
            while !released() {
                sleep(Duration::from_millis(5)).await
            }
        })
        .await
        .expect("the read-ahead was cancelled with the stream");
    }

    #[tokio::test]
    async fn a_stream_on_a_file_the_commit_left_out_reads_it() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let file = put(&root, "f", &big).await;
        let mut input = reader(&file, 0);
        root.unlink_at("f").await.unwrap();
        commit(&first).await;
        assert!(first.fs.kept(file.ino()).is_some());

        assert_eq!(input.blocking_read(1 << 20).await.unwrap(), &big[..]);
        drop((input, file));
    }

    #[tokio::test]
    async fn a_stream_on_a_discarded_turn_is_told_io_and_then_closed() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"abc").await;
        first.discard().await;

        let mut input = reader(&file, 0);
        assert_eq!(told(input.blocking_read(10).await), Some(Errno::Io));
        assert!(matches!(input.blocking_read(10).await, Err(StreamError::Closed)));
        let mut out = writer(&file, At::End);
        out.write(Bytes::from_static(b"x")).unwrap();
        assert_eq!(told(out.check_write()), Some(Errno::ReadOnly));
    }
}
