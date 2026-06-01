//! End-to-end: drive the real `rq` binary through index → search → learn.
//!
//! Hermetic and reproducible — no shell `cd`, no git required. Each run uses an
//! isolated `RQ_DB`, a fresh temp repo, and sets the subprocess working
//! directory, so the shell's cwd is irrelevant.

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

/// Run the built binary with an isolated db and a set working directory.
fn rq(db: &Path, cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(args)
        .current_dir(cwd)
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

/// `git init` a directory (no commits needed) so it reads as a git repo.
fn git_init(dir: &Path) {
    let _ = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(dir)
        .output();
}

#[test]
fn index_search_and_learn_through_the_cli() {
    let (dir, db) = scratch("learn");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\nend\n").unwrap();

    // index the working directory
    let (ok, out) = rq(&db, &dir, &["--index"]);
    assert!(ok, "index failed: {out}");
    assert!(out.contains("symbol"), "index output: {out}");

    // search — the tie breaks alphabetically, so HandlerA leads
    let (ok, out) = rq(&db, &dir, &["handler"]);
    assert!(ok, "search failed: {out}");
    assert!(
        first_line(&out).contains("HandlerA"),
        "search output: {out}"
    );

    // record that the user opened HandlerB for "handler"
    let (ok, _) = rq(
        &db,
        &dir,
        &["--record", "--file", "beta.rb", "--line", "1", "handler"],
    );
    assert!(ok, "record failed");

    // now HandlerB leads
    let (ok, out) = rq(&db, &dir, &["handler"]);
    assert!(ok, "second search failed: {out}");
    assert!(
        first_line(&out).contains("HandlerB"),
        "after learning, expected HandlerB first: {out}"
    );

    // status shows the repo
    let (ok, out) = rq(&db, &dir, &["--status"]);
    assert!(ok, "status failed: {out}");
    assert!(out.contains("local:"), "status output: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn record_is_a_searchable_word_not_a_subcommand() {
    let (dir, db) = scratch("disambig");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // `rq record` searches for the symbol "record" (no hook, no match here)
    let (ok, out) = rq(&db, &dir, &["record"]);
    assert!(!ok, "no-match search should exit non-zero");
    assert!(out.is_empty(), "expected no result lines, got: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_and_ndjson_output() {
    let (dir, db) = scratch("json");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // --json: a pretty array with named fields
    let (ok, out) = rq(&db, &dir, &["handler", "--json"]);
    assert!(ok, "json search failed: {out}");
    assert!(
        out.trim_start().starts_with('['),
        "expected a JSON array: {out}"
    );
    assert!(out.contains("\"name\": \"HandlerA\""), "name field: {out}");
    assert!(out.contains("\"file\": \"alpha.rb\""), "file field: {out}");
    assert!(out.contains("\"repo\":"), "repo field: {out}");
    assert!(
        out.contains("\"signature\": \"class HandlerA\""),
        "signature: {out}"
    );

    // --ndjson: one compact object per line
    let (ok, out) = rq(&db, &dir, &["handler", "--ndjson"]);
    assert!(ok, "ndjson search failed: {out}");
    let first = out.lines().next().unwrap_or("");
    assert!(
        first.starts_with('{') && first.ends_with('}'),
        "object per line: {out}"
    );
    assert!(
        first.contains("\"name\":\"HandlerA\""),
        "compact name: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn path_filter_restricts_results() {
    let (dir, db) = scratch("path");
    fs::create_dir_all(dir.join("app/services")).unwrap();
    fs::create_dir_all(dir.join("app/models")).unwrap();
    fs::write(
        dir.join("app/services/widget.rb"),
        "class WidgetService\nend\n",
    )
    .unwrap();
    fs::write(dir.join("app/models/widget.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // unfiltered: both files match "widget"
    let (_, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert!(
        out.contains("app/models/widget.rb"),
        "expected models hit: {out}"
    );
    assert!(
        out.contains("app/services/widget.rb"),
        "expected services hit: {out}"
    );

    // --path app/services: only the services result survives
    let (ok, out) = rq(&db, &dir, &["widget", "--path", "app/services", "--ndjson"]);
    assert!(ok, "path search failed: {out}");
    assert!(
        out.contains("app/services/widget.rb"),
        "services hit kept: {out}"
    );
    assert!(
        !out.contains("app/models/widget.rb"),
        "models hit filtered out: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn limit_caps_the_number_of_results() {
    let (dir, db) = scratch("limit");
    fs::write(dir.join("a.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class HandlerB\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(&db, &dir, &["handler", "--limit", "1", "--ndjson"]);
    assert!(ok, "limited search failed: {out}");
    assert_eq!(out.lines().count(), 1, "expected exactly one result: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn index_a_subset_of_a_repo() {
    let (dir, db) = scratch("subset");
    fs::create_dir_all(dir.join("app/services")).unwrap();
    fs::create_dir_all(dir.join("app/models")).unwrap();
    fs::write(dir.join("app/services/charge.rb"), "class Charge\nend\n").unwrap();
    fs::write(dir.join("app/models/account.rb"), "class Account\nend\n").unwrap();
    // a git repo, so a search won't live-scan the whole tree (defeating the point)
    git_init(&dir);

    // index only the services subtree
    let (ok, out) = rq(&db, &dir, &["--index", "--path", "app/services"]);
    assert!(ok, "subset index failed: {out}");
    assert!(out.contains("(partial)"), "expected partial marker: {out}");

    // the indexed subtree is searchable, with a repo-relative path
    let (ok, out) = rq(&db, &dir, &["charge", "--ndjson"]);
    assert!(ok, "charge search failed: {out}");
    assert!(
        out.contains("\"file\":\"app/services/charge.rb\""),
        "subset hit: {out}"
    );

    // a symbol outside the subset isn't found — and a search does NOT silently
    // full-index over the deliberate partial index
    let (ok, _) = rq(&db, &dir, &["account"]);
    assert!(!ok, "account is outside the indexed subset, should miss");
    let (_, status) = rq(&db, &dir, &["--status"]);
    assert!(
        status.contains("partial"),
        "coverage stays partial: {status}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bare_invocation_prints_help() {
    let (dir, db) = scratch("help");
    let (ok, out) = rq(&db, &dir, &[]);
    assert!(ok, "bare rq should exit 0");
    assert!(out.contains("Reference Query"), "help banner: {out}");
    assert!(out.contains("Usage:"), "usage in help: {out}");

    let _ = fs::remove_dir_all(&dir);
}
