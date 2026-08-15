//! Regression: a bounded warm pass must make real, incremental progress on a
//! repo too big to index in one pass. The bug was that candidate collection
//! shared the budget with parsing and ran first — so on a large repo the walk
//! consumed the budget and zero files were parsed, and repeated searches made no
//! (or nondeterministic) progress. Indexing is now a fused walk→parse→write
//! pipeline; each bounded pass advances coverage and persists as it goes.
//!
//! Bounded by the count cap (`RQ_COLLECT_CAP`) rather than wall-clock time, so
//! the assertions are deterministic instead of racing a millisecond budget. The
//! cap goes to the *child's* environment: this test used to set it on its own
//! process, which was only safe while it had a test binary to itself.
//!
//! The repo is `git init`-ed but left uncommitted on purpose. Warming enumerates
//! tracked files when it can; with nothing tracked it falls back to the
//! filesystem walk, which is the path the original bug lived on.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FILES: usize = 1000;
const CAP: usize = 200;

fn scratch() -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir();
    let dir = base.join(format!("rq-warmprog-{}", std::process::id()));
    let db = base.join(format!("rq-warmprog-{}.db", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
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
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .output();
    (dir, db)
}

/// One capped warm pass: a search that misses drives the sweep and returns.
/// Detached warming is off so no child races the next assertion.
fn warm_pass(db: &Path, dir: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["Nonexistent", "--no-record"])
        .current_dir(dir)
        .env("RQ_DB", db)
        .env("RQ_WARM_DETACH", "0")
        .env("RQ_COLLECT_CAP", CAP.to_string())
        .output()
        .expect("run rq");
    // a miss is exit 1 (definitive) or 2 (still warming) — both are fine here
    assert_ne!(
        out.status.code(),
        Some(101),
        "rq panicked: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Files this repo has indexed so far, per `rq --status`.
fn indexed_files(db: &Path, dir: &Path) -> usize {
    let out = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["--status", "--ndjson"])
        .current_dir(dir)
        .env("RQ_DB", db)
        .output()
        .expect("run rq --status");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("files").and_then(|f| f.as_u64()))
        .max()
        .unwrap_or(0) as usize
}

#[test]
fn a_bounded_pass_makes_incremental_progress() {
    let (dir, db) = scratch();

    warm_pass(&db, &dir);
    let first = indexed_files(&db, &dir);
    assert!(first > 0, "a bounded pass made no progress");
    assert!(
        first < FILES,
        "a bounded pass is partial, not the whole repo: {first}"
    );

    // repeated passes grow coverage monotonically until everything is indexed
    let mut last = first;
    for _ in 0..FILES / CAP + 2 {
        if last >= FILES {
            break;
        }
        warm_pass(&db, &dir);
        let now = indexed_files(&db, &dir);
        assert!(now > last, "coverage stalled at {last}");
        last = now;
    }
    assert_eq!(last, FILES, "every file indexed after enough passes");

    let _ = fs::remove_dir_all(&dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
}
