//! Regression: a bounded warm pass must make progress even on a repo too big to
//! index in one budget. The bug was that candidate *collection* (a cheap walk)
//! shared the time budget with *parsing* and ran first — so on a large repo the
//! walk consumed the whole budget and zero files were parsed, and repeated
//! searches made no (or nondeterministic) progress.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rq::index;
use rq::store::Store;

/// ~5000 files — enough that walking/stat-ing them all exceeds a 1ms budget.
fn big_scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-warmprog-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    for d in 0..50 {
        let sub = dir.join(format!("d{d:02}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..100 {
            fs::write(
                sub.join(format!("m{f:03}.rb")),
                format!("class C{d}_{f}\nend\n"),
            )
            .unwrap();
        }
    }
    dir
}

fn total_files(store: &Store, dir: &Path) -> i64 {
    let id = store
        .repository_id(&index::detect_identity(dir).to_string())
        .unwrap()
        .unwrap();
    store.repo_totals(id).unwrap().0
}

#[test]
fn a_tiny_budget_still_makes_progress_on_a_large_repo() {
    let dir = big_scratch();
    let mut store = Store::open_in_memory().unwrap();

    // 1ms budget: collecting 5000 paths alone takes longer than this, so a
    // time-bounded walk would parse zero. Collection must not starve parsing.
    let s1 = index::index_budgeted(&mut store, &dir, &[], Duration::from_millis(1), None).unwrap();
    assert!(
        s1.files_indexed > 0,
        "a bounded pass indexed nothing (collection starved parsing): {s1:?}"
    );

    // repeated passes grow coverage monotonically (real progress, not a fixed
    // nondeterministic slice)
    let after1 = total_files(&store, &dir);
    index::index_budgeted(&mut store, &dir, &[], Duration::from_millis(50), None).unwrap();
    let after2 = total_files(&store, &dir);
    assert!(
        after2 > after1,
        "coverage did not grow across passes: {after1} -> {after2}"
    );

    fs::remove_dir_all(&dir).ok();
}
