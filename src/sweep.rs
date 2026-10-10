//! The sweep: what a turn could not clean up. A tree's objects leak when the host of a turn died after it uploaded
//! them, and when a commit's outcome was never known. Once a day for each app, serve lists the app's names, and for
//! each one walks its tree from its head, lists its objects, and deletes those that the tree does not name and that
//! are too old for any turn to be about to name them. A delete is a delete marker: readers go on by version, and the
//! lifecycle expires what is noncurrent. `docs/decisions.md` has the argument that this is safe, and its limits.
use crate::engine::TOTAL;
use crate::name::{self, is_name};
use crate::store::{self, Store};
use crate::tree;
use futures_util::TryStreamExt;
use futures_util::future::{try_join, try_join_all};
use object_store::{PutMode, path::Path};
use percent_encoding::percent_decode_str;
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, timeout_at};
use wasmtime::{Result, bail};

/// How old an object must be to be deleted. An object that a head may yet name was made by a turn that is still under
/// way, or by one that has landed since the sweep read the head: the assertions below tie this to how long one lasts.
const GRACE: Duration = Duration::from_secs(3600);
/// The longest an invocation can last, whatever its function is set to: Lambda's own limit.
const LAMBDA_MAX: Duration = Duration::from_secs(900);
/// What the clocks of Lambda and of S3 may differ by, and then some: an object is as old as S3 says.
const SKEW: Duration = Duration::from_secs(300);
/// How long a run goes on, of serve's `TOTAL` and 30 seconds: what is left is for the cursor, the log and the response.
pub const RUN: Duration = Duration::from_secs(240);
/// The most nodes a name's walk reads. A walk reads `IN_FLIGHT` at once, some 600 in a second, so this is most of a
/// run: more than that cannot be finished, and is not begun. A name has nothing like it unless it holds millions of
/// values of a kilobyte or more in its nodes (`docs/decisions.md`).
const NODES_MAX: usize = 100_000;
/// The key, in `values/`, of where a run that stopped early got to. A name starts with a letter or a digit.
const CURSOR: &str = ".sweep";

// Every object a head names was made in a turn, which lasts `TOTAL` at most (and Lambda's `LAMBDA_MAX` whatever
// serve's own limits are) and commits at its end, or is in the tree of the head the turn began from. The sweep reads a
// head after it takes the time that sets the age to delete at, so an object a later head names is younger than that
// time less `TOTAL` or `LAMBDA_MAX`, and `SKEW` for the clocks, and `GRACE` is more than that. Were a limit raised past
// this, the sweep would delete what a commit is about to name, so the build fails instead.
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

/// When the app's sweep runs, as a cron expression in UTC: a minute of the day taken from the SHA-256 of its name, so
/// that apps are spread over the day, with no state and no flexible window.
pub fn schedule(app: &str) -> String {
    let minute = u64::from_str_radix(&store::hash(app.as_bytes())[..16], 16).unwrap_or_default() % 1440;
    format!("{} {} * * *", minute % 60, minute / 60)
}

/// Sweeps `app` as far as `until`, deleting what is older than `GRACE` before `now` and is no name's: see the module.
pub async fn run(store: &Store, app: &str, now: SystemTime, until: Instant) -> Result<Report> {
    sweep(store, app, (now, until), NODES_MAX).await
}

async fn sweep(store: &Store, app: &str, (now, until): (SystemTime, Instant), nodes: usize) -> Result<Report> {
    let mut report = Report { done: true, ..Default::default() };
    // A store that keeps no versions deletes nothing, as a snapshot may still read it, and a clock before `GRACE` has
    // no object that old.
    let cutoff = now.checked_sub(GRACE).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).filter(|_| store.versioned);
    let Some(cutoff) = cutoff.map(|t| t.as_millis() as u64) else { return Ok(report) };
    // The heads are listed before any is read, and the age is set before they are listed: a name that has none yet has
    // objects of turns that are not yet over, which are younger than the age. (The names are listed at the same time:
    // a list of them is of the objects there are, and the heads decide which of those a tree names.)
    let (heads, names) = try_join(heads(store, app), names(store, app)).await?;
    let from = cursor(store, app).await;
    let todo = names.range::<str, _>((from.as_deref().map_or(Unbounded, Excluded), Unbounded));
    let mut last = None;
    for (i, name) in todo.enumerate() {
        if Instant::now() >= until {
            report.done = false;
            break;
        }
        let walked = timeout_at(until, survey(store, app, name, (&heads, cutoff), nodes)).await;
        let garbage = match walked {
            Ok(Ok(Some(garbage))) => garbage,
            Ok(Ok(None)) => {
                tracing::warn!(app, name, nodes, "sweep: the name has more nodes than a run reads, so it is not swept");
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

/// The objects of `name` that nothing names and that were made at `cutoff` (Unix milliseconds) or before, with their
/// sizes, if its tree was read in full; `None` if it has more than `nodes`. An error if the head or any node cannot be
/// read, or is not what it should be, and so there is no telling what is named.
async fn survey(
    store: &Store,
    app: &str,
    name: &str,
    (heads, cutoff): (&HashSet<String>, u64),
    nodes: usize,
) -> Result<Option<Vec<(Path, u64)>>> {
    // A name with no head names no object. One that has a head is read from the store, and must be there: it was
    // listed, and no head is ever deleted, so an answer that it is missing says only that it cannot be read.
    let mut live = HashSet::new();
    if heads.contains(name) {
        let Some((head, _)) = name::read(store, &name::path(app, name)).await? else {
            bail!("the head is listed, and is not there to be read")
        };
        match head.reachable(store, app, name, nodes).await? {
            Some(ids) => live = ids,
            None => return Ok(None),
        }
    }
    Ok(Some(unnamed(store, app, name, (cutoff, &live)).await?))
}

/// The objects of `name` that are not in `live` and were made at `cutoff` or before, with their sizes. S3 lists 1,000
/// keys a request, in sequence, so a name of millions would take longer than a run to list. Its ids are random, so
/// they are spread evenly over the 16 digits they may begin with: each digit's are listed on their own, all at once.
async fn unnamed(
    store: &Store,
    app: &str,
    name: &str,
    (cutoff, live): (u64, &HashSet<u128>),
) -> Result<Vec<(Path, u64)>> {
    let listings = (0..16).map(|digit| partition(store, app, name, digit, (cutoff, live)));
    Ok(try_join_all(listings).await?.concat())
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
        if u64::try_from(meta.last_modified.timestamp_millis()).unwrap_or_default() > cutoff {
            continue;
        }
        let id = candidate(app, name, &meta.location);
        if id.is_some_and(|id| id >> 124 == u128::from(digit) && !live.contains(&id)) {
            garbage.push((meta.location, meta.size));
        }
    }
    Ok(garbage)
}

/// The id of the object at `path`, if it is one the tree of `name` could have made, and so one the sweep may delete:
/// directly under the name's own prefix, with an id of 32 lowercase hex digits, and no other key at all.
fn candidate(app: &str, name: &str, path: &Path) -> Option<u128> {
    let id = path.filename()?;
    let number = tree::number(id)?;
    (*path == tree::object(app, name, id)).then_some(number)
}

/// The names that have a head, as the engine could have made them.
async fn heads(store: &Store, app: &str) -> Result<HashSet<String>> {
    let listed = store.list(&store::app(app, &["names"])).await?;
    Ok(listed.objects.iter().filter_map(|o| named(app, "names", &o.location)).collect())
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
    match store.get(&cursor_path(app), None, 256).await {
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
    use crate::tree::counting::{Counts, counting};
    use crate::tree::{Limits, Shape, Tree};
    use bytes::Bytes;
    use std::io;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::{Arc, Mutex};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// Limits that make a few keys a tree of several nodes, and a value of more than 8 bytes an object of its own.
    fn tiny() -> Limits {
        Limits { node: 1400, inline: 8, budget: 2048, entries: 1000, data: 100_000, blob: 100 }
    }

    /// A tree of `keys` values that are each an object, in `name`, with its head: the objects that are in `name` now.
    async fn plant(store: &Store, name: &str, keys: usize) -> BTreeSet<Path> {
        let mut tree = Tree::open(store, "a", name, &Shape::default(), tiny()).unwrap();
        let edits = (0..keys).map(|i| (format!("k/{i:03}"), Some(tree.item(Bytes::from(vec![i as u8; 40]))))).collect();
        tree.write(edits).await.unwrap();
        let shape = tree.finish().await.unwrap();
        head(store, name, serde_json::to_vec(&serde_json::json!({ "tree": shape })).unwrap()).await;
        held(store, name).await
    }

    async fn head(store: &Store, name: &str, json: Vec<u8>) {
        store.put(&name::path("a", name), json.into(), PutMode::Overwrite).await.unwrap();
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
            let (bytes, _) = store.get(path, None, 1 << 20).await.unwrap().unwrap();
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
            assert_eq!(named("a", "names", &at("names", name)).as_deref(), Some(name), "{name}");
        }
        let long = "n".repeat(129);
        for name in [CURSOR, ".hidden", "_x", "-x", "~x", "a b", "a/b", "a\\b", "é", "a\u{0}b", &long] {
            assert_eq!(named("a", "values", &at("values", name)), None, "{name:?}");
        }
        let parse = |s: &str| Path::parse(s).unwrap();
        assert_eq!(named("a", "values", &parse("apps/a/values/x%7ey")), None, "an escape in the wrong case");
        assert_eq!(named("a", "values", &at("names", "n")), None, "a name of another kind");
        assert_eq!(named("a", "names", &at("values", "n")), None, "a name of another kind");
        assert_eq!(named("a", "values", &store::app("b", &["values", "n"])), None, "another app's");
        assert_eq!(named("a", "values", &store::app("a", &["values", "n", "m"])), None, "deeper than a name");
        assert_eq!(named("a", "values", &store::app("a", &["values"])), None, "no name at all");
    }

    #[test]
    fn schedules_spread_the_apps_over_the_day() {
        assert_eq!(schedule("app"), schedule("app"));
        let times: HashSet<_> = (0..500).map(|i| schedule(&format!("app{i}"))).collect();
        assert!(times.len() > 300, "{} times for 500 apps", times.len());
        for time in times {
            let fields: Vec<_> = time.split(' ').collect();
            let (minute, hour) = (fields[0].parse::<u32>().unwrap(), fields[1].parse::<u32>().unwrap());
            assert!(minute < 60 && hour < 24 && fields[2..] == ["*", "*", "*"], "{time}");
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

        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 2, bytes: 10, done: true, ..Default::default() });
        assert!(!there(&store, &one).await && !there(&store, &other).await);
        for key in &odd {
            assert!(there(&store, &Path::parse(key).unwrap()).await, "{key} was deleted");
        }
        assert!(held(&store, "n").await.is_superset(&live));
    }

    #[tokio::test]
    async fn a_name_without_a_head_names_nothing_and_a_head_is_read_at_each_run() {
        let (store, _) = counting();
        // No head: no object of the name is named, however many. Nor is any when the head's tree is empty.
        let (one, two) = (stray(&store, "a", "m").await, stray(&store, "a", "m").await);
        let three = stray(&store, "a", "e").await;
        head(&store, "e", b"{}".to_vec()).await;
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 2, deleted: 3, bytes: 15, done: true, ..Default::default() });
        for path in [one, two, three] {
            assert!(!there(&store, &path).await);
        }

        // A head is read from the store when its name is swept, and no earlier answer is kept: the tree that the head
        // names now is whole, and the tree that it named before is not named by it.
        let first = plant(&store, "n", 20).await;
        assert_eq!(run(&store, "a", later(), long()).await.unwrap().deleted, 0);
        let both = plant(&store, "n", 20).await;
        let second: BTreeSet<_> = both.difference(&first).cloned().collect();
        assert_eq!(second.len(), both.len() - first.len());
        assert_eq!(run(&store, "a", later(), long()).await.unwrap().deleted, first.len());
        assert_eq!(held(&store, "n").await, second);
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
        head(&store, "a1", b"{".to_vec()).await; // not JSON
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
        let text = log.text();
        let warned: Vec<_> = text.lines().filter(|line| line.contains("WARN")).collect();
        assert_eq!(warned.len(), 3, "{text}");
        for (name, line) in ["a1", "a2", "a3"].into_iter().zip(warned) {
            assert!(line.contains("app=\"a\"") && line.contains(&format!("name=\"{name}\"")), "{line}");
        }
    }

    #[tokio::test]
    async fn a_head_that_is_listed_and_not_there_is_an_error_and_one_never_listed_is_not() {
        let (store, _) = counting();
        let strays = [stray(&store, "a", "n").await, stray(&store, "a", "n").await];
        let cutoff = (later() - GRACE).duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let listed: HashSet<String> = ["n".to_owned()].into();
        let err = survey(&store, "a", "n", (&listed, cutoff), usize::MAX).await.unwrap_err();
        assert!(format!("{err:#}").contains("listed"), "{err:#}");
        let garbage = survey(&store, "a", "n", (&HashSet::new(), cutoff), usize::MAX).await.unwrap().unwrap();
        assert_eq!(garbage.iter().map(|(path, _)| path).collect::<BTreeSet<_>>(), strays.iter().collect());
    }

    #[tokio::test]
    async fn a_name_of_more_nodes_than_a_run_reads_is_passed_over_and_logged() {
        let log = Log::default();
        let _logging = log.capture();
        let (store, _) = counting();
        let big = plant(&store, "big", 60).await;
        plant(&store, "small", 2).await;
        assert!(big.len() > 60 + 3, "{} objects are too few to pass over", big.len());
        let (in_big, in_small) = (stray(&store, "a", "big").await, stray(&store, "a", "small").await);

        let report = sweep(&store, "a", (later(), long()), 3).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, skipped: 1, done: true, ..Default::default() });
        assert!(there(&store, &in_big).await && !there(&store, &in_small).await);
        let text = log.text();
        let warned: Vec<_> = text.lines().filter(|line| line.contains("WARN")).collect();
        assert_eq!(warned.len(), 1, "{text}");
        assert!(warned[0].contains("app=\"a\"") && warned[0].contains("name=\"big\""), "{text}");
    }

    #[tokio::test]
    async fn a_name_is_listed_by_the_first_digit_of_its_ids_in_full_and_once() {
        let (store, counts) = counting();
        // Ids at both ends of every digit's range and one in the middle, so that every boundary has an id beside it on
        // each side; and keys that no tree made, which sort among them: after each range, and outside them all.
        let ids: Vec<String> =
            (0..16).flat_map(|digit| ["0", "8", "f"].map(|x| format!("{digit:x}{}", x.repeat(31)))).collect();
        let mut odd: Vec<String> = (0..16)
            .flat_map(|d| [format!("{d:x}g"), format!("{d:x}g{}", "0".repeat(30)), format!("{d:x}{}g", "f".repeat(30))])
            .collect();
        odd.extend(["A", "g", "-"].map(|c| format!("{c}{}", "0".repeat(31))));
        let at = |key: &String| tree::object("a", "n", key);
        for key in ids.iter().chain(&odd) {
            store.put(&at(key), Bytes::from_static(b"x"), PutMode::Overwrite).await.unwrap();
        }

        // What a tree names is not found, and the rest is, each once, whichever digit it begins with.
        let live: HashSet<u128> = ids.iter().filter(|id| id.ends_with('8')).filter_map(|id| tree::number(id)).collect();
        counts.listed.store(0, SeqCst);
        let mut found = unnamed(&store, "a", "n", (u64::MAX, &live)).await.unwrap();
        found.sort();
        let want: Vec<_> = ids.iter().filter(|id| !id.ends_with('8')).map(|id| (at(id), 1)).collect();
        assert_eq!(want.len(), 32);
        assert_eq!(found, want);
        // Each listing is read to the end of its range, and no further: every key once, and a key past each range.
        let (keys, read) = (ids.len() + odd.len(), counts.listed.load(SeqCst));
        assert!(read <= keys + 16, "{read} keys were read of {keys}: a listing went past its range");

        // A run deletes every id of a name with no head, each once, and not a key that no tree made.
        let report = run(&store, "a", later(), long()).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 48, bytes: 48, done: true, ..Default::default() });
        assert_eq!(counts.deletes.load(SeqCst), 48, "each once");
        assert_eq!(held(&store, "n").await, odd.iter().map(at).collect());
    }

    /// Names `n1` to `n4`, each with a head, a tree whose root the head holds, and an object that nothing names. A read
    /// takes a second of the clock, which a test pauses: a name costs its head, and a run costs its cursor besides.
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
    async fn stops_at_its_time_and_the_next_run_goes_on_from_there() {
        let (store, counts, strays) = slow().await;
        // The cursor is read in a second, and the heads of `n1` in another: `n2` would be read at three.
        let until = Instant::now() + Duration::from_millis(2500);
        let report = sweep(&store, "a", (later(), until), NODES_MAX).await.unwrap();
        assert_eq!(report, Report { swept: 1, deleted: 1, bytes: 5, ..Default::default() });
        assert_eq!(cursor(&store, "a").await.as_deref(), Some("n1"));
        counts.delay.store(0, SeqCst);
        assert!(!there(&store, &strays[0]).await);
        for stray in &strays[1..] {
            assert!(there(&store, stray).await, "n2 to n4 are not swept");
        }

        // The next run begins after `n1`: the cursor, and the heads of `n2` to `n4`, and no more.
        counts.delay.store(1000, SeqCst);
        counts.gets.store(0, SeqCst);
        let report = sweep(&store, "a", (later(), long()), NODES_MAX).await.unwrap();
        assert_eq!(report, Report { swept: 3, deleted: 3, bytes: 15, done: true, ..Default::default() });
        assert_eq!(counts.gets.load(SeqCst), 4);
        counts.delay.store(0, SeqCst);
        assert!(!there(&store, &cursor_path("a")).await, "a run that went through every name begins again");
        for stray in &strays {
            assert!(!there(&store, stray).await);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn passes_over_a_first_name_that_a_run_is_too_short_for() {
        let (store, counts, strays) = slow().await;
        // A run that has a second and a half reads the cursor, and then cannot read one head: every run is spent on
        // the name it begins with, and is not given it twice.
        for name in ["n1", "n2", "n3"] {
            let until = Instant::now() + Duration::from_millis(1500);
            let report = sweep(&store, "a", (later(), until), NODES_MAX).await.unwrap();
            assert_eq!(report, Report { skipped: 1, ..Default::default() }, "{name}");
            counts.delay.store(0, SeqCst);
            assert_eq!(cursor(&store, "a").await.as_deref(), Some(name));
            counts.delay.store(1000, SeqCst);
        }
        // The last name is passed over too, and with no name after it the run has gone through them all.
        let until = Instant::now() + Duration::from_millis(1500);
        let report = sweep(&store, "a", (later(), until), NODES_MAX).await.unwrap();
        assert_eq!(report, Report { skipped: 1, done: true, ..Default::default() });
        counts.delay.store(0, SeqCst);
        assert!(!there(&store, &cursor_path("a")).await);
        for stray in &strays {
            assert!(there(&store, stray).await, "a name that was passed over keeps what it has");
        }
    }
}
