//! Regression: a bounded warm pass must make real, incremental progress on a
//! repo too big to index in one pass. The bug was that candidate collection
//! shared the budget with parsing and ran first — so on a large repo the walk
//! consumed the budget and zero files were parsed, and repeated searches made no
//! (or nondeterministic) progress. Indexing is now a fused walk→parse→write
//! pipeline; each bounded pass advances coverage and persists as it goes.
//!
//! Bounded here by the count cap (`RQ_COLLECT_CAP`) rather than wall-clock time,
//! so the assertions are deterministic instead of racing a millisecond budget.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rq::index;
use rq::store::Store;

const FILES: usize = 1000;
const CAP: usize = 200;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-warmprog-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    for d in 0..50 {
        let sub = dir.join(format!("d{d:02}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..(FILES / 50) {
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
fn a_bounded_pass_makes_incremental_progress() {
    // Cap each pass to a slice of the repo, deterministically — no timing race.
    // SAFETY: single-test binary, set before any indexing; nothing else reads env.
    unsafe { std::env::set_var("RQ_COLLECT_CAP", CAP.to_string()) };

    let dir = scratch();
    let mut store = Store::open_in_memory().unwrap();
    // generous budget: the cap, not the clock, bounds the pass
    let budget = Duration::from_secs(5);

    let s1 = index::index_budgeted(&mut store, &dir, &[], budget, None).unwrap();
    assert!(
        s1.files_indexed > 0,
        "a bounded pass made no progress: {s1:?}"
    );
    assert!(
        s1.files_indexed < FILES,
        "a bounded pass is partial, not the whole repo: {s1:?}"
    );

    // repeated passes grow coverage monotonically until everything is indexed
    let mut last = total_files(&store, &dir);
    for _ in 0..FILES / CAP + 2 {
        if last as usize >= FILES {
            break;
        }
        index::index_budgeted(&mut store, &dir, &[], budget, None).unwrap();
        let now = total_files(&store, &dir);
        assert!(now > last, "coverage stalled at {last}");
        last = now;
    }
    assert_eq!(
        last as usize, FILES,
        "every file indexed after enough passes"
    );

    fs::remove_dir_all(&dir).ok();
}
