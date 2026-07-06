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

fn git(dir: &PathBuf, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Commit times keep flowing into the recency signal across incremental
/// reindexes (the capture reads only the commits since the last one).
#[test]
fn commit_times_capture_survives_incremental_reindex() {
    let dir = std::env::temp_dir().join(format!("rq-it-git-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "t"]);

    fs::write(dir.join("alpha.rb"), "class Alpha\nend\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);

    let mut store = Store::open_in_memory().unwrap();
    index_path(&mut store, &dir).unwrap();

    // second commit, then an incremental reindex — the capture now reads only
    // the new commit (old..HEAD) yet both files carry a commit time
    fs::write(dir.join("beta.rb"), "class Beta\nend\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "two"]);
    index_path(&mut store, &dir).unwrap();

    let ts_of = |name: &str| {
        store
            .search_candidates(name, 10, false)
            .unwrap()
            .into_iter()
            .find(|c| c.name.to_lowercase() == name)
            .unwrap()
            .git_ts
    };
    let alpha = ts_of("alpha").expect("alpha has a commit time");
    let beta = ts_of("beta").expect("beta has a commit time");
    assert!(beta >= alpha);

    // the capture marker tracks HEAD, so an unmoved HEAD skips the git log
    let identity = store.coverage_overview().unwrap()[0].identity.clone();
    let repo_id = store.repository_id(&identity).unwrap().unwrap();
    assert_eq!(
        store.git_ts_head(repo_id).unwrap().as_deref(),
        Some(git(&dir, &["rev-parse", "HEAD"]).as_str())
    );

    fs::remove_dir_all(&dir).ok();
}
