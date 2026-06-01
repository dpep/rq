//! Phase 3: a recorded selection, once rolled up, lifts that result in ranking.

use std::fs;
use std::path::PathBuf;

use rq::index;
use rq::search;
use rq::store::Store;

fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-learn-{}-{label}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_selection_changes_future_ranking() {
    let dir = scratch_dir("rank");
    // Two equally-good prefix matches for "handler".
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .repository_id(&index::detect_identity(&dir).to_string())
        .unwrap()
        .unwrap();

    // Baseline: the tie breaks alphabetically, so HandlerA leads.
    let before =
        search::search(&store, "handler", None, &search::ActiveFiles::default(), 10).unwrap();
    assert_eq!(before[0].name, "HandlerA");

    // The user picks HandlerB for "handler". Record it, then roll it up.
    store
        .record_event(
            "select",
            Some("handler"),
            Some(repo),
            Some("beta.rb"),
            Some(1),
            None,
        )
        .unwrap();
    assert_eq!(store.aggregate_events(100).unwrap(), 1);

    // Now HandlerB wins, carrying a learned feature.
    let after =
        search::search(&store, "handler", None, &search::ActiveFiles::default(), 10).unwrap();
    assert_eq!(after[0].name, "HandlerB");
    assert!(after[0].features.iter().any(|f| f.name == "learned"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_shorter_query_selection_informs_a_longer_one() {
    let dir = scratch_dir("prefix");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\nend\n").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let repo = store
        .repository_id(&index::detect_identity(&dir).to_string())
        .unwrap()
        .unwrap();

    // The pick was made for the shorter query "han".
    store
        .record_event(
            "select",
            Some("han"),
            Some(repo),
            Some("beta.rb"),
            Some(1),
            None,
        )
        .unwrap();
    store.aggregate_events(100).unwrap();

    // Typing the longer "handler" still benefits.
    let hits =
        search::search(&store, "handler", None, &search::ActiveFiles::default(), 10).unwrap();
    assert_eq!(hits[0].name, "HandlerB");
    assert!(hits[0].features.iter().any(|f| f.name == "learned"));

    fs::remove_dir_all(&dir).ok();
}
