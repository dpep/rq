//! End-to-end: walk a directory of Ruby, persist symbols, read coverage back.

use std::fs;
use std::path::PathBuf;

use reference_query::index::index_path;
use reference_query::store::Store;

/// A unique temp directory for this test process (no tempfile dependency).
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-it-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn indexes_a_directory_of_ruby_end_to_end() {
    let dir = scratch_dir();
    fs::write(
        dir.join("refund.rb"),
        "module Billing\n  class RefundProcessor\n    def perform\n    end\n  end\nend\n",
    )
    .unwrap();
    // a non-Ruby file is ignored
    fs::write(dir.join("notes.txt"), "ignore me").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let stats = index_path(&mut store, &dir).unwrap();

    assert_eq!(stats.files_seen, 1, "only the .rb file is a known language");
    assert_eq!(stats.files_indexed, 1);
    // module + class + method
    assert_eq!(stats.symbols, 3);

    let overview = store.coverage_overview().unwrap();
    assert_eq!(overview.len(), 1);
    assert_eq!(overview[0].symbols, 3);
    assert_eq!(overview[0].status, "complete");

    fs::remove_dir_all(&dir).ok();
}
