//! Shared scaffolding for the in-crate tests: build a throwaway indexed repo,
//! and ask it what ranks first.
//!
//! Only what more than one test file needs. A test that sets up something of
//! its own — a git history, several files, a deliberately stale index — keeps
//! that setup where it is read.

use std::fs;
use std::path::PathBuf;

use crate::index;
use crate::search::{self, ActiveFiles};
use crate::store::Store;

/// Write `source` as `name` into a throwaway repo dir of its own and index it,
/// returning the store and the dir (the caller removes it).
///
/// `tag` keeps each test's dir distinct — they run as parallel threads of one
/// process, so a shared path would let one test's cleanup wipe another's
/// fixture mid-index. Keep tags unique across every caller, not just within a
/// file.
pub(crate) fn indexed(tag: &str, name: &str, source: &str) -> (Store, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rq-fixture-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), source).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    (store, dir)
}

/// The top-ranked hit for `query`, or a panic naming the query that found
/// nothing. Ordering is what these tests assert, so the first hit is the
/// answer.
pub(crate) fn top(store: &Store, query: &str) -> search::Hit {
    let hits = search::search(store, query, None, None, &ActiveFiles::default(), 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits.hits.into_iter().next().unwrap()
}
