//! Layer 4: search works against an un-indexed directory via a live scan.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use rq::search;

fn scratch_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-live-{}-{label}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn live_search_finds_symbols_without_an_index() {
    let dir = scratch_dir("basic");
    fs::write(
        dir.join("refund.rb"),
        "module Billing\n  class RefundProcessor\n    def perform\n    end\n  end\nend\n",
    )
    .unwrap();

    // No Store, no `rq index` — scan the directory live (unbounded, skip nothing).
    let hits = search::live_search(&dir, "refundproc", 10, &HashSet::new(), None);
    assert_eq!(
        hits.first().map(|h| h.name.as_str()),
        Some("RefundProcessor")
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn live_search_skips_already_indexed_files() {
    let dir = scratch_dir("skip");
    fs::write(dir.join("a.rb"), "class Alpha\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Beta\nend\n").unwrap();

    // pretend a.rb is already in the index: a live fallback shouldn't re-surface it
    let skip: HashSet<String> = ["a.rb".to_string()].into_iter().collect();
    let alpha = search::live_search(&dir, "alpha", 10, &skip, None);
    assert!(alpha.is_empty(), "skipped file's symbols are not rescanned");
    // a file not in the skip set is still found
    let beta = search::live_search(&dir, "beta", 10, &skip, None);
    assert_eq!(beta.first().map(|h| h.name.as_str()), Some("Beta"));

    fs::remove_dir_all(&dir).ok();
}
