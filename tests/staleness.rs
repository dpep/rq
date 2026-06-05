//! Phase 2: lazy staleness validation — a changed or deleted file is picked up
//! when its symbols are revalidated, without a full reindex.

use std::fs;
use std::path::PathBuf;

use rq::index::{self, Refresh};
use rq::search;
use rq::store::Store;

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-stale-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn refresh_picks_up_edits_and_deletes() {
    let dir = scratch_dir();
    let file = dir.join("a.rb");
    fs::write(&file, "class Foo\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .repository_id(&index::detect_identity(&dir).to_string())
        .unwrap()
        .unwrap();

    assert_eq!(
        search::search(&store, "Foo", None, &search::ActiveFiles::default(), 5).unwrap()[0].name,
        "Foo"
    );

    // Edit the file: Foo → Bar. Revalidating the file updates the index.
    fs::write(&file, "class Bar\nend\n").unwrap();
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Updated
    );
    assert!(
        search::search(&store, "Foo", None, &search::ActiveFiles::default(), 5)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search::search(&store, "Bar", None, &search::ActiveFiles::default(), 5).unwrap()[0].name,
        "Bar"
    );

    // Delete the file. A search-time refresh is deliberately non-destructive — a
    // failed read isn't proof of deletion, so it leaves the entry rather than
    // risk forgetting live data on a bad root — and Bar stays findable.
    fs::remove_file(&file).unwrap();
    assert_eq!(
        index::refresh_file(&mut store, repo, &dir, "a.rb").unwrap(),
        Refresh::Unchanged
    );
    assert!(
        !search::search(&store, "Bar", None, &search::ActiveFiles::default(), 5)
            .unwrap()
            .is_empty(),
        "a search never forgets — the entry survives until a reindex reconciles it"
    );

    // An indexing pass sees the whole tree and reconciles the deletion away.
    index::index_path(&mut store, &dir).unwrap();
    assert!(
        search::search(&store, "Bar", None, &search::ActiveFiles::default(), 5)
            .unwrap()
            .is_empty(),
        "reconciled away by indexing"
    );

    fs::remove_dir_all(&dir).ok();
}
