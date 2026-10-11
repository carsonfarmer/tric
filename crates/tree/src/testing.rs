//! A store for tests: in memory, it counts what is asked of it, can be slowed down, and fails the puts past a count.
//! It is behind the `testing` feature, for the tests of what is built on the tree.
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

/// What a [`counting`] store was asked, and how it is to answer. Every field may be read and set at any time.
#[derive(Debug, Default)]
pub struct Counts {
    /// The gets begun.
    pub gets: AtomicUsize,
    /// The puts begun, those that failed included.
    pub puts: AtomicUsize,
    /// The paths given to deletes.
    pub deletes: AtomicUsize,
    /// How many puts succeed, in all: those past it fail. [`counting`] begins it at `usize::MAX`.
    pub allow: AtomicUsize,
    /// How long a get takes, in milliseconds.
    pub delay: AtomicU64,
    /// The gets under way.
    pub flight: AtomicUsize,
    /// The most gets that were ever under way together.
    pub peak: AtomicUsize,
    /// The objects that listings have given, as a listing is read.
    pub listed: AtomicUsize,
    /// The listings begun, by `list` and by `list_with_offset`.
    pub listings: AtomicUsize,
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

impl Counting {
    fn counted(
        &self,
        listing: BoxStream<'static, object_store::Result<ObjectMeta>>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let counts = self.counts.clone();
        counts.listings.fetch_add(1, SeqCst);
        listing
            .inspect(move |_| {
                counts.listed.fetch_add(1, SeqCst);
            })
            .boxed()
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
        self.counted(self.inner.list(prefix))
    }

    /// Gives only the keys after `offset`, as S3 does, so that what is counted is what a request would return.
    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.counted(self.inner.list_with_offset(prefix, offset))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}

/// A store in memory that counts what is asked of it, and the counts, which also set how it answers.
pub fn counting() -> (Arc<dyn ObjectStore>, Arc<Counts>) {
    let counts = Arc::new(Counts { allow: AtomicUsize::new(usize::MAX), ..Default::default() });
    (Arc::new(Counting { inner: InMemory::new(), counts: counts.clone() }), counts)
}
