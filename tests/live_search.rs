//! Layer 4: search works against an un-indexed directory via a live scan.

use std::fs;
use std::path::PathBuf;

use rq::search;

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-live-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn live_search_finds_symbols_without_an_index() {
    let dir = scratch_dir();
    fs::write(
        dir.join("refund.rb"),
        "module Billing\n  class RefundProcessor\n    def perform\n    end\n  end\nend\n",
    )
    .unwrap();

    // No Store, no `rq index` — scan the directory live.
    let hits = search::live_search(&dir, "refundproc", 10);
    assert_eq!(
        hits.first().map(|h| h.name.as_str()),
        Some("RefundProcessor")
    );

    fs::remove_dir_all(&dir).ok();
}
