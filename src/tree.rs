//! A name's tree: the `tric-tree` crate, with tric's limits, under the name's own prefix in the app's bucket.
pub use tric_tree::{Config, Error, Full, IN_FLIGHT, Item, Root, Tree, Values, number};

use crate::store::{self, Store};
use object_store::path::Path;

/// The most keys a name holds.
pub const ENTRIES_MAX: u64 = 4 << 20;
/// The most bytes of values a name holds.
pub const DATA_MAX: u64 = 16 << 30;

/// The tree's configuration for a name: the crate's defaults, with tric's limits.
pub fn config() -> Config {
    Config { entries: ENTRIES_MAX, data: DATA_MAX, ..Config::default() }
}

/// Where a name's tree keeps its objects: `apps/<app>/values/<name>`.
pub fn prefix(app: &str, name: &str) -> Path {
    store::app(app, &["values", name])
}

/// Where the object `id` of a name's tree is.
pub fn object(app: &str, name: &str, id: &str) -> Path {
    tric_tree::object(&prefix(app, name), id)
}

/// The tree of `app`'s name `name` at `root`, in `store` and its cache.
pub fn open(store: &Store, app: &str, name: &str, root: &Root, config: Config) -> Result<Tree, Error> {
    Tree::open(store.inner.clone(), prefix(app, name), store.cache.clone(), root, config)
}

#[cfg(test)]
pub(crate) mod counting {
    use crate::store::Store;
    use std::sync::Arc;
    pub use tric_tree::testing::Counts;

    /// A versioned store in memory that counts what is asked of it, as [`tric_tree::testing::counting`] does.
    pub fn counting() -> (Store, Arc<Counts>) {
        let (inner, counts) = tric_tree::testing::counting();
        (Store { inner, once: None, versioned: true, cache: Arc::default() }, counts)
    }
}
