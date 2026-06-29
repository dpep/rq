//! Opportunistic, time-bounded warming: the first query never blocks on a full
//! walk. A tiny budget still indexes the active (branch) files; a full sweep
//! marks coverage complete, and across sweeps the index tracks added and
//! deleted files.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use reference_query::index;
use reference_query::search::{self, ActiveFiles};
use reference_query::store::Store;

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-budget-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn finds(store: &Store, query: &str) -> bool {
    !search::search(store, query, None, &ActiveFiles::default(), 5)
        .unwrap()
        .is_empty()
}

#[test]
fn a_zero_budget_still_indexes_the_active_files() {
    let dir = scratch_dir("active");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    // budget exhausted immediately: only the active file is indexed, the walk is
    // skipped, and coverage is left "warming" (not yet complete)
    index::index_budgeted(
        &mut store,
        &dir,
        &["a.rb".to_string()],
        Duration::ZERO,
        None,
    )
    .unwrap();

    assert!(finds(&store, "Widget"), "active file is indexed regardless");
    assert!(!finds(&store, "Gadget"), "the walk hasn't run yet");
    assert_eq!(store.coverage_overview().unwrap()[0].status, "warming");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_full_sweep_completes_and_tracks_added_and_deleted_files() {
    let dir = scratch_dir("sweep");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let ample = Duration::from_secs(5);
    index::index_budgeted(&mut store, &dir, &[], ample, None).unwrap();

    assert!(finds(&store, "Widget"));
    assert!(finds(&store, "Gadget"));
    assert_eq!(store.coverage_overview().unwrap()[0].status, "complete");

    // a new file appears — a later sweep picks it up
    fs::write(dir.join("c.rb"), "class Sprocket\nend\n").unwrap();
    index::index_budgeted(&mut store, &dir, &[], ample, None).unwrap();
    assert!(finds(&store, "Sprocket"));

    // a file is deleted — a completed sweep reconciles it out of the index
    fs::remove_file(dir.join("b.rb")).unwrap();
    index::index_budgeted(&mut store, &dir, &[], ample, None).unwrap();
    assert!(
        !finds(&store, "Gadget"),
        "deleted file's symbols are forgotten"
    );
    assert!(finds(&store, "Widget"), "surviving files remain");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_empty_source_tree_never_reports_complete() {
    // a tree with no indexable source must not settle as "complete" with zero
    // files — that's almost always a failed enumeration, and (with warm-skip) it
    // would strand the repo at zero. It stays "warming" so queries keep polling.
    let dir = scratch_dir("empty-src");
    fs::write(dir.join("notes.txt"), "not source\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_budgeted(&mut store, &dir, &[], Duration::from_secs(5), None).unwrap();

    assert_eq!(store.coverage_overview().unwrap()[0].status, "warming");
    assert!(!finds(&store, "anything"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_cancelled_pass_stops_early_and_stays_warming() {
    // The interactive cold-start escalation runs a long warm the user can abort
    // with Ctrl-C. A set cancel flag must halt the walk promptly — leaving
    // coverage "warming" (not falsely "complete") while keeping whatever batches
    // it already committed.
    let dir = scratch_dir("cancel");
    for i in 0..20 {
        fs::write(
            dir.join(format!("m{i:02}.rb")),
            format!("class C{i}\nend\n"),
        )
        .unwrap();
    }

    let mut store = Store::open_in_memory().unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(true); // aborted before it starts
    let ample = Duration::from_secs(5); // time isn't the bound here — the abort is
    let stats =
        index::index_budgeted_cancellable(&mut store, &dir, &[], ample, None, &cancel).unwrap();

    assert!(
        stats.files_indexed < 20,
        "a cancelled pass didn't index the whole repo: {stats:?}"
    );
    assert_eq!(
        store.coverage_overview().unwrap()[0].status,
        "warming",
        "an aborted sweep is never finalized as complete"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_content_scan_returns_only_matching_files_to_persist() {
    use std::collections::HashSet;

    let dir = scratch_dir("scanq");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();

    // content-scan for "widget": only a.rb contains it, so only it comes back —
    // ready for the warming fallback to persist (fold the scan into the index)
    let scanned = index::scan(&dir, &HashSet::new(), None, Some(b"widget"));
    assert_eq!(scanned.len(), 1, "only the matching file: {scanned:?}");
    assert_eq!(scanned[0].path, "a.rb");
    assert!(scanned[0].symbols.iter().any(|s| s.name == "Widget"));
}
