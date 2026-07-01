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

/// Run a `-v` search and report whether it spawned a background warm (traced to
/// stderr). Lets tests assert on the warm decision without timing flakiness.
fn warmed(db: &Path, cwd: &Path, query: &str) -> bool {
    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["-v", query, "--no-record"])
        .current_dir(cwd)
        .env("RQ_DB", db)
        .output()
        .expect("run rq");
    String::from_utf8_lossy(&run.stderr).contains("background warm")
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
fn a_qualified_query_resolves_to_the_method_in_the_named_scope() {
    // `Foo::Bar#baz` must find the `baz` defined inside `Foo::Bar` and, since a
    // scope match exists, suppress the same-named `baz` in another scope.
    let (dir, db) = scratch("qualified");
    fs::write(
        dir.join("a.rb"),
        "module Foo\n  class Bar\n    def baz; end\n  end\nend\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.rb"),
        "module Other\n  class Bar\n    def baz; end\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(&db, &dir, &["Foo::Bar#baz", "--no-record", "--ndjson"]);
    assert!(ok, "search failed: {out}");
    assert!(out.contains("a.rb"), "in-scope baz surfaces: {out}");
    assert!(
        !out.contains("b.rb"),
        "out-of-scope baz is gated out: {out}"
    );

    // fallback: no `baz` lives in `Nope::Bar`, so both still surface rather than
    // returning nothing — the scope was a hint, not a hard filter
    let (ok, out) = rq(&db, &dir, &["Nope::Bar#baz", "--no-record", "--ndjson"]);
    assert!(ok, "fallback search failed: {out}");
    assert!(
        out.contains("a.rb") && out.contains("b.rb"),
        "no scope match falls back to all candidates: {out}"
    );

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

    assert!(
        !warmed(&db, &dir, "widget"),
        "clean complete repo should not re-warm"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_tracked_edit_warms_but_a_new_untracked_file_does_not() {
    // the dirty check skips the untracked-file scan for speed: a tracked edit
    // still triggers a warm (so the change is picked up), but a brand-new
    // untracked file is the accepted tradeoff — not seen until committed/indexed
    let (dir, db) = scratch("dirty-check");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    fs::write(dir.join("a.rb"), "class Widget\n  def go; end\nend\n").unwrap();
    assert!(warmed(&db, &dir, "widget"), "tracked edit triggers a warm");

    // restore the tracked file to its committed content (tree clean again), then
    // add an untracked file — which the cheaper check intentionally ignores
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();
    assert!(
        !warmed(&db, &dir, "widget"),
        "a new untracked file does not trigger a warm (accepted tradeoff)"
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
fn open_launches_and_records_the_pick() {
    // `rq --open` picks the top hit, records it as a selection (so ranking
    // learns), and execs the launcher. RQ_OPEN drives a harmless command here.
    let (dir, db) = scratch("open");
    fs::write(dir.join("user.rb"), "class User\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // RQ_OPEN runs `true` — exits 0, no editor needed; non-TTY takes the top hit
    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["--open", "user"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .env("RQ_OPEN", "true")
        .output()
        .expect("run rq");
    assert!(run.status.success(), "open should exit 0 via the launcher");

    // with no launcher and no editor, --open prints the resolved path:line
    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["--open", "user", "--no-record"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .env("PATH", "/nonexistent") // hide any `code` on PATH
        .output()
        .expect("run rq");
    let printed = String::from_utf8_lossy(&run.stdout);
    assert!(
        printed.trim().ends_with("user.rb:1"),
        "prints resolved path:line: {printed}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_honors_json_output() {
    // --drop --json/-J report what was removed, so a script can act on it
    let (dir, db) = scratch("drop-json");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (ok, out) = rq(&db, &dir, &["--drop", "--json"]);
    assert!(ok, "drop --json failed: {out}");
    assert!(out.trim_start().starts_with('{'), "json object: {out}");
    assert!(out.contains("\"dropped\": true"), "reports dropped: {out}");

    // already gone → compact ndjson, dropped:false, still exit 0 (idempotent)
    let (ok, out) = rq(&db, &dir, &["--drop", "--ndjson"]);
    assert!(ok, "second drop should not error: {out}");
    let line = out.lines().next().unwrap_or("");
    assert!(
        line.starts_with('{') && line.contains("\"dropped\":false"),
        "ndjson dropped:false: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_removes_a_repos_index() {
    // --drop is the inverse of --index: the repo disappears from coverage, and
    // dropping again is idempotent (a clear message, no error)
    let (dir, db) = scratch("drop");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (_, before) = rq(&db, &dir, &["--status"]);
    assert!(before.contains("symbol"), "indexed before drop: {before}");

    let (ok, out) = rq(&db, &dir, &["--drop"]);
    assert!(ok, "drop failed: {out}");
    assert!(out.contains("dropped"), "drop confirms: {out}");

    let (ok2, after) = rq(&db, &dir, &["--status"]);
    assert!(ok2, "status after drop: {after}");
    assert!(
        !after.contains("symbol"),
        "coverage gone after drop: {after}"
    );

    // idempotent: nothing left to drop, but not an error
    let (ok3, again) = rq(&db, &dir, &["--drop"]);
    assert!(ok3, "second drop should not error: {again}");
    assert!(again.contains("not indexed"), "idempotent message: {again}");

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
fn status_and_index_honor_json_output() {
    let (dir, db) = scratch("ops-json");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();

    // --index --json: a single object with this-run and total counts
    let (ok, out) = rq(&db, &dir, &["--index", "--json"]);
    assert!(ok, "index --json failed: {out}");
    assert!(
        out.trim_start().starts_with('{'),
        "index json object: {out}"
    );
    assert!(out.contains("\"files_added\""), "index run counts: {out}");
    assert!(out.contains("\"repo\""), "index repo field: {out}");

    // --status --json: an array of coverage rows
    let (ok, out) = rq(&db, &dir, &["--status", "--json"]);
    assert!(ok, "status --json failed: {out}");
    assert!(
        out.trim_start().starts_with('['),
        "status json array: {out}"
    );
    assert!(
        out.contains("\"status\": \"complete\""),
        "status field: {out}"
    );

    // --status -J: one compact object per line
    let (ok, out) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert!(ok, "status -J failed: {out}");
    let line = out.lines().next().unwrap_or("");
    assert!(
        line.starts_with('{') && line.ends_with('}') && line.contains("\"repo\""),
        "ndjson object per line: {out}"
    );

    // with nothing indexed, --status --json is still well-formed (an empty array)
    rq(&db, &dir, &["--drop"]);
    let (ok, out) = rq(&db, &dir, &["--status", "--json"]);
    assert!(ok && out.trim() == "[]", "empty status json is []: {out}");

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
fn path_filter_accepts_absolute_and_relative_paths() {
    let (dir, db) = scratch("path-abs");
    fs::create_dir_all(dir.join("app/services")).unwrap();
    fs::create_dir_all(dir.join("app/models")).unwrap();
    fs::write(
        dir.join("app/services/widget.rb"),
        "class WidgetService\nend\n",
    )
    .unwrap();
    fs::write(dir.join("app/models/widget.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // absolute, ./-relative, and bare repo-relative all normalize to the same
    // filter and keep only the services hit.
    let abs = dir.join("app/services");
    let abs = abs.to_str().unwrap();
    for spec in [abs, "./app/services", "app/services"] {
        let (ok, out) = rq(&db, &dir, &["widget", "--path", spec, "--ndjson"]);
        assert!(ok, "path search failed for {spec:?}: {out}");
        assert!(
            out.contains("app/services/widget.rb"),
            "services kept for {spec:?}: {out}"
        );
        assert!(
            !out.contains("app/models/widget.rb"),
            "models filtered for {spec:?}: {out}"
        );
    }

    // a path outside the repo normalizes to nothing, not everything
    let (_, out) = rq(
        &db,
        &dir,
        &["widget", "--path", "/nonexistent/elsewhere", "--ndjson"],
    );
    assert!(out.trim().is_empty(), "outside path yields no hits: {out}");

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
fn a_cold_repo_blocks_to_an_answer_instead_of_a_false_miss() {
    // The core fix: a query against an unindexed repo would otherwise hit the
    // bounded budget and see a *false* "no matches" while the symbol sits
    // unindexed. With a tiny answer budget the old bounded path gives up first;
    // now the query blocks and keeps indexing until the answer appears. This is
    // the *programmatic* (--json, non-TTY) path — correctness for agents/scripts,
    // no progress UI.
    let (dir, db) = scratch("block-json");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);

    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["Widget", "--no-record", "--json"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .env("RQ_ANSWER_BUDGET_MS", "1") // bounded path would give up immediately
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "a programmatic query should block until it finds the symbol; \
         stdout={out:?} stderr={:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(out.contains("widget.rb"), "found in the right file: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_interactive_cold_repo_shows_progress_and_finds_the_answer() {
    // The human path: forcing interactive turns on the stderr heads-up + Ctrl-C
    // handling, but the blocking-until-answered behavior is the same as --json.
    let (dir, db) = scratch("block-tty");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);

    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["Widget", "--no-record"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .env("RQ_ANSWER_BUDGET_MS", "1")
        .env("RQ_ASSUME_INTERACTIVE", "1") // pretend a TTY
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(run.status.success(), "should find the symbol: {out:?}");
    assert!(out.contains("widget.rb"), "found in the right file: {out}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_incomplete_index_reports_an_indeterminate_miss_not_a_definitive_one() {
    // A miss while the index is still warming is "not yet", not "absent". Capping
    // the pass below the repo size keeps coverage "warming", so a query for a
    // symbol that isn't in the indexed slice must exit 2 (indeterminate) — letting
    // an agent retry rather than conclude the symbol doesn't exist.
    let (dir, db) = scratch("indeterminate");
    for i in 0..20 {
        fs::write(
            dir.join(format!("m{i:02}.rb")),
            format!("class Widget{i}\nend\n"),
        )
        .unwrap();
    }
    git_init_commit(&dir);

    let run = Command::new(env!("CARGO_BIN_EXE_rq"))
        .args(["Nonexistent", "--no-record", "--json"])
        .current_dir(&dir)
        .env("RQ_DB", &db)
        .env("RQ_COLLECT_CAP", "5") // one pass can't finish → stays "warming"
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert_eq!(out.trim(), "[]", "empty JSON array on a miss: {out:?}");
    assert_eq!(
        run.status.code(),
        Some(2),
        "an incomplete-index miss is indeterminate (exit 2), not definitive"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn symbols_outlines_a_file_in_line_order() {
    let (dir, db) = scratch("symbols");
    fs::write(
        dir.join("widget.rb"),
        "class Widget\n  def build\n  end\n  def render\n  end\nend\n",
    )
    .unwrap();
    fs::write(dir.join("other.rb"), "class Other\nend\n").unwrap();
    git_init_commit(&dir);

    // ndjson outline: the file's symbols, in line order, with kind/parent/signature.
    let (ok, out) = rq(&db, &dir, &["--symbols", "widget.rb", "--ndjson"]);
    assert!(ok, "symbols failed: {out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "class + two methods: {out}");
    assert!(
        first_line(&out).contains("\"name\":\"Widget\""),
        "class first: {out}"
    );
    assert!(out.contains("\"name\":\"build\""), "build present: {out}");
    assert!(
        out.contains("\"parent\":\"Widget\""),
        "method nests under class: {out}"
    );
    assert!(
        out.contains("\"signature\":\"def build\""),
        "signature read: {out}"
    );
    // scoped to the named file only
    assert!(!out.contains("Other"), "other file excluded: {out}");

    // --kind filters the outline to just methods (drops the class).
    let (ok, out) = rq(
        &db,
        &dir,
        &["--symbols", "widget.rb", "-k", "method", "--ndjson"],
    );
    assert!(ok, "filtered symbols failed: {out}");
    assert_eq!(out.lines().count(), 2, "two methods only: {out}");
    assert!(
        !out.contains("\"name\":\"Widget\""),
        "class filtered out: {out}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bare_invocation_prints_help() {
    let (dir, db) = scratch("help");
    let (ok, out) = rq(&db, &dir, &[]);
    assert!(ok, "bare rq should exit 0");
    assert!(
        out.contains("rq finds where a symbol is defined"),
        "help banner: {out}"
    );
    assert!(out.contains("Usage:"), "usage in help: {out}");

    let _ = fs::remove_dir_all(&dir);
}
