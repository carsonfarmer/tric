//! The sweep, which is the collector: a commit deletes nothing, so what a tree no longer names, and what a turn made
//! and never committed, is left in the bucket for it. Every so often for each app, a day unless the operator sets
//! another interval, serve lists the app's names, and for each one lists the versions of its head that the bucket
//! keeps, reads them, marks what their trees name, lists the name's objects, and deletes those that are not marked and
//! are too old for any turn to be about to name them. A head that is replaced, or deleted, is a version the lifecycle
//! keeps for its noncurrent days: that is a name's history, and the objects its trees name are kept as long, so that a
//! head can be put back as it was. A delete is a delete marker: readers go on by version, and the lifecycle expires
//! what is noncurrent.
//!
//! It is safe in this order: the time that sets the age, then a name's versions, then its objects. A head put after the
//! versions were listed is a commit's, which writes over the head that was current then, or over one put since, and so
//! names what that head named and what turns made that ended after the listing, which are younger than the age. So the
//! versions listed are the roots. The latest of them cannot expire within a run, so one that reads as gone is an error;
//! one that is noncurrent and has expired since it was listed is no root, as no commit writes over a version that is
//! not current. Putting a version back is no commit, and must keep clear of that: `docs/decisions.md` has what it must
//! do, the rest of the argument, and its limits.
use crate::aws::{Aws, Version, Versions};
use crate::engine::TOTAL;
use crate::name::{self, Head, is_name};
use crate::store::{self, Store};
use crate::tree;
use bytes::Bytes;
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{ObjectMeta, PutMode, path::Path};
use percent_encoding::percent_decode_str;
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, timeout_at};
use wasmtime::{Result, bail, error::Context};

/// How old an object must be to be deleted. An object that a head may yet name was made by a turn that is still under
/// way, or by one that has landed since the sweep listed the versions: the assertions below tie this to how long one
/// lasts.
const GRACE: Duration = Duration::from_secs(3600);
/// The longest an invocation can last, whatever its function is set to: Lambda's own limit.
const LAMBDA_MAX: Duration = Duration::from_secs(900);
/// What the clocks of Lambda and of S3 may differ by, and then some: an object is as old as S3 says.
const SKEW: Duration = Duration::from_secs(300);
/// How long a run goes on, of serve's `TOTAL` and 30 seconds: what is left is for the cursor, the log and the response.
pub const RUN: Duration = Duration::from_secs(240);
/// The most nodes a name's marks read, of all its versions' trees together, in which a node that several share is read
/// once. A mark reads `IN_FLIGHT` at once, some 600 in a second, so this is most of a run: more than that cannot be
/// finished, and is not begun. A name has nothing like it unless it holds millions of values of a kilobyte or more in
/// its nodes (`docs/decisions.md`).
const NODES_MAX: usize = 100_000;
/// The most versions of a name's head, delete markers among them, that a run lists and reads. Their heads are read
/// `IN_FLIGHT` at once, some 15 seconds of a run, and their trees marked one after another, each reading only what none
/// before it did, which is what its commit changed. The versions kept are those of the last day or two, so a name has
/// more only if it commits more often than every 9 to 17 seconds, all day, and such a name is not swept
/// (`docs/decisions.md`).
const VERSIONS_MAX: usize = 10_000;
/// The key, in `values/`, of where a run that stopped early got to. A name starts with a letter or a digit.
const CURSOR: &str = ".sweep";
/// The keys in a page of a listing: S3 gives 1,000 at most, whatever `max-keys` asks for.
const PAGE: usize = 1000;

// Every object a head names was made in a turn, which lasts `TOTAL` at most (and Lambda's `LAMBDA_MAX` whatever
// serve's own limits are) and commits at its end, or is in the tree of the head the turn's commit wrote over. The sweep
// lists a name's versions after it takes the time that sets the age to delete at, so an object that a head put since
// names, and that no version listed names, is younger than that time less `TOTAL` or `LAMBDA_MAX`, and `SKEW` for the
// clocks, and `GRACE` is more than that. Were a limit raised past this, the sweep would delete what a commit is about
// to name, so the build fails instead.
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
    /// The names it did not sweep as they were too large, or took too long to be the first of a run.
    pub skipped: usize,
    /// The names it did not sweep as something was wrong with them, which was logged.
    pub failed: usize,
    /// Whether it went through every name, and not only to its time.
    pub done: bool,
}

/// The versions of heads that the bucket keeps. S3's are listed with ListObjectVersions, which object_store has no
/// call for, and read by their ids; a test's are its own, as the memory store keeps none.
#[async_trait::async_trait]
pub trait History: Sync {
    /// A page of the versions and the delete markers of the objects whose keys begin with `prefix`, after the key and
    /// version id `from` if given. Pages are in the order of their keys, and the keys in a page are not in order.
    async fn versions(&self, prefix: &Path, from: Option<(String, String)>) -> Result<Versions>;

    /// The object at `path` as it was at `version`, if the bucket keeps that version still; it must be `max` bytes or
    /// less. Only an answer that it is not there is `None`: anything else that keeps it from being read is an error.
    async fn read(&self, path: &Path, version: &str, max: u64) -> Result<Option<Bytes>>;
}

/// A bucket on S3, its versions listed with `aws` and read with `store`, which have the same credentials.
pub struct Bucket {
    pub aws: Aws,
    pub name: String,
    pub store: Store,
}

#[async_trait::async_trait]
impl History for Bucket {
    async fn versions(&self, prefix: &Path, from: Option<(String, String)>) -> Result<Versions> {
        self.aws.versions(&self.name, prefix.as_ref(), from).await
    }

    async fn read(&self, path: &Path, version: &str, max: u64) -> Result<Option<Bytes>> {
        self.store.version(path, version, max).await
    }
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

/// Sweeps `app` as far as `until`, deleting what is older than `GRACE` before `now` and is named by no version of a
/// head that `history` keeps: see the module.
pub async fn run(store: &Store, history: &dyn History, app: &str, now: SystemTime, until: Instant) -> Result<Report> {
    sweep(store, history, app, (now, until), (NODES_MAX, VERSIONS_MAX)).await
}

async fn sweep(
    store: &Store,
    history: &dyn History,
    app: &str,
    (now, until): (SystemTime, Instant),
    max: (usize, usize),
) -> Result<Report> {
    let mut report = Report { done: true, ..Default::default() };
    // A store that keeps no versions deletes nothing, as a snapshot may still read it, and a clock before `GRACE` has
    // no object that old.
    let cutoff = now.checked_sub(GRACE).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).filter(|_| store.versioned);
    let Some(cutoff) = cutoff.map(|t| t.as_millis() as u64) else { return Ok(report) };
    // The age is set before anything is listed. The names are those with objects, which a name's versions decide
    // among; a name with objects and no version yet has those of turns that are not over, which are younger.
    let names = names(store, app).await?;
    let from = cursor(store, app).await;
    let todo = names.range::<str, _>((from.as_deref().map_or(Unbounded, Excluded), Unbounded));
    let mut last = None;
    for (i, name) in todo.enumerate() {
        if Instant::now() >= until {
            report.done = false;
            break;
        }
        let walked = timeout_at(until, survey(store, history, app, name, cutoff, max)).await;
        let garbage = match walked {
            Ok(Ok(Survey::Garbage(garbage))) => garbage,
            Ok(Ok(Survey::Over(what))) => {
                tracing::warn!(app, name, "sweep: the name has more {what} than a run reads, so it is not swept");
                report.skipped += 1;
                last = Some(name);
                continue;
            }
            Ok(Err(e)) => {
                tracing::warn!(app, name, "sweep: the name is not swept: {e:#}");
                report.failed += 1;
                last = Some(name);
                continue;
            }
            // The first name of a run that is too slow for one is too slow for any run, and is passed over, or none
            // after it would ever be swept. Any other is for the next run to begin with.
            Err(_) if i == 0 => {
                tracing::warn!(app, name, "sweep: the name cannot be read within a run, so it is not swept");
                report.skipped += 1;
                last = Some(name);
                continue;
            }
            Err(_) => {
                report.done = false;
                break;
            }
        };
        let bytes: u64 = garbage.iter().map(|(_, size)| size).sum();
        let paths = garbage.into_iter().map(|(path, _)| path).collect();
        // A delete that is cut short is finished by the next run, which lists what is left.
        let Ok(deleted) = timeout_at(until, store.delete_many(paths)).await else {
            report.done = false;
            break;
        };
        if deleted > 0 {
            tracing::info!(app, name, deleted, bytes, "sweep: reclaimed");
        }
        report.swept += 1;
        report.deleted += deleted;
        report.bytes += bytes;
        last = Some(name);
    }
    match (report.done, from, last) {
        (true, Some(_), _) => forget(store, app).await,
        (false, _, Some(name)) => remember(store, app, name).await,
        _ => {}
    }
    Ok(report)
}

/// What a survey of a name finds: the objects to delete, with their sizes, or what the name has more of than a run
/// reads, which is not read.
#[derive(Debug)]
enum Survey {
    Garbage(Vec<(Path, u64)>),
    Over(&'static str),
}

/// The objects of `name` that no version of its head names and that were made at `cutoff` (Unix milliseconds) or
/// before, if its versions and their trees were read in full, which `max` bounds: at most so many nodes, and so many
/// versions. An error if the versions cannot be listed, if one that is listed cannot be read for any reason but that it
/// has expired, or if a tree cannot be read whole: then there is no telling what is named.
async fn survey(
    store: &Store,
    history: &dyn History,
    app: &str,
    name: &str,
    cutoff: u64,
    (nodes, versions): (usize, usize),
) -> Result<Survey> {
    let path = name::path(app, name);
    let Some(kept) = listed(history, &path, versions).await? else { return Ok(Survey::Over("versions")) };
    // A name whose head is not among the versions has none, as a head is never deleted but by a delete marker. One
    // that is there all the same was missed by the listing, or was made since, and its tree is not to be deleted
    // either way: the name waits for the next run.
    if !kept.iter().any(|v| v.latest && !v.marker) && store.head(&path).await? {
        bail!("the head is there, and is not among the versions listed")
    }
    let heads: Vec<Version> = kept.into_iter().filter(|v| !v.marker).collect();
    let path = &path;
    let read = |version: Version| async move {
        let read = history.read(path, &version.id, name::HEAD_MAX as u64).await;
        (version, read)
    };
    let mut reads = stream::iter(heads).map(read).buffer_unordered(tree::IN_FLIGHT);
    // One set for every version, so that what their trees share is read once, and one bound for them all.
    let (mut live, mut left) = (HashSet::new(), nodes);
    while let Some((version, read)) = reads.next().await {
        let head: Head = match read? {
            Some(json) => serde_json::from_slice(&json).with_context(|| format!("{path} at {}", version.id))?,
            None if version.latest => bail!("the head's version {} is listed, and is not there to be read", version.id),
            // It expired after it was listed, and no commit writes over a version that is not current.
            None => continue,
        };
        let Some(read) = head.mark(store, app, name, &mut live, left).await? else { return Ok(Survey::Over("nodes")) };
        left -= read;
    }
    Ok(Survey::Garbage(unnamed(store, app, name, (cutoff, &live)).await?))
}

/// The versions of the head at `path`, and its delete markers, as the bucket lists them now; `None` if there are more
/// than `max`, which are not listed further. The listing is of the key as a prefix, which lists the keys that begin
/// with it too, and those come after it: a page with one of them is the last that has any of the key's.
async fn listed(history: &dyn History, path: &Path, max: usize) -> Result<Option<Vec<Version>>> {
    let mut kept = vec![];
    let mut from = None;
    loop {
        let page = history.versions(path, from).await?;
        let mut past = false;
        for version in page.versions {
            match version.key == path.as_ref() {
                true => kept.push(version),
                false => past = true,
            }
        }
        if kept.len() > max {
            return Ok(None);
        }
        match page.next {
            Some(next) if !past => from = Some(next),
            _ => return Ok(Some(kept)),
        }
    }
}

/// The objects of `name` that are not in `live` and were made at `cutoff` or before, with their sizes. A name of a page
/// of objects or fewer is one listing, and that is nearly every name. A name of more is listed by the first digit of
/// its ids: S3 lists 1,000 keys a request, in sequence, so a name of millions would take longer than a run to list, and
/// its ids are random, so they are spread evenly over the 16 digits they may begin with. Each digit's are listed on
/// their own, all at once.
///
/// A listing asks for a page when it is read past the one it has, and not before (`object_store` pages by stream), so
/// that stopping at the key after a page costs two requests, and a name of a page or fewer never asks for a second.
async fn unnamed(
    store: &Store,
    app: &str,
    name: &str,
    (cutoff, live): (u64, &HashSet<u128>),
) -> Result<Vec<(Path, u64)>> {
    let mut objects = store.objects(&store::app(app, &["values", name]), None);
    let mut page = Vec::with_capacity(PAGE);
    while let Some(meta) = objects.try_next().await? {
        if page.len() == PAGE {
            drop(objects);
            let listings = (0..16).map(|digit| partition(store, app, name, digit, (cutoff, live)));
            return Ok(try_join_all(listings).await?.concat());
        }
        page.push(meta);
    }
    Ok(page
        .into_iter()
        .filter(|meta| deletable(app, name, meta, (cutoff, live)).is_some())
        .map(|meta| (meta.location, meta.size))
        .collect())
}

/// What `unnamed` finds among the ids of `name` that begin with `digit` (0 to 15). The listing begins after the key
/// `<before>g`, where `before` is the digit before, and ends at the first key from `<digit>g`: an id has a hex digit
/// second, and `g` follows `f`, so the ids of `digit` lie between, and those of no other digit do. The digit is also
/// checked of each id, so that an id is in one listing only, whatever a store does with the offset; S3 lists in key
/// order, so the end of a listing passes by no id of its digit.
async fn partition(
    store: &Store,
    app: &str,
    name: &str,
    digit: u32,
    (cutoff, live): (u64, &HashSet<u128>),
) -> Result<Vec<(Path, u64)>> {
    let prefix = store::app(app, &["values", name]);
    let after = digit.checked_sub(1).map(|before| prefix.clone().join(format!("{before:x}g")));
    let end = prefix.clone().join(format!("{digit:x}g"));
    let mut garbage = vec![];
    let mut objects = store.objects(&prefix, after.as_ref());
    while let Some(meta) = objects.try_next().await? {
        if meta.location >= end {
            break;
        }
        if deletable(app, name, &meta, (cutoff, live)).is_some_and(|id| id >> 124 == u128::from(digit)) {
            garbage.push((meta.location, meta.size));
        }
    }
    Ok(garbage)
}

/// The id of `meta`, if it is an object the sweep may delete: one the tree of `name` could have made, that is not in
/// `live`, and that was made at `cutoff` or before.
fn deletable(app: &str, name: &str, meta: &ObjectMeta, (cutoff, live): (u64, &HashSet<u128>)) -> Option<u128> {
    let made = u64::try_from(meta.last_modified.timestamp_millis()).unwrap_or_default();
    candidate(app, name, &meta.location).filter(|id| made <= cutoff && !live.contains(id))
}

/// The id of the object at `path`, if it is one the tree of `name` could have made, and so one the sweep may delete:
/// directly under the name's own prefix, with an id of 32 lowercase hex digits, and no other key at all.
fn candidate(app: &str, name: &str, path: &Path) -> Option<u128> {
    let id = path.filename()?;
    let number = tree::number(id)?;
    (*path == tree::object(app, name, id)).then_some(number)
}

/// The names that have objects, in order, as the engine could have made them.
async fn names(store: &Store, app: &str) -> Result<BTreeSet<String>> {
    let listed = store.list(&store::app(app, &["values"])).await?;
    Ok(listed.common_prefixes.iter().filter_map(|p| named(app, "values", p)).collect())
}

/// The name that `listed` is under `dir`, if it is one the engine could have made: a name, whose path, built as every
/// path is, is the one that was listed. So a key of any other spelling, of a name that is no name, or of an odd shape,
/// is no name's, and is not touched.
fn named(app: &str, dir: &str, listed: &Path) -> Option<String> {
    let name = percent_decode_str(listed.filename()?).decode_utf8().ok()?;
    (is_name(&name) && *listed == store::app(app, &[dir, &name])).then(|| name.into_owned())
}

fn cursor_path(app: &str) -> Path {
    store::app(app, &["values", CURSOR])
}

/// The name that the last run got to, if it stopped early. Anything else (none, or not a name) starts over, which
/// costs a walk again and nothing more.
async fn cursor(store: &Store, app: &str) -> Option<String> {
    match store.get(&cursor_path(app), 256).await {
        Ok(Some((bytes, _))) => String::from_utf8(bytes.to_vec()).ok().filter(|name| is_name(name)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(app, "sweep: the cursor cannot be read: {e:#}");
            None
        }
    }
}

/// Records that the run got to `name`. If that fails the next run does the names again, and no more.
async fn remember(store: &Store, app: &str, name: &str) {
    let put = store.put(&cursor_path(app), name.to_owned().into(), PutMode::Overwrite).await;
    if let Err(e) = put {
        tracing::warn!(app, name, "sweep: the cursor cannot be written: {e:#}");
    }
}

/// Records that a run went through every name, so the next begins again.
async fn forget(store: &Store, app: &str) {
    if let Err(e) = store.delete(&cursor_path(app)).await {
        tracing::warn!(app, "sweep: the cursor cannot be deleted: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::Cron;
    use crate::tree::counting::{Counts, counting};
    use crate::tree::{Config, Root};
    use std::collections::BTreeMap;
    use std::io;
    use std::ops::Bound::Included;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};
    use wasmtime::{ensure, format_err};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// A config that makes a few keys a tree of several nodes, and a value of more than 8 bytes an object of its own.
    fn tiny() -> Config {
        Config { node: 1400, inline: 8, budget: 2048, entries: 1000, data: 100_000, blob: 100, ..tree::config() }
    }

    /// A key's versions, the newest first: an id, and the head, or nothing for a delete marker.
    type Lineage = Vec<(String, Option<Bytes>)>;

    /// The versions of heads that a test's bucket keeps, as S3 keeps them: a put of a head is a version, and a delete
    /// a delete marker, each the latest when it is made, and kept until a test expires it. They are listed `page` at a
    /// time, a page's delete markers first, as RustFS lists them. A version is read as slowly as a get of the store
    /// is, and counted; a delete marker cannot be read (S3 answers 405), nor can what a test refuses.
    struct Kept {
        store: Store,
        counts: Arc<Counts>,
        /// The versions read.
        reads: AtomicUsize,
        /// The versions of each key.
        keys: Mutex<BTreeMap<String, Lineage>>,
        made: AtomicUsize,
        page: usize,
        /// The pages listed.
        pages: AtomicUsize,
        /// The versions that are listed, and are gone when they are read: they expired in between.
        gone: Mutex<HashSet<String>>,
        /// The versions that cannot be read, as when the read is refused.
        refused: Mutex<HashSet<String>>,
    }

    impl Kept {
        fn new(store: &Store, counts: Arc<Counts>) -> Self {
            Self {
                store: store.clone(),
                counts,
                reads: AtomicUsize::new(0),
                keys: Mutex::default(),
                made: AtomicUsize::new(0),
                page: 3,
                pages: AtomicUsize::new(0),
                gone: Mutex::default(),
                refused: Mutex::default(),
            }
        }

        /// Puts `json` as the head of `name`, in the store and as its latest version, whose id it returns.
        async fn put(&self, name: &str, json: Vec<u8>) -> String {
            self.store.put(&name::path("a", name), json.clone().into(), PutMode::Overwrite).await.unwrap();
            self.made(name, Some(json.into()))
        }

        /// Deletes the head of `name`, from the store and as a delete marker that is its latest version.
        async fn delete(&self, name: &str) -> String {
            self.store.delete(&name::path("a", name)).await.unwrap();
            self.made(name, None)
        }

        fn made(&self, name: &str, json: Option<Bytes>) -> String {
            let id = format!("v{:04}", self.made.fetch_add(1, SeqCst));
            let mut keys = self.keys.lock().unwrap();
            keys.entry(name::path("a", name).to_string()).or_default().insert(0, (id.clone(), json));
            id
        }

        /// Expires the version `id`, as the lifecycle does one that is noncurrent.
        fn expire(&self, id: &str) {
            for versions in self.keys.lock().unwrap().values_mut() {
                versions.retain(|(v, _)| v != id);
            }
        }
    }

    #[async_trait::async_trait]
    impl History for Kept {
        async fn versions(&self, prefix: &Path, from: Option<(String, String)>) -> Result<Versions> {
            self.pages.fetch_add(1, SeqCst);
            let keys = self.keys.lock().unwrap();
            let range = (Included(prefix.as_ref()), Unbounded);
            let under = keys.range::<str, _>(range).take_while(|(key, _)| key.starts_with(prefix.as_ref()));
            let all: Vec<Version> = under
                .flat_map(|(key, versions)| {
                    versions.iter().enumerate().map(|(i, (id, json))| Version {
                        key: key.clone(),
                        id: id.clone(),
                        latest: i == 0,
                        marker: json.is_none(),
                    })
                })
                .collect();
            let start =
                from.map_or(0, |(key, id)| all.iter().position(|v| (&v.key, &v.id) == (&key, &id)).unwrap() + 1);
            let mut versions: Vec<_> = all[start..].iter().take(self.page).cloned().collect();
            let next = (all.len() > start + self.page).then(|| versions.last().map(|v| (v.key.clone(), v.id.clone())));
            versions.sort_by_key(|v| !v.marker);
            Ok(Versions { versions, next: next.flatten() })
        }

        async fn read(&self, path: &Path, version: &str, max: u64) -> Result<Option<Bytes>> {
            self.reads.fetch_add(1, SeqCst);
            let delay = self.counts.delay.load(SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            ensure!(!self.refused.lock().unwrap().contains(version), "403 Forbidden: {path} at {version}");
            if self.gone.lock().unwrap().contains(version) {
                return Ok(None);
            }
            let keys = self.keys.lock().unwrap();
            let Some((_, json)) = keys.get(path.as_ref()).and_then(|v| v.iter().find(|(id, _)| id == version)) else {
                return Ok(None);
            };
            let json =
                json.clone().ok_or_else(|| format_err!("405 Method Not Allowed: {version} is a delete marker"))?;
            ensure!(json.len() as u64 <= max, "{path} at {version} is over {max} bytes");
            Ok(Some(json))
        }
    }

    /// A store that keeps versions, with the versions of its heads.
    fn kept() -> (Store, Kept) {
        let (store, counts) = counting();
        let kept = Kept::new(&store, counts);
        (store, kept)
    }

    /// A tree of `keys` values that are each an object, in `name`, from the tree of `from` if given, with its head as
    /// the name's latest version: the head's id, its tree, and the objects that are in `name` now.
    async fn commit(kept: &Kept, name: &str, keys: usize, from: &Root) -> (String, Root, BTreeSet<Path>) {
        let mut tree = tree::open(&kept.store, "a", name, from, tiny()).unwrap();
        let edits = (0..keys).map(|i| (format!("k/{i:03}"), Some(Bytes::from(vec![i as u8; 40])))).collect();
        tree.write(edits).await.unwrap();
        let root = tree.finish().await.unwrap();
        let id = kept.put(name, serde_json::to_vec(&serde_json::json!({ "tree": root })).unwrap()).await;
        (id, root, held(&kept.store, name).await)
    }

    /// A tree of `keys` values that are each an object, in `name`, with its head: the objects that are in `name` now.
    async fn plant(kept: &Kept, name: &str, keys: usize) -> BTreeSet<Path> {
        commit(kept, name, keys, &Root::default()).await.2
    }

    /// The objects of `name`, as `a` has them.
    async fn held(store: &Store, name: &str) -> BTreeSet<Path> {
        let objects = store.objects(&store::app("a", &["values", name]), None).try_collect::<Vec<_>>().await.unwrap();
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
            ("the cursor", store::app("a", &["values", CURSOR])),
            ("a cursor of a name", tree::object("a", "n", CURSOR)),
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
        assert!(!is_name(CURSOR), "the cursor is no name's");
        for name in ["n", "a.b", "x~y", "Z:1", "a-b_c", "9", &"n".repeat(128)] {
            assert_eq!(named("a", "values", &at("values", name)).as_deref(), Some(name), "{name}");
        }
        let long = "n".repeat(129);
        for name in [CURSOR, ".hidden", "_x", "-x", "~x", "a b", "a/b", "a\\b", "é", "a\u{0}b", &long] {
            assert_eq!(named("a", "values", &at("values", name)), None, "{name:?}");
        }
        let parse = |s: &str| Path::parse(s).unwrap();
        assert_eq!(named("a", "values", &parse("apps/a/values/x%7ey")), None, "an escape in the wrong case");
        assert_eq!(named("a", "values", &at("names", "n")), None, "a name of another kind");
        assert_eq!(named("a", "values", &store::app("b", &["values", "n"])), None, "another app's");
        assert_eq!(named("a", "values", &store::app("a", &["values", "n", "m"])), None, "deeper than a name");
        assert_eq!(named("a", "values", &store::app("a", &["values"])), None, "no name at all");
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
    async fn the_cursor_is_a_name_or_nothing() {
        let (store, _) = counting();
        assert_eq!(cursor(&store, "a").await, None);
        let cursors = [
            (b"n2".to_vec(), Some("n2")),
            (b"".to_vec(), None),
            (b"a b".to_vec(), None),
            (b"../x".to_vec(), None),
            (b"\xff\xfe".to_vec(), None),
            (vec![b'n'; 300], None),
        ];
        for (bytes, name) in cursors {
            store.put(&cursor_path("a"), Bytes::from(bytes.clone()), PutMode::Overwrite).await.unwrap();
            assert_eq!(cursor(&store, "a").await.as_deref(), name, "{bytes:?}");
        }
    }

    #[tokio::test]
    async fn deletes_what_no_tree_names_and_leaves_the_trees() {
        let (store, kept) = kept();
        let live = plant(&kept, "n", 60).await;
        let other = plant(&kept, "m", 5).await;
        assert!(live.len() > 60 + 3, "{} objects are too few for a tree of nodes", live.len());
        for _ in 0..3 {
            stray(&store, "a", "n").await;
        }
        let elsewhere = stray(&store, "b", "n").await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 3, bytes: 15, done: true, ..Default::default() });
        assert_eq!(held(&store, "n").await, live);
        assert_eq!(held(&store, "m").await, other);
        assert!(there(&store, &elsewhere).await, "another app's is not this app's to delete");

        // What is left is a tree that is whole, which a second run reads through again and finds nothing in.
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, done: true, ..Default::default() });
    }

    #[tokio::test]
    async fn keeps_what_is_younger_than_the_grace_to_the_millisecond() {
        let (store, kept) = kept();
        plant(&kept, "n", 3).await;
        let old = stray(&store, "a", "n").await;
        std::thread::sleep(Duration::from_millis(20));
        let edge = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let young = stray(&store, "a", "n").await;

        // The grace before `edge + GRACE` is `edge`: what was made before it is old enough, and nothing after.
        let report = run(&store, &kept, "a", edge + GRACE, long()).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!there(&store, &old).await && there(&store, &young).await);
        // Now nothing is old enough, and a clock that is before the grace has nothing that old.
        let report = run(&store, &kept, "a", SystemTime::now(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, done: true, ..Default::default() });
        let report = run(&store, &kept, "a", UNIX_EPOCH + Duration::from_secs(10), long()).await.unwrap();
        assert_eq!(report, Report { done: true, ..Default::default() });
        assert!(there(&store, &young).await);
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!there(&store, &young).await);
    }

    #[tokio::test]
    async fn deletes_nothing_where_versions_are_not_kept() {
        let store = Store::memory();
        let path = stray(&store, "a", "n").await;
        let kept = Kept::new(&store, Arc::default());
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { done: true, ..Default::default() });
        assert!(there(&store, &path).await, "a snapshot may still read it");
        assert_eq!(kept.pages.load(SeqCst), 0, "nor lists a version");
    }

    #[tokio::test]
    async fn deletes_only_keys_a_tree_could_have_made() {
        let (store, kept) = kept();
        let live = plant(&kept, "n", 3).await;
        let id = store::random();
        let (upper, short, long_id) = (id.to_uppercase(), &id[1..], format!("{id}0"));
        let (tmp, wide) = (format!("{id}.tmp"), "n".repeat(129));
        let odd = [
            format!("apps/a/values/n/{upper}"),
            format!("apps/a/values/n/{short}"),
            format!("apps/a/values/n/{long_id}"),
            format!("apps/a/values/n/{tmp}"),
            format!("apps/a/values/n/{CURSOR}"),
            format!("apps/a/values/n/sub/{id}"),
            format!("apps/a/values/{CURSOR}"),
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
            // Not a name, so that the key of the cursor among them is no cursor to go on from.
            store.put(&path, Bytes::from_static(b"odd keys"), PutMode::Overwrite).await.unwrap();
        }
        // Two that go, to show the sweep went through the rest: one in a name with a tree, and one with no head.
        let (one, other) = (stray(&store, "a", "n").await, stray(&store, "a", "m").await);

        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert!(!there(&store, &one).await && !there(&store, &other).await);
        for key in &odd {
            assert!(there(&store, &Path::parse(key).unwrap()).await, "{key} was deleted");
        }
        assert!(held(&store, "n").await.is_superset(&live));
    }

    #[tokio::test]
    async fn a_name_without_a_head_names_nothing_and_its_versions_are_read_at_each_run() {
        let (store, kept) = kept();
        // No head: no object of the name is named, however many. Nor is any when the head's tree is empty.
        let (one, two) = (stray(&store, "a", "m").await, stray(&store, "a", "m").await);
        let three = stray(&store, "a", "e").await;
        kept.put("e", b"{}".to_vec()).await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 3, bytes: 15, done: true, ..Default::default() });
        for path in [one, two, three] {
            assert!(!there(&store, &path).await);
        }

        // The versions are listed and read when the name is swept, and no earlier answer is kept: a tree that a
        // version names now is whole, and one that no version names any more is not named.
        let (first, _, before) = commit(&kept, "n", 20, &Root::default()).await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!((report.deleted, report.failed), (0, 0));
        let both = plant(&kept, "n", 20).await;
        let second: BTreeSet<_> = both.difference(&before).cloned().collect();
        assert_eq!(second.len(), both.len() - before.len());
        assert_eq!(run(&store, &kept, "a", later(), long()).await.unwrap().deleted, 0, "the first is kept still");
        kept.expire(&first);
        assert_eq!(run(&store, &kept, "a", later(), long()).await.unwrap().deleted, before.len());
        assert_eq!(held(&store, "n").await, second);
    }

    #[tokio::test]
    async fn what_a_noncurrent_version_alone_names_is_kept_while_the_version_is() {
        let (store, kept) = kept();
        // A head and then another from it, as commits are: the second shares most of the first's tree, and replaces a
        // value and the nodes above it, which the first alone names now.
        let (first, root, before) = commit(&kept, "n", 60, &Root::default()).await;
        let (second, _, after) = commit(&kept, "n", 1, &root).await;
        let made: BTreeSet<_> = after.difference(&before).cloned().collect();
        assert!(made.len() >= 2, "the second made a value and a node at least");
        let strays = [stray(&store, "a", "n").await, stray(&store, "a", "n").await];

        // Both versions are roots: the strays go, and every object of either tree stays.
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert_eq!(held(&store, "n").await, after);
        assert!(!there(&store, &strays[0]).await && !there(&store, &strays[1]).await);
        assert_eq!(kept.reads.load(SeqCst), 2);

        // Once the first has expired, what only it named goes, and what the second names stays.
        kept.expire(&first);
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        let left = held(&store, "n").await;
        assert!(report.deleted >= 2 && report.failed == 0, "{report:?}");
        assert_eq!(report.deleted, after.len() - left.len());
        assert!(left.is_superset(&made) && left.is_subset(&after));

        // And once the second has too, with a head of an empty tree after it, what is left was the second's alone.
        kept.expire(&second);
        kept.put("n", b"{}".to_vec()).await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report.deleted, left.len());
        assert!(held(&store, "n").await.is_empty());
    }

    #[tokio::test]
    async fn the_versions_of_a_name_are_marked_into_one_set_under_one_bound() {
        let (mut store, kept) = kept();
        let (_, first, _) = commit(&kept, "n", 60, &Root::default()).await;
        let (_, second, _) = commit(&kept, "n", 1, &first).await;
        let mut each = vec![];
        for root in [&first, &second] {
            let tree = tree::open(&store, "a", "n", root, tiny()).unwrap();
            each.push(tree.mark(&mut HashSet::new(), usize::MAX).await.unwrap().unwrap());
        }
        store.cache = Arc::default(); // so that each node marked is a get, as a mark keeps none in the cache
        kept.counts.gets.store(0, SeqCst);
        let report = sweep(&store, &kept, "a", (later(), long()), (NODES_MAX, VERSIONS_MAX)).await.unwrap();
        assert_eq!((report.swept, report.skipped), (1, 0));
        let read = kept.counts.gets.load(SeqCst) - 1; // and the cursor

        // A node that both trees have is read once: so fewer are read than the two have, and more than either has.
        assert!(read < each[0] + each[1] && read > each[0].max(each[1]), "{read} nodes read, of {each:?}");
        // The bound is of what they read together, which each alone is under.
        let report = sweep(&store, &kept, "a", (later(), long()), (read - 1, VERSIONS_MAX)).await.unwrap();
        assert_eq!((report.swept, report.skipped), (0, 1));
        let report = sweep(&store, &kept, "a", (later(), long()), (read, VERSIONS_MAX)).await.unwrap();
        assert_eq!((report.swept, report.skipped), (1, 0));
    }

    #[tokio::test]
    async fn a_deleted_head_keeps_its_objects_while_its_versions_are_kept() {
        let (store, kept) = kept();
        let (id, _, live) = commit(&kept, "n", 20, &Root::default()).await;
        kept.delete("n").await;
        let stray = stray(&store, "a", "n").await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, done: true, ..Default::default() });
        assert!(!there(&store, &stray).await);
        assert_eq!(held(&store, "n").await, live, "a head can be put back as it was while its version is kept");

        // When it expires, only the delete marker is left, and that names nothing.
        kept.expire(&id);
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!((report.swept, report.deleted, report.failed), (1, live.len(), 0));
        assert!(held(&store, "n").await.is_empty());
    }

    #[tokio::test]
    async fn a_delete_marker_is_no_root_and_is_not_read() {
        let (store, kept) = kept();
        // Markers that are the latest and that are not, among versions: a marker read would fail the name.
        let (first, _, live) = commit(&kept, "n", 5, &Root::default()).await;
        kept.delete("n").await;
        kept.put("n", b"{}".to_vec()).await;
        kept.delete("n").await;
        let (m1, m2) = (stray(&store, "a", "n").await, stray(&store, "a", "m").await);
        kept.put("m", b"{}".to_vec()).await;
        kept.delete("m").await;
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert!(!there(&store, &m1).await && !there(&store, &m2).await);
        assert_eq!(held(&store, "n").await, live);
        // The two versions of `n` and the one of `m`, and no marker.
        assert_eq!(kept.reads.load(SeqCst), 3);
        kept.expire(&first);
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report.deleted, live.len());
    }

    #[tokio::test]
    async fn the_latest_version_gone_is_an_error_and_a_noncurrent_one_gone_is_no_root() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, kept) = kept();
        let (first, _, before) = commit(&kept, "n", 20, &Root::default()).await;
        let (second, _, _) = commit(&kept, "n", 20, &Root::default()).await;
        let stray = stray(&store, "a", "n").await;

        // The latest cannot expire within a run, so one that reads as gone says only that it cannot be read.
        kept.gone.lock().unwrap().insert(second.clone());
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { failed: 1, done: true, ..Default::default() });
        assert!(there(&store, &stray).await);
        assert!(log.warned()[0].contains(&second), "{}", log.text());

        // A noncurrent one that expired after it was listed is no root.
        kept.gone.lock().unwrap().clear();
        kept.gone.lock().unwrap().insert(first);
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report.deleted, before.len() + 1);
        assert!(!there(&store, &stray).await);
    }

    #[tokio::test]
    async fn a_version_that_cannot_be_read_or_a_head_the_listing_misses_loses_nothing() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, kept) = kept();
        let mut strays = vec![];
        let mut firsts = vec![];
        for name in ["a1", "a2", "a3"] {
            firsts.push(commit(&kept, name, 5, &Root::default()).await.0);
            commit(&kept, name, 5, &Root::default()).await;
            strays.push(stray(&store, "a", name).await);
        }
        // A noncurrent version whose read is refused, as a 403 is: only a 404 says that it is gone.
        kept.refused.lock().unwrap().insert(firsts[0].clone());
        // A head that is there, and that the listing does not have.
        kept.keys.lock().unwrap().remove(name::path("a", "a2").as_ref());
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, failed: 2, done: true, ..Default::default() });
        assert!(there(&store, &strays[0]).await && there(&store, &strays[1]).await && !there(&store, &strays[2]).await);
        let warned = log.warned();
        assert_eq!(warned.len(), 2, "{}", log.text());
        assert!(warned[0].contains("name=\"a1\"") && warned[0].contains("403"), "{}", warned[0]);
        assert!(warned[1].contains("name=\"a2\"") && warned[1].contains("not among the versions"), "{}", warned[1]);
    }

    #[tokio::test]
    async fn a_name_that_cannot_be_read_whole_loses_nothing_and_the_rest_are_swept() {
        let log = Log::default();
        let _logging = log.capture();
        let (mut store, kept) = kept();
        let mut strays = vec![];
        let mut planted = vec![];
        for name in ["a1", "a2", "a3", "a4", "a5"] {
            planted.push(plant(&kept, name, 20).await);
            strays.push(stray(&store, "a", name).await);
        }
        kept.put("a1", b"{".to_vec()).await; // not JSON
        let missing = node(&store, &planted[1]).await;
        store.delete(&missing).await.unwrap(); // a node that is gone
        let changed = node(&store, &planted[2]).await;
        store.put(&changed, Bytes::from_static(b"{}"), PutMode::Overwrite).await.unwrap(); // and one that is not it
        store.cache = Arc::default(); // a node in the cache was whole when it was put there

        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, failed: 3, done: true, ..Default::default() });
        for (i, stray) in strays.iter().enumerate() {
            assert_eq!(there(&store, stray).await, i < 3, "the stray of a{}", i + 1);
        }
        let warned = log.warned();
        assert_eq!(warned.len(), 3, "{}", log.text());
        for (name, line) in ["a1", "a2", "a3"].into_iter().zip(warned) {
            assert!(line.contains("app=\"a\"") && line.contains(&format!("name=\"{name}\"")), "{line}");
        }
    }

    #[tokio::test]
    async fn the_versions_of_a_name_are_its_own_and_not_those_of_a_name_it_begins() {
        let (store, kept) = kept();
        // `n` has two versions, and `n2` and `n~` (`n%7E`), whose keys begin with `n`'s, have more after them.
        for _ in 0..2 {
            kept.put("n", b"{}".to_vec()).await;
        }
        for _ in 0..7 {
            kept.put("n2", b"{}".to_vec()).await;
            kept.put("n~", b"{}".to_vec()).await;
        }
        let path = name::path("a", "n");
        let versions = listed(&kept, &path, 2).await.unwrap().unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.iter().all(|v| v.key == path.as_ref()));
        assert_eq!(kept.pages.load(SeqCst), 1, "a page with another key is the last");
        // A bound of the name's own versions is not reached by another's.
        assert_eq!(listed(&kept, &path, 1).await.unwrap(), None);
        let all = listed(&kept, &name::path("a", "n2"), 7).await.unwrap().unwrap();
        assert_eq!((all.len(), all.iter().filter(|v| v.latest).count()), (7, 1), "over pages");

        let (strays, other) = ([stray(&store, "a", "n").await], stray(&store, "a", "n2").await);
        let report = sweep(&store, &kept, "a", (later(), long()), (NODES_MAX, 7)).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert!(!there(&store, &strays[0]).await && !there(&store, &other).await);
    }

    #[tokio::test]
    async fn a_name_of_more_versions_than_a_run_reads_is_passed_over_and_logged() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, kept) = kept();
        let hot = plant(&kept, "hot", 3).await;
        for _ in 0..9 {
            kept.put("hot", b"{}".to_vec()).await;
        }
        plant(&kept, "cold", 3).await;
        let (in_hot, in_cold) = (stray(&store, "a", "hot").await, stray(&store, "a", "cold").await);

        let report = sweep(&store, &kept, "a", (later(), long()), (NODES_MAX, 4)).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, skipped: 1, done: true, ..Default::default() });
        assert!(there(&store, &in_hot).await && !there(&store, &in_cold).await);
        assert!(held(&store, "hot").await.is_superset(&hot));
        // `cold`'s version and none of `hot`'s ten, whose listing stops at the page that is past the bound: the first
        // of `cold`'s, and two of the four of `hot`'s.
        assert_eq!(kept.reads.load(SeqCst), 1);
        assert_eq!(kept.pages.load(SeqCst), 1 + 2);
        let warned = log.warned();
        assert_eq!(warned.len(), 1, "{}", log.text());
        assert!(warned[0].contains("name=\"hot\"") && warned[0].contains("more versions"), "{}", warned[0]);
    }

    #[tokio::test]
    async fn a_name_of_more_nodes_than_a_run_reads_is_passed_over_and_logged() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, kept) = kept();
        let big = plant(&kept, "big", 60).await;
        plant(&kept, "small", 2).await;
        assert!(big.len() > 60 + 3, "{} objects are too few to pass over", big.len());
        let (in_big, in_small) = (stray(&store, "a", "big").await, stray(&store, "a", "small").await);

        let report = sweep(&store, &kept, "a", (later(), long()), (3, VERSIONS_MAX)).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, skipped: 1, done: true, ..Default::default() });
        assert!(there(&store, &in_big).await && !there(&store, &in_small).await);
        let warned = log.warned();
        assert_eq!(warned.len(), 1, "{}", log.text());
        assert!(warned[0].contains("app=\"a\"") && warned[0].contains("name=\"big\""), "{}", warned[0]);
        assert!(warned[0].contains("more nodes"), "{}", warned[0]);
    }

    /// `n` distinct ids, evenly spread over the 128 bits, and so over every first digit, each ending in a `1`.
    fn spread(n: usize) -> Vec<String> {
        let step = u128::MAX / n.max(1) as u128;
        (0..n).map(|i| format!("{:032x}", (i as u128 * step) | 1)).collect()
    }

    async fn plant_keys(store: &Store, keys: impl IntoIterator<Item = &String>) {
        for key in keys {
            store.put(&tree::object("a", "n", key), Bytes::from_static(b"x"), PutMode::Overwrite).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_name_of_a_page_is_one_listing_and_a_name_of_more_is_the_first_and_sixteen() {
        for n in [0, 1, PAGE, PAGE + 1, 2 * PAGE] {
            let (store, counts) = counting();
            let ids = spread(n);
            plant_keys(&store, &ids).await;
            // The even ones are named by a tree.
            let live: HashSet<u128> = ids.iter().step_by(2).filter_map(|id| tree::number(id)).collect();
            let mut found = unnamed(&store, "a", "n", (u64::MAX, &live)).await.unwrap();
            found.sort();
            let want: Vec<_> = ids.iter().skip(1).step_by(2).map(|id| (tree::object("a", "n", id), 1)).collect();
            assert_eq!(found, want, "{n} objects");

            // A page is the one listing, read to its end. More is the first, read to the key after its page and no
            // further, and one for each digit, each read to the end of its range and a key past it.
            let (listings, read) = (counts.listings.load(SeqCst), counts.listed.load(SeqCst));
            match n > PAGE {
                false => assert_eq!((listings, read), (1, n), "{n} objects"),
                true => {
                    assert_eq!(listings, 1 + 16, "{n} objects");
                    assert!(read <= (PAGE + 1) + n + 16, "{n} objects: {read} keys were read");
                }
            }
        }
    }

    #[tokio::test]
    async fn the_listing_by_digit_has_every_id_once_and_no_key_that_is_not_one() {
        let (store, kept) = kept();
        let counts = kept.counts.clone();
        // Ids at both ends of every digit's range and one in the middle, so that every boundary has an id beside it on
        // each side; and keys that no tree made, which sort among them: after each range, and outside them all. A
        // page and one more of keys, so that the name is listed by digit.
        let ids: Vec<String> =
            (0..16).flat_map(|digit| ["0", "8", "f"].map(|x| format!("{digit:x}{}", x.repeat(31)))).collect();
        let mut odd: Vec<String> = (0..16)
            .flat_map(|d| [format!("{d:x}g"), format!("{d:x}g{}", "0".repeat(30)), format!("{d:x}{}g", "f".repeat(30))])
            .collect();
        odd.extend(["A", "g", "-"].map(|c| format!("{c}{}", "0".repeat(31))));
        let rest = spread(PAGE + 1 - ids.len() - odd.len());
        plant_keys(&store, ids.iter().chain(&odd).chain(&rest)).await;
        let at = |key: &String| tree::object("a", "n", key);

        // What a tree names is not found, and the rest is, each once, whichever digit it begins with.
        let live: HashSet<u128> = ids.iter().filter(|id| id.ends_with('8')).filter_map(|id| tree::number(id)).collect();
        let mut found = unnamed(&store, "a", "n", (u64::MAX, &live)).await.unwrap();
        found.sort();
        let mut want: Vec<_> = ids.iter().filter(|id| !id.ends_with('8')).chain(&rest).map(|id| (at(id), 1)).collect();
        want.sort();
        assert_eq!(want.len(), 32 + rest.len());
        assert_eq!(found, want);
        assert_eq!(counts.listings.load(SeqCst), 1 + 16);

        // A run deletes every id of a name with no head, each once, and not a key that no tree made.
        let report = run(&store, &kept, "a", later(), long()).await.unwrap();
        let count = ids.len() + rest.len();
        assert_eq!(report, Report { swept: 1, deleted: count, bytes: count as u64, done: true, ..Default::default() });
        assert_eq!(counts.deletes.load(SeqCst), count, "each once");
        assert_eq!(held(&store, "n").await, odd.iter().map(at).collect());
    }

    /// Names `n1` to `n4`, each with a head, a tree whose root the head holds, and an object that nothing names. A read
    /// takes a second of the clock, which a test pauses: a name costs its head's version, and a run its cursor besides.
    async fn slow() -> (Store, Kept, Vec<Path>) {
        let (store, kept) = kept();
        let mut strays = vec![];
        for name in ["n1", "n2", "n3", "n4"] {
            plant(&kept, name, 2).await;
            strays.push(stray(&store, "a", name).await);
        }
        kept.counts.delay.store(1000, SeqCst);
        (store, kept, strays)
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_its_time_and_the_next_run_goes_on_from_there() {
        let (store, kept, strays) = slow().await;
        let counts = kept.counts.clone();
        // The cursor is read in a second, and the head of `n1` in another: `n2`'s would be read at three.
        let until = Instant::now() + Duration::from_millis(2500);
        let report = sweep(&store, &kept, "a", (later(), until), (NODES_MAX, VERSIONS_MAX)).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, ..Default::default() });
        counts.delay.store(0, SeqCst);
        assert_eq!(cursor(&store, "a").await.as_deref(), Some("n1"));
        assert!(!there(&store, &strays[0]).await);
        for stray in &strays[1..] {
            assert!(there(&store, stray).await, "n2 to n4 are not swept");
        }

        // The next run begins after `n1`: the cursor, and the heads of `n2` to `n4`, and no more.
        counts.delay.store(1000, SeqCst);
        counts.gets.store(0, SeqCst);
        kept.reads.store(0, SeqCst);
        let report = sweep(&store, &kept, "a", (later(), long()), (NODES_MAX, VERSIONS_MAX)).await.unwrap();
        assert_eq!(report, Report { swept: 3, deleted: 3, bytes: 15, done: true, ..Default::default() });
        assert_eq!((counts.gets.load(SeqCst), kept.reads.load(SeqCst)), (1, 3));
        counts.delay.store(0, SeqCst);
        assert!(!there(&store, &cursor_path("a")).await, "a run that went through every name begins again");
        for stray in &strays {
            assert!(!there(&store, stray).await);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn passes_over_a_first_name_that_a_run_is_too_short_for() {
        let (store, kept, strays) = slow().await;
        let counts = kept.counts.clone();
        // A run that has a second and a half reads the cursor, and then cannot read one head: every run is spent on
        // the name it begins with, and is not given it twice.
        for name in ["n1", "n2", "n3"] {
            let until = Instant::now() + Duration::from_millis(1500);
            let report = sweep(&store, &kept, "a", (later(), until), (NODES_MAX, VERSIONS_MAX)).await.unwrap();
            assert_eq!(report, Report { skipped: 1, ..Default::default() }, "{name}");
            counts.delay.store(0, SeqCst);
            assert_eq!(cursor(&store, "a").await.as_deref(), Some(name));
            counts.delay.store(1000, SeqCst);
        }
        // The last name is passed over too, and with no name after it the run has gone through them all.
        let until = Instant::now() + Duration::from_millis(1500);
        let report = sweep(&store, &kept, "a", (later(), until), (NODES_MAX, VERSIONS_MAX)).await.unwrap();
        assert_eq!(report, Report { skipped: 1, done: true, ..Default::default() });
        counts.delay.store(0, SeqCst);
        assert!(!there(&store, &cursor_path("a")).await);
        for stray in &strays {
            assert!(there(&store, stray).await, "a name that was passed over keeps what it has");
        }
    }
}
