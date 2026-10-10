//! Shared scaffolding for the in-crate tests: build a throwaway indexed repo,
//! and ask it what ranks first.
//!
//! Only what more than one test file needs. A test that sets up something of
//! its own — a git history, several files, a deliberately stale index — keeps
//! that setup where it is read.

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::index;
use crate::search::{self, Context};
use crate::store::Store;

/// A fresh temp directory, `rq-{label}-{pid}-{n}`, removed on drop — so a
/// failing assert leaks nothing. `n` counts up per process: tests run as
/// threads of one, and two sharing a label never share a dir. A database a
/// test opens goes inside, and its `-wal`, `-shm` and `.lock` go with it.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(label: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        #[allow(
            clippy::disallowed_methods,
            reason = "the one place an in-crate test names the temp dir"
        )]
        let dir = std::env::temp_dir().join(format!("rq-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Write `source` as `name` into a throwaway repo dir of its own and index it,
/// returning the store and the dir, which goes when it drops.
pub(crate) fn indexed(tag: &str, name: &str, source: &str) -> (Store, Scratch) {
    indexed_files(tag, &[(name, source)])
}

/// [`indexed`] for a repo of several files.
pub(crate) fn indexed_files(tag: &str, files: &[(&str, &str)]) -> (Store, Scratch) {
    let dir = Scratch::new(&format!("fixture-{tag}"));
    for (name, source) in files {
        fs::write(dir.join(name), source).unwrap();
    }

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    (store, dir)
}

/// The top-ranked hit for `query`, or a panic naming the query that found
/// nothing. Ordering is what these tests assert, so the first hit is the
/// answer.
pub(crate) fn top(store: &Store, query: &str) -> search::Hit {
    let hits = search::search(store, query, None, None, &Context::default(), 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits.hits.into_iter().next().unwrap()
}
