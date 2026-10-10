//! Phase 2: lazy staleness validation — a changed or deleted file is picked up
//! when its symbols are revalidated, without a full reindex.

use std::fs;
use std::path::PathBuf;

use crate::index::{self, Refresh};
use crate::search;
use crate::store::Store;
use crate::tests::support::Scratch;

fn scratch_dir() -> Scratch {
    Scratch::new("stale")
}

#[test]
fn refresh_picks_up_edits_and_deletes() {
    let dir = scratch_dir();
    let file = dir.join("a.rb");
    fs::write(&file, "class Foo\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .checkout(&dir.canonicalize().unwrap().to_string_lossy())
        .unwrap()
        .unwrap();

    assert_eq!(
        search::search(&store, "Foo", None, None, &search::Context::default(), 5).unwrap()[0].name,
        "Foo"
    );

    // Edit the file: Foo → Bar. Revalidating the file updates the index.
    fs::write(&file, "class Bar\nend\n").unwrap();
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Updated
    );
    assert!(
        search::search(&store, "Foo", None, None, &search::Context::default(), 5)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search::search(&store, "Bar", None, None, &search::Context::default(), 5).unwrap()[0].name,
        "Bar"
    );

    // Delete the file. A search-time refresh is deliberately non-destructive — a
    // failed read isn't proof of deletion, so it leaves the entry rather than
    // risk forgetting live data on a bad root — and Bar stays in the index,
    // reported missing for the caller to leave out of its answer.
    fs::remove_file(&file).unwrap();
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Missing
    );
    assert!(
        !search::search(&store, "Bar", None, None, &search::Context::default(), 5)
            .unwrap()
            .is_empty(),
        "a search never forgets — the entry survives until a reindex reconciles it"
    );

    // An indexing pass sees the whole tree and reconciles the deletion away.
    index::index_path(&mut store, &dir).unwrap();
    assert!(
        search::search(&store, "Bar", None, None, &search::Context::default(), 5)
            .unwrap()
            .is_empty(),
        "reconciled away by indexing"
    );
}

/// A same-second edit is still picked up: mtimes are stored at nanosecond
/// resolution (git's racy-mtime fix), so two writes within one second differ
/// and the incremental skip can't mistake the later one for "unchanged".
#[test]
fn racy_mtime_edit_is_reindexed() {
    let dir = Scratch::new("racy");
    let path = dir.join("a.rb");

    // pin both writes into the *same second*, 1 ms apart — under second
    // resolution the second edit is invisible to a stat
    let base = std::time::SystemTime::now() + std::time::Duration::from_secs(300);
    let pin = |p: &PathBuf, at: std::time::SystemTime| {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };

    fs::write(&path, "class Alpha\nend\n").unwrap();
    pin(&path, base);
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();

    fs::write(&path, "class Beta\nend\n").unwrap();
    pin(&path, base + std::time::Duration::from_millis(1));
    index::index_path(&mut store, &dir).unwrap();

    let hits = search::search(&store, "Beta", None, None, &search::Context::default(), 5).unwrap();
    assert_eq!(hits.first().map(|h| h.name.as_str()), Some("Beta"));
}

/// A touch without an edit is unchanged, and its new mtime is remembered so
/// later checks can skip the read; a real edit after it is still picked up.
#[test]
fn refresh_remembers_a_touch_and_still_sees_the_next_edit() {
    let dir = Scratch::new("touch");
    let path = dir.join("a.rb");
    let pin = |at: std::time::SystemTime| {
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    let base = std::time::SystemTime::now() + std::time::Duration::from_secs(300);

    fs::write(&path, "class Alpha\nend\n").unwrap();
    pin(base);
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .checkout(&dir.canonicalize().unwrap().to_string_lossy())
        .unwrap()
        .unwrap();

    let touched = base + std::time::Duration::from_secs(1);
    pin(touched);
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Unchanged
    );
    let stored = store.file_mtime(repo.id, "a.rb").unwrap().flatten();
    let expected = touched
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(stored, Some(expected), "the touch's mtime is recorded");

    fs::write(&path, "class Beta\nend\n").unwrap();
    pin(touched + std::time::Duration::from_millis(1));
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Updated
    );
}

#[test]
fn refresh_leaves_a_file_an_ignore_rule_now_excludes() {
    let dir = Scratch::new("stale-ignored");
    let file = dir.join("a.rb");
    fs::write(&file, "class Foo\nend\n").unwrap();
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .checkout(&dir.canonicalize().unwrap().to_string_lossy())
        .unwrap()
        .unwrap();

    // ignored, then edited: no pass reads it now, so a search mustn't either
    fs::write(dir.join(".ignore"), "a.rb\n").unwrap();
    fs::write(&file, "class Bar\nend\n").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Unchanged
    );
    let found = |q: &str| {
        !search::search(&store, q, None, None, &search::Context::default(), 5)
            .unwrap()
            .is_empty()
    };
    assert!(!found("Bar"));
    assert!(
        found("Foo"),
        "left for a sweep to drop, never forgotten by a search"
    );
}
