//! The sweep, which is the collector: a commit deletes nothing, so what a tree no longer names, and what a turn made
//! and never committed, is left in the bucket for it. Every so often for each app, a day unless the operator sets
//! another interval, serve lists the app's heads and its names, and for each name reads its head, marks what its tree
//! names, lists the name's objects, and deletes those that are not marked and are too old for any turn to be about to
//! name them. The head as it is now is the only root: what a head that was replaced named, and the head now does not,
//! goes with the rest. A delete is a delete marker, so a reader that holds an older head goes on by version until the
//! lifecycle expires what is noncurrent.
//!
//! It is safe in this order: the time that sets the age, then the listing of the heads, then a name's head, then its
//! objects. A head put after its name's was read is a commit's, which writes over that head or one put since, and so
//! names what that head named and what turns made that ended after the listing, which are younger than the age; and a
//! head put on a name that the listing has none of is a commit that creates it, which names only what its turns made,
//! as young. So the heads read are the roots. S3 answers 403 for a key that is not there to a caller that cannot list
//! the bucket, which reads as not found, so only the listing says that a name has no head: one that is listed and
//! cannot be read is an error, and its name is not swept. `docs/decisions.md` has the rest of the argument, and its
//! limits.
use crate::engine::TOTAL;
use crate::name::{self, Head, is_name};
use crate::store::{self, Store};
use crate::tree;
use futures_util::{TryStreamExt, future};
use object_store::{ObjectMeta, path::Path};
use percent_encoding::percent_decode_str;
use rand::seq::SliceRandom;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, timeout_at};
use wasmtime::{Result, bail};

/// How old an object must be to be deleted. An object that a head may yet name was made by a turn that is still under
/// way, or by one that has landed since the sweep listed the heads: the assertions below tie this to how long one
/// lasts.
const GRACE: Duration = Duration::from_secs(3600);
/// The longest an invocation can last, whatever its function is set to: Lambda's own limit.
const LAMBDA_MAX: Duration = Duration::from_secs(900);
/// What the clocks of Lambda and of S3 may differ by, and then some: an object is as old as S3 says.
const SKEW: Duration = Duration::from_secs(300);
/// How long a run goes on, of serve's `TOTAL` and 30 seconds: what is left is for the log and the response.
pub const RUN: Duration = Duration::from_secs(240);
/// The most nodes a name's mark reads. A mark reads `IN_FLIGHT` at once, some 600 in a second, so this is most of a
/// run: more than that cannot be finished, and is not begun. A name has nothing like it unless it holds millions of
/// values of a kilobyte or more in its nodes (`docs/decisions.md`).
const NODES_MAX: usize = 100_000;
/// The objects deleted at once, as S3 deletes 1,000 at most a request: what a run deletes before its time is up stays
/// deleted.
const PAGE: usize = 1000;

// Every object a head names was made in a turn, which lasts `TOTAL` at most (and Lambda's `LAMBDA_MAX` whatever
// serve's own limits are) and commits at its end, or is in the tree of the head the turn's commit wrote over. The sweep
// lists the heads after it takes the time that sets the age to delete at, so an object that a head put since names,
// and that no head read names, is younger than that time less `TOTAL` or `LAMBDA_MAX`, and `SKEW` for the clocks, and
// `GRACE` is more than that. Were a limit raised past this, the sweep would delete what a commit is about to name, so
// the build fails instead.
const _: () = assert!(GRACE.as_secs() >= 3600, "the grace is an hour at least");
const _: () = assert!(TOTAL.as_secs() + SKEW.as_secs() < GRACE.as_secs(), "a guest can outlast the grace");
const _: () = assert!(LAMBDA_MAX.as_secs() + SKEW.as_secs() < GRACE.as_secs(), "an invocation can outlast the grace");
const _: () = assert!(RUN.as_secs() + 30 <= TOTAL.as_secs(), "a run is longer than serve is");

/// What a run did.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    /// The names it swept to the end.
    pub swept: usize,
    /// The objects it deleted, and their bytes.
    pub deleted: usize,
    pub bytes: u64,
    /// The names it did not sweep as they were too large.
    pub skipped: usize,
    /// The names it did not sweep as something was wrong with them, which was logged.
    pub failed: usize,
    /// Whether it went through every name, and not only to its time.
    pub done: bool,
}

/// How often each app is swept: every so many hours, which divide a day, or every so many days. The operator sets it
/// with `TRIC_SWEEP_EVERY`, as `6h` or `2d`, and a day is the default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Every {
    Hours(u32),
    Days(u32),
}

impl FromStr for Every {
    type Err = String;

    /// `<n>h`, for n of 1, 2, 3, 4, 6, 8, 12 or 24, or `<n>d`, for n of 1 to 31; `24h` is `1d`.
    fn from_str(s: &str) -> Result<Self, String> {
        let number = |n: &str| n.bytes().all(|b| b.is_ascii_digit()).then(|| n.parse::<u32>().ok()).flatten();
        let every = match (s.strip_suffix('h').and_then(number), s.strip_suffix('d').and_then(number)) {
            (Some(24), _) => Some(Every::Days(1)),
            (Some(h), _) if h > 0 && 24 % h == 0 => Some(Every::Hours(h)),
            (_, Some(d)) if (1..=31).contains(&d) => Some(Every::Days(d)),
            _ => None,
        };
        every.ok_or_else(|| {
            format!("{s:?} is not hours that divide a day (1h, 2h, 3h, 4h, 6h, 8h, 12h or 24h) nor days from 1d to 31d")
        })
    }
}

/// When the app's sweep runs, every `every`, as a cron expression in UTC: at a minute of its own within the interval,
/// taken from the SHA-256 of its name, so that apps are spread over it, with no state and no flexible window. Every so
/// many hours is at that minute of each of them from the first; every so many days is at that minute of the day, on the
/// days of the month that are one more than a multiple of them, so a month's last interval may be shorter.
pub fn schedule(app: &str, every: Every) -> String {
    let hash = u64::from_str_radix(&store::hash(app.as_bytes())[..16], 16).unwrap_or_default();
    match every {
        Every::Hours(hours) => {
            let minute = hash % (u64::from(hours) * 60);
            format!("{} {}-23/{hours} * * *", minute % 60, minute / 60)
        }
        Every::Days(days) => {
            let minute = hash % 1440;
            let days = if days == 1 { "*".to_owned() } else { format!("*/{days}") };
            format!("{} {} {days} * *", minute % 60, minute / 60)
        }
    }
}

/// Sweeps `app` as far as `until`, deleting what is older than `GRACE` before `now` and is named by no head: see the
/// module.
pub async fn run(store: &Store, app: &str, now: SystemTime, until: Instant) -> Result<Report> {
    sweep(store, app, (now, until), NODES_MAX).await
}

async fn sweep(store: &Store, app: &str, (now, until): (SystemTime, Instant), nodes: usize) -> Result<Report> {
    let mut report = Report { done: true, ..Default::default() };
    // A store that keeps no versions deletes nothing, as a snapshot may still read it, and a clock before `GRACE` has
    // no object that old.
    let cutoff = now.checked_sub(GRACE).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).filter(|_| store.versioned);
    let Some(cutoff) = cutoff.map(|t| t.as_millis() as u64) else { return Ok(report) };
    // The age is set before anything is listed. Only the listing of the heads says which names have one (see the
    // module). The names are those with objects, and a run takes them in an order of its own, so that one that takes
    // long holds up no other for good, as those after it in a fixed order would be.
    let heads = store.list(&store::app(app, &["names"])).await?;
    let heads: HashSet<Path> = heads.objects.into_iter().map(|o| o.location).collect();
    let mut names = names(store, app).await?;
    names.shuffle(&mut rand::rng());
    for name in &names {
        if Instant::now() >= until {
            report.done = false;
            break;
        }
        let head = heads.contains(&name::path(app, name));
        let swept = timeout_at(until, reclaim(store, app, name, head, (cutoff, nodes), &mut report)).await;
        match swept {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(app, name, "sweep: the name is not swept: {e:#}");
                report.failed += 1;
            }
            // What the name deleted before then stays deleted, and the rest is for a run to come.
            Err(_) => {
                report.done = false;
                break;
            }
        }
    }
    Ok(report)
}

/// Deletes the objects of `name` that were made at `cutoff` (Unix milliseconds) or before and that its head does not
/// name, if it has one, as `head` says; a page at a time, each counted in `report` as it is deleted. A name whose tree
/// is more than `nodes` is not read, and nothing of it is deleted. An error if the head cannot be read, or its tree
/// read whole: then there is no telling what is named.
async fn reclaim(
    store: &Store,
    app: &str,
    name: &str,
    head: bool,
    (cutoff, nodes): (u64, usize),
    report: &mut Report,
) -> Result<()> {
    let mut live = HashSet::new();
    if head {
        // Listed, and not there to be read, is refused, or deleted since: either way it is not a name without a head.
        let Some((head, _)) = store.json::<Head>(&name::path(app, name), name::HEAD_MAX as u64).await? else {
            bail!("the head is listed, and cannot be read")
        };
        if head.mark(store, app, name, &mut live, nodes).await?.is_none() {
            tracing::warn!(app, name, "sweep: the name has more nodes than a run reads, so it is not swept");
            report.skipped += 1;
            return Ok(());
        }
    }
    let objects = store.objects(&store::app(app, &["values", name]));
    let garbage = objects.try_filter(|meta| future::ready(deletable(app, name, meta, (cutoff, &live))));
    let mut pages = garbage.try_chunks(PAGE);
    while let Some(page) = pages.try_next().await.map_err(|e| e.1)? {
        let bytes: u64 = page.iter().map(|meta| meta.size).sum();
        let deleted = store.delete_many(page.into_iter().map(|meta| meta.location).collect()).await;
        tracing::info!(app, name, deleted, bytes, "sweep: reclaimed");
        report.deleted += deleted;
        report.bytes += bytes;
    }
    report.swept += 1;
    Ok(())
}

/// Whether `meta` is an object the sweep may delete: one the tree of `name` could have made, that is not in `live`,
/// and that was made at `cutoff` or before.
fn deletable(app: &str, name: &str, meta: &ObjectMeta, (cutoff, live): (u64, &HashSet<u128>)) -> bool {
    let made = u64::try_from(meta.last_modified.timestamp_millis()).unwrap_or_default();
    candidate(app, name, &meta.location).is_some_and(|id| made <= cutoff && !live.contains(&id))
}

/// The id of the object at `path`, if it is one the tree of `name` could have made, and so one the sweep may delete:
/// directly under the name's own prefix, with an id of 32 lowercase hex digits, and no other key at all.
fn candidate(app: &str, name: &str, path: &Path) -> Option<u128> {
    let id = path.filename()?;
    let number = tree::number(id)?;
    (*path == tree::object(app, name, id)).then_some(number)
}

/// The names that have objects, as the engine could have made them.
async fn names(store: &Store, app: &str) -> Result<Vec<String>> {
    let listed = store.list(&store::app(app, &["values"])).await?;
    Ok(listed.common_prefixes.iter().filter_map(|p| named(app, p)).collect())
}

/// The name that `listed` is in `values/`, if it is one the engine could have made: a name, whose path, built as every
/// path is, is the one that was listed. So a key of any other spelling, of a name that is no name, or of an odd shape,
/// is no name's, and is not touched.
fn named(app: &str, listed: &Path) -> Option<String> {
    let name = percent_decode_str(listed.filename()?).decode_utf8().ok()?;
    (is_name(&name) && *listed == store::app(app, &["values", &name])).then(|| name.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::Cron;
    use crate::tree::counting::{Counts, counting};
    use crate::tree::{Config, Root};
    use bytes::Bytes;
    use futures_util::StreamExt;
    use futures_util::stream::{self, BoxStream};
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectStore, PutMode, PutMultipartOptions,
        PutOptions, PutPayload, PutResult,
    };
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};
    use std::{fmt, io};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// A config that makes a few keys a tree of several nodes, and a value of more than 8 bytes an object of its own.
    fn tiny() -> Config {
        Config { node: 1400, inline: 8, budget: 2048, entries: 1000, data: 100_000, blob: 100 }
    }

    /// A store in memory that keeps versions, refuses what a test says, as S3 answers 403 to a caller it does not
    /// allow, has a key that a test says is gone not be there, though it is listed, and waits as long as a test says
    /// before each delete.
    #[derive(Debug, Default)]
    struct Refusing {
        inner: InMemory,
        /// The keys whose gets are refused, and the prefixes whose listings are.
        refused: Mutex<HashSet<Path>>,
        /// The keys that are listed, and are not there to be got.
        gone: Mutex<HashSet<Path>>,
        /// How long each delete waits before it deletes anything, in milliseconds.
        delay: AtomicU64,
    }

    impl fmt::Display for Refusing {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("refusing")
        }
    }

    fn refused(at: &Path) -> object_store::Error {
        object_store::Error::PermissionDenied { path: at.to_string(), source: "403 Forbidden".into() }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Refusing {
        async fn put_opts(&self, at: &Path, payload: PutPayload, opts: PutOptions) -> object_store::Result<PutResult> {
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
            if self.refused.lock().unwrap().contains(at) {
                return Err(refused(at));
            }
            if self.gone.lock().unwrap().contains(at) {
                return Err(object_store::Error::NotFound { path: at.to_string(), source: "404 Not Found".into() });
            }
            self.inner.get_opts(at, opts).await
        }

        fn delete_stream(
            &self,
            paths: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            let delay = Duration::from_millis(self.delay.load(SeqCst));
            let paths = stream::once(async move {
                tokio::time::sleep(delay).await;
                paths
            });
            self.inner.delete_stream(paths.flatten().boxed())
        }

        fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
            if let Some(prefix) = prefix.filter(|p| self.refused.lock().unwrap().contains(p)) {
                return Err(refused(prefix));
            }
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, opts).await
        }
    }

    fn refusing() -> (Store, Arc<Refusing>) {
        let refusing = Arc::new(Refusing::default());
        (Store { inner: refusing.clone(), once: None, versioned: true, cache: Arc::default() }, refusing)
    }

    /// A head whose tree is empty.
    const EMPTY: &[u8] = br#"{"commit":"c"}"#;

    /// Puts `json` as the head of `name`.
    async fn head(store: &Store, name: &str, json: &[u8]) {
        store.put(&name::path("a", name), Bytes::copy_from_slice(json), PutMode::Overwrite).await.unwrap();
    }

    /// A tree of `keys` values that are each an object, in `name`, from the tree of `from` if given, with its head: its
    /// tree, and the objects that are in `name` now.
    async fn commit(store: &Store, name: &str, keys: usize, from: &Root) -> (Root, BTreeSet<Path>) {
        let mut tree = tree::open(store, "a", name, from, tiny()).unwrap();
        let edits = (0..keys).map(|i| (format!("k/{i:03}"), Some(Bytes::from(vec![i as u8; 40])))).collect();
        tree.write(edits).await.unwrap();
        let root = tree.finish().await.unwrap();
        let json = serde_json::json!({ "tree": root, "commit": store::random() });
        head(store, name, &serde_json::to_vec(&json).unwrap()).await;
        (root, held(store, name).await)
    }

    /// A tree of `keys` values that are each an object, in `name`, with its head: the objects that are in `name` now.
    async fn plant(store: &Store, name: &str, keys: usize) -> BTreeSet<Path> {
        commit(store, name, keys, &Root::default()).await.1
    }

    /// The objects that the tree at `root` of `name` is made of, and how many of them are nodes.
    async fn marked(store: &Store, name: &str, root: &Root) -> (BTreeSet<Path>, usize) {
        let mut live = HashSet::new();
        let tree = tree::open(store, "a", name, root, tiny()).unwrap();
        let nodes = tree.mark(&mut live, usize::MAX).await.unwrap().unwrap();
        (live.into_iter().map(|id| tree::object("a", name, &format!("{id:032x}"))).collect(), nodes)
    }

    /// The objects of `name`, as `a` has them.
    async fn held(store: &Store, name: &str) -> BTreeSet<Path> {
        let objects = store.objects(&store::app("a", &["values", name])).try_collect::<Vec<_>>().await.unwrap();
        objects.into_iter().map(|o| o.location).collect()
    }

    /// An object of `app`'s `name` that no tree names, made now: 5 bytes.
    async fn stray(store: &Store, app: &str, name: &str) -> Path {
        let path = tree::object(app, name, &store::random());
        store.put(&path, Bytes::from_static(b"stray"), PutMode::Overwrite).await.unwrap();
        path
    }

    async fn there(store: &Store, path: &Path) -> bool {
        store.head(path).await.unwrap()
    }

    /// A node among `objects`, as opposed to a value: nodes are JSON, and the values the tests plant are not.
    async fn node(store: &Store, objects: &BTreeSet<Path>) -> Path {
        for path in objects {
            let (bytes, _) = store.get(path, 1 << 20).await.unwrap().unwrap();
            if bytes.first() == Some(&b'{') {
                return path.clone();
            }
        }
        panic!("no node");
    }

    /// A time at which everything that exists now is old enough to go.
    fn later() -> SystemTime {
        SystemTime::now() + GRACE * 2
    }

    fn long() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    /// What the thread logs, from `capture` until its guard drops.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Log {
        fn capture(&self) -> tracing::subscriber::DefaultGuard {
            let log = self.clone();
            tracing::subscriber::set_default(
                tracing_subscriber::fmt().with_writer(move || log.clone()).with_ansi(false).finish(),
            )
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        fn warned(&self) -> Vec<String> {
            self.text().lines().filter(|line| line.contains("WARN")).map(str::to_owned).collect()
        }

        /// The one warning about `name`.
        fn about(&self, name: &str) -> String {
            let about: Vec<_> = self.warned().into_iter().filter(|l| l.contains(&format!("name=\"{name}\""))).collect();
            assert_eq!(about.len(), 1, "{}", self.text());
            about[0].clone()
        }
    }

    #[test]
    fn only_the_key_of_a_tree_object_is_a_candidate() {
        let id = |s: &str| u128::from_str_radix(s, 16).unwrap();
        assert_eq!(candidate("a", "n", &tree::object("a", "n", ID)), Some(id(ID)));
        assert_eq!(candidate("a", "x~y", &tree::object("a", "x~y", ID)), Some(id(ID)), "a name needs its escapes");
        let parse = |s: &str| Path::parse(s).unwrap();
        let odd = [
            ("upper case", tree::object("a", "n", &ID.to_uppercase())),
            ("31 digits", tree::object("a", "n", &ID[1..])),
            ("33 digits", tree::object("a", "n", &format!("{ID}0"))),
            ("not hex", tree::object("a", "n", &ID.replace('0', "g"))),
            ("a suffix", tree::object("a", "n", &format!("{ID}.tmp"))),
            ("escaped", parse(&format!("apps/a/values/n/%30{}", &ID[1..]))),
            ("deeper", store::app("a", &["values", "n", "d", ID])),
            ("another name", tree::object("a", "m", ID)),
            ("another app", tree::object("b", "n", ID)),
            ("not escaped as it is", parse(&format!("apps/a/values/x%7ey/{ID}"))),
            ("not in values", store::app("a", &["names", "n", ID])),
            ("a head", name::path("a", "n")),
            ("a name", store::app("a", &["values", "n"])),
            ("nothing of the app", Path::parse(ID).unwrap()),
        ];
        for (what, path) in odd {
            let name = if what == "not escaped as it is" { "x~y" } else { "n" };
            assert_eq!(candidate("a", name, &path), None, "{what}: {path}");
        }
    }

    #[test]
    fn only_a_name_the_engine_could_have_made_is_a_name() {
        let at = |dir: &str, name: &str| store::app("a", &[dir, name]);
        for name in ["n", "a.b", "x~y", "Z:1", "a-b_c", "9", &"n".repeat(128)] {
            assert_eq!(named("a", &at("values", name)).as_deref(), Some(name), "{name}");
        }
        let long = "n".repeat(129);
        for name in [".hidden", "_x", "-x", "~x", "a b", "a/b", "a\\b", "é", "a\u{0}b", &long] {
            assert_eq!(named("a", &at("values", name)), None, "{name:?}");
        }
        let parse = |s: &str| Path::parse(s).unwrap();
        assert_eq!(named("a", &parse("apps/a/values/x%7ey")), None, "an escape in the wrong case");
        assert_eq!(named("a", &at("names", "n")), None, "a name of another kind");
        assert_eq!(named("a", &store::app("b", &["values", "n"])), None, "another app's");
        assert_eq!(named("a", &store::app("a", &["values", "n", "m"])), None, "deeper than a name");
        assert_eq!(named("a", &store::app("a", &["values"])), None, "no name at all");
    }

    #[test]
    fn an_interval_is_hours_that_divide_a_day_or_whole_days() {
        let every = |s: &str| s.parse::<Every>();
        for h in [1, 2, 3, 4, 6, 8, 12] {
            assert_eq!(every(&format!("{h}h")), Ok(Every::Hours(h)));
        }
        for d in [1, 2, 7, 31] {
            assert_eq!(every(&format!("{d}d")), Ok(Every::Days(d)));
        }
        assert_eq!(every("24h"), Ok(Every::Days(1)), "a day of hours is a day");
        assert_eq!(every("06h"), Ok(Every::Hours(6)));
        let bad =
            ["0h", "5h", "7h", "9h", "48h", "0d", "32d", "1w", "h", "d", "", "1", "+6h", "-1d", "1.5d", "6H", " 1d"];
        for s in bad {
            let e = every(s).unwrap_err();
            assert!(e.contains(&format!("{s:?}")) && e.contains("1h, 2h, 3h, 4h, 6h, 8h, 12h or 24h"), "{e}");
        }
    }

    /// The minutes of `day` (a Unix second at its start) at which `expr` fires.
    fn fires(expr: &str, day: u64) -> Vec<u64> {
        let cron = Cron::parse(expr).unwrap();
        (0..1440).map(|m| day + m * 60).filter(|t| cron.matches(*t)).collect()
    }

    /// 2026-10-01, a Thursday, at midnight UTC; the month has 31 days.
    const OCTOBER: u64 = 1_790_812_800;

    #[test]
    fn schedules_spread_the_apps_over_the_interval_and_a_day_is_as_it_was() {
        // Daily, at a minute of the day, as the sweep always was: a change of interval is a change of expression.
        assert_eq!(schedule("app", Every::Days(1)), schedule("app", "24h".parse().unwrap()));
        let times: HashSet<_> = (0..500).map(|i| schedule(&format!("app{i}"), Every::Days(1))).collect();
        assert!(times.len() > 300, "{} times for 500 apps", times.len());
        for time in times {
            let fields: Vec<_> = time.split(' ').collect();
            let (minute, hour) = (fields[0].parse::<u32>().unwrap(), fields[1].parse::<u32>().unwrap());
            assert!(minute < 60 && hour < 24 && fields[2..] == ["*", "*", "*"], "{time}");
        }
        // Every so many hours: that many times a day, as far apart, at the app's minute within them.
        for h in [1, 2, 3, 4, 6, 8, 12] {
            let mut minutes = HashSet::new();
            for i in 0..200 {
                let expr = schedule(&format!("app{i}"), Every::Hours(h));
                let at = fires(&expr, OCTOBER);
                assert_eq!(at.len() as u32, 24 / h, "{expr}");
                assert!(at.windows(2).all(|w| w[1] - w[0] == u64::from(h) * 3600), "{expr}");
                assert_eq!(expr, schedule(&format!("app{i}"), Every::Hours(h)));
                minutes.insert((at[0] - OCTOBER) / 60);
            }
            assert!(minutes.len() > (200.min(h * 60) / 2) as usize, "{} minutes of {h}h for 200 apps", minutes.len());
        }
        // Every so many days: once on each of those days of the month, so never longer apart than that.
        for d in [2, 3, 7, 31] {
            let expr = schedule("app", Every::Days(d));
            let days: Vec<u64> = (0..31).filter(|i| !fires(&expr, OCTOBER + i * 86_400).is_empty()).collect();
            assert_eq!(days, (0..31).step_by(d as usize).collect::<Vec<u64>>(), "{expr}");
            assert!((0..31).all(|i| fires(&expr, OCTOBER + i * 86_400).len() <= 1), "{expr}");
            assert!(Cron::parse(&expr).unwrap().eventbridge().starts_with("cron("));
        }
    }

    #[tokio::test]
    async fn deletes_what_no_tree_names_and_leaves_the_trees() {
        let (store, _) = counting();
        let live = plant(&store, "n", 60).await;
        let other = plant(&store, "m", 5).await;
        assert!(live.len() > 60 + 3, "{} objects are too few for a tree of nodes", live.len());
        for _ in 0..3 {
            stray(&store, "a", "n").await;
        }
        let elsewhere = stray(&store, "b", "n").await;
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 3, bytes: 15, done: true, ..Default::default() });
        assert_eq!(held(&store, "n").await, live);
        assert_eq!(held(&store, "m").await, other);
        assert!(there(&store, &elsewhere).await, "another app's is not this app's to delete");

        // What is left is a tree that is whole, which a second run reads through again and finds nothing in.
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, done: true, ..Default::default() });
    }

    #[tokio::test]
    async fn keeps_what_is_younger_than_the_grace_to_the_millisecond() {
        let (store, _) = counting();
        plant(&store, "n", 3).await;
        let old = stray(&store, "a", "n").await;
        std::thread::sleep(Duration::from_millis(20));
        let edge = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let young = stray(&store, "a", "n").await;

        // The grace before `edge + GRACE` is `edge`: what was made before it is old enough, and nothing after.
        let report = run(&store, "a", edge + GRACE, long()).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!there(&store, &old).await && there(&store, &young).await);
        // Now nothing is old enough, and a clock that is before the grace has nothing that old.
        let report = run(&store, "a", SystemTime::now(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, done: true, ..Default::default() });
        let report = run(&store, "a", UNIX_EPOCH + Duration::from_secs(10), long()).await.unwrap();
        assert_eq!(report, Report { done: true, ..Default::default() });
        assert!(there(&store, &young).await);
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!there(&store, &young).await);
    }

    #[tokio::test]
    async fn deletes_nothing_where_versions_are_not_kept() {
        let store = Store::memory();
        let path = stray(&store, "a", "n").await;
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { done: true, ..Default::default() });
        assert!(there(&store, &path).await, "a snapshot may still read it");
    }

    #[tokio::test]
    async fn deletes_only_keys_a_tree_could_have_made() {
        let (store, _) = counting();
        let live = plant(&store, "n", 3).await;
        let id = store::random();
        let (upper, short, long_id) = (id.to_uppercase(), &id[1..], format!("{id}0"));
        let (tmp, wide) = (format!("{id}.tmp"), "n".repeat(129));
        let odd = [
            format!("apps/a/values/n/{upper}"),
            format!("apps/a/values/n/{short}"),
            format!("apps/a/values/n/{long_id}"),
            format!("apps/a/values/n/{tmp}"),
            format!("apps/a/values/n/sub/{id}"),
            format!("apps/a/values/.hidden/{id}"),
            format!("apps/a/values/_x/{id}"),
            format!("apps/a/values/x%7ey/{id}"),
            format!("apps/a/values/{wide}/{id}"),
            "apps/a/names/.junk".to_owned(),
            format!("apps/b/values/n/{id}"),
            "apps/a/current".to_owned(),
        ];
        for key in &odd {
            let path = Path::parse(key).unwrap();
            store.put(&path, Bytes::from_static(b"odd keys"), PutMode::Overwrite).await.unwrap();
        }
        // Two that go, to show the sweep went through the rest: one in a name with a tree, and one with no head.
        let (one, other) = (stray(&store, "a", "n").await, stray(&store, "a", "m").await);

        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert!(!there(&store, &one).await && !there(&store, &other).await);
        for key in &odd {
            assert!(there(&store, &Path::parse(key).unwrap()).await, "{key} was deleted");
        }
        assert!(held(&store, "n").await.is_superset(&live));
    }

    #[tokio::test]
    async fn a_name_without_a_head_names_nothing_and_its_head_is_read_at_each_run() {
        let (store, _) = counting();
        // No head: no object of the name is named, however many. Nor is any when the head's tree is empty.
        let (one, two) = (stray(&store, "a", "m").await, stray(&store, "a", "m").await);
        let three = stray(&store, "a", "e").await;
        head(&store, "e", EMPTY).await;
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 3, bytes: 15, done: true, ..Default::default() });
        for path in [one, two, three] {
            assert!(!there(&store, &path).await);
        }

        // The head is read when the name is swept, and no earlier answer is kept: a tree that the head names now is
        // whole, and one that it no longer names is not named.
        let before = plant(&store, "n", 20).await;
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!((report.deleted, report.failed), (0, 0));
        let both = plant(&store, "n", 20).await;
        let second: BTreeSet<_> = both.difference(&before).cloned().collect();
        assert_eq!(second.len(), both.len() - before.len());
        assert_eq!(run(&store, "a", later(), long()).await.unwrap().deleted, before.len());
        assert_eq!(held(&store, "n").await, second);
    }

    #[tokio::test]
    async fn what_a_replaced_head_alone_named_goes() {
        let (store, _) = counting();
        // A head and then another from it, as commits are: the second shares most of the first's tree, and replaces a
        // value and the nodes above it, which the first alone names now.
        let (first, before) = commit(&store, "n", 60, &Root::default()).await;
        let (second, after) = commit(&store, "n", 1, &first).await;
        let made: BTreeSet<_> = after.difference(&before).cloned().collect();
        assert!(made.len() >= 2, "the second made a value and a node at least");
        let (named, _) = marked(&store, "n", &second).await;
        let replaced: BTreeSet<_> = before.difference(&named).cloned().collect();
        assert!(replaced.len() >= 2, "the first named a value and a node that the second does not");
        let strays = [stray(&store, "a", "n").await, stray(&store, "a", "n").await];

        // The head as it is now is the only root: the strays go, and what the first alone named, and the second's
        // tree stays whole.
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!((report.swept, report.deleted, report.failed), (1, after.len() - named.len() + 2, 0));
        assert_eq!(held(&store, "n").await, named);
        assert!(named.is_superset(&made) && named.is_disjoint(&replaced));
        assert!(!there(&store, &strays[0]).await && !there(&store, &strays[1]).await);
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, done: true, ..Default::default() });
    }

    #[tokio::test]
    async fn a_listed_head_that_cannot_be_read_loses_nothing() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, refusing) = refusing();
        let names = ["a1", "a2", "a3"];
        let mut planted = vec![];
        for name in names {
            plant(&store, name, 5).await;
            stray(&store, "a", name).await;
            planted.push(held(&store, name).await);
        }
        // A head whose read is refused, as S3 refuses one to a caller that may not list the bucket whether it is there
        // or not, and a head that is listed and then is not there: neither says that the name has no head, and there
        // is no telling what either names.
        refusing.refused.lock().unwrap().insert(name::path("a", "a1"));
        refusing.gone.lock().unwrap().insert(name::path("a", "a2"));
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, failed: 2, done: true, ..Default::default() });
        assert_eq!(held(&store, "a1").await, planted[0]);
        assert_eq!(held(&store, "a2").await, planted[1]);
        assert_eq!(held(&store, "a3").await.len(), planted[2].len() - 1);
        assert_eq!(log.warned().len(), 2, "{}", log.text());
        for name in ["a1", "a2"] {
            let line = log.about(name);
            assert!(line.contains("app=\"a\"") && line.contains("the head is listed, and cannot be read"), "{line}");
        }

        // A listing that is refused, of the heads or of the names, is the run's error, and nothing is deleted.
        let late = stray(&store, "a", "a3").await;
        for listing in ["names", "values"] {
            *refusing.refused.lock().unwrap() = HashSet::from([store::app("a", &[listing])]);
            let e = run(&store, "a", later(), long()).await.unwrap_err();
            assert!(format!("{e:#}").contains("403"), "{e:#}");
        }
        assert!(there(&store, &late).await);
        assert_eq!(held(&store, "a1").await, planted[0]);
    }

    #[tokio::test]
    async fn a_name_that_cannot_be_read_whole_loses_nothing_and_the_rest_are_swept() {
        let log = Log::default();
        let _logging = log.capture();
        let (mut store, _) = counting();
        let mut strays = vec![];
        let mut planted = vec![];
        for name in ["a1", "a2", "a3", "a4", "a5"] {
            planted.push(plant(&store, name, 20).await);
            strays.push(stray(&store, "a", name).await);
        }
        head(&store, "a1", b"{").await; // not JSON
        let missing = node(&store, &planted[1]).await;
        store.delete(&missing).await.unwrap(); // a node that is gone
        let changed = node(&store, &planted[2]).await;
        store.put(&changed, Bytes::from_static(b"{}"), PutMode::Overwrite).await.unwrap(); // and one that is not it
        store.cache = Arc::default(); // a node in the cache was whole when it was put there

        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, failed: 3, done: true, ..Default::default() });
        for (i, stray) in strays.iter().enumerate() {
            assert_eq!(there(&store, stray).await, i < 3, "the stray of a{}", i + 1);
        }
        assert_eq!(log.warned().len(), 3, "{}", log.text());
        for name in ["a1", "a2", "a3"] {
            assert!(log.about(name).contains("app=\"a\""), "{}", log.text());
        }
    }

    #[tokio::test]
    async fn a_name_of_more_nodes_than_a_run_reads_is_passed_over_and_logged() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, _) = counting();
        let (root, big) = commit(&store, "big", 60, &Root::default()).await;
        plant(&store, "small", 2).await;
        let (_, nodes) = marked(&store, "big", &root).await;
        assert!(nodes > 3, "{nodes} nodes are too few to pass over");
        let (in_big, in_small) = (stray(&store, "a", "big").await, stray(&store, "a", "small").await);

        // A node more than the bound is passed over, whole.
        let report = sweep(&store, "a", (later(), long()), nodes - 1).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, skipped: 1, done: true, ..Default::default() });
        assert!(there(&store, &in_big).await && !there(&store, &in_small).await);
        assert!(held(&store, "big").await.is_superset(&big));
        let warned = log.warned();
        assert_eq!(warned.len(), 1, "{}", log.text());
        assert!(warned[0].contains("app=\"a\"") && warned[0].contains("name=\"big\""), "{}", warned[0]);
        assert!(warned[0].contains("more nodes"), "{}", warned[0]);
        // And as many as the bound is not.
        let report = sweep(&store, "a", (later(), long()), nodes).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 1, bytes: 5, done: true, ..Default::default() });
        assert!(!there(&store, &in_big).await);
    }

    #[tokio::test(start_paused = true)]
    async fn deletes_a_page_at_a_time_and_what_it_deleted_stays_deleted() {
        let (store, refusing) = refusing();
        for _ in 0..2 * PAGE + 1 {
            stray(&store, "a", "n").await;
        }
        // A delete of a page takes a second, of a clock that the test pauses: a run of a second and a half deletes a
        // page, and its time is up while it deletes the second.
        refusing.delay.store(1000, SeqCst);
        let until = Instant::now() + Duration::from_millis(1500);
        let report = run(&store, "a", later(), until).await.unwrap();
        assert_eq!(report, Report { deleted: PAGE, bytes: 5 * PAGE as u64, ..Default::default() });
        assert_eq!(held(&store, "n").await.len(), PAGE + 1);

        // The next run lists what is left, and deletes it.
        let report = run(&store, "a", later(), long()).await.unwrap();
        let rest =
            Report { swept: 1, deleted: PAGE + 1, bytes: 5 * (PAGE as u64 + 1), done: true, ..Default::default() };
        assert_eq!(report, rest);
        assert!(held(&store, "n").await.is_empty());
    }

    /// Names `n1` to `n4`, each with a head, a tree whose root the head holds, and an object that nothing names. A get
    /// takes a second of the clock, which a test pauses: a name costs the get of its head.
    async fn slow() -> (Store, Arc<Counts>, Vec<Path>) {
        let (store, counts) = counting();
        let mut strays = vec![];
        for name in ["n1", "n2", "n3", "n4"] {
            plant(&store, name, 2).await;
            strays.push(stray(&store, "a", name).await);
        }
        counts.delay.store(1000, SeqCst);
        (store, counts, strays)
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_its_time_and_runs_to_come_reach_every_name() {
        let (store, counts, strays) = slow().await;
        let left = async || {
            counts.delay.store(0, SeqCst);
            let mut left = 0;
            for stray in &strays {
                left += usize::from(there(&store, stray).await);
            }
            counts.delay.store(1000, SeqCst);
            left
        };
        // Two heads are read in two seconds, and the third would be at three.
        let until = Instant::now() + Duration::from_millis(2500);
        let report = run(&store, "a", later(), until).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, ..Default::default() });
        assert_eq!(left().await, 2);

        // A run takes the names in an order of its own, so runs with time for one name each reach every name, as
        // runs that all began with the same one would not.
        let mut runs = 0;
        while left().await > 0 {
            let until = Instant::now() + Duration::from_millis(1500);
            let report = run(&store, "a", later(), until).await.unwrap();
            assert_eq!((report.swept, report.done), (1, false));
            runs += 1;
            assert!(runs < 200, "{runs} runs, and a name is not swept yet");
        }
    }
}
