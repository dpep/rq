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

/// `git init` + commit everything, so files are *tracked* (warming enumerates a
/// committed repo from `git ls-files`, not a filesystem walk).
fn git_init_commit(dir: &Path) {
    git_init(dir);
    let git = |args: &[&str]| {
        let _ = Command::new("git").args(args).current_dir(dir).output();
    };
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=t@e.st",
        "-c",
        "user.name=test",
        "commit",
        "-qm",
        "init",
    ]);
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
fn a_strong_match_suppresses_the_scattered_tail() {
    // when the query lands an exact/prefix name match, fuzzy near-matches are
    // dropped — `employeescontroller` keeps EmployeesController, not the scattered
    // EmployeeStatusController (employee + s + …controller, skipping "tatus").
    let (dir, db) = scratch("gate");
    fs::write(
        dir.join("employees_controller.rb"),
        "class EmployeesController\nend\n",
    )
    .unwrap();
    fs::write(
        dir.join("employee_status_controller.rb"),
        "class EmployeeStatusController\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (_, out) = rq(
        &db,
        &dir,
        &["employeescontroller", "--no-record", "--ndjson"],
    );
    assert!(out.contains("EmployeesController"), "exact kept: {out}");
    assert!(
        !out.contains("EmployeeStatusController"),
        "scattered fuzzy dropped: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_wildcard_bridges_an_explicit_gap() {
    // `*` reaches across words the fuzzy matcher deliberately won't skip:
    // `widget*controller` finds WidgetAlphaBravoController, where the plain
    // `widgetcontroller` query is rejected (it would have to skip whole words).
    // An unrelated file stays out.
    let (dir, db) = scratch("wildcard");
    fs::write(
        dir.join("widget_alpha_bravo_controller.rb"),
        "class WidgetAlphaBravoController\nend\n",
    )
    .unwrap();
    fs::write(dir.join("gadget_service.rb"), "class GadgetService\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(&db, &dir, &["widget*controller", "--no-record", "--ndjson"]);
    assert!(ok, "wildcard search should match: {out}");
    assert!(
        out.contains("WidgetAlphaBravoController"),
        "star bridges the gap: {out}"
    );
    assert!(!out.contains("GadgetService"), "non-match excluded: {out}");

    // the same query without the star is too scattered for the fuzzy matcher
    let (matched, _) = rq(&db, &dir, &["widgetcontroller", "--no-record"]);
    assert!(!matched, "plain fuzzy won't skip whole words");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cold_index_builds_a_working_fuzzy_index() {
    // a cold `--index` defers per-row FTS and rebuilds the trigram index in bulk;
    // a mid-word substring (not a prefix of the name) only resolves through that
    // FTS recall, so this proves the bulk rebuild produced a usable index
    let (dir, db) = scratch("coldfts");
    fs::write(dir.join("a.rb"), "class AlphaWidgetController\nend\n").unwrap();
    let (ok, out) = rq(&db, &dir, &["--index"]);
    assert!(ok, "index failed: {out}");

    // "widget" is mid-word in AlphaWidgetController — exact/prefix can't reach it
    let (ok, out) = rq(&db, &dir, &["widget", "--no-record"]);
    assert!(ok, "fuzzy recall should find it: {out}");
    assert!(out.contains("AlphaWidgetController"), "fts recall: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_compact_namespaced_class_is_found_by_its_leaf_name() {
    // `class A::B::EmployeesController` must be found by `employeescontroller`
    // (its leaf), and must survive next to a top-level EmployeesController — the
    // relevance gate shouldn't prune a legitimate exact-leaf match
    let (dir, db) = scratch("namespaced");
    fs::write(
        dir.join("a.rb"),
        "class My::Module::EmployeesController\n  def index; end\nend\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.rb"),
        "class EmployeesController\n  def show; end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(
        &db,
        &dir,
        &["employeescontroller", "--no-record", "--ndjson"],
    );
    assert!(ok, "search failed: {out}");
    // both files surface — the namespaced one isn't pruned
    assert!(out.contains("a.rb"), "namespaced class kept: {out}");
    assert!(out.contains("b.rb"), "top-level class kept: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_clean_complete_repo_does_not_re_warm_on_search() {
    // a fully-indexed, clean git repo is provably unchanged (HEAD matches, no
    // dirty files), so a search must skip the background warm entirely rather
    // than re-walk the whole tree per query — at any size
    let (dir, db) = scratch("nowarm");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    // -v traces "background warm" to stderr only when it actually warms
    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["-v", "widget", "--no-record"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .output()
        .expect("run rq");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        !stderr.contains("background warm"),
        "clean complete repo should not re-warm: {stderr}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn indexing_a_subdir_scopes_to_it_but_keeps_root_relative_paths() {
    // `rq --index <subdir>` indexes only that subtree (not the whole repo), yet
    // stores paths relative to the repo root so a later search still resolves them
    let (dir, db) = scratch("index-subdir");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::create_dir_all(dir.join("other")).unwrap();
    fs::write(dir.join("sub/a.rb"), "class InScope\nend\n").unwrap();
    fs::write(dir.join("other/b.rb"), "class OutOfScope\nend\n").unwrap();
    git_init_commit(&dir);

    let (ok, out) = rq(&db, &dir, &["--index", "sub"]);
    assert!(ok, "scoped index failed: {out}");
    assert!(out.contains("partial"), "subdir index is partial: {out}");
    assert!(
        out.contains("1 files"),
        "indexed exactly the one in-scope file: {out}"
    );

    // the in-scope class is found, at a repo-root-relative path
    let (ok, out) = rq(&db, &dir, &["inscope", "--no-record", "--ndjson"]);
    assert!(ok, "search failed: {out}");
    assert!(out.contains("InScope"), "in-scope class indexed: {out}");
    assert!(
        out.contains("\"file\":\"sub/a.rb\""),
        "path is root-relative, not subdir-relative: {out}"
    );

    // the out-of-scope class was never walked
    let (found, _) = rq(&db, &dir, &["outofscope", "--no-record"]);
    assert!(!found, "out-of-scope subtree not indexed");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn empty_status_points_at_the_real_index_flag() {
    // the hint must name the actual flag (`rq --index`), not a non-existent
    // `rq index` subcommand
    let (dir, db) = scratch("empty-status");
    let (ok, out) = rq(&db, &dir, &["--status"]);
    assert!(ok, "status on an empty db should succeed: {out}");
    assert!(out.contains("rq --index"), "hint names the flag: {out}");

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
fn positional_paths_filter_like_rg() {
    let (dir, db) = scratch("pospath");
    fs::create_dir_all(dir.join("app/services")).unwrap();
    fs::create_dir_all(dir.join("app/models")).unwrap();
    fs::write(
        dir.join("app/services/widget.rb"),
        "class WidgetService\nend\n",
    )
    .unwrap();
    fs::write(dir.join("app/models/widget.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // path given positionally after the query, rg-style
    let (ok, out) = rq(&db, &dir, &["widget", "app/services", "--ndjson"]);
    assert!(ok, "positional-path search failed: {out}");
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
fn no_record_still_returns_results() {
    let (dir, db) = scratch("norec");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(&db, &dir, &["widget", "--no-record", "--ndjson"]);
    assert!(ok, "no-record search failed: {out}");
    assert!(out.contains("\"name\":\"Widget\""), "result present: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn kind_filter_scopes_by_symbol_kind() {
    let (dir, db) = scratch("kind");
    fs::write(dir.join("a.rb"), "class Charge\n  def charge\n  end\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // both a class and a method match "charge"
    let (_, out) = rq(&db, &dir, &["charge", "--ndjson"]);
    assert!(out.contains("\"kind\":\"class\""), "class present: {out}");
    assert!(out.contains("\"kind\":\"method\""), "method present: {out}");

    // -k m (shortcut for method) keeps only the method
    let (ok, out) = rq(&db, &dir, &["charge", "-k", "m", "--ndjson"]);
    assert!(ok, "kind search failed: {out}");
    assert!(out.contains("\"kind\":\"method\""), "method kept: {out}");
    assert!(
        !out.contains("\"kind\":\"class\""),
        "class filtered out: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn first_query_warms_the_index_without_an_explicit_reindex() {
    // A git repo that was never explicitly indexed: the first query opportunistically
    // warms the index (time-bounded) and still answers.
    let (dir, db) = scratch("warm");
    git_init(&dir);
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();

    let (ok, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert!(ok, "cold search failed: {out}");
    assert!(out.contains("\"name\":\"Widget\""), "result present: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explicitly_indexes_and_recognizes_a_non_git_directory() {
    // A non-git directory (no git_init): you can still index it explicitly, and
    // a later search recognizes it as the current repo and self-heals.
    let (dir, db) = scratch("nongit");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();

    let (ok, out) = rq(&db, &dir, &["--index"]);
    assert!(ok, "index of a non-git dir failed: {out}");

    let (ok, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert!(ok, "search failed: {out}");
    assert!(out.contains("\"name\":\"Widget\""), "result present: {out}");
    // recognized as the current repo → the current-repo boost applies
    assert!(
        out.contains("current_repo"),
        "current-repo boost applied: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn lang_filter_scopes_by_language() {
    let (dir, db) = scratch("lang");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("widget.rs"), "pub struct Widget {}\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // both languages match "widget"
    let (_, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert!(out.contains("\"language\":\"ruby\""), "ruby present: {out}");
    assert!(out.contains("\"language\":\"rust\""), "rust present: {out}");

    // -x rs keeps only rust
    let (ok, out) = rq(&db, &dir, &["widget", "-x", "rs", "--ndjson"]);
    assert!(ok, "lang search failed: {out}");
    assert!(out.contains("\"language\":\"rust\""), "rust kept: {out}");
    assert!(
        !out.contains("\"language\":\"ruby\""),
        "ruby filtered: {out}"
    );

    // -x r matches ruby + rust (prefix match) — both kept
    let (_, out) = rq(&db, &dir, &["widget", "-x", "r", "--ndjson"]);
    assert!(
        out.contains("\"language\":\"ruby\"") && out.contains("\"language\":\"rust\""),
        "both kept for -x r: {out}"
    );

    // spelled out keeps only that language
    let (_, out) = rq(&db, &dir, &["widget", "-x", "ruby", "--ndjson"]);
    assert!(out.contains("\"language\":\"ruby\""), "ruby kept: {out}");
    assert!(
        !out.contains("\"language\":\"rust\""),
        "rust filtered: {out}"
    );

    // -x py matches neither → no results
    let (ok, out) = rq(&db, &dir, &["widget", "-x", "py", "--ndjson"]);
    assert!(!ok, "no python here, should exit non-zero");
    assert!(out.is_empty(), "expected no results: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn searching_from_a_subdirectory_uses_the_repo_root() {
    // Regression: warming/indexing must key off the repo root, not the cwd. A
    // search run from a subdirectory previously re-keyed the same repo under
    // subdir-relative paths and a fresh checkout root, so the reconcile and
    // staleness revalidation forgot everything indexed from the root.
    let (dir, db) = scratch("subdir");
    git_init(&dir);
    let sub = dir.join("nested");
    fs::create_dir_all(&sub).unwrap();
    fs::write(dir.join("top.rb"), "class TopWidget\nend\n").unwrap();
    fs::write(sub.join("deep.rb"), "class DeepWidget\nend\n").unwrap();

    // index the whole repo from its root — paths are repo-root-relative
    let (ok, _) = rq(&db, &dir, &["--index"]);
    assert!(ok);
    let (_, out) = rq(&db, &dir, &["DeepWidget", "--no-record"]);
    assert!(out.contains("nested/deep.rb"), "root-relative path: {out}");

    // searching from the subdirectory must reuse the same repo, not fork a new
    // one keyed at the subdir — the top-level symbol stays found, and there's
    // still exactly one repository with both files.
    let (_, out) = rq(&db, &sub, &["TopWidget", "--no-record"]);
    assert!(
        out.contains("TopWidget"),
        "top symbol found from subdir: {out}"
    );
    let (_, status) = rq(&db, &dir, &["--status"]);
    assert_eq!(
        status.lines().count(),
        1,
        "one repo, not re-keyed: {status}"
    );
    assert!(status.contains("2 files"), "both files retained: {status}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn warming_a_committed_repo_indexes_tracked_source() {
    // A committed git repo enumerates candidates from `git ls-files` (reading
    // git's index) instead of walking the filesystem — the path that keeps a huge
    // repo from burning its warm budget on a non-source tree. The tracked source
    // is found on the first search; a non-source file is ignored.
    let (dir, db) = scratch("gitwarm");
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::write(dir.join("lib/widget.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("README.md"), "# docs, not source\n").unwrap();
    git_init_commit(&dir);

    let (ok, out) = rq(&db, &dir, &["Widget", "--no-record"]);
    assert!(ok, "warmed search failed: {out}");
    assert!(
        out.contains("lib/widget.rb"),
        "tracked source warmed: {out}"
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
