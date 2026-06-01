//! End-to-end: drive the real `rq` binary through index → search → learn.
//!
//! Hermetic and reproducible — no `cd`, no git required. Each run uses an
//! isolated `RQ_DB` and a fresh temp repo, and invokes the compiled binary via
//! `CARGO_BIN_EXE_rq`. `-C` points rq at the repo so the shell's cwd is
//! irrelevant.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unique temp paths for this test (repo dir + db file), cleaned first.
fn scratch(label: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir();
    let dir = base.join(format!("rq-e2e-{}-{label}", std::process::id()));
    let db = base.join(format!("rq-e2e-{}-{label}.db", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", db.display()));
    }
    fs::create_dir_all(&dir).unwrap();
    (dir, db)
}

/// Run the built binary with an isolated db; return (success, stdout).
fn rq(db: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(args)
        .env("RQ_DB", db)
        .output()
        .expect("run rq");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[test]
fn index_search_and_learn_through_the_cli() {
    let (dir, db) = scratch("learn");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\nend\n").unwrap();
    let dir_s = dir.to_str().unwrap();

    // index
    let (ok, out) = rq(&db, &["--index", dir_s]);
    assert!(ok, "index failed: {out}");
    assert!(out.contains("symbol"), "index output: {out}");

    // search — the tie breaks alphabetically, so HandlerA leads
    let (ok, out) = rq(&db, &["-C", dir_s, "handler"]);
    assert!(ok, "search failed: {out}");
    assert!(
        first_line(&out).contains("HandlerA"),
        "search output: {out}"
    );

    // record that the user opened HandlerB for "handler"
    let (ok, _) = rq(
        &db,
        &[
            "-C", dir_s, "--record", "--file", "beta.rb", "--line", "1", "handler",
        ],
    );
    assert!(ok, "record failed");

    // now HandlerB leads
    let (ok, out) = rq(&db, &["-C", dir_s, "handler"]);
    assert!(ok, "second search failed: {out}");
    assert!(
        first_line(&out).contains("HandlerB"),
        "after learning, expected HandlerB first: {out}"
    );

    // status shows the repo
    let (ok, out) = rq(&db, &["--status"]);
    assert!(ok, "status failed: {out}");
    assert!(out.contains("local:"), "status output: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn record_is_a_searchable_word_not_a_subcommand() {
    let (dir, db) = scratch("disambig");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    let dir_s = dir.to_str().unwrap();
    rq(&db, &["--index", dir_s]);

    // `rq record` searches for the symbol "record" (no hook, no match here)
    let (ok, out) = rq(&db, &["-C", dir_s, "record"]);
    assert!(!ok, "no-match search should exit non-zero");
    assert!(out.is_empty(), "expected no result lines, got: {out}");

    let _ = fs::remove_dir_all(&dir);
}
