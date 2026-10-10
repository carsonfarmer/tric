//! `wasi:filesystem@0.3` over [`Handle`]s: the same descriptors as [`super::p2`], with the streams of the component
//! model's own async, which carry a read, a write or a listing, and a future that tells how it ended. The preopen is
//! the one directory `/`, and only for a request that addresses a name; any other has none.
//!
//! The stream of a read or a listing is a producer, and the stream of a write is a consumer, which the guest's runtime
//! polls. A producer reads when it is polled and the guest has room, and drops the read it is waiting for if the guest
//! cancels, so it holds nothing that is not its own: the tree is held shared to plan a read, and not to fetch it, and a
//! read that is dropped is made again. A consumer takes what the guest writes as a write of its own, which is admitted
//! then and nothing cancels: it waits for it to land before it takes more, and a stream that is closed leaves the
//! write to finish, and tells the future how it ended. See the rules of [`super`].
use super::{Ahead, At, Entries, Errno, Fault, Handle, Kind, Mode, PAGE, READ_MAX, Res, Stat, Time, WRITE_CAP, wrote};
use crate::engine::Host;
use bytes::Bytes;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::{iter, mem};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasSelf, Linker, Resource, Source, StreamConsumer, StreamProducer,
    StreamReader, StreamResult, VecBuffer,
};

/// The bytes a stream reads at a time at the least, for a guest that has room for fewer, and for the host.
const CHUNK: usize = 64 << 10;

wasmtime::component::bindgen!({
    path: "wit/filesystem-p3",
    world: "imports",
    imports: {
        "wasi:filesystem/types.[method]descriptor.read-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.write-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.append-via-stream": store | trappable,
        "wasi:filesystem/types.[method]descriptor.read-directory": store | trappable,
        "wasi:filesystem/preopens.get-directories": async | trappable,
        default: trappable,
    },
    trappable_error_type: { "wasi:filesystem/types.error-code" => Fault },
    with: {
        "wasi:clocks": wasmtime_wasi::p3::bindings::clocks,
        "wasi:filesystem/types.descriptor": super::Handle,
    },
});
use wasi::filesystem::preopens;
use wasi::filesystem::types::{
    self, Advice, DescriptorFlags, DescriptorStat, DescriptorType, DirectoryEntry, ErrorCode, Filesize,
    MetadataHashValue, NewTimestamp, OpenFlags, PathFlags,
};
use wasmtime_wasi::p3::bindings::clocks::system_clock::Instant;

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

fn instant(nanos: u64) -> Instant {
    Instant { seconds: (nanos / 1_000_000_000) as i64, nanoseconds: (nanos % 1_000_000_000) as u32 }
}

impl From<Stat> for DescriptorStat {
    fn from(s: Stat) -> Self {
        Self {
            type_: s.kind.into(),
            link_count: s.links,
            size: s.size,
            data_access_timestamp: Some(instant(s.atime)),
            data_modification_timestamp: Some(instant(s.mtime)),
            status_change_timestamp: Some(instant(s.ctime)),
        }
    }
}

/// The time to set. A time before the epoch is one the tree cannot hold.
fn time(t: NewTimestamp) -> Res<Time> {
    match t {
        NewTimestamp::NoChange => Ok(Time::Keep),
        NewTimestamp::Now => Ok(Time::Now),
        NewTimestamp::Timestamp(i) => Time::at(u64::try_from(i.seconds).map_err(|_| Errno::Overflow)?, i.nanoseconds),
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
}

impl types::HostDescriptor for Host {
    fn drop(&mut self, fd: Resource<Handle>) -> wasmtime::Result<()> {
        Ok(self.table.delete(fd).map(drop)?)
    }
}

/// The descriptors a call names, cloned out of the table so that nothing of the table is held across an await.
trait Descriptors {
    fn fd(&self, fd: &Resource<Handle>) -> Result<Handle, Fault>;
    fn fds(&self, a: &Resource<Handle>, b: &Resource<Handle>) -> Result<(Handle, Handle), Fault>;
}

impl<U> Descriptors for Accessor<U, HasSelf<Host>> {
    fn fd(&self, fd: &Resource<Handle>) -> Result<Handle, Fault> {
        self.with(|mut store| store.get().fd(fd))
    }

    fn fds(&self, a: &Resource<Handle>, b: &Resource<Handle>) -> Result<(Handle, Handle), Fault> {
        self.with(|mut store| {
            let host = store.get();
            Ok((host.fd(a)?, host.fd(b)?))
        })
    }
}

/// The result of a stream's future, told to it once.
type Done = oneshot::Sender<Result<(), ErrorCode>>;

impl<U> types::HostDescriptorWithStore<U> for HasSelf<Host> {
    fn read_via_stream(
        mut store: Access<'_, U, Self>,
        fd: Resource<Handle>,
        offset: Filesize,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        let file = store.get().table.get(&fd)?.clone();
        let (tx, rx) = oneshot::channel();
        // A directory, or a descriptor that is not for reading, has a stream with nothing in it, and says why.
        let stream = match file.reading() {
            Ok(()) => StreamReader::new(&mut store, ReadStream::new(file, offset, tx))?,
            Err(e) => {
                _ = tx.send(Err(e.into()));
                StreamReader::new(&mut store, iter::empty::<u8>())?
            }
        };
        Ok((stream, FutureReader::new(&mut store, rx)?))
    }

    fn write_via_stream(
        mut store: Access<'_, U, Self>,
        fd: Resource<Handle>,
        data: StreamReader<u8>,
        offset: Filesize,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write(&mut store, fd, data, At::Offset(offset))
    }

    fn append_via_stream(
        mut store: Access<'_, U, Self>,
        fd: Resource<Handle>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        write(&mut store, fd, data, At::End)
    }

    fn read_directory(
        mut store: Access<'_, U, Self>,
        fd: Resource<Handle>,
    ) -> wasmtime::Result<(StreamReader<DirectoryEntry>, FutureReader<Result<(), ErrorCode>>)> {
        let dir = store.get().table.get(&fd)?.clone();
        let (tx, rx) = oneshot::channel();
        let stream = match dir.entries() {
            Ok(entries) => StreamReader::new(&mut store, ListStream { entries, fetch: None, done: Some(tx) })?,
            Err(e) => {
                _ = tx.send(Err(e.into()));
                StreamReader::new(&mut store, iter::empty::<DirectoryEntry>())?
            }
        };
        Ok((stream, FutureReader::new(&mut store, rx)?))
    }

    async fn advise(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        _offset: Filesize,
        _length: Filesize,
        _advice: Advice,
    ) -> Result<(), Fault> {
        match store.fd(&fd)?.kind() {
            Kind::File => Ok(()),
            _ => Err(Errno::BadDescriptor.into()),
        }
    }

    async fn sync_data(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<(), Fault> {
        store.fd(&fd).map(drop) // a commit is the only sync there is
    }

    async fn get_flags(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<DescriptorFlags, Fault> {
        let file = store.fd(&fd)?;
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

    async fn get_type(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<DescriptorType, Fault> {
        Ok(store.fd(&fd)?.kind().into())
    }

    async fn set_size(store: &Accessor<U, Self>, fd: Resource<Handle>, size: Filesize) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.set_size(size).await?)
    }

    async fn set_times(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        atime: NewTimestamp,
        mtime: NewTimestamp,
    ) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.set_times(time(atime)?, time(mtime)?).await?)
    }

    async fn sync(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<(), Fault> {
        store.fd(&fd).map(drop)
    }

    async fn create_directory_at(store: &Accessor<U, Self>, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.mkdir_at(&path).await?)
    }

    async fn stat(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<DescriptorStat, Fault> {
        Ok(store.fd(&fd)?.stat().await?.into())
    }

    async fn stat_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
    ) -> Result<DescriptorStat, Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        Ok(store.fd(&fd)?.stat_at(&path, follow).await?.into())
    }

    async fn set_times_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
        atime: NewTimestamp,
        mtime: NewTimestamp,
    ) -> Result<(), Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        Ok(store.fd(&fd)?.set_times_at(&path, follow, time(atime)?, time(mtime)?).await?)
    }

    async fn link_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        old_path_flags: PathFlags,
        old_path: String,
        new_fd: Resource<Handle>,
        new_path: String,
    ) -> Result<(), Fault> {
        let (old_dir, new_dir) = store.fds(&fd, &new_fd)?;
        if old_path_flags.contains(PathFlags::SYMLINK_FOLLOW) {
            return Err(Errno::Invalid.into()); // a link to what a symlink points to is not made
        }
        Ok(old_dir.link_at(&old_path, &new_dir, &new_path).await?)
    }

    async fn open_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
        open_flags: OpenFlags,
        flags: DescriptorFlags,
    ) -> Result<Resource<Handle>, Fault> {
        let dir = store.fd(&fd)?;
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
        store.with(|mut store| store.get().add(file))
    }

    async fn readlink_at(store: &Accessor<U, Self>, fd: Resource<Handle>, path: String) -> Result<String, Fault> {
        Ok(store.fd(&fd)?.readlink_at(&path).await?)
    }

    async fn remove_directory_at(store: &Accessor<U, Self>, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.rmdir_at(&path).await?)
    }

    async fn rename_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        old_path: String,
        new_fd: Resource<Handle>,
        new_path: String,
    ) -> Result<(), Fault> {
        let (old_dir, new_dir) = store.fds(&fd, &new_fd)?;
        Ok(old_dir.rename_at(&old_path, &new_dir, &new_path).await?)
    }

    async fn symlink_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        old_path: String,
        new_path: String,
    ) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.symlink_at(&new_path, &old_path).await?) // `old_path` is what the link says
    }

    async fn unlink_file_at(store: &Accessor<U, Self>, fd: Resource<Handle>, path: String) -> Result<(), Fault> {
        Ok(store.fd(&fd)?.unlink_at(&path).await?)
    }

    async fn is_same_object(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        other: Resource<Handle>,
    ) -> wasmtime::Result<bool> {
        store.with(|mut store| {
            let table = &store.get().table;
            Ok(table.get(&fd)?.same(table.get(&other)?))
        })
    }

    async fn metadata_hash(store: &Accessor<U, Self>, fd: Resource<Handle>) -> Result<MetadataHashValue, Fault> {
        let (lower, upper) = store.fd(&fd)?.hash();
        Ok(MetadataHashValue { lower, upper })
    }

    async fn metadata_hash_at(
        store: &Accessor<U, Self>,
        fd: Resource<Handle>,
        path_flags: PathFlags,
        path: String,
    ) -> Result<MetadataHashValue, Fault> {
        let follow = path_flags.contains(PathFlags::SYMLINK_FOLLOW);
        let (lower, upper) = store.fd(&fd)?.hash_at(&path, follow).await?;
        Ok(MetadataHashValue { lower, upper })
    }
}

/// Pipes `data` into the file from `at`, if the turn can be written; or else closes it, and says why on the future.
fn write<U>(
    store: &mut Access<'_, U, HasSelf<Host>>,
    fd: Resource<Handle>,
    mut data: StreamReader<u8>,
    at: At,
) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
    let (tx, rx) = oneshot::channel();
    match store.get().writer(&fd) {
        Ok(file) => data.pipe(&mut *store, WriteSink { file, at, task: None, done: Some(tx) })?,
        Err(Fault::Errno(e)) => {
            data.close(&mut *store)?;
            _ = tx.send(Err(e.into()));
        }
        Err(Fault::Trap(e)) => return Err(e),
    }
    FutureReader::new(&mut *store, rx)
}

/// A stream that reads a file from an offset. The offset moves on when what was read is delivered, so a read that is
/// dropped is made again from the same place.
struct ReadStream {
    file: Handle,
    offset: u64,
    ahead: Ahead,
    /// The read under way, which is dropped if the guest cancels, so as to hold nothing.
    fetch: Option<BoxFuture<'static, Res<(Bytes, bool)>>>,
    done: Option<Done>,
}

impl ReadStream {
    fn new(file: Handle, offset: u64, done: Done) -> Self {
        Self { file, offset, ahead: Ahead::default(), fetch: None, done: Some(done) }
    }

    /// Ends the stream's future, if it has not ended.
    fn close(&mut self, result: Res<()>) {
        if let Some(done) = self.done.take() {
            _ = done.send(result.map_err(Into::into));
        }
    }
}

impl Drop for ReadStream {
    fn drop(&mut self) {
        self.close(Ok(())); // the guest closed the stream
    }
}

impl<D> StreamProducer<D> for ReadStream {
    type Item = u8;
    type Buffer = Bytes;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, u8, Bytes>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        let want = dst.remaining(&mut store);
        if want == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed)); // a guest that only waits for something to read
        }
        let len = want.map_or(CHUNK, |n| n.clamp(CHUNK, READ_MAX as usize)) as u64;
        let mut fetch = me.fetch.take().unwrap_or_else(|| {
            let (file, offset) = (me.file.clone(), me.offset);
            async move { file.read(offset, len).await }.boxed()
        });
        let read = match fetch.as_mut().poll(cx) {
            Poll::Ready(read) => read,
            Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)), // `fetch` is dropped
            Poll::Pending => {
                me.fetch = Some(fetch);
                return Poll::Pending;
            }
        };
        Poll::Ready(Ok(match read {
            Ok((data, _)) if data.is_empty() => {
                me.close(Ok(()));
                StreamResult::Dropped
            }
            Ok((data, _)) => {
                me.ahead.landed(&me.file, me.offset, data.len() as u64);
                me.offset += data.len() as u64;
                dst.set_buffer(data);
                StreamResult::Completed
            }
            Err(e) => {
                me.close(Err(e));
                StreamResult::Dropped
            }
        }))
    }
}

/// A stream that lists a directory, a page at a time, as the tree has the entries when each page is read.
struct ListStream {
    entries: Entries,
    /// The page being read, which is dropped if the guest cancels.
    fetch: Option<BoxFuture<'static, Res<super::Page>>>,
    done: Option<Done>,
}

impl ListStream {
    fn close(&mut self, result: Res<()>) {
        if let Some(done) = self.done.take() {
            _ = done.send(result.map_err(Into::into));
        }
    }
}

impl Drop for ListStream {
    fn drop(&mut self) {
        self.close(Ok(()));
    }
}

impl<D> StreamProducer<D> for ListStream {
    type Item = DirectoryEntry;
    type Buffer = VecBuffer<DirectoryEntry>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        mut dst: Destination<'a, DirectoryEntry, VecBuffer<DirectoryEntry>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        let want = dst.remaining(&mut store);
        if want == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        loop {
            let mut batch = Vec::new();
            while batch.len() < want.map_or(PAGE, |n| n.min(PAGE)) {
                let Some((name, kind)) = me.entries.pop() else { break };
                batch.push(DirectoryEntry { type_: kind.into(), name });
            }
            if !batch.is_empty() {
                dst.set_buffer(batch.into());
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            // The page is used up: the next, or the end.
            let fetch = me.fetch.take().or_else(|| me.entries.more());
            let Some(mut fetch) = fetch else {
                me.close(Ok(()));
                return Poll::Ready(Ok(StreamResult::Dropped));
            };
            match fetch.as_mut().poll(cx) {
                Poll::Ready(Ok(page)) => me.entries.fill(page),
                Poll::Ready(Err(e)) => {
                    me.close(Err(e));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => {
                    me.fetch = Some(fetch);
                    return Poll::Pending;
                }
            }
        }
    }
}

/// A stream that writes a file from an offset, or at its end, a write at a time.
struct WriteSink {
    file: Handle,
    at: At,
    /// The write under way, and how long it is.
    task: Option<(JoinHandle<Res<usize>>, usize)>,
    done: Option<Done>,
}

impl WriteSink {
    fn close(&mut self, result: Res<()>) {
        if let Some(done) = self.done.take() {
            _ = done.send(result.map_err(Into::into));
        }
    }
}

impl Drop for WriteSink {
    /// The future tells how the last write ended, if the guest closed the stream before it did.
    fn drop(&mut self) {
        match (self.done.take(), self.task.take()) {
            (Some(done), None) => _ = done.send(Ok(())),
            (Some(done), Some((task, len))) => {
                // Without a runtime the store is being dropped, and there is no guest to tell.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move { _ = done.send(wrote(task.await, len).map_err(Into::into)) });
                }
            }
            (None, _) => {}
        }
    }
}

impl<D> StreamConsumer<D> for WriteSink {
    type Item = u8;

    /// Takes what the guest wrote as one write, and waits for it to land before it takes more: that is the guest's
    /// backpressure. Nothing here cancels a write, so a guest that cancels its own waits for it too, which it does not
    /// wait long for: the write goes on by itself.
    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        source: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let me = &mut *self;
        if me.task.is_none() {
            let mut source = source.as_direct(store);
            let data = source.remaining();
            if data.is_empty() {
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            let len = data.len().min(WRITE_CAP);
            match me.file.write_task(me.at, Bytes::copy_from_slice(&data[..len])) {
                Ok(task) => {
                    source.mark_read(len);
                    me.task = Some((task, len));
                }
                Err(e) => {
                    me.close(Err(e));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
        let Some((task, len)) = &mut me.task else { return Poll::Ready(Ok(StreamResult::Completed)) };
        let done = ready!(Pin::new(task).poll(cx));
        let len = mem::take(len);
        me.task = None;
        Poll::Ready(Ok(match wrote(done, len) {
            Ok(()) => {
                if let At::Offset(p) = &mut me.at {
                    *p += len as u64;
                }
                StreamResult::Completed
            }
            Err(e) => {
                me.close(Err(e));
                StreamResult::Dropped
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::ops::BLOCK;
    use super::super::tests::{commit, put, read, text, turn};
    use super::*;
    use crate::store::Store;
    use crate::tree::counting::counting;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};
    use wasmtime::component::Lift;

    /// A store for the streams of the host to be made and piped in, as the component model's async has them.
    fn host() -> wasmtime::Store<()> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true);
        wasmtime::Store::new(&wasmtime::Engine::new(&config).unwrap(), ())
    }

    /// A consumer on the host that gathers what a stream yields, as a guest that reads on to the end would.
    struct Gather<T>(Arc<Mutex<Vec<T>>>);

    impl<D, T: Lift + Send + Sync + 'static> StreamConsumer<D> for Gather<T> {
        type Item = T;

        fn poll_consume(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            mut store: StoreContextMut<D>,
            mut source: Source<'_, T>,
            _: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let mut batch = Vec::with_capacity(source.remaining(&mut store));
            source.read(&mut store, &mut batch)?;
            self.0.lock().unwrap().append(&mut batch);
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }

    /// A producer on the host whose writes are the chunks it is sent, a write each, until its sender is dropped.
    struct Feed(mpsc::UnboundedReceiver<Bytes>);

    impl<D> StreamProducer<D> for Feed {
        type Item = u8;
        type Buffer = Bytes;

        fn poll_produce<'a>(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            _: StoreContextMut<'a, D>,
            mut dst: Destination<'a, u8, Bytes>,
            finish: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            match self.0.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    dst.set_buffer(chunk);
                    Poll::Ready(Ok(StreamResult::Completed))
                }
                Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
                Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    /// How a stream ended, as the name of its error, which the bindings give no equality to compare.
    type Ended = Result<(), String>;

    fn ended(result: Result<(), ErrorCode>) -> Ended {
        result.map_err(|code| format!("{code:?}").trim_start_matches("ErrorCode::").to_string())
    }

    /// Reads `file` from `offset` with a stream to the end, and returns what it read and how the stream ended.
    async fn drain(file: &Handle, offset: u64) -> (Vec<u8>, Ended) {
        let (tx, rx) = oneshot::channel();
        let (file, gathered) = (file.clone(), Arc::<Mutex<Vec<u8>>>::default());
        let got = gathered.clone();
        host()
            .run_concurrent(async move |accessor| {
                let stream = accessor.with(|mut s| StreamReader::new(&mut s, ReadStream::new(file, offset, tx)));
                accessor.with(|mut s| stream.unwrap().pipe(&mut s, Gather(got))).unwrap();
                rx.await.unwrap()
            })
            .await
            .map(|result| (mem::take(&mut *gathered.lock().unwrap()), ended(result)))
            .unwrap()
    }

    /// Writes `chunks` to `file` with a stream, a write each, and returns how the stream ended.
    async fn pour(file: &Handle, at: At, chunks: Vec<Bytes>) -> Ended {
        let (tx, rx) = oneshot::channel();
        let (feed, chunks_in) = mpsc::unbounded_channel();
        chunks.into_iter().for_each(|chunk| feed.send(chunk).unwrap());
        drop(feed);
        let file = file.clone();
        let sink = WriteSink { file, at, task: None, done: Some(tx) };
        host()
            .run_concurrent(async move |accessor| {
                let stream = accessor.with(|mut s| StreamReader::new(&mut s, Feed(chunks_in))).unwrap();
                accessor.with(|mut s| stream.pipe(&mut s, sink)).unwrap();
                rx.await.unwrap()
            })
            .await
            .map(ended)
            .unwrap()
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_reads_a_file_from_an_offset_to_its_end() {
        let first = turn(&Store::memory()).await;
        let big = pattern(BLOCK as usize * 3 + 5);
        let file = put(&Handle::root(&first), "f", &big).await;

        assert_eq!(drain(&file, 1000).await, (big[1000..].to_vec(), Ok(())));
        assert_eq!(drain(&file, big.len() as u64).await, (vec![], Ok(())), "from the end");
        assert_eq!(drain(&file, big.len() as u64 + 99).await, (vec![], Ok(())), "from past it");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_reads_outlives_the_answer() {
        let store = Store::memory();
        let first = turn(&store).await;
        let file = put(&Handle::root(&first), "f", b"0123456789").await;
        commit(&first).await;
        assert_eq!(drain(&file, 2).await, (b"23456789".to_vec(), Ok(())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_reads_on_has_the_blocks_after_in_the_cache() {
        let (store, counts) = counting();
        let first = turn(&store).await;
        let big = pattern(BLOCK as usize * 12);
        put(&Handle::root(&first), "f", &big).await;
        commit(&first).await;

        // The next request is on another instance, which has read nothing, and a store that is not near.
        let cold = Store { cache: Arc::default(), ..store };
        let file = Handle::root(&turn(&cold).await).open("f", read()).await.unwrap();
        counts.gets.store(0, SeqCst);
        counts.delay.store(20, SeqCst);
        counts.peak.store(0, SeqCst);
        assert_eq!(drain(&file, 0).await, (big, Ok(())));
        assert_eq!(counts.gets.load(SeqCst), 12, "every block was fetched once, and none past the end");
        assert!(counts.peak.load(SeqCst) >= 4, "the blocks after the one read were fetched together");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_on_a_discarded_turn_is_told_io() {
        let first = turn(&Store::memory()).await;
        let file = put(&Handle::root(&first), "f", b"abc").await;
        first.discard().await;
        assert_eq!(drain(&file, 0).await, (vec![], Err("Io".into())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_writes_lands_in_the_file() {
        let store = Store::memory();
        let first = turn(&store).await;
        let root = Handle::root(&first);
        let file = put(&root, "f", b"").await;

        let big = pattern(WRITE_CAP * 2 + 7);
        let chunks = vec![Bytes::from_static(b"hello"), Bytes::from_static(b" world"), Bytes::from(big.clone())];
        assert_eq!(pour(&file, At::Offset(0), chunks).await, Ok(()));
        let all = [&b"hello world"[..], &big].concat();
        assert_eq!(file.read(0, all.len() as u64 + 1).await.unwrap().0, all);

        // Appended, it goes on from the end, and from there on its own.
        let end = Bytes::from_static(b"!");
        assert_eq!(pour(&file, At::End, vec![end.clone(), end]).await, Ok(()));
        assert_eq!(file.read(all.len() as u64, 10).await.unwrap().0, &b"!!"[..]);
        // A stream that is not given anything leaves the file as it is.
        assert_eq!(pour(&file, At::Offset(0), vec![]).await, Ok(()));
        assert_eq!(file.stat().await.unwrap().size, all.len() as u64 + 2);

        commit(&first).await;
        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.unwrap().len(), all.len() + 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_writes_outlives_the_answer() {
        let store = Store::memory();
        let first = turn(&store).await;
        let file = put(&Handle::root(&first), "f", b"").await;

        let (tx, rx) = oneshot::channel();
        let (feed, chunks) = mpsc::unbounded_channel();
        let sink = WriteSink { file: file.clone(), at: At::Offset(0), task: None, done: Some(tx) };
        let answered = host()
            .run_concurrent(async |accessor| {
                let stream = accessor.with(|mut s| StreamReader::new(&mut s, Feed(chunks))).unwrap();
                accessor.with(|mut s| stream.pipe(&mut s, sink)).unwrap();
                feed.send(Bytes::from_static(b"hello")).unwrap();
                timeout(Duration::from_secs(5), async {
                    while file.stat().await.unwrap().size != 5 {
                        sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the write landed");
                // The commit is the answer; what comes after it is refused, and the stream is told.
                commit(&first).await;
                feed.send(Bytes::from_static(b" world")).unwrap();
                rx.await.unwrap()
            })
            .await
            .unwrap();
        assert_eq!(ended(answered), Err("ReadOnly".into()));

        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"hello"[..]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_that_is_closed_leaves_its_write_to_finish() {
        let store = Store::memory();
        let first = turn(&store).await;
        let file = put(&Handle::root(&first), "f", b"").await;

        // The write is taken, and the stream closed before it is done: the future says how it ended.
        let (tx, rx) = oneshot::channel();
        let task = file.write_task(At::Offset(0), Bytes::from_static(b"kept")).unwrap();
        drop(WriteSink { file: file.clone(), at: At::Offset(0), task: Some((task, 4)), done: Some(tx) });
        assert_eq!(ended(rx.await.unwrap()), Ok(()));
        commit(&first).await;
        let next = Handle::root(&turn(&store).await);
        assert_eq!(text(&next, "f").await.as_deref(), Ok(&b"kept"[..]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_on_a_discarded_turn_cannot_write() {
        let first = turn(&Store::memory()).await;
        let file = put(&Handle::root(&first), "f", b"abc").await;
        first.discard().await;
        assert_eq!(pour(&file, At::End, vec![Bytes::from_static(b"x")]).await, Err("ReadOnly".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_directory_stream_lists_every_entry_in_pages() {
        let first = turn(&Store::memory()).await;
        let root = Handle::root(&first);
        let count = PAGE + 200;
        for i in 0..count {
            put(&root, &format!("f{i:05}"), b"").await;
        }
        root.mkdir_at("a-dir").await.unwrap();
        root.symlink_at("z-link", "f00000").await.unwrap();

        let (tx, rx) = oneshot::channel();
        let entries = Arc::<Mutex<Vec<DirectoryEntry>>>::default();
        let (listing, got) =
            (ListStream { entries: root.entries().unwrap(), fetch: None, done: Some(tx) }, entries.clone());
        let result = host()
            .run_concurrent(async move |accessor| {
                let stream = accessor.with(|mut s| StreamReader::new(&mut s, listing)).unwrap();
                accessor.with(|mut s| stream.pipe(&mut s, Gather(got))).unwrap();
                rx.await.unwrap()
            })
            .await
            .unwrap();
        assert_eq!(ended(result), Ok(()));

        let listed = entries.lock().unwrap();
        let mut names: Vec<_> = listed.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names.len(), count + 2);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count + 2, "each entry once");
        let kind = |name: &str| listed.iter().find(|e| e.name == name).map(|e| e.type_.clone());
        assert!(matches!(kind("a-dir"), Some(DescriptorType::Directory)));
        assert!(matches!(kind("z-link"), Some(DescriptorType::SymbolicLink)));
        assert!(matches!(kind("f00007"), Some(DescriptorType::RegularFile)));
    }
}
