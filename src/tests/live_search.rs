//! Layer 4: search works against an un-indexed directory via a live scan.

use std::collections::HashSet;
use std::fs;

use crate::tests::support::Scratch;
use crate::{index, search};

fn scratch_dir(label: &str) -> Scratch {
    Scratch::new(&format!("live-{label}"))
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
    let scan = search::live_search(
        &index::LiveTree::detect(&dir),
        "refundproc",
        10,
        &HashSet::new(),
        None,
        false,
        &search::Context::default(),
    );
    assert_eq!(
        scan.hits.first().map(|h| h.name.as_str()),
        Some("RefundProcessor")
    );
    assert_eq!(scan.files, 1, "counts the files it parsed");
    assert!(
        scan.hits.iter().all(|h| h.source == search::Source::Live),
        "every result says it came from a live scan"
    );
}

#[test]
fn live_search_skips_already_indexed_files() {
    let dir = scratch_dir("skip");
    fs::write(dir.join("a.rb"), "class Alpha\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Beta\nend\n").unwrap();

    // pretend a.rb is already in the index: a live fallback shouldn't re-surface it
    let skip: HashSet<String> = ["a.rb".to_string()].into_iter().collect();
    let alpha = search::live_search(
        &index::LiveTree::detect(&dir),
        "alpha",
        10,
        &skip,
        None,
        false,
        &search::Context::default(),
    )
    .hits;
    assert!(alpha.is_empty(), "skipped file's symbols are not rescanned");
    // a file not in the skip set is still found
    let beta = search::live_search(
        &index::LiveTree::detect(&dir),
        "beta",
        10,
        &skip,
        None,
        false,
        &search::Context::default(),
    )
    .hits;
    assert_eq!(beta.first().map(|h| h.name.as_str()), Some("Beta"));
}

#[test]
fn prefilter_parses_substring_matches_and_misses_fuzzy() {
    let dir = scratch_dir("prefilter");
    fs::write(dir.join("a.rb"), "class Alpha\nend\n").unwrap();

    // substring query: the pre-filter keeps the file and finds it
    let hit = search::live_search(
        &index::LiveTree::detect(&dir),
        "alph",
        10,
        &HashSet::new(),
        None,
        true,
        &search::Context::default(),
    )
    .hits;
    assert_eq!(hit.first().map(|h| h.name.as_str()), Some("Alpha"));

    // "apa" is a subsequence of Alpha but not a substring — the pre-filter can't
    // see it, so a filtered scan misses; an unfiltered scan still finds it (this
    // is why the CLI retries unfiltered when a filtered scan is empty).
    assert!(
        search::live_search(
            &index::LiveTree::detect(&dir),
            "apa",
            10,
            &HashSet::new(),
            None,
            true,
            &search::Context::default()
        )
        .hits
        .is_empty(),
        "pre-filter can't see a fuzzy (non-substring) match"
    );
    let full = search::live_search(
        &index::LiveTree::detect(&dir),
        "apa",
        10,
        &HashSet::new(),
        None,
        false,
        &search::Context::default(),
    )
    .hits;
    assert_eq!(full.first().map(|h| h.name.as_str()), Some("Alpha"));
}
