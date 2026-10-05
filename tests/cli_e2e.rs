//! End-to-end: drive the real `rq` binary through index → search.
//!
//! Hermetic and reproducible — no shell `cd`, no git required. Each run uses an
//! isolated `RQ_DB`, a fresh temp repo, and sets the subprocess working
//! directory, so the shell's cwd is irrelevant.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Scratch, git_cmd, rq, rq_both, rq_cmd};

/// A fresh repo dir for this test, removed on drop, and a db path beside it.
fn scratch(label: &str) -> (Scratch, PathBuf) {
    let dir = Scratch::new(&format!("e2e-{label}"));
    let db = dir.db();
    (dir, db)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Whether a search finds work for the warm child: a `-v` miss, which runs
/// the worktree check itself and spawns the child only when something moved
/// (a hit hands the check to the child unasked). The harness waits for the
/// child, so its reindex has landed when this returns.
fn warmed(db: &Path, cwd: &Path) -> bool {
    let run = rq_cmd(db, cwd)
        .args(["-v", "NoSuchSymbolAnywhere"])
        .output()
        .expect("run rq");
    String::from_utf8_lossy(&run.stderr).contains("background warm (detached)")
}

/// Run git in `dir`, failing the test if it fails: a missing or broken git
/// would otherwise surface as a puzzling assert much later.
fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `git init` a directory (no commits needed) so it reads as a git repo.
fn git_init(dir: &Path) {
    git(dir, &["init", "-q"]);
}

/// `git init` + commit everything, so files are *tracked* (warming enumerates a
/// committed repo from `git ls-files`, not a filesystem walk).
fn git_init_commit(dir: &Path) {
    git_init(dir);
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=test",
            "commit",
            "--allow-empty",
            "-qm",
            "init",
        ],
    );
}

#[test]
fn index_search_and_status_through_the_cli() {
    let (dir, db) = scratch("basic");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\nend\n").unwrap();

    // index the working directory
    let (code, out) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index failed: {out}");
    assert!(out.contains("symbol"), "index output: {out}");

    // search — the tie breaks alphabetically, so HandlerA leads
    let (code, out) = rq(&db, &dir, &["handler"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert!(
        first_line(&out).contains("HandlerA"),
        "search output: {out}"
    );

    // status shows the repo
    let (code, out) = rq(&db, &dir, &["--status"]);
    assert_eq!(code, 0, "status failed: {out}");
    assert!(out.contains("local:"), "status output: {out}");
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

    let (_, out) = rq(&db, &dir, &["employeescontroller", "--ndjson"]);
    assert!(out.contains("EmployeesController"), "exact kept: {out}");
    assert!(
        !out.contains("EmployeeStatusController"),
        "scattered fuzzy dropped: {out}"
    );
}

#[test]
fn a_wildcard_bridges_an_explicit_gap() {
    // `*` reaches across words: `widget*controller` finds
    // WidgetAlphaBravoController. An unrelated file stays out.
    let (dir, db) = scratch("wildcard");
    fs::write(
        dir.join("widget_alpha_bravo_controller.rb"),
        "class WidgetAlphaBravoController\nend\n",
    )
    .unwrap();
    fs::write(dir.join("gadget_service.rb"), "class GadgetService\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["widget*controller", "--ndjson"]);
    assert_eq!(code, 0, "wildcard search should match: {out}");
    assert!(
        out.contains("WidgetAlphaBravoController"),
        "star bridges the gap: {out}"
    );
    assert!(!out.contains("GadgetService"), "non-match excluded: {out}");

    // without the star, the fuzzy matcher skips the words too, at a price
    let (code, out) = rq(&db, &dir, &["widgetcontroller"]);
    assert!(
        code == 0 && out.contains("WidgetAlphaBravoController"),
        "{out}"
    );
}

#[test]
fn cold_index_builds_a_working_fuzzy_index() {
    // a cold `--index` suspends per-row index upkeep and builds in bulk at the
    // end; a mid-word substring (not a prefix of the name) only resolves
    // through fuzzy recall, so this proves the bulk pass produced a usable index
    let (dir, db) = scratch("coldindex");
    fs::write(dir.join("a.rb"), "class AlphaWidgetController\nend\n").unwrap();
    let (code, out) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index failed: {out}");

    // "widget" is mid-word in AlphaWidgetController — exact/prefix can't reach it
    let (code, out) = rq(&db, &dir, &["widget"]);
    assert_eq!(code, 0, "fuzzy recall should find it: {out}");
    assert!(out.contains("AlphaWidgetController"), "fuzzy recall: {out}");
}

#[test]
fn the_name_index_answers_fuzzy_queries_and_keeps_up_with_edits() {
    // fuzzy recall reads the name index: built at the end of a cold index,
    // then appended to as files are written
    let (dir, db) = scratch("nameindex");
    fs::write(dir.join("a.rb"), "class AlphaWidgetController\nend\n").unwrap();
    fs::write(dir.join("connection_pool.rb"), "module Base\nend\n").unwrap();
    let (code, out) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index failed: {out}");
    let scan = |query: &str| {
        let out = rq_cmd(&db, &dir)
            .args([query, "--json"])
            .output()
            .expect("run rq");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // an abbreviation, not a substring of the name
    assert!(scan("wdgctl").contains("AlphaWidgetController"));
    // a module named only by its file
    assert!(scan("conpool").contains("\"Base\""));

    fs::write(dir.join("b.rb"), "class BetaGadgetFactory\nend\n").unwrap();
    let (code, out) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "reindex failed: {out}");
    assert!(
        scan("gdgfac").contains("BetaGadgetFactory"),
        "an appended name"
    );
}

#[test]
fn a_search_that_warms_a_cold_repo_leaves_a_working_fuzzy_index() {
    // the same, when the first contact is a search rather than `--index`: the
    // budgeted warm it runs must leave the name index usable, both for this
    // answer and for the next query
    let (dir, db) = scratch("coldwarm");
    fs::write(dir.join("a.rb"), "class AlphaWidgetController\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class BetaGadget\nend\n").unwrap();
    git_init_commit(&dir);

    let (code, out) = rq(&db, &dir, &["widget"]);
    assert!(
        code == 0 && out.contains("AlphaWidgetController"),
        "warm: {out}"
    );
    let (code, out) = rq(&db, &dir, &["gadget"]);
    assert!(code == 0 && out.contains("BetaGadget"), "next query: {out}");
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

    let (code, out) = rq(&db, &dir, &["employeescontroller", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    // both files surface — the namespaced one isn't pruned
    assert!(out.contains("a.rb"), "namespaced class kept: {out}");
    assert!(out.contains("b.rb"), "top-level class kept: {out}");
}

#[test]
fn a_bare_class_name_answers_the_top_level_class_first() {
    // a nested one has the bigger body, which used to decide it
    let (dir, db) = scratch("top-level");
    for (path, src) in [
        (
            "app/models/account.rb",
            "class Account < ApplicationRecord\n  has_many :users\n  has_many :invoices\nend\n",
        ),
        (
            "app/services/billing/providers/account.rb",
            "module Billing\n  module Providers\n    class Account\n      def charge\n        1\n      end\n    end\n  end\nend\n",
        ),
        (
            "app/reports/types/account.rb",
            "module Reports\n  module Types\n    class Account\n    end\n  end\nend\n",
        ),
    ] {
        let file = dir.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, src).unwrap();
    }
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["Account", "--json"]);
    assert_eq!(code, 0, "search failed: {out}");
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(hits[0]["file"], "app/models/account.rb", "{out}");
    assert!(
        hits[0]["features"]
            .as_array()
            .unwrap()
            .contains(&"top_level".into())
    );
}

#[test]
fn foo_dot_new_finds_the_constructor() {
    // `Widget.new` runs `initialize` (Ruby) / `__init__` (Python) — the name
    // the user typed is not the name the definition carries.
    let (dir, db) = scratch("constructor");
    fs::write(
        dir.join("a.rb"),
        "class Widget\n  def initialize; end\n  def self.build; end\nend\nclass Other\n  def initialize; end\nend\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.py"),
        "class Gadget:\n    def __init__(self):\n        pass\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["Widget.new", "--ndjson"]);
    assert_eq!(code, 0, "Widget.new should resolve: {out}");
    assert!(out.contains("\"initialize\""), "finds initialize: {out}");
    assert!(out.contains("\"line\":2"), "the one in Widget: {out}");
    assert!(!out.contains("\"line\":6"), "not Other's: {out}");

    let (code, out) = rq(&db, &dir, &["Gadget.new", "--ndjson"]);
    assert!(code == 0 && out.contains("__init__"), "python too: {out}");

    // `.` is a scope separator generally, not only for `new`
    let (code, out) = rq(&db, &dir, &["Widget.build", "--ndjson"]);
    assert!(
        code == 0 && out.contains("\"build\""),
        "class method: {out}"
    );

    // a slip in the scope recovers on the typo retry, flagged as a guess
    let (code, out) = rq(&db, &dir, &["Widgit.new", "--ndjson"]);
    assert!(
        code == 0 && out.contains("\"line\":2"),
        "scope typo recovers: {out}"
    );
    assert!(!out.contains("\"confidence\":1.0"), "not certain: {out}");

    // no scope answers `Widget.Builder`, so `.` falls back to a one-char wildcard
    fs::write(dir.join("c.rb"), "class Widget2Builder\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    let (code, out) = rq(&db, &dir, &["Widget.Builder", "--ndjson"]);
    assert!(
        code == 0 && out.contains("Widget2Builder"),
        "wildcard fallback: {out}"
    );
}

#[test]
fn foo_dot_new_without_its_own_constructor_finds_the_class() {
    // Widget inherits `initialize`; rq doesn't track inheritance, so the class
    // is the answer — never a similarly named class's constructor.
    let (dir, db) = scratch("inherited-constructor");
    fs::write(
        dir.join("a.rb"),
        "class Base\n  def initialize; end\nend\nclass Widget < Base\n  def run; end\nend\nclass Widgey\n  def initialize; end\nend\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.py"),
        "class Gadget(Base):\n    def run(self):\n        pass\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["Widget.new", "--ndjson"]);
    assert_eq!(code, 0, "Widget.new should resolve: {out}");
    assert_eq!(out.lines().count(), 1, "just the class: {out}");
    assert!(
        out.contains("\"class\"") && out.contains("\"line\":4"),
        "Widget: {out}"
    );
    assert!(out.contains("constructor_owner"), "flagged: {out}");
    assert!(out.contains("\"confidence\":0.75"), "not certain: {out}");

    let (code, out) = rq(&db, &dir, &["Gadget.new", "--ndjson"]);
    assert!(code == 0 && out.contains("\"Gadget\""), "python too: {out}");
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

    let (code, out) = rq(&db, &dir, &["Foo::Bar#baz", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert!(out.contains("a.rb"), "in-scope baz surfaces: {out}");
    assert!(
        !out.contains("b.rb"),
        "out-of-scope baz is gated out: {out}"
    );

    // A scope nothing lives in is a miss, not a fallback to every candidate.
    // It used to fall back, which made a made-up owner indistinguishable from
    // the real one whenever the leaf name was unique.
    let (code, out) = rq(&db, &dir, &["Nope::Bar#baz", "--ndjson"]);
    assert_ne!(code, 0, "an unmatched scope must not succeed: {out}");
    assert!(out.contains("scope_not_found"), "and says why: {out}");
    // reported as "not there, but here" — the useful half of the answer
    assert!(out.contains("found_in"), "names where it does live: {out}");
}

#[test]
fn a_package_or_module_scope_is_found_in_the_path() {
    // Go packages and Python modules name a scope no parent records: the
    // directory does. Two `Widget`s, one per package.
    let (dir, db) = scratch("path-scope");
    for (path, src) in [
        ("shop/widget.go", "package shop\n\ntype Widget struct{}\n"),
        ("store/widget.go", "package store\n\ntype Widget struct{}\n"),
        ("pkg/db/models/query.py", "class Widget:\n    pass\n"),
    ] {
        let file = dir.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, src).unwrap();
    }
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["store.Widget", "--ndjson"]);
    assert_eq!(code, 0, "package scope resolves: {out}");
    assert!(first_line(&out).contains("store/widget.go"), "{out}");
    assert!(
        !out.contains("shop/"),
        "the other package is out of scope: {out}"
    );

    let (code, out) = rq(&db, &dir, &["pkg.db.models.Widget", "--ndjson"]);
    assert!(code == 0 && first_line(&out).contains("query.py"), "{out}");

    // a scope neither a parent nor a path holds is still a miss, and the
    // name's real home is reported only for that very name
    let (code, out) = rq(&db, &dir, &["orders.Widget", "--ndjson"]);
    assert!(code != 0 && out.contains("found_in"), "{out}");
    let (code, out) = rq(&db, &dir, &["orders.Widgt", "--ndjson"]);
    assert!(
        code != 0 && !out.contains("found_in"),
        "a fuzzy leaf is no home: {out}"
    );
}

#[test]
fn a_godoc_receiver_reads_as_its_type() {
    // `(*Widget).Build` is how godoc names a method; the `*` is not a glob
    let (dir, db) = scratch("godoc-receiver");
    fs::write(
        dir.join("widget.go"),
        "package shop\n\ntype Widget struct{}\n\nfunc (w *Widget) Build() {}\n\ntype Gadget struct{}\n\nfunc (g *Gadget) Build() {}\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["(*Widget).Build", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert!(
        first_line(&out).contains("\"parent\":\"Widget\""),
        "Widget's Build: {out}"
    );
    assert!(!out.contains("Gadget"), "not Gadget's: {out}");
}

#[test]
fn json_results_carry_the_definition_span() {
    // a result reports line..=end_line so a caller can read the exact span
    let (dir, db) = scratch("endline");
    fs::write(
        dir.join("a.rb"),
        "class Widget\n  def go\n    1\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["Widget", "-l", "1", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    // class Widget spans line 1 through its `end` on line 5
    assert!(out.contains("\"line\":1"), "start line present: {out}");
    assert!(out.contains("\"end_line\":5"), "end line present: {out}");
    // absent optionals are omitted, not null — same shape as --symbols
    assert!(
        !out.contains("\"parent\":null"),
        "no null fields in hit JSON: {out}"
    );
}

#[test]
fn show_prints_the_body_when_confident_and_lists_when_not() {
    let (dir, db) = scratch("show");
    fs::write(
        dir.join("a.rb"),
        "class Widget\n  def go\n    42\n  end\nend\n",
    )
    .unwrap();
    // two same-named classes so a bare fuzzy query is ambiguous
    fs::write(dir.join("b.rb"), "class Thing\nend\nclass Thang\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // confident (exact, dominant): --show prints the full source span as `body`
    let (code, out) = rq(&db, &dir, &["--show", "Widget", "-j"]);
    assert_eq!(code, 0, "show failed: {out}");
    assert!(
        out.contains("\"body\""),
        "confident show carries body: {out}"
    );
    assert!(
        out.contains("def go"),
        "body has the definition source: {out}"
    );
    assert!(!out.contains("\"score\""), "score dropped from JSON: {out}");
    assert!(out.contains("\"confidence\""), "confidence present: {out}");

    // ambiguous (two fuzzy `Th*` matches): no body, falls back to the list
    let (_ok, out) = rq(&db, &dir, &["--show", "Th", "-j"]);
    assert!(
        !out.contains("\"body\""),
        "ambiguous show prints no body: {out}"
    );
}

#[test]
fn a_leading_kind_keyword_filters_like_dash_k() {
    // `rq class Widget` and `rq method go` scope by kind without needing -k, and
    // without quoting. A same-named symbol of another kind is filtered out.
    let (dir, db) = scratch("kindkw");
    fs::write(
        dir.join("a.rb"),
        "SIZE = 5\nclass Widget\n  def go; end\nend\nmodule Widget\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    // unquoted keyword restricts to the class, dropping the module of the same name
    let (code, out) = rq(&db, &dir, &["class", "Widget", "--ndjson"]);
    assert_eq!(code, 0, "keyword search failed: {out}");
    assert!(out.contains("\"kind\":\"class\""), "class kept: {out}");
    assert!(
        !out.contains("\"kind\":\"module\""),
        "module filtered: {out}"
    );

    // the method keyword finds the def; equivalent to -k method
    let (code, out) = rq(&db, &dir, &["method", "go", "--ndjson"]);
    assert_eq!(code, 0, "method keyword failed: {out}");
    assert!(out.contains("\"kind\":\"method\""), "method found: {out}");

    // constants are indexed and reachable via the keyword form too
    let (code, out) = rq(&db, &dir, &["constant", "SIZE", "--ndjson"]);
    assert_eq!(code, 0, "constant keyword failed: {out}");
    assert!(
        out.contains("\"kind\":\"constant\""),
        "constant found: {out}"
    );
}

#[test]
fn a_search_does_not_leak_another_indexed_repo() {
    // two repos in one index; a query from inside repo A must never surface repo
    // B's definitions — the reported leak. `--all-repos` opts back into both.
    let (dir_a, db) = scratch("repo-a");
    let dir_b = Scratch::new("e2e-repo-b");
    fs::write(dir_a.join("a.rb"), "class Alpha\nend\n").unwrap();
    fs::write(dir_b.join("b.rb"), "class Gadget\nend\n").unwrap();
    rq(&db, &dir_a, &["--index"]);
    rq(&db, &dir_b, &["--index"]);

    // Gadget lives only in repo B; from repo A it's a definitive miss, not B's hit
    let (code, out) = rq(&db, &dir_a, &["Gadget", "--ndjson"]);
    assert_ne!(code, 0, "no Gadget in repo A — should miss");
    assert!(!out.contains("b.rb"), "must not leak repo B: {out}");
    assert!(
        out.contains("\"status\":\"no_match\""),
        "reports no_match: {out}"
    );

    // --all-repos opts into the cross-repo search and finds it
    let (code, out) = rq(&db, &dir_a, &["Gadget", "--all-repos", "--ndjson"]);
    assert_eq!(code, 0, "--all-repos should find Gadget in repo B: {out}");
    assert!(out.contains("b.rb"), "cross-repo hit surfaces: {out}");

    // -a is the short form of the same flag
    let (code, out) = rq(&db, &dir_a, &["Gadget", "-a", "--ndjson"]);
    assert_eq!(code, 0, "-a should find Gadget in repo B: {out}");
    assert!(out.contains("b.rb"), "cross-repo hit surfaces: {out}");
}

#[test]
fn a_repo_with_no_commits_is_a_repo_like_any_other() {
    // An unborn HEAD (`git init`, files not yet committed) must not read as
    // "no repo": its first query is scoped to it, not to every indexed repo,
    // and once indexed it settles like a committed repo instead of reindexing
    // (and calling every miss provisional) on each search.
    let (dir_a, db) = scratch("unborn-a");
    let dir_b = Scratch::new("e2e-unborn-b");
    fs::write(dir_b.join("b.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir_b);
    rq(&db, &dir_b, &["--index"]);
    fs::write(dir_a.join("a.rb"), "class Gadget\nend\n").unwrap();
    git_init(&dir_a);

    let (code, out) = rq(&db, &dir_a, &["Widget", "--json"]);
    assert_ne!(code, 0, "no Widget here — the first query must miss: {out}");
    assert!(!out.contains("b.rb"), "must not leak the other repo: {out}");

    let (code, out) = rq(&db, &dir_a, &["Gadget", "--json"]);
    assert_eq!(code, 0, "its own untracked file is indexed: {out}");
    let root = dir_a.canonicalize().unwrap();
    assert!(out.contains(&*root.to_string_lossy()), "{out}");
    assert!(
        !warmed(&db, &dir_a),
        "an indexed, unchanged unborn repo does not re-warm"
    );
}

#[test]
fn structured_results_name_the_checkout_root_their_file_is_relative_to() {
    // Each result carries its own `root`: `--all-repos` spans checkouts, so a
    // caller can't assume the cwd's repo is the one `file` is relative to.
    let (dir_a, db) = scratch("root-a");
    let dir_b = Scratch::new("e2e-root-b");
    fs::create_dir_all(dir_b.join("lib")).unwrap();
    fs::write(dir_a.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir_b.join("lib/b.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir_a);
    git_init_commit(&dir_b);
    rq(&db, &dir_a, &["--index"]);
    rq(&db, &dir_b, &["--index"]);
    let root_of = |d: &Path| d.canonicalize().unwrap().to_string_lossy().into_owned();
    let expect = |file: &str| {
        if file == "a.rb" {
            root_of(&dir_a)
        } else {
            root_of(&dir_b)
        }
    };

    let (code, out) = rq(&db, &dir_a, &["Widget", "--all-repos", "--json"]);
    assert_eq!(code, 0, "hit: {out}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("json array");
    assert_eq!(rows.len(), 2, "both repos: {out}");
    for row in &rows {
        let file = row["file"].as_str().unwrap();
        assert_eq!(row["root"].as_str(), Some(expect(file).as_str()), "{out}");
    }

    // ndjson carries the same field, from a subdirectory of the other repo too
    let (code, out) = rq(&db, &dir_b.join("lib"), &["Widget", "-a", "--ndjson"]);
    assert_eq!(code, 0, "hit: {out}");
    for line in out.lines() {
        let row: serde_json::Value = serde_json::from_str(line).expect("ndjson line");
        let file = row["file"].as_str().unwrap();
        assert_eq!(row["root"].as_str(), Some(expect(file).as_str()), "{out}");
    }

    // an outline names its root as well
    let (code, out) = rq(&db, &dir_b.join("lib"), &["--symbols", "b.rb", "--ndjson"]);
    assert_eq!(code, 0, "outline: {out}");
    let row: serde_json::Value = serde_json::from_str(first_line(&out)).expect("ndjson");
    assert_eq!(row["file"], "lib/b.rb");
    assert_eq!(row["root"].as_str(), Some(root_of(&dir_b).as_str()));
}

#[test]
fn another_repos_exact_match_does_not_hide_a_fuzzy_one_here() {
    // Repo B defines `wdgt` exactly; repo A only has `Widget`, an abbreviation
    // match. Searching from A must still find Widget — B's exact hit is out of
    // scope, so it can't be what lets recall skip the fuzzy layers.
    let (dir_a, db) = scratch("fuzzy-a");
    let dir_b = Scratch::new("e2e-fuzzy-b");
    fs::write(dir_a.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir_b.join("b.rb"), "def wdgt\nend\n").unwrap();
    rq(&db, &dir_a, &["--index"]);
    rq(&db, &dir_b, &["--index"]);

    let (code, out) = rq(&db, &dir_a, &["wdgt", "--ndjson"]);
    assert_eq!(code, 0, "Widget in repo A should answer `wdgt`: {out}");
    assert!(out.contains("a.rb"), "finds this repo's Widget: {out}");
    assert!(!out.contains("b.rb"), "must not leak repo B: {out}");
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

    assert!(!warmed(&db, &dir), "clean complete repo should not re-warm");
}

#[test]
fn a_tracked_edit_warms_but_a_new_untracked_file_does_not() {
    // the dirty check skips the untracked-file scan for speed: a tracked edit
    // still triggers a warm (so the change is picked up), but a brand-new
    // untracked file is the accepted tradeoff — not seen until committed/indexed
    let (dir, db) = scratch("dirty-check");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    fs::write(dir.join("c.rb"), "class Gizmo\n  def go; end\nend\n").unwrap();
    assert!(warmed(&db, &dir), "tracked edit triggers a warm");

    // restore the tracked file to its committed content (tree clean again) —
    // itself a change the index has to take in — then add an untracked file,
    // which the cheaper check intentionally ignores
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    assert!(warmed(&db, &dir), "the restore is reindexed");
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();
    assert!(
        !warmed(&db, &dir),
        "a new untracked file does not trigger a warm (accepted tradeoff)"
    );
}

#[test]
fn two_clones_of_one_repo_each_read_their_own_files() {
    // Two checkouts share one identity (same remote), so one set of rows. A
    // search from either must revalidate and read source from *that* checkout
    // — not whichever was recorded first or last.
    let (dir_a, db) = scratch("clone-a");
    let dir_b = Scratch::new("e2e-clone-b");
    for (dir, tag) in [(&dir_a, "a"), (&dir_b, "b")] {
        fs::write(dir.join("w.rb"), format!("class Widget # {tag}\nend\n")).unwrap();
        git_init_commit(dir);
        let _ = git_cmd(dir)
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ])
            .output();
    }
    rq(&db, &dir_a, &["--index"]);
    rq(&db, &dir_b, &["--index"]);

    for (dir, tag) in [(&dir_a, "a"), (&dir_b, "b"), (&dir_a, "a")] {
        let (code, out) = rq(&db, dir, &["Widget", "--ndjson"]);
        assert_eq!(code, 0, "hit: {out}");
        assert!(
            out.contains(&format!("\"signature\":\"class Widget # {tag}\"")),
            "clone {tag} reads its own source: {out}"
        );
    }
}

#[test]
fn a_hit_leaves_the_worktree_check_to_the_warm_child() {
    // A hit on a complete repo must not wait on `git status`: it hands the
    // check to `rq --warm`, which bows out when nothing moved and sweeps when
    // something did.
    let (dir, db) = scratch("warm-child");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    let warm = |dir: &Path| {
        let out = rq_cmd(&db, dir)
            .env_remove("RQ_WARM_DETACH")
            .args(["-v", "--warm"])
            .output()
            .expect("run rq --warm");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let idle = warm(&dir);
    assert!(idle.contains("nothing to do"), "clean: {idle}");
    assert!(
        !idle.contains("(budget"),
        "clean repo must not sweep: {idle}"
    );

    fs::write(dir.join("c.rb"), "class Gizmo\n  def go; end\nend\n").unwrap();
    let busy = warm(&dir);
    assert!(busy.contains("(budget"), "an edit is swept: {busy}");

    // last: the child this hit spawns holds the single-flight lock a while
    let hit = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["Widget", "--profile", "--json"])
        .output()
        .expect("run rq");
    let profile = String::from_utf8_lossy(&hit.stderr);
    assert!(hit.status.success(), "hit: {profile}");
    assert!(
        !profile.contains("worktree changed?") && !profile.contains("staleness"),
        "a hit must not run the check itself: {profile}"
    );
}

/// Run a `-v` hit with detach on (and optional extra env), and report whether
/// it spawned the detached warm child — waiting for the child, so its git
/// calls are done before the test touches the repo again.
fn spawned_warm(db: &Path, cwd: &Path, env: &[(&str, &str)]) -> bool {
    let out = rq_cmd(db, cwd)
        .args(["-v", "Widget"])
        .env("RQ_WARM_DETACH", "wait")
        .envs(env.iter().copied())
        .output()
        .expect("run rq");
    assert!(out.status.success(), "a hit");
    String::from_utf8_lossy(&out.stderr).contains("background warm (detached)")
}

/// Run `rq --warm` to completion, as the detached child would.
fn warm_now(db: &Path, cwd: &Path) {
    rq_cmd(db, cwd)
        .env_remove("RQ_WARM_DETACH")
        .arg("--warm")
        .output()
        .expect("run rq --warm");
}

#[test]
fn a_hit_skips_the_warm_spawn_once_a_warm_found_nothing_moved() {
    let (dir, db) = scratch("warm-verified");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    warm_now(&db, &dir);
    assert!(!spawned_warm(&db, &dir, &[]), "verified clean: no spawn");
    assert!(!spawned_warm(&db, &dir, &[]), "and again, back to back");

    // last: the child this hit spawns may outlive the test
    assert!(
        spawned_warm(&db, &dir, &[("RQ_WARM_RECHECK_MS", "0")]),
        "an expired verdict spawns"
    );
}

#[test]
fn outside_git_a_miss_that_found_nothing_moved_spares_hits_the_spawn() {
    // every check outside git walks the tree; a fresh verdict is reused
    let (dir, db) = scratch("warm-verified-nongit");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    let (code, _) = rq(&db, &dir, &["Nosuch"]);
    assert_eq!(code, 1);
    assert!(
        !spawned_warm(&db, &dir, &[]),
        "verified by the miss: no spawn"
    );
    assert!(
        spawned_warm(&db, &dir, &[("RQ_WARM_RECHECK_MS", "0")]),
        "an expired verdict spawns"
    );
}

#[test]
fn staging_or_committing_voids_the_verdict_but_a_bare_edit_waits() {
    let (dir, db) = scratch("warm-moved");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    warm_now(&db, &dir);

    // An unstaged edit touches nothing in .git, so only the window catches it.
    fs::write(dir.join("c.rb"), "class Gizmo\n  def go; end\nend\n").unwrap();
    assert!(
        !spawned_warm(&db, &dir, &[]),
        "a bare edit waits out the recheck window"
    );

    let _ = git_cmd(&dir).args(["add", "c.rb"]).output();
    assert!(spawned_warm(&db, &dir, &[]), "staging spawns");

    git_init_commit(&dir);
    assert!(spawned_warm(&db, &dir, &[]), "a commit spawns");
}

#[test]
fn a_dirty_tree_whose_edits_are_indexed_reads_as_unchanged() {
    // Dirty is not the same as stale: once the edit has been indexed, a search
    // must neither re-warm on every query nor call a miss "still warming".
    let (dir, db) = scratch("dirty-indexed");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    fs::write(dir.join("README.md"), "docs\n").unwrap();
    git_init_commit(&dir);
    fs::write(dir.join("c.rb"), "class Gizmo\n  def go; end\nend\n").unwrap();
    fs::write(dir.join("README.md"), "edited docs\n").unwrap();
    rq(&db, &dir, &["--index"]);

    assert!(
        !warmed(&db, &dir),
        "an indexed edit (and a non-source one) must not re-warm"
    );
    let miss = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["Nonexistent"])
        .output()
        .expect("run rq");
    assert_eq!(
        miss.status.code(),
        Some(1),
        "a definitive miss, not warming"
    );

    // a further edit is still seen
    fs::write(dir.join("c.rb"), "class Gizmo\n  def stop; end\nend\n").unwrap();
    assert!(warmed(&db, &dir), "a new edit still warms");
}

fn git_checkout_file(dir: &Path, file: &str) {
    let _ = git_cmd(dir).args(["checkout", "--", file]).output();
}

/// Symbols the index holds for the repo, per `--status`.
fn indexed_symbols(db: &Path, dir: &Path) -> i64 {
    let (_, out) = rq(db, dir, &["--status", "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&out).expect("status json");
    rows[0]["symbols"].as_i64().expect("symbols")
}

#[test]
fn a_discarded_edit_is_reindexed() {
    // `git checkout -- f` leaves the tree clean, so `git status` no longer
    // names f — yet the index still holds the edit. The staleness check has to
    // remember which files it took in as edits, or the edit's symbols linger
    // until something else happens to reindex f.
    let (dir, db) = scratch("discarded-edit");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    let clean = indexed_symbols(&db, &dir);

    fs::write(dir.join("c.rb"), "class Gizmo\n  def spin; end\nend\n").unwrap();
    // the query that notices the edit says "retry" and leaves the reindex to
    // the warm child; the retry answers from it
    let (code, out) = rq(&db, &dir, &["spin", "--ndjson"]);
    assert_eq!(code, 2, "the edit is pending: {out}");
    let (code, out) = rq(&db, &dir, &["spin", "--ndjson"]);
    assert_eq!(code, 0, "and found on the retry: {out}");
    assert_eq!(indexed_symbols(&db, &dir), clean + 1);

    git_checkout_file(&dir, "c.rb");
    assert!(warmed(&db, &dir), "the discard is seen as a change");
    assert_eq!(
        indexed_symbols(&db, &dir),
        clean,
        "the edit's symbol is gone"
    );
    let (code, out) = rq(&db, &dir, &["spin", "--ndjson"]);
    assert_ne!(code, 0, "no longer found: {out}");
}

#[test]
fn a_tracked_file_deleted_without_git_rm_is_forgotten() {
    let (dir, db) = scratch("unstaged-delete");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("doomed.rb"), "class Doomed\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    fs::remove_file(dir.join("doomed.rb")).unwrap();

    let prod = [("RQ_WARM_DETACH", "wait")];
    let (code, out, _) = rq_full(&db, &dir, &["Nosuch", "--json"], &prod, None);
    assert_eq!(code, 2, "the delete is pending work: {out}");
    // the child forgot it: a miss is a miss again, and the file no hit
    for query in ["Nosuch", "Doomed"] {
        let (code, out, _) = rq_full(&db, &dir, &[query, "--json"], &prod, None);
        assert_eq!(code, 1, "{query}: {out}");
    }
}

#[test]
fn a_file_a_branch_switch_deleted_is_not_a_hit() {
    let (dir, db) = scratch("switched-away");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    let git = |args: &[&str]| {
        let out = git_cmd(&dir)
            .args(["-c", "user.email=t@e.st", "-c", "user.name=test"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    let trunk = String::from_utf8(
        git_cmd(&dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    git(&["checkout", "-qb", "feature"]);
    fs::write(
        dir.join("b.rb"),
        "class Branchy\n  def branch_only; end\nend\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "feature"]);
    rq(&db, &dir, &["--index"]);
    git(&["checkout", "-q", trunk.trim()]);

    let (code, out) = rq(&db, &dir, &["branch_only", "--ndjson"]);
    assert_ne!(code, 0, "b.rb is gone with the branch: {out}");
}

#[test]
fn a_discarded_deletion_is_found_again() {
    // The costly direction: an edit removed a method, the index took that in,
    // and discarding it brings the method back. Until reindexed, a search for
    // it is a confident "no match" (exit 1) for a symbol that is right there.
    let (dir, db) = scratch("discarded-deletion");
    fs::write(dir.join("c.rb"), "class Gizmo\n  def spin; end\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    fs::write(dir.join("c.rb"), "class Gizmo\nend\n").unwrap();
    rq(&db, &dir, &["spin"]); // revalidates c.rb: the method is gone
    let (code, _) = rq(&db, &dir, &["spin"]);
    assert_ne!(code, 0, "the deletion is indexed");

    git_checkout_file(&dir, "c.rb");
    rq(&db, &dir, &["spin"]); // notices the discard and reindexes
    let (code, out) = rq(&db, &dir, &["spin", "--ndjson"]);
    assert_eq!(code, 0, "the restored method is found: {out}");
}

/// Three `save`s; unanchored, the tie falls to path order and Gadget's leads.
fn three_saves(label: &str) -> (Scratch, PathBuf) {
    let (dir, db) = scratch(label);
    fs::create_dir_all(dir.join("app/models")).unwrap();
    fs::create_dir_all(dir.join("lib")).unwrap();
    let class = |name: &str, extra: &str| {
        format!("class {name}\n  def save\n  end\n  def persist\n    save{extra}\n  end\nend\n")
    };
    fs::write(dir.join("app/models/widget.rb"), class("Widget", "")).unwrap();
    fs::write(dir.join("app/models/gadget.rb"), class("Gadget", "")).unwrap();
    fs::write(dir.join("lib/aaa.rb"), class("Aaa", "")).unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    (dir, db)
}

fn top_file(ndjson: &str) -> String {
    let row: serde_json::Value = serde_json::from_str(first_line(ndjson)).expect("ndjson");
    row["file"].as_str().unwrap_or_default().to_string()
}

#[test]
fn an_anchor_ranks_the_enclosing_class_first() {
    let (dir, db) = three_saves("anchor");
    let (_, plain) = rq(&db, &dir, &["save", "-k", "method", "--ndjson"]);
    assert_eq!(
        top_file(&plain),
        "app/models/gadget.rb",
        "baseline: {plain}"
    );
    assert!(!plain.contains("enclosing") && !plain.contains("proximity"));

    // line 5 is the `save` call inside Widget#persist
    let at = "app/models/widget.rb:5";
    let (code, out) = rq(
        &db,
        &dir,
        &["save", "-k", "method", "--anchor", at, "--ndjson"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(top_file(&out), "app/models/widget.rb", "{out}");
    let row: serde_json::Value = serde_json::from_str(first_line(&out)).unwrap();
    let features: Vec<&str> = row["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str())
        .collect();
    assert!(features.contains(&"enclosing") && features.contains(&"proximity"));

    // text + --explain name both features; a column is accepted
    let (_, text) = rq(
        &db,
        &dir,
        &[
            "save",
            "-k",
            "method",
            "--anchor",
            "app/models/widget.rb:5:7",
            "--explain",
        ],
    );
    assert!(
        first_line(&text).starts_with("app/models/widget.rb"),
        "{text}"
    );
    assert!(
        text.contains("enclosing 60") && text.contains("proximity 90"),
        "{text}"
    );

    // FILE resolves against the cwd, here a subdirectory
    let (_, sub) = rq(
        &db,
        &dir.join("app"),
        &[
            "save",
            "-k",
            "method",
            "--anchor",
            "models/widget.rb:5",
            "--json",
        ],
    );
    let rows: Vec<serde_json::Value> = serde_json::from_str(&sub).expect("json");
    assert_eq!(rows[0]["file"], "app/models/widget.rb", "{sub}");

    // every line of a batch is asked from the same place
    let mut child = rq_cmd(&db, &dir)
        .args(["-J", "-k", "method", "--anchor", at])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn rq");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"save\npersist\n").unwrap();
    }
    let batch = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let first_for = |q: &str| {
        batch
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|r| r["query"] == q)
            .map(|r| r["file"].as_str().unwrap_or_default().to_string())
    };
    assert_eq!(
        first_for("save").as_deref(),
        Some("app/models/widget.rb"),
        "{batch}"
    );
    assert_eq!(
        first_for("persist").as_deref(),
        Some("app/models/widget.rb"),
        "{batch}"
    );
}

#[test]
fn an_anchor_in_a_test_file_waives_that_files_test_penalty() {
    let (dir, db) = scratch("anchor-test-file");
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::create_dir_all(dir.join("tests/helpers")).unwrap();
    fs::write(
        dir.join("lib/api.ts"),
        "export class Api {\n  request() {\n    return 1;\n  }\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("tests/helpers/api.ts"),
        "export class FakeApi {\n  request() {\n    return 2;\n  }\n}\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (_, plain) = rq(&db, &dir, &["request", "--ndjson"]);
    assert_eq!(top_file(&plain), "lib/api.ts", "baseline: {plain}");
    // asked from inside the test helper, its own method is the context
    let (code, out) = rq(
        &db,
        &dir,
        &["request", "--anchor", "tests/helpers/api.ts:3", "--ndjson"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(top_file(&out), "tests/helpers/api.ts", "{out}");
    assert!(!first_line(&out).contains("test_path"), "{out}");
}

#[test]
fn an_anchor_file_the_index_has_not_seen_is_read_live() {
    // A file created after indexing, reopening Widget: nothing about it is in
    // the index, so only a live read knows line 3 sits inside Widget.
    let (dir, db) = three_saves("anchor-live");
    fs::write(
        dir.join("app/models/widget_ext.rb"),
        "class Widget\n  def again\n    save\n  end\nend\n",
    )
    .unwrap();
    let at = "app/models/widget_ext.rb:3";
    let (code, out) = rq(
        &db,
        &dir,
        &["save", "-k", "method", "--anchor", at, "--ndjson"],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(top_file(&out), "app/models/widget.rb", "{out}");
}

#[test]
fn two_modes_at_once_are_a_usage_error() {
    let (dir, db) = scratch("two-modes");
    let code = |args: &[&str]| {
        rq_cmd(&db, &dir)
            .args(args)
            .output()
            .expect("run rq")
            .status
            .code()
    };
    for args in [
        &["--usage", "--drop"][..],
        &["--symbols", "a.rb", "--usage"],
        &["--warm", "--usage"],
        &["--status", "--index"],
        &["--completions", "bash", "--status"],
        &["--usage", "--show", "Widget"],
    ] {
        assert_eq!(code(args), Some(64), "{args:?}");
    }
    // a mode still takes the flags that shape it
    for args in [
        &["--status", "--json"][..],
        &["--usage", "--ndjson"],
        &["--index", ".", "--path", "lib", "--json"],
        &["--drop", "--json"],
    ] {
        assert_ne!(code(args), Some(64), "{args:?}");
    }
}

#[test]
fn an_anchor_is_rejected_where_it_means_nothing() {
    let (dir, db) = scratch("anchor-bad");
    for args in [
        &["--status", "--anchor", "a.rb:1"][..],
        &["--symbols", "a.rb", "--anchor", "a.rb:1"],
        &["save", "--anchor", "a.rb"],
        &["save", "--anchor", "a.rb:0"],
    ] {
        let (code, out, err) = rq_both(&db, &dir, args);
        assert_ne!(code, 0, "{args:?} should fail: {out}");
        assert!(err.contains("--anchor"), "{args:?}: {err}");
    }
}

#[test]
fn indexing_a_subdir_scopes_to_it_but_keeps_root_relative_paths() {
    // `rq --index <subdir>` seeds only that subtree in this run, yet stores
    // paths relative to the repo root so a later search still resolves them
    let (dir, db) = scratch("index-subdir");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::create_dir_all(dir.join("other")).unwrap();
    fs::write(dir.join("sub/a.rb"), "class InScope\nend\n").unwrap();
    fs::write(dir.join("other/b.rb"), "class OutOfScope\nend\n").unwrap();
    git_init_commit(&dir);

    let (code, out) = rq(&db, &dir, &["--index", "sub"]);
    assert_eq!(code, 0, "scoped index failed: {out}");
    assert!(out.contains("subtree"), "subdir index is a seed: {out}");
    assert!(
        out.contains("1 files"),
        "this run indexed exactly the one in-scope file: {out}"
    );

    // the seeded class is found, at a repo-root-relative path
    let (code, out) = rq(&db, &dir, &["inscope", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert!(out.contains("InScope"), "in-scope class indexed: {out}");
    assert!(
        out.contains("\"file\":\"sub/a.rb\""),
        "path is root-relative, not subdir-relative: {out}"
    );

    // the seed is not a fence: searching warms the rest of the repo, so the
    // out-of-scope class is found — and persisted
    let (code, out) = rq(&db, &dir, &["outofscope", "--ndjson"]);
    assert_eq!(code, 0, "warming finds the out-of-scope class: {out}");
    assert!(out.contains("\"file\":\"other/b.rb\""), "warm hit: {out}");
    let (_, status) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert!(
        status.contains("\"files\":2"),
        "warming persisted the rest of the repo: {status}"
    );
}

#[test]
fn empty_status_points_at_the_real_index_flag() {
    // the hint must name the actual flag (`rq --index`), not a non-existent
    // `rq index` subcommand
    let (dir, db) = scratch("empty-status");
    let (code, out) = rq(&db, &dir, &["--status"]);
    assert_eq!(code, 0, "status on an empty db should succeed: {out}");
    assert!(out.contains("rq --index"), "hint names the flag: {out}");
}

#[test]
fn open_launches_the_top_hit() {
    // `rq --open` picks the top hit and execs the launcher. RQ_OPEN drives a
    // harmless command here.
    let (dir, db) = scratch("open");
    fs::write(dir.join("user.rb"), "class User\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // RQ_OPEN runs `true` — exits 0, no editor needed; non-TTY takes the top hit
    let run = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["--open", "user"])
        .env("RQ_OPEN", "true")
        .output()
        .expect("run rq");
    assert!(run.status.success(), "open should exit 0 via the launcher");

    // a launcher with no placeholder is handed path:line as its last argument;
    // one with a placeholder gets exactly what it asked for
    let launched = |template: &str| {
        let run = rq_cmd(&db, &dir)
            .env_remove("RQ_WARM_DETACH")
            .args(["--open", "user"])
            .env("RQ_OPEN", template)
            .output()
            .expect("run rq");
        String::from_utf8_lossy(&run.stdout).trim().to_string()
    };
    let bare = launched("echo --wait");
    assert!(
        bare.starts_with("--wait ") && bare.ends_with("user.rb:1"),
        "{bare}"
    );
    assert_eq!(launched("echo line={line}"), "line=1");

    // with no launcher and no editor, --open prints the resolved path:line
    let run = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["--open", "user"])
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
}

#[test]
fn open_resolves_a_hit_against_its_own_checkout() {
    // under --all-repos the top hit can live in another repo; its file is
    // relative to that checkout, not the one rq was run from
    let (dir_a, db) = scratch("open-a");
    let dir_b = Scratch::new("e2e-open-b");
    fs::write(dir_a.join("a.rb"), "class Alpha\nend\n").unwrap();
    fs::write(dir_b.join("b.rb"), "class Gadget\nend\n").unwrap();
    rq(&db, &dir_a, &["--index"]);
    rq(&db, &dir_b, &["--index"]);

    let run = rq_cmd(&db, &dir_a)
        .env_remove("RQ_WARM_DETACH")
        .args(["-a", "--open", "Gadget"])
        .env("RQ_OPEN", "echo {file}")
        .output()
        .expect("run rq");
    let opened = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let expected = dir_b.canonicalize().unwrap().join("b.rb");
    assert_eq!(opened, expected.to_string_lossy());
}

#[test]
fn web_links_the_newest_pushed_commit() {
    let (dir, db) = scratch("web");
    fs::write(dir.join("user.rb"), "\nclass User\nend\n").unwrap();
    git_init_commit(&dir);
    let git = |args: &[&str]| {
        let out = git_cmd(&dir).args(args).output().expect("run git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    // BROWSER=echo stands in for the browser, so the URL lands on stdout
    let web = || {
        let run = rq_cmd(&db, &dir)
            .args(["-w", "user"])
            .env("BROWSER", "echo")
            .output()
            .expect("run rq");
        (
            run.status.success(),
            String::from_utf8_lossy(&run.stdout).trim().to_string(),
        )
    };

    // no remote → nothing to link to, but say what was picked and how else to
    // reach it
    rq(&db, &dir, &["--index"]);
    assert!(!web().0, "a local-only repo has no URL");
    let (_, _, err) = rq_both(&db, &dir, &["-w", "user"]);
    assert!(err.contains("user.rb:2"), "names the match: {err}");
    assert!(
        err.contains("-o") && err.contains("git remote add"),
        "{err}"
    );

    git(&["remote", "add", "origin", "git@github.com:org/app.git"]);
    rq(&db, &dir, &["--drop"]);
    rq(&db, &dir, &["--index"]);

    // nothing pushed yet: fall back to the host's default branch
    let (ok, url) = web();
    assert!(ok, "web should exit 0 via the launcher");
    assert_eq!(url, "https://github.com/org/app/blob/HEAD/user.rb#L2");

    // pushed, then committed past: link the pushed sha, not the unpushed HEAD
    let pushed = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&[
        "-c",
        "user.email=t@e.st",
        "-c",
        "user.name=test",
        "commit",
        "-qm",
        "local",
        "--allow-empty",
    ]);
    assert_eq!(
        web().1,
        format!("https://github.com/org/app/blob/{pushed}/user.rb#L2")
    );
}

#[test]
fn drop_honors_json_output() {
    // --drop --json/-J report what was removed, so a script can act on it
    let (dir, db) = scratch("drop-json");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["--drop", "--json"]);
    assert_eq!(code, 0, "drop --json failed: {out}");
    assert!(out.trim_start().starts_with('{'), "json object: {out}");
    assert!(out.contains("\"dropped\": true"), "reports dropped: {out}");

    // already gone → compact ndjson, dropped:false, still exit 0 (idempotent)
    let (code, out) = rq(&db, &dir, &["--drop", "--ndjson"]);
    assert_eq!(code, 0, "second drop should not error: {out}");
    let line = out.lines().next().unwrap_or("");
    assert!(
        line.starts_with('{') && line.contains("\"dropped\":false"),
        "ndjson dropped:false: {out}"
    );
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

    let (code, out) = rq(&db, &dir, &["--drop"]);
    assert_eq!(code, 0, "drop failed: {out}");
    assert!(out.contains("dropped"), "drop confirms: {out}");

    let (code, after) = rq(&db, &dir, &["--status"]);
    assert_eq!(code, 0, "status after drop: {after}");
    assert!(
        !after.contains("symbol"),
        "coverage gone after drop: {after}"
    );

    // idempotent: nothing left to drop, but not an error
    let (code, again) = rq(&db, &dir, &["--drop"]);
    assert_eq!(code, 0, "second drop should not error: {again}");
    assert!(again.contains("not indexed"), "idempotent message: {again}");
}

#[test]
fn status_reads_a_first_index_in_progress_as_warming() {
    // A first pass commits files long before it writes coverage; until then
    // the repo is partially indexed, not "never" indexed.
    let (dir, db) = scratch("status-first-pass");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM coverage", [])
        .unwrap();

    let (code, out) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert_eq!(code, 0, "status failed: {out}");
    assert!(out.contains("\"status\":\"warming\""), "mid-pass: {out}");
    assert!(out.contains("\"symbols\":1"), "keeps its totals: {out}");
}

#[test]
fn record_is_a_searchable_word_not_a_subcommand() {
    let (dir, db) = scratch("disambig");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // `rq record` searches for the symbol "record" (no hook, no match here)
    let (code, out) = rq(&db, &dir, &["record"]);
    assert_ne!(code, 0, "no-match search should exit non-zero");
    assert!(out.is_empty(), "expected no result lines, got: {out}");
}

#[test]
fn status_and_index_honor_json_output() {
    let (dir, db) = scratch("ops-json");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();

    // --index --json: a single object with this-run and total counts
    let (code, out) = rq(&db, &dir, &["--index", "--json"]);
    assert_eq!(code, 0, "index --json failed: {out}");
    assert!(
        out.trim_start().starts_with('{'),
        "index json object: {out}"
    );
    assert!(out.contains("\"files_added\""), "index run counts: {out}");
    assert!(out.contains("\"repo\""), "index repo field: {out}");

    // --status --json: an array of coverage rows
    let (code, out) = rq(&db, &dir, &["--status", "--json"]);
    assert_eq!(code, 0, "status --json failed: {out}");
    assert!(
        out.trim_start().starts_with('['),
        "status json array: {out}"
    );
    assert!(
        out.contains("\"status\": \"complete\""),
        "status field: {out}"
    );

    // --status -J: one compact object per line
    let (code, out) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert_eq!(code, 0, "status -J failed: {out}");
    let line = out.lines().next().unwrap_or("");
    assert!(
        line.starts_with('{') && line.ends_with('}') && line.contains("\"repo\""),
        "ndjson object per line: {out}"
    );

    // with nothing indexed, --status --json is still well-formed (an empty array)
    rq(&db, &dir, &["--drop"]);
    let (code, out) = rq(&db, &dir, &["--status", "--json"]);
    assert!(
        code == 0 && out.trim() == "[]",
        "empty status json is []: {out}"
    );
}

#[test]
fn json_and_ndjson_output() {
    let (dir, db) = scratch("json");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // --json: a pretty array with named fields
    let (code, out) = rq(&db, &dir, &["handler", "--json"]);
    assert_eq!(code, 0, "json search failed: {out}");
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
    let (code, out) = rq(&db, &dir, &["handler", "--ndjson"]);
    assert_eq!(code, 0, "ndjson search failed: {out}");
    let first = out.lines().next().unwrap_or("");
    assert!(
        first.starts_with('{') && first.ends_with('}'),
        "object per line: {out}"
    );
    assert!(
        first.contains("\"name\":\"HandlerA\""),
        "compact name: {out}"
    );
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
    let (code, out) = rq(&db, &dir, &["widget", "--path", "app/services", "--ndjson"]);
    assert_eq!(code, 0, "path search failed: {out}");
    assert!(
        out.contains("app/services/widget.rb"),
        "services hit kept: {out}"
    );
    assert!(
        !out.contains("app/models/widget.rb"),
        "models hit filtered out: {out}"
    );
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
        let (code, out) = rq(&db, &dir, &["widget", "--path", spec, "--ndjson"]);
        assert_eq!(code, 0, "path search failed for {spec:?}: {out}");
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
    assert!(
        !out.contains("\"file\":"),
        "outside path yields no hits: {out}"
    );
}

#[test]
fn limit_caps_the_number_of_results() {
    let (dir, db) = scratch("limit");
    // more definitions than the default cap, so `--limit 0` is distinguishable
    // from the default rather than just fitting under it
    let body: String = (0..12)
        .map(|i| format!("class Handler{i}\nend\n"))
        .collect();
    fs::write(dir.join("a.rb"), body).unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["handler", "--limit", "1", "--ndjson"]);
    assert_eq!(code, 0, "limited search failed: {out}");
    assert_eq!(out.lines().count(), 1, "expected exactly one result: {out}");

    let (code, out) = rq(&db, &dir, &["handler", "--ndjson"]);
    assert_eq!(code, 0, "default search failed: {out}");
    assert_eq!(out.lines().count(), 10, "default caps at 10: {out}");

    // 0 lifts the cap rather than asking for nothing
    let (code, out) = rq(&db, &dir, &["handler", "--limit", "0", "--ndjson"]);
    assert_eq!(code, 0, "unlimited search failed: {out}");
    assert_eq!(out.lines().count(), 12, "expected every match: {out}");
}

#[test]
fn a_scope_that_matches_nothing_is_a_miss_not_the_top_hit() {
    let (dir, db) = scratch("scope");
    // one globally-unique method under a real owner — the condition under which
    // a wrong owner used to be silently discarded
    fs::write(
        dir.join("a.rb"),
        "module Shop\n  class Cart\n    def recalculate_totals\n      1\n    end\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    // the real owner resolves
    let (code, out) = rq(&db, &dir, &["Cart#recalculate_totals", "--ndjson"]);
    assert_eq!(code, 0, "real owner should resolve: {out}");
    assert!(out.contains("recalculate_totals"), "found it: {out}");

    // a made-up owner must not return that same definition. It used to, at
    // confidence 1.0 — the strongest signal available, on the one query whose
    // constraint had been thrown away.
    let (code, out) = rq(&db, &dir, &["NoSuchClass#recalculate_totals", "--ndjson"]);
    assert_ne!(code, 0, "a bogus owner must not succeed: {out}");
    assert!(
        !out.contains("\"confidence\":1.0"),
        "and never at full confidence: {out}"
    );
    // and it's distinguishable from a name that simply isn't there, since
    // "wrong owner" is the more useful answer of the two
    assert!(
        out.contains("scope_not_found"),
        "reports the scope miss: {out}"
    );
    assert!(out.contains("Shop::Cart"), "says where it does live: {out}");

    let (_, out) = rq(&db, &dir, &["Cart#no_such_method_at_all", "--ndjson"]);
    assert!(
        out.contains("no_match"),
        "an absent name is still a plain miss: {out}"
    );
}

#[test]
fn a_class_is_reachable_by_the_name_of_its_file() {
    let (dir, db) = scratch("pathrecall");
    // the class isn't named after the file, so only path recall can reach it
    fs::write(
        dir.join("billing.rb"),
        "class Invoicer\n  def run\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["billing", "--ndjson"]);
    assert_eq!(code, 0, "path recall should find it: {out}");
    assert!(out.contains("Invoicer"), "the class in billing.rb: {out}");
}

#[test]
fn a_short_abbreviation_still_reaches_a_longer_name() {
    let (dir, db) = scratch("abbrev");
    fs::write(
        dir.join("a.rb"),
        "class UserAccount\n  def run\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    // `usr` skips letters, so neither an exact nor a prefix match reaches it —
    // this is what the first-character anchor pass exists for
    let (code, out) = rq(&db, &dir, &["usr", "--ndjson"]);
    assert_eq!(code, 0, "short abbreviation should resolve: {out}");
    assert!(
        out.contains("UserAccount"),
        "reached the longer name: {out}"
    );
}

#[test]
fn one_name_declared_in_several_files_is_one_result() {
    let (dir, db) = scratch("reopen");
    // a module reopened across files, the Ruby (and Rust `impl`) reality —
    // four rows for one answer is the opposite of what ranking is for
    fs::write(dir.join("a.rb"), "module Shop\n  module Cart\n  end\nend\n").unwrap();
    fs::write(
        dir.join("b.rb"),
        "module Shop\n  module Cart\n    def add\n      1\n    end\n  end\nend\n",
    )
    .unwrap();
    fs::write(dir.join("c.rb"), "module Shop\n  module Cart\n  end\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq(&db, &dir, &["Cart", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert_eq!(out.lines().count(), 1, "one result for one name: {out}");
    // and the fold is lossless — the other declarations are still reported
    assert!(
        out.contains("\"declarations\":3"),
        "counts the declarations: {out}"
    );
    assert!(out.contains("also_in"), "names where the others are: {out}");
}

#[test]
fn a_non_ascii_name_is_an_exact_match_however_it_is_cased() {
    // the index folds names with Unicode lowercasing; the query must fold the
    // same way, or `über` misses `Über` (and `ÜBER` misses both)
    let (dir, db) = scratch("unicode");
    fs::write(dir.join("a.py"), "class Über:\n    pass\n").unwrap();
    rq(&db, &dir, &["--index"]);
    for query in ["Über", "über", "ÜBER", "übe"] {
        let (code, out) = rq(&db, &dir, &[query, "--ndjson"]);
        assert_eq!(code, 0, "{query}: {out}");
        let row: serde_json::Value = serde_json::from_str(first_line(&out)).expect("ndjson");
        assert_eq!(row["name"], "Über", "{query}: {out}");
        let features = row["features"].to_string();
        assert!(
            features.contains("\"exact\"") || features.contains("\"prefix\""),
            "{query} is a literal match: {out}"
        );
    }
}

#[test]
fn a_typo_still_finds_the_definition() {
    let (dir, db) = scratch("typo");
    fs::write(
        dir.join("a.rb"),
        "class ConnectionPool\n  def checkout\n  end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    // swapped letters and a doubled one both used to be hard misses: every
    // query character has to appear in order for a subsequence match
    for typo in ["connectoin_pool", "connection_poool"] {
        let (code, out) = rq(&db, &dir, &[typo, "--ndjson"]);
        assert_eq!(code, 0, "{typo} should find something: {out}");
        assert!(out.contains("ConnectionPool"), "{typo} finds it: {out}");
    }
    // a real word that simply isn't there is still a definitive miss
    let (code, _) = rq(&db, &dir, &["WidgetFactory", "--ndjson"]);
    assert_ne!(code, 0, "an absent symbol is still a miss");
}

#[test]
fn confidence_and_total_do_not_depend_on_the_limit() {
    let (dir, db) = scratch("confidence");
    let body: String = (0..6).map(|i| format!("class Widget{i}\nend\n")).collect();
    fs::write(dir.join("a.rb"), body).unwrap();
    rq(&db, &dir, &["--index"]);

    // confidence is a comparison against the runner-up, so measuring it over
    // the returned window made `-l 1` unconditionally certain — and that
    // reading is what gates --show
    let conf = |n: &str| {
        let (_, out) = rq(&db, &dir, &["Widget", "--ndjson", "-l", n]);
        out.lines().next().unwrap_or_default().to_string()
    };
    let one = conf("1");
    assert!(
        !one.contains("\"confidence\":1.0"),
        "-l 1 isn't automatically certain: {one}"
    );
    // and the caller can tell how big the set it saw ten of actually was
    assert!(
        one.contains("\"total\":6"),
        "total reports the full match count: {one}"
    );

    // a filter narrows what total counts, but the limit still doesn't cut it
    fs::write(dir.join("b.rb"), "def widget_helper\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    for args in [&["class", "Widget"][..], &["Widget", "-k", "class"][..]] {
        let (_, out) = rq(&db, &dir, &[args, &["--ndjson", "-l", "1"]].concat());
        assert!(out.contains("\"total\":6"), "{args:?}: {out}");
    }

    // --explain reaches structured output instead of being silently dropped
    let (_, out) = rq(&db, &dir, &["Widget", "--ndjson", "-e", "-l", "1"]);
    assert!(
        out.contains("\"explain\""),
        "explain is carried in JSON: {out}"
    );
    // in whole points, as the text shows them — not the float the sum is made of
    let row: serde_json::Value = serde_json::from_str(first_line(&out)).expect("ndjson");
    let explain = row["explain"].as_object().expect("explain map");
    assert!(
        explain.contains_key("extent"),
        "a fractional feature: {out}"
    );
    let (_, text) = rq(&db, &dir, &["Widget", "-e", "-l", "1"]);
    for (name, value) in explain {
        let v = value.as_f64().unwrap();
        assert_eq!(v, v.round(), "{name} is whole: {out}");
        assert!(text.contains(&format!("{name} {v}")), "{name} {v}: {text}");
    }
}

#[test]
fn an_unknown_kind_or_lang_is_an_error_not_a_miss() {
    let (dir, db) = scratch("badflag");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // a typo used to come back as a definitive no_match — the one exit code a
    // script is supposed to trust as "this symbol does not exist"
    let (code, out) = rq(&db, &dir, &["Widget", "-k", "banana"]);
    assert_ne!(code, 0, "unknown kind should fail: {out}");
    assert!(
        !out.contains("no_match"),
        "reported as an error, not a miss: {out}"
    );
    let (code, out) = rq(&db, &dir, &["Widget", "-x", "cobol"]);
    assert_ne!(code, 0, "unknown lang should fail: {out}");
    assert!(
        !out.contains("no_match"),
        "reported as an error, not a miss: {out}"
    );
    // a real one still works
    let (code, out) = rq(&db, &dir, &["Widget", "-k", "class"]);
    assert_eq!(code, 0, "known kind still searches: {out}");
}

#[test]
fn usage_counts_searches_by_caller_and_flags() {
    let (dir, db) = scratch("usage");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);

    // nothing recorded yet exits 1, like a search that finds nothing
    let (code, out) = rq(&db, &dir, &["--usage", "--ndjson"]);
    assert_ne!(code, 0, "empty usage should exit non-zero: {out}");
    // and says so on stderr, as an empty search does: stdout stays data
    let (_, out, err) = rq_both(&db, &dir, &["--usage"]);
    assert_eq!((out.as_str(), err.trim()), ("", "no usage recorded yet"));

    rq(&db, &dir, &["Widget", "--ndjson"]);
    rq(&db, &dir, &["Widget", "--json", "--all-repos"]);
    rq(&db, &dir, &["NoSuchThing", "--ndjson"]);

    let (code, out) = rq(&db, &dir, &["--usage", "--ndjson"]);
    assert_eq!(code, 0, "usage failed: {out}");
    // the flag set is recorded, so agentic calls are separable from bare ones
    assert!(
        out.contains("\"flags\":\"ndjson\""),
        "bare ndjson row: {out}"
    );
    assert!(
        out.contains("\"flags\":\"json,all-repos\""),
        "flag set recorded in order: {out}"
    );
    // the miss is counted as a search that found nothing, not dropped: the two
    // bare `--ndjson` calls share a row, and one of them found nothing
    assert!(
        out.contains("\"searches\":2") && out.contains("\"misses\":1"),
        "the miss is counted, not dropped: {out}"
    );
    // a definitive miss against a ready index is not a warming one — rq
    // separates them in its exit codes, so the counts must too
    assert!(
        out.contains("\"warming\":0"),
        "an indexed repo's miss isn't counted as warming: {out}"
    );
    assert!(
        out.contains("\"on_complete\":2"),
        "queries against a complete index are counted: {out}"
    );

    // --show counts once whether it prints the body or falls through to the
    // list, which counts on its own exit
    fs::write(dir.join("b.rb"), "class Widgetry\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    let (_, shown) = rq(&db, &dir, &["Widget", "--show", "--ndjson"]);
    assert!(shown.contains("\"body\""), "confident --show: {shown}");
    let (_, fell) = rq(&db, &dir, &["Widg", "--show", "--ndjson"]);
    assert!(!fell.contains("\"body\""), "--show fell through: {fell}");
    let (_, out) = rq(&db, &dir, &["--usage", "--ndjson"]);
    let show_row = out
        .lines()
        .find(|l| l.contains("\"flags\":\"ndjson,show\""))
        .unwrap_or_else(|| panic!("no --show row: {out}"));
    assert!(show_row.contains("\"searches\":2"), "{show_row}");
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

    // seed only the services subtree: this run indexes just that, and
    // coverage stays warming (a seed, not a fence)
    let (code, out) = rq(&db, &dir, &["--index", "--path", "app/services", "-J"]);
    assert_eq!(code, 0, "subset index failed: {out}");
    assert!(out.contains("\"scope\":\"subtree\""), "seed marker: {out}");
    assert!(out.contains("\"files\":1"), "seeded one file: {out}");
    let (_, status) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert!(
        status.contains("\"status\":\"warming\""),
        "a seed leaves coverage warming: {status}"
    );

    // the seeded subtree is searchable, with a repo-relative path
    let (code, out) = rq(&db, &dir, &["charge", "--ndjson"]);
    assert_eq!(code, 0, "charge search failed: {out}");
    assert!(
        out.contains("\"file\":\"app/services/charge.rb\""),
        "subset hit: {out}"
    );

    // a symbol outside the seed: warming continues over the rest of the repo,
    // finds it, and persists it
    let (code, out) = rq(&db, &dir, &["account", "--ndjson"]);
    assert_eq!(code, 0, "warming finds the unseeded symbol: {out}");
    assert!(
        out.contains("\"file\":\"app/models/account.rb\""),
        "warm hit: {out}"
    );
    let (_, status) = rq(&db, &dir, &["--status", "--ndjson"]);
    assert!(
        status.contains("\"files\":2"),
        "warming persisted the rest: {status}"
    );
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
    let (code, out) = rq(&db, &dir, &["widget", "app/services", "--ndjson"]);
    assert_eq!(code, 0, "positional-path search failed: {out}");
    assert!(
        out.contains("app/services/widget.rb"),
        "services hit kept: {out}"
    );
    assert!(
        !out.contains("app/models/widget.rb"),
        "models hit filtered out: {out}"
    );
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
    let (code, out) = rq(&db, &dir, &["charge", "-k", "m", "--ndjson"]);
    assert_eq!(code, 0, "kind search failed: {out}");
    assert!(out.contains("\"kind\":\"method\""), "method kept: {out}");
    assert!(
        !out.contains("\"kind\":\"class\""),
        "class filtered out: {out}"
    );
}

#[test]
fn kind_type_means_any_named_type() {
    let (dir, db) = scratch("kind-type");
    fs::write(
        dir.join("a.rs"),
        "pub struct Gizmo;
pub type GizmoRef = Gizmo;
pub enum Mode { Gizmos }
fn gizmo() {}
",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    // `type` is an alias or a struct; `alias` only the alias
    let (code, out) = rq(&db, &dir, &["giz", "-k", "type", "--ndjson"]);
    assert_eq!(code, 0, "kind search failed: {out}");
    assert!(out.contains("\"name\":\"GizmoRef\""), "alias kept: {out}");
    assert!(out.contains("\"name\":\"Gizmo\""), "struct kept: {out}");
    assert!(!out.contains("\"kind\":\"function\""), "fn dropped: {out}");
    let (_, out) = rq(&db, &dir, &["giz", "-k", "alias", "--ndjson"]);
    assert!(!out.contains("\"name\":\"Gizmo\""), "struct dropped: {out}");
    let (_, out) = rq(&db, &dir, &["Gizmos", "-k", "member", "--ndjson"]);
    assert!(out.contains("\"kind\":\"variant\""), "variant found: {out}");
}

#[test]
fn kind_constant_selects_constants_across_languages() {
    let (dir, db) = scratch("kind-const");
    fs::write(
        dir.join("limits.go"),
        "package limits\n\nconst MaxRetries = 3\n\nfunc MaxRetriesFor() int { return MaxRetries }\n",
    )
    .unwrap();
    fs::write(
        dir.join("limits.py"),
        "MAX_RETRIES = 3\n\ndef max_retries_for():\n    return MAX_RETRIES\n",
    )
    .unwrap();
    fs::write(
        dir.join("limits.ts"),
        "export const retryLimit = 3;\n\nexport function retryLimitFor() {\n  return retryLimit;\n}\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    for (query, lang) in [
        ("MaxRetries", "go"),
        ("MAX_RETRIES", "python"),
        ("retryLimit", "typescript"),
    ] {
        let (code, out) = rq(&db, &dir, &[query, "-k", "constant", "--ndjson"]);
        assert_eq!(code, 0, "{lang} constant search failed: {out}");
        assert!(
            out.contains(&format!("\"kind\":\"constant\",\"language\":\"{lang}\"")),
            "{lang} constant kept: {out}"
        );
        assert!(
            !out.contains("\"kind\":\"function\""),
            "{lang} function filtered: {out}"
        );
    }
}

#[test]
fn generated_code_ranks_below_hand_written_code() {
    let (dir, db) = scratch("generated");
    // the generated file's name even matches the query, as stringer's do
    fs::write(
        dir.join("kind_string.go"),
        "// Code generated by \"stringer -type=Kind\"; DO NOT EDIT.\n\npackage shop\n\nfunc (i Kind) String() string {\n\treturn \"\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("widget.go"),
        "package shop\n\ntype Widget struct{}\n\nfunc (w Widget) String() string {\n\treturn \"\"\n}\n",
    )
    .unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    let first = |db: &Path| first_line(&rq(db, &dir, &["String", "--ndjson"]).1).to_string();
    assert!(first(&db).contains("widget.go"), "{}", first(&db));

    // An index from before v19 never read the header. Upgrading queues every
    // file for re-extraction (the store's migration tests cover the ladder);
    // this is that queued state, and the next pass picks the marker up.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "UPDATE files SET generated = 0, content_hash = 'stale:' || content_hash; \
             UPDATE checkout_files SET mtime = NULL;",
        )
        .unwrap();
    }
    rq(&db, &dir, &["--index"]);
    assert!(first(&db).contains("widget.go"), "{}", first(&db));
}

#[test]
fn an_index_from_before_constants_gains_them_on_the_next_search() {
    let (dir, db) = scratch("v14");
    fs::write(
        dir.join("limits.go"),
        "package limits\n\nconst MaxRetries = 3\n\nfunc Build() {}\n",
    )
    .unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    // what a v13 binary left behind, as v14 queued it (the store's migration
    // tests cover the ladder): the same file, no constant, stamps forgotten
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DELETE FROM symbols WHERE kind = 'constant'; \
             UPDATE files SET content_hash = 'stale:' || content_hash; \
             UPDATE checkout_files SET mtime = NULL; \
             UPDATE coverage SET status = 'warming';",
        )
        .unwrap();
    }

    // The repo is warming. A search that the old rows can't answer then holds
    // for the in-process warm (detach is off here), which re-parses the file
    // the migration un-stamped.
    let (code, out) = rq(&db, &dir, &["MaxRetries", "--ndjson"]);
    assert_eq!(code, 0, "constant found after the upgrade: {out}");
    assert!(
        first_line(&out).contains("\"kind\":\"constant\""),
        "constant ranks first: {out}"
    );
    let (_, status) = rq(&db, &dir, &["--status", "--json"]);
    assert!(
        status.contains("\"status\": \"complete\""),
        "the sweep completed: {status}"
    );
}

#[test]
fn first_query_warms_the_index_without_an_explicit_reindex() {
    // A git repo that was never explicitly indexed: the first query opportunistically
    // warms the index (time-bounded) and still answers.
    let (dir, db) = scratch("warm");
    git_init(&dir);
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();

    let (code, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert_eq!(code, 0, "cold search failed: {out}");
    assert!(out.contains("\"name\":\"Widget\""), "result present: {out}");
}

#[test]
fn explicitly_indexes_and_recognizes_a_non_git_directory() {
    // A non-git directory (no git_init): you can still index it explicitly, and
    // a later search recognizes it as the current repo and self-heals.
    let (dir, db) = scratch("nongit");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();

    let (code, out) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index of a non-git dir failed: {out}");

    let (code, out) = rq(&db, &dir, &["widget", "--ndjson"]);
    assert_eq!(code, 0, "search failed: {out}");
    assert!(out.contains("\"name\":\"Widget\""), "result present: {out}");
    // recognized as the current repo → the current-repo boost applies
    assert!(
        out.contains("current_repo"),
        "current-repo boost applied: {out}"
    );
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
    let (code, out) = rq(&db, &dir, &["widget", "-x", "rs", "--ndjson"]);
    assert_eq!(code, 0, "lang search failed: {out}");
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
    let (code, out) = rq(&db, &dir, &["widget", "-x", "py", "--ndjson"]);
    assert_ne!(code, 0, "no python here, should exit non-zero");
    assert!(
        out.contains("\"status\":\"no_match\""),
        "a definitive miss reports no_match: {out}"
    );
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
    let (code, _) = rq(&db, &dir, &["--index"]);
    assert_eq!(code, 0);
    let (_, out) = rq(&db, &dir, &["DeepWidget"]);
    assert!(out.contains("nested/deep.rb"), "root-relative path: {out}");

    // searching from the subdirectory must reuse the same repo, not fork a new
    // one keyed at the subdir — the top-level symbol stays found, and there's
    // still exactly one repository with both files.
    let (_, out) = rq(&db, &sub, &["TopWidget"]);
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

    let (code, out) = rq(&db, &dir, &["Widget"]);
    assert_eq!(code, 0, "warmed search failed: {out}");
    assert!(
        out.contains("lib/widget.rb"),
        "tracked source warmed: {out}"
    );
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

    let run = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["Widget", "--json"])
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
}

#[test]
fn an_interactive_cold_repo_shows_progress_and_finds_the_answer() {
    // The human path: forcing interactive turns on the stderr heads-up + Ctrl-C
    // handling, but the blocking-until-answered behavior is the same as --json.
    let (dir, db) = scratch("block-tty");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);

    let run = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["Widget"])
        .env("RQ_ANSWER_BUDGET_MS", "1")
        .env("RQ_ASSUME_INTERACTIVE", "1") // pretend a TTY
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(run.status.success(), "should find the symbol: {out:?}");
    assert!(out.contains("widget.rb"), "found in the right file: {out}");
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

    let run = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["Nonexistent", "--json"])
        .env("RQ_COLLECT_CAP", "5") // one pass can't finish → stays "warming"
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(
        out.contains("\"status\": \"warming\""),
        "an incomplete-index miss reports warming in JSON: {out:?}"
    );
    assert_eq!(
        run.status.code(),
        Some(2),
        "an incomplete-index miss is indeterminate (exit 2), not definitive"
    );
}

#[test]
fn a_cold_search_finds_a_symbol_beyond_what_one_pass_parses() {
    // The file naming the query is indexed ahead of walk order, so the first
    // search answers even when the pass is capped short of it — by a method
    // whose name is nowhere in its path.
    let (dir, db) = scratch("demand");
    for i in 0..20 {
        fs::write(
            dir.join(format!("m{i:02}.rb")),
            format!("class M{i}\nend\n"),
        )
        .unwrap();
    }
    fs::write(
        dir.join("m19.rb"),
        "class M19\n  def render_totals\n  end\nend\n",
    )
    .unwrap();
    git_init_commit(&dir);

    let run = rq_cmd(&db, &dir)
        .args(["M19#render_totals", "--json"])
        .env("RQ_COLLECT_CAP", "2")
        .output()
        .expect("run rq");
    let out = String::from_utf8_lossy(&run.stdout);
    assert_eq!(
        run.status.code(),
        Some(0),
        "answered on the first search: {out}"
    );
    assert!(out.contains("m19.rb"), "found in the right file: {out}");
}

#[test]
fn no_wait_returns_without_blocking_on_a_rebuild() {
    // `--no-wait` is the agent/script escape hatch: a query issued while the index
    // is (re)building must answer from the committed index *now*, never
    // block-until-answered on the writer. A default programmatic miss on an
    // incomplete index blocks up to the wait budget before giving up (see
    // `an_incomplete_index_reports_an_indeterminate_miss_not_a_definitive_one`);
    // here the wait budget is 10 minutes, so if --no-wait blocked at all this test
    // would hang. It returns at once instead — an honest `warming` miss (exit 2,
    // "retry"), not a multi-minute hang and not a false definitive absence.
    let (dir, db) = scratch("no-wait");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);

    // Both spellings must return at once (not block): the bare flag and its
    // duration form `--wait 0`. RQ_WAIT_BUDGET_MS is 10 minutes throughout, so a
    // block on either would hang the test.
    for flags in [["--no-wait"].as_slice(), ["--wait", "0"].as_slice()] {
        let mut args = vec!["Nonexistent", "--json"];
        args.extend_from_slice(flags);
        let run = rq_cmd(&db, &dir)
            .args(&args)
            .envs(NO_CHILD) // the index stays incomplete across both runs
            .env("RQ_WAIT_BUDGET_MS", "600000") // a block, if it happened, would hang the test
            .output()
            .expect("run rq");
        let out = String::from_utf8_lossy(&run.stdout);
        assert!(
            out.contains("\"status\": \"warming\""),
            "{flags:?} miss on a not-yet-complete index reports warming (retry), \
             not a block or a false absence: {out:?}"
        );
        assert_eq!(
            run.status.code(),
            Some(2),
            "{flags:?} miss on an incomplete index is indeterminate (exit 2)"
        );
    }
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
    let (code, out) = rq(&db, &dir, &["--symbols", "widget.rb", "--ndjson"]);
    assert_eq!(code, 0, "symbols failed: {out}");
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
    let (code, out) = rq(
        &db,
        &dir,
        &["--symbols", "widget.rb", "-k", "method", "--ndjson"],
    );
    assert_eq!(code, 0, "filtered symbols failed: {out}");
    assert_eq!(out.lines().count(), 2, "two methods only: {out}");
    assert!(
        !out.contains("\"name\":\"Widget\""),
        "class filtered out: {out}"
    );

    // a file that defines nothing (of that kind) is a miss, as a search's is
    let (code, out) = rq(
        &db,
        &dir,
        &["--symbols", "other.rb", "-k", "method", "--json"],
    );
    assert!(
        code != 0 && out.contains("\"status\": \"no_match\""),
        "{out}"
    );
}

#[test]
fn a_class_method_says_it_is_one_everywhere_a_result_is_printed() {
    let (dir, db) = scratch("singleton");
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::write(
        dir.join("lib/widget.rb"),
        "class Widget\n  def self.clock\n    Clock.new\n  end\n  class << self\n    private\n    def register(name)\n      name\n    end\n  end\n  def size\n  end\n  def self.hidden_build\n  end\n  private_class_method :hidden_build\nend\n",
    )
    .unwrap();
    git_init_commit(&dir);

    // --symbols: true on the class's own methods, omitted (never false or
    // null) on everything else
    let (code, out) = rq(&db, &dir, &["--symbols", "lib/widget.rb", "--ndjson"]);
    assert_eq!(code, 0, "{out}");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("ndjson line"))
        .collect();
    let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap().clone();
    assert_eq!(row("clock")["singleton"], true, "{out}");
    assert_eq!(row("register")["singleton"], true, "{out}");
    assert_eq!(row("register")["visibility"], "private", "{out}");
    assert!(row("size").get("singleton").is_none(), "{out}");
    assert!(row("Widget").get("singleton").is_none(), "{out}");
    // `private_class_method` hides a class method after its def
    assert_eq!(row("hidden_build")["singleton"], true, "{out}");
    assert_eq!(row("hidden_build")["visibility"], "private", "{out}");

    // a search hit, --json and batch alike
    let (code, out) = rq(&db, &dir, &["register", "--json"]);
    assert_eq!(code, 0, "{out}");
    let hits: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(hits[0]["singleton"], true, "{out}");
    let (code, out) = rq_stdin(&db, &dir, &["-J", "-l", "1"], "clock\nsize\n");
    assert_eq!(code, 0, "{out}");
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("ndjson line"))
        .collect();
    let answer = |q: &str| rows.iter().find(|r| r["query"] == q).unwrap().clone();
    assert_eq!(answer("clock")["singleton"], true, "{out}");
    assert!(answer("size").get("singleton").is_none(), "{out}");

    // text says so where it names the kind
    let (code, out) = rq(&db, &dir, &["--symbols", "lib/widget.rb"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("singleton method clock · Widget"), "{out}");
    assert!(out.contains("  method size · Widget"), "{out}");
}

#[test]
fn symbols_reflects_the_file_as_it_is_on_disk_now() {
    // On a complete index the outline must track the file itself — an edit, a
    // brand-new untracked file, a deletion — without a repo-wide re-index.
    let (dir, db) = scratch("symbols-fresh");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    fs::write(
        dir.join("widget.rb"),
        "class Widget\n  def added\n  end\nend\n",
    )
    .unwrap();
    let (code, out) = rq(&db, &dir, &["--symbols", "widget.rb", "--ndjson"]);
    assert!(
        code == 0 && out.contains("\"name\":\"added\""),
        "edit shows: {out}"
    );

    fs::write(dir.join("fresh.rb"), "class Fresh\nend\n").unwrap();
    let (code, out) = rq(&db, &dir, &["--symbols", "fresh.rb", "--ndjson"]);
    assert!(
        code == 0 && out.contains("\"name\":\"Fresh\""),
        "new file shows: {out}"
    );

    fs::remove_file(dir.join("widget.rb")).unwrap();
    let (code, out) = rq(&db, &dir, &["--symbols", "widget.rb", "--ndjson"]);
    assert_ne!(code, 0, "a deleted file has no outline: {out}");
    assert!(!out.contains("Widget"), "no stale rows: {out}");

    let (code, out) = rq(&db, &dir, &["--symbols", "notes.txt", "--ndjson"]);
    assert_ne!(code, 0, "an unsupported file has no outline: {out}");
}

#[test]
fn symbols_outlines_its_file_on_a_cold_repo_too_big_for_one_pass() {
    // The named file is indexed first however far the pass gets — including a
    // short name that looks nothing like its own path.
    let (dir, db) = scratch("symbols-cold");
    for i in 0..20 {
        fs::write(
            dir.join(format!("m{i:02}.rb")),
            format!("class M{i}\nend\n"),
        )
        .unwrap();
    }
    git_init_commit(&dir);

    let out = rq_cmd(&db, &dir)
        .args(["--symbols", "m19.rb", "--ndjson"])
        .env("RQ_COLLECT_CAP", "2")
        .output()
        .expect("run rq");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("\"name\":\"M19\""),
        "the file is outlined: {text}"
    );
}

#[test]
fn bare_invocation_prints_help() {
    let (dir, db) = scratch("help");
    let (code, out) = rq(&db, &dir, &[]);
    assert_eq!(code, 0, "bare rq should exit 0");
    assert!(
        out.contains("rq finds the code you're looking for"),
        "help banner: {out}"
    );
    assert!(out.contains("Usage:"), "usage in help: {out}");
}

#[test]
fn detached_warm_finishes_coverage_in_the_background() {
    let (dir, db) = scratch("detach");
    for i in 0..30 {
        fs::write(
            dir.join(format!("f{i:02}.rb")),
            format!("class K{i:02}\nend\n"),
        )
        .unwrap();
    }
    git_init_commit(&dir);

    // Cap each in-process pass tiny so the answering query can't finish the
    // sweep itself; detached warming (on) must pick up the remainder. The
    // child inherits the cap, so it needs several passes — exercising its
    // sweep-until-complete loop too.
    let out = rq_cmd(&db, &dir)
        .args(["K00"])
        .env("RQ_WARM_DETACH", "1")
        .env("RQ_COLLECT_CAP", "5")
        .output()
        .expect("run rq");
    assert!(
        out.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // the detached child completes the sweep with no further queries
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (_, status) = rq(&db, &dir, &["--status", "--ndjson"]);
        if status.contains("\"status\":\"complete\"") && status.contains("\"files\":30") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached warm never completed: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Run the binary with queries piped on stdin, returning stdout.
fn rq_stdin(db: &Path, cwd: &Path, args: &[&str], stdin: &str) -> (i32, String) {
    use std::io::Write;
    let mut child = rq_cmd(db, cwd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run rq");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(stdin.as_bytes())
        .expect("write queries");
    let out = child.wait_with_output().expect("rq exits");
    (
        common::code(&out),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn batch_answers_every_piped_query_and_says_which_is_which() {
    // One process, many questions: each row has to name the query it answers,
    // or a caller can't tell whose results are whose.
    let (dir, db) = scratch("batch");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    let (code, out) = rq_stdin(
        &db,
        &dir,
        &["-J", "-l", "1"],
        "widget\ngadget\nnosuchthing\n",
    );
    assert_eq!(code, 0, "a batch that found something exits 0: {out}");

    let rows: Vec<serde_json::Value> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ndjson line"))
        .collect();
    let for_query = |q: &str| {
        rows.iter()
            .find(|r| r["query"] == q)
            .unwrap_or_else(|| panic!("no row for {q}: {out}"))
            .clone()
    };
    assert_eq!(for_query("widget")["name"], "Widget");
    assert_eq!(for_query("gadget")["name"], "Gadget");
    // a miss is reported, not silently dropped — otherwise it's
    // indistinguishable from a query that never ran
    assert_eq!(for_query("nosuchthing")["status"], "no_match");
}

#[test]
fn a_miss_in_an_indexed_tree_outside_git_is_definitive() {
    // No git to ask whether the tree moved: rq compares it with the index.
    // Unchanged, a miss is absent (1), every time; it used to say `warming`
    // (2) forever, with nothing pending.
    let (dir, db) = scratch("nongit-miss");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    let prod = [("RQ_WARM_DETACH", "wait")];
    for _ in 0..2 {
        let (code, out, _) = rq_full(&db, &dir, &["Nosuch", "--json"], &prod, None);
        assert_eq!(
            (code, json(&out)["status"].as_str()),
            (1, Some("no_match")),
            "{out}"
        );
    }
    // an edit is pending work: the miss says retry, and the retry finds it
    fs::write(dir.join("b.rb"), "class Nosuch\nend\n").unwrap();
    let (code, out, _) = rq_full(&db, &dir, &["Gadget", "--json"], &prod, None);
    assert_eq!(
        (code, json(&out)["status"].as_str()),
        (2, Some("warming")),
        "{out}"
    );
    let (code, out, _) = rq_full(&db, &dir, &["Nosuch", "--json"], &prod, None);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn no_wait_outside_git_and_unindexed_is_a_plain_miss() {
    // nothing indexes a dir rq doesn't track: the live scan is the answer
    let (dir, db) = scratch("nongit-unindexed-nowait");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    for args in [["Nosuch", "--json"], ["Nosuch", "--no-wait"]] {
        let (code, out) = rq(&db, &dir, &args);
        assert_eq!(code, 1, "{args:?}: {out}");
    }
    let (code, out) = rq(&db, &dir, &["Widget", "--no-wait"]);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_file_rq_cannot_decode_or_open_does_not_keep_a_tree_warming() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, db) = scratch("nongit-unreadable");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("latin.rb"), b"# \xe9 latin-1\nclass Gadget\nend\n").unwrap();
    let locked = dir.join("locked.rb");
    fs::write(&locked, "class Locked\nend\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    rq(&db, &dir, &["--index"]);
    let prod = [("RQ_WARM_DETACH", "wait")];

    let (code, out, _) = rq_full(&db, &dir, &["Gadget", "--json"], &prod, None);
    assert_eq!(code, 0, "a non-UTF-8 file is still read: {out}");
    for _ in 0..2 {
        let (code, out, _) = rq_full(&db, &dir, &["Nosuch", "--json"], &prod, None);
        assert_eq!(code, 1, "{out}");
    }
    // readable again is a change, and the retry reads it
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    let (code, out, _) = rq_full(&db, &dir, &["Locked", "--json"], &prod, None);
    assert_eq!(code, 2, "{out}");
    let (code, out, _) = rq_full(&db, &dir, &["Locked", "--json"], &prod, None);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn an_edit_in_a_repo_with_no_commits_is_noticed() {
    // git tracks nothing yet, so it can't say what moved: the tree is compared
    let (dir, db) = scratch("unborn-edit");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init(&dir);
    rq(&db, &dir, &["--index"]);
    let prod = [("RQ_WARM_DETACH", "wait")];
    let (code, out, _) = rq_full(&db, &dir, &["Nosuch", "--json"], &prod, None);
    assert_eq!(code, 1, "unchanged: {out}");

    fs::write(dir.join("a.rb"), "class Widget\n  def added; end\nend\n").unwrap();
    let (code, out, _) = rq_full(&db, &dir, &["added", "--json"], &prod, None);
    assert_eq!(code, 2, "{out}");
    let (code, out, _) = rq_full(&db, &dir, &["added", "--json"], &prod, None);
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_batch_notices_an_edit_to_a_complete_repo() {
    let (dir, db) = scratch("batch-edit");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    fs::write(dir.join("a.rb"), "class Widget\n  def added; end\nend\n").unwrap();

    // --no-wait: the miss is provisional, and the child it leaves reads the edit
    let (code, out, _) = rq_full(&db, &dir, &["-J", "--no-wait"], &NO_CHILD, Some("added\n"));
    assert_eq!(
        (code, json(&out)["status"].as_str()),
        (2, Some("warming")),
        "{out}"
    );
    let prod = [("RQ_WARM_DETACH", "wait")];
    let (code, out, _) = rq_full(&db, &dir, &["-J", "--no-wait"], &prod, Some("added\n"));
    assert_eq!(code, 0, "{out}");

    // otherwise a batch takes the edit in before answering
    fs::write(dir.join("a.rb"), "class Widget\n  def later; end\nend\n").unwrap();
    let (code, out, _) = rq_full(&db, &dir, &["-J"], &NO_CHILD, Some("later\n"));
    assert_eq!(code, 0, "{out}");
}

#[test]
fn batch_refuses_the_output_and_flags_it_cannot_frame() {
    let (dir, db) = scratch("batch-flags");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);

    // --json would have to frame N result sets as one array; --ndjson is the
    // shape that streams
    let (code, _) = rq_stdin(&db, &dir, &["--json"], "widget\n");
    assert_ne!(code, 0, "--json is refused for a batch");
    // --show/--open act on one result
    let (code, _) = rq_stdin(&db, &dir, &["-J", "--show"], "widget\n");
    assert_ne!(code, 0, "--show is refused for a batch");
}

/// A half-built index nobody is filling: no warm child finishes it between
/// one assert and the next. For tests that stage a partial index.
const NO_CHILD: [(&str, &str); 1] = [("RQ_WARM_DETACH", "0")];

/// Run the binary with extra env and optional piped stdin; hand back the exit
/// code, stdout and stderr.
fn rq_full(
    db: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&str>,
) -> (i32, String, String) {
    use std::io::Write;
    let mut cmd = rq_cmd(db, cwd);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("run rq");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("rq exits");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_structured_caller_gets_its_errors_as_json() {
    let (dir, db) = scratch("json-errors");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    rq(&db, &dir, &["--index"]);
    // a file where the database's directory should be: nothing can be created
    let blocked = dir.join("blocked");
    fs::write(&blocked, "").unwrap();
    // (a corrupt database is rebuilt rather than reported: tests/recovery.rs)
    let unwritable = blocked.join("rq.db");
    let unwritable = unwritable.to_str().unwrap();
    // args, extra env, piped stdin, the error kind expected
    type Case<'a> = (
        &'a [&'a str],
        &'a [(&'a str, &'a str)],
        Option<&'a str>,
        &'a str,
    );
    let cases: &[Case] = &[
        (&["Widget", "-k", "banana", "--json"], &[], None, "usage"),
        (&["Widget", "-x", "cobol", "-J"], &[], None, "usage"),
        (&["  ", "--json"], &[], None, "usage"),
        // clap rejects these before rq has read its own flags
        (&["Widget", "--wait", "soon", "--json"], &[], None, "usage"),
        (&["Widget", "-ej", "--wait", "soon"], &[], None, "usage"),
        (&["-w", "Widget", "-J"], &[], None, "usage"),
        (&["--no-such-flag", "Widget"], &[], None, "usage"),
        (&["--json"], &[], Some("Widget\n"), "usage"),
        (
            &["Widget", "--json"],
            &[("RQ_DB", unwritable)],
            None,
            "database",
        ),
        (
            &["Widget", "-J"],
            &[("RQ_DB", unwritable)],
            None,
            "database",
        ),
        (
            &["--status", "--json"],
            &[("RQ_DB", unwritable)],
            None,
            "database",
        ),
        (&["--symbols", "gone.rb", "--json"], &[], None, "not_found"),
    ];
    // sysexits(3): none shared with a hit (0), a miss (1) or warming (2)
    let exit_code = |kind| match kind {
        "usage" => 64,
        "not_found" => 66,
        "database" => 74,
        other => panic!("no expected exit code for {other}"),
    };
    for &(args, env, stdin, kind) in cases {
        let (code, out, err) = rq_full(&db, &dir, args, env, stdin);
        assert_eq!(code, exit_code(kind), "{args:?}: exit code for {kind}");
        // no structured flag: this case is about the text path's exit code
        if !args.iter().any(|a| matches!(*a, "--json" | "-J" | "-ej")) {
            assert!(out.is_empty(), "{args:?}: nothing on stdout: {out:?}");
            continue;
        }
        let obj: serde_json::Value = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{args:?}: one JSON object on stdout ({e}): {out:?}"));
        assert_eq!(obj["kind"], kind, "{args:?}: {obj}");
        assert_eq!(
            obj["code"], code,
            "{args:?}: the object carries the exit code"
        );
        let message = obj["error"].as_str().expect("an error message");
        // the human still reads it on stderr, the same words
        assert!(
            err.contains(message),
            "{args:?}: stderr {err:?} vs {message:?}"
        );

        // without a structured flag nothing lands on stdout, and the exit
        // code is the same (`-w` alone is legal, so it has nothing to compare)
        let plain: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| !matches!(*a, "--json" | "-J"))
            .map(|a| if a == "-ej" { "-e" } else { a })
            .collect();
        if stdin.is_none() && !args.contains(&"-w") {
            let (plain_code, out, _) = rq_full(&db, &dir, &plain, env, None);
            assert_eq!(plain_code, code, "{plain:?}: same exit code as text");
            assert!(out.is_empty(), "{plain:?}: nothing on stdout: {out:?}");
        }
    }
    // a value that merely contains `j` is not the flag
    let (_, out, _) = rq_full(&db, &dir, &["Widget", "-xj", "-k", "banana"], &[], None);
    assert!(out.is_empty(), "`-xj` is --lang j, not --json: {out:?}");
}

#[test]
fn help_and_version_are_not_usage_errors() {
    let (dir, db) = scratch("help-version");
    for args in [["--help"], ["-h"], ["--version"], ["-V"]] {
        let (code, out, _) = rq_full(&db, &dir, &args, &[], None);
        assert_eq!(code, 0, "{args:?} succeeds");
        assert!(!out.is_empty(), "{args:?} prints to stdout");
    }
    let (_, help, _) = rq_full(&db, &dir, &["--help"], &[], None);
    assert!(help.contains("EXIT CODES"), "--help lists them: {help}");
    assert!(help.contains("--drop"), "and the commands' own: {help}");
}

#[test]
fn commands_with_nothing_to_do_still_succeed() {
    // A command that ran but had nothing to report or remove still succeeded;
    // exit 1 stays a search's "absent" (and --usage's "nothing recorded").
    let (dir, db) = scratch("nothing-to-do");
    for args in [&["--status"][..], &["--drop"], &["--index"], &["--index"]] {
        let (code, out, _) = rq_full(&db, &dir, args, &[], None);
        assert_eq!(code, 0, "{args:?}: {out}");
    }
}

/// `--profile` on an index run reports phases, counters and the slowest files —
/// to stderr, structured when the output mode is, and nothing at all when off.
#[test]
fn index_profile_reports_phases_and_counters() {
    let (dir, db) = scratch("index-profile");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    fs::write(dir.join("beta.rb"), "class HandlerB\ndef go\nend\nend\n").unwrap();

    // off: stdout carries the result, stderr stays silent
    let (code, out, err) = rq_both(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index failed: {out}");
    assert!(
        !err.contains("walk+parse+write"),
        "profile leaked with --profile off: {err}"
    );

    // text: a human-readable table on stderr, stdout untouched
    let (dir2, db2) = scratch("index-profile-text");
    fs::write(dir2.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    let (code, out, err) = rq_both(&db2, &dir2, &["--index", "--profile"]);
    assert_eq!(code, 0, "index failed: {out}");
    for phase in [
        "index: setup",
        "index: walk+parse+write",
        "index: store writes",
    ] {
        assert!(err.contains(phase), "missing {phase} in profile: {err}");
    }
    for counter in ["files seen", "files parsed", "batches", "parse jobs"] {
        assert!(err.contains(counter), "missing {counter} in profile: {err}");
    }
    assert!(err.contains("slowest"), "missing slowest files: {err}");
    assert!(
        !out.contains("walk+parse+write"),
        "profile must not pollute stdout: {out}"
    );

    // json: one object on stderr with the same phase names; stdout stays parseable
    let (dir3, db3) = scratch("index-profile-json");
    fs::write(dir3.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    let (code, out, err) = rq_both(&db3, &dir3, &["--index", "--profile", "--json"]);
    assert_eq!(code, 0, "index failed: {out}");
    let stdout_json: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("stdout not json ({e}): {out}"));
    assert_eq!(stdout_json["scope"], "full");
    let profile: serde_json::Value = serde_json::from_str(err.trim())
        .unwrap_or_else(|e| panic!("profile not json ({e}): {err}"));
    assert!(profile["total_ms"].is_number(), "total_ms missing: {err}");
    let names: Vec<&str> = profile["phases"]
        .as_array()
        .expect("phases array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"index: walk+parse+write"),
        "phases: {names:?}"
    );
    assert_eq!(profile["counters"]["files parsed"], 1);
    assert!(
        profile["counters"]["parse jobs"].as_u64().unwrap_or(0) >= 1,
        "parse jobs: {err}"
    );
    let slowest = profile["slowest"].as_array().expect("slowest array");
    assert_eq!(slowest.len(), 1, "one file parsed → one slowest entry");
    assert_eq!(slowest[0]["file"], "alpha.rb");
}

/// A search's profile covers the whole run: the time to the first answer is
/// marked within `total`, and a miss reports too.
#[test]
fn search_profile_covers_the_whole_run() {
    let (dir, db) = scratch("search-profile");
    fs::write(dir.join("alpha.rb"), "class HandlerA\nend\n").unwrap();
    let (code, out, _) = rq_both(&db, &dir, &["--index"]);
    assert_eq!(code, 0, "index failed: {out}");

    let phases = |args: &[&str]| {
        let (_, _, err) = rq_both(&db, &dir, args);
        let profile: serde_json::Value = serde_json::from_str(err.trim())
            .unwrap_or_else(|e| panic!("profile not json ({e}): {err}"));
        let total = profile["total_ms"].as_f64().expect("total_ms");
        let phases: Vec<(String, f64)> = profile["phases"]
            .as_array()
            .expect("phases array")
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap_or_default().to_string(),
                    p["ms"].as_f64().unwrap_or(0.0),
                )
            })
            .collect();
        (total, phases)
    };

    let (total, hit) = phases(&["HandlerA", "--profile", "--json"]);
    let answer = hit
        .iter()
        .find(|(n, _)| n == "first answer")
        .unwrap_or_else(|| panic!("no first answer: {hit:?}"));
    assert!(answer.1 <= total, "first answer after total: {hit:?}");
    // the usage write is bookkeeping: it must never hold up the answer
    let at = |name: &str| hit.iter().position(|(n, _)| n == name);
    assert!(
        at("first answer") < at("after: record usage"),
        "usage recorded before the answer: {hit:?}"
    );

    let (_, miss) = phases(&["Nonexistent", "--profile", "--json"]);
    assert!(
        miss.iter().any(|(n, _)| n == "query"),
        "miss unreported: {miss:?}"
    );

    let (_, symbols) = phases(&["--symbols", "alpha.rb", "--profile", "--json"]);
    assert!(
        symbols.iter().any(|(n, _)| n == "symbols: query"),
        "{symbols:?}"
    );
}

#[test]
fn a_ruby_predicate_is_found_by_its_full_name() {
    // `?` ends a Ruby predicate's name; typed in full it must not read as a
    // one-char wildcard that can never match the name's own `?`
    let (dir, db) = scratch("predicate");
    fs::write(
        dir.join("a.rb"),
        "class Widget\n  def empty?; end\n  def save!; end\n  def only_uploads?; end\nend\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    for q in ["empty?", "Widget#empty?", "save!", "only_uploads?"] {
        let (code, out) = rq(&db, &dir, &[q]);
        assert!(code == 0 && out.contains("a.rb"), "{q}: {out}");
    }
    // a `?` inside a query is still a wildcard
    let (code, out) = rq(&db, &dir, &["emp?y?"]);
    assert!(code == 0 && out.contains("empty?"), "glob: {out}");
    // and a glob crosses the name's `_` the way it ignores the query's
    let (code, out) = rq(&db, &dir, &["only_up*s"]);
    assert!(
        code == 0 && out.contains("only_uploads?"),
        "glob over _: {out}"
    );
}

#[test]
fn foo_dot_new_finds_the_class_outside_git_too() {
    // an untracked dir answers from a live scan, which must fall back to the
    // class like the index does when the constructor is implicit
    let (dir, db) = scratch("constructor-live");
    fs::write(
        dir.join("account.rb"),
        "module Account\n  class Ledger\n    def self.open\n      new\n    end\n  end\nend\n",
    )
    .unwrap();
    for q in ["Ledger.new", "Account::Ledger.new"] {
        let (code, out) = rq(&db, &dir, &[q, "--ndjson"]);
        assert!(
            code == 0 && out.contains("\"class\"") && out.contains("\"line\":2"),
            "{q}: {out}"
        );
    }
}

#[test]
fn no_wait_on_an_unindexed_repo_answers_from_a_live_scan() {
    // --no-wait won't index before answering, so a git repo with no finished
    // pass — never indexed, or just dropped — is scanned live instead: a hit
    // says `live`, and a miss is `warming` because the index can't vouch yet.
    let (dir, db) = scratch("no-wait-live");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    let source = |out: &str| {
        let row: serde_json::Value = serde_json::from_str(first_line(out)).expect("a JSON row");
        row["source"].as_str().unwrap_or_default().to_string()
    };

    // a live scan stands in for an index nobody has read yet: it says so,
    // and nothing read backs its confidence
    let unread = |out: &str| {
        let row: serde_json::Value = serde_json::from_str(first_line(out)).expect("a JSON row");
        let w = &row["warming"];
        (
            w["read"].as_i64(),
            w["of"].as_i64(),
            row["confidence"].as_f64(),
        )
    };
    let (code, out, _) = rq_full(&db, &dir, &["Widget", "-J", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 0, "{out}");
    assert_eq!(source(&out), "live", "cold: {out}");
    assert_eq!(unread(&out), (Some(0), Some(1), Some(0.0)), "cold: {out}");
    let (code, _, _) = rq_full(&db, &dir, &["Gizmo", "-J", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 2, "a cold miss is warming, not absent");

    rq(&db, &dir, &["--index"]);
    let (_, out, _) = rq_full(&db, &dir, &["Widget", "-J", "--no-wait"], &NO_CHILD, None);
    assert_eq!(source(&out), "index", "indexed: {out}");

    rq(&db, &dir, &["--drop"]);
    let (_, out, _) = rq_full(&db, &dir, &["Widget", "-J", "--no-wait"], &NO_CHILD, None);
    assert_eq!(source(&out), "live", "dropped: {out}");
    assert_eq!(
        unread(&out),
        (Some(0), Some(1), Some(0.0)),
        "dropped: {out}"
    );
}

#[test]
fn a_live_scan_answer_says_so() {
    // never indexed and not a git repo, so the only answer is a live scan
    let (dir, db) = scratch("live-source");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    let source = |out: &str| {
        let row: serde_json::Value = serde_json::from_str(first_line(out)).expect("a JSON row");
        row["source"].as_str().unwrap_or_default().to_string()
    };

    // quiet by default: the row says where it came from, stderr says nothing
    let (code, out, err) = rq_full(&db, &dir, &["Widget", "-J"], &[], None);
    assert_eq!(code, 0, "{out}");
    assert_eq!(source(&out), "live", "{out}");
    // nothing indexes a directory rq doesn't track: the scan is the answer
    assert!(!out.contains("\"warming\""), "{out}");
    assert!(!err.contains("live scan"), "no note without -v: {err}");

    // -v names the scan and what it cost; --profile shows its span
    let (_, _, err) = rq_full(&db, &dir, &["Widget", "-J", "-v", "--profile"], &[], None);
    assert!(
        err.contains("answered from a live scan of") && err.contains("1 file in"),
        "-v notes the live answer: {err}"
    );
    assert!(err.contains("live scan: prefiltered"), "profiled: {err}");

    // a miss scans too, but answered nothing
    let (code, _, err) = rq_full(&db, &dir, &["Gizmo", "-J", "-v"], &[], None);
    assert_eq!(code, 1);
    assert!(
        !err.contains("answered from a live scan"),
        "a miss isn't a live answer: {err}"
    );

    // --usage counts the live answers apart
    let (_, usage, _) = rq_full(&db, &dir, &["--usage", "-J"], &[], None);
    let live: i64 = usage
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|row| row["live"].as_i64())
        .sum();
    assert_eq!(live, 2, "two searches answered from a live scan: {usage}");

    // once indexed, the same query answers from the index
    rq(&db, &dir, &["--index"]);
    let (_, out, _) = rq_full(&db, &dir, &["Widget", "-J"], &[], None);
    assert_eq!(source(&out), "index", "{out}");
}

#[test]
fn an_index_pass_whose_writes_fail_exits_instead_of_hanging() {
    // A failed batch write stops the consumer, but the parse workers and the
    // walk keep sending into bounded channels nobody drains: past one channel's
    // worth of files in flight, the pass parks at 0% CPU forever. A lock that
    // outlasts busy_timeout does this for real; a trigger makes it certain.
    let (dir, db) = scratch("sink-error");
    for i in 0..2500 {
        fs::write(
            dir.join(format!("w{i}.rb")),
            format!("def widget_{i}\nend\n"),
        )
        .unwrap();
    }
    git_init_commit(&dir);
    rq(&db, &dir, &["--status"]); // lay down the schema
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_symbols BEFORE INSERT ON symbols \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();

    let mut child = rq_cmd(&db, &dir)
        .args(["--index", "."])
        .env("RQ_JOBS", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run rq");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let status = status.expect("rq --index hung after a failed write");
    assert!(!status.success(), "a failed write is an error, not success");
}

#[test]
fn concurrent_first_queries_share_a_fresh_database() {
    // Every opener of a new database used to read user_version 0 and lay down
    // the schema itself, and the WAL switch ran before busy_timeout: some of a
    // burst failed with "table already exists" or "database is locked".
    let (dir, db) = scratch("fresh-burst");
    fs::write(dir.join("widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    let runs: Vec<_> = (0..16)
        .map(|_| {
            rq_cmd(&db, &dir)
                .args(["--no-wait", "--json", "Widget"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("run rq")
        })
        .collect();
    let failures: Vec<String> = runs
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .filter(|o| o.status.code() == Some(74))
        .map(|o| {
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn an_empty_query_points_at_the_outline() {
    let (dir, db) = scratch("empty-query");
    let out = rq_cmd(&db, &dir)
        .env_remove("RQ_WARM_DETACH")
        .args(["", "-k", "interface", "--json"])
        .output()
        .expect("run rq");

    assert_eq!(out.status.code(), Some(64));
    let err: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error json");
    assert_eq!(err["kind"], "usage");
    assert!(
        err["error"].as_str().unwrap().contains("--symbols"),
        "{err}"
    );
}

#[test]
fn a_relative_rq_db_is_a_usage_error() {
    let (dir, _) = scratch("relative-db");
    let out = rq_cmd(Path::new("dbs/rq.db"), &dir)
        .args(["Widget", "--json"])
        .output()
        .expect("run rq");
    let created = dir.join("dbs").exists();

    assert_eq!(out.status.code(), Some(64));
    let err: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error json");
    assert_eq!(err["kind"], "usage");
    assert!(err["error"].as_str().unwrap().contains("RQ_DB"), "{err}");
    assert!(!created, "nothing is written beside the caller");
}

#[test]
fn an_empty_rq_db_means_the_default_and_a_directory_is_refused() {
    let (dir, _) = scratch("empty-db");
    let run = |db: &str| {
        rq_cmd(Path::new(db), &dir)
            .env_remove("RQ_WARM_DETACH")
            .args(["--status", "--json"])
            .env("HOME", &*dir)
            .output()
            .expect("run rq")
    };
    let empty = run("");
    let default_made = dir.join(".local/share/rq/rq.db").exists();
    let slash = format!("{}/dbs/", dir.display());
    let trailing = run(&slash);
    let dir_made = dir.join("dbs").exists();

    assert_eq!(empty.status.code(), Some(0));
    assert!(default_made, "the default path under HOME");
    assert_eq!(trailing.status.code(), Some(64));
    let err: serde_json::Value = serde_json::from_slice(&trailing.stdout).expect("error json");
    assert_eq!(err["kind"], "usage");
    assert!(!dir_made, "nothing is created for a refused path");
}

#[test]
fn kind_field_selects_fields_across_languages() {
    let (dir, db) = scratch("kind-field");
    fs::write(
        dir.join("hit.rs"),
        "pub struct Hit {\n    pub score: f64,\n}\n\nimpl Hit {\n    pub fn score(&self) -> f64 {\n        self.score\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("item.go"),
        "package item\n\ntype Item struct {\n\tLabel string\n}\n\nfunc Label() string { return \"\" }\n",
    )
    .unwrap();
    fs::write(
        dir.join("item.ts"),
        "export interface Item {\n  weight: number;\n}\n\nexport function weight() {\n  return 1;\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("item.py"),
        "class Item:\n    price: int = 0\n\ndef price():\n    return 0\n",
    )
    .unwrap();
    rq(&db, &dir, &["--index"]);

    for (query, lang, owner) in [
        ("score", "rust", "Hit"),
        ("Label", "go", "Item"),
        ("weight", "typescript", "Item"),
        ("price", "python", "Item"),
    ] {
        // the method or function the field shares a name with ranks first
        let (_, all) = rq(&db, &dir, &[query, "--ndjson"]);
        let first = all.lines().next().unwrap_or_default();
        assert!(!first.contains("\"kind\":\"field\""), "{lang}: {all}");

        for args in [vec![query, "-k", "field"], vec!["field", query]] {
            let (code, out) = rq(&db, &dir, &[&args[..], &["--ndjson"]].concat());
            assert_eq!(code, 0, "{lang} field search failed: {out}");
            assert!(
                out.contains(&format!("\"kind\":\"field\",\"language\":\"{lang}\"")),
                "{lang} field kept: {out}"
            );
            assert!(
                out.contains(&format!("\"parent\":\"{owner}\"")),
                "{lang}: {out}"
            );
            assert_eq!(out.lines().count(), 1, "{lang}: {out}");
        }
    }
}

/// A repo where a prefix match for `User` sits in one directory and the exact
/// definition in another, so seeding one directory leaves a partial index.
fn prefix_and_exact(label: &str) -> (Scratch, PathBuf) {
    let (dir, db) = scratch(label);
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join("models")).unwrap();
    fs::write(
        dir.join("app/fields.rb"),
        "class UserFieldsController\nend\n",
    )
    .unwrap();
    fs::write(dir.join("models/user.rb"), "class User\nend\n").unwrap();
    git_init_commit(&dir);
    (dir, db)
}

fn json(out: &str) -> serde_json::Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
}

#[test]
fn a_prefix_match_from_a_partial_index_is_provisional_not_an_answer() {
    let (dir, db) = prefix_and_exact("partial-prefix");
    rq(&db, &dir, &["--index", "app"]);

    let (code, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 2, "not an answer yet: {out}");
    let v = json(&out);
    assert_eq!(v["status"], "warming", "{out}");
    assert_eq!(v["provisional"][0]["name"], "UserFieldsController", "{out}");
    assert_eq!(v["warming"]["read"], 1, "{out}");
    assert_eq!(v["warming"]["of"], 2, "{out}");

    // text lists what it has, and says it may change
    let (code, out, err) = rq_full(&db, &dir, &["User", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 2);
    assert!(out.contains("UserFieldsController"), "{out}");
    assert!(err.contains("1 of 2 files read"), "{err}");
    assert!(err.contains("indexing stopped part-way"), "{err}");

    // --show has nothing settled to show, and says only that
    let (code, _, err) = rq_full(&db, &dir, &["User", "--show", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 2);
    assert!(err.contains("no settled match"), "{err}");
    assert!(!err.contains("narrow the query"), "{err}");
}

#[test]
fn an_exact_match_from_a_partial_index_answers_and_says_so() {
    let (dir, db) = prefix_and_exact("partial-exact");
    rq(&db, &dir, &["--index", "models"]);

    let (code, out, err) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 0, "{out}");
    let top = &json(&out)[0];
    assert_eq!(top["name"], "User", "{out}");
    let w = &top["warming"];
    assert_eq!((&w["read"], &w["of"]), (&1.into(), &2.into()), "{out}");
    assert_eq!(w["interrupted"], true, "nothing is indexing: {out}");
    assert!(w["hint"].as_str().unwrap().contains("rq --index"), "{out}");
    let partial = top["confidence"].as_f64().unwrap();
    assert!(partial <= 0.5, "scaled by the half read: {out}");
    assert!(
        err.is_empty(),
        "structured output keeps stderr quiet: {err}"
    );

    // nothing continues it here (no detached warm): text says what JSON does
    let (_, _, err) = rq_full(&db, &dir, &["User", "--no-wait"], &NO_CHILD, None);
    assert!(err.contains("indexing stopped part-way"), "{err}");
    assert!(!err.contains("still indexing"), "{err}");

    // --show judges which definition was meant, as --open does; the share
    // read is disclosed, not a reason to withhold the body
    let (code, out, err) = rq_full(&db, &dir, &["User", "--show", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("class User"), "{out}");
    assert!(err.contains("1 of 2 files read"), "{err}");
    assert!(!err.contains("narrow the query"), "{err}");
    // ...and --open says the same before handing off
    let (code, _, err) = rq_full(
        &db,
        &dir,
        &["User", "--open", "--no-wait"],
        &[("RQ_OPEN", "true")],
        None,
    );
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("1 of 2 files read"), "{err}");

    // once complete, the same answer carries no disclosure and its own confidence
    rq(&db, &dir, &["--index"]);
    let spans: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM meta WHERE key LIKE 'span:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(spans, 0, "a complete checkout keeps no span");
    let (code, out, _) = rq_full(&db, &dir, &["User", "--json"], &NO_CHILD, None);
    assert_eq!(code, 0);
    let top = &json(&out)[0];
    assert!(top.get("warming").is_none(), "{out}");
    assert!(top["confidence"].as_f64().unwrap() > partial, "{out}");
}

#[test]
fn a_search_waits_on_another_process_still_indexing() {
    // Another writer holds the index mid-build: this search's own warm can't
    // write, and ending is not the index being done. It waits, and answers
    // with the definition once the writer lets go.
    let (dir, db) = prefix_and_exact("partial-other");
    rq(&db, &dir, &["--index", "app"]);
    let (conn, mark) = hold_as_another_indexer(&db, &dir);

    let child = rq_cmd(&db, &dir)
        .args(["User", "--json", "--wait", "30s"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("run rq");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    conn.execute_batch("COMMIT").unwrap();
    conn.execute("DELETE FROM meta WHERE key = ?1", rusqlite::params![mark])
        .unwrap();

    let run = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&run.stdout);
    assert_eq!(run.status.code(), Some(0), "{out}");
    assert_eq!(json(&out)[0]["name"], "User", "{out}");
}

/// Hold `db` the way another rq mid-pass does: a live pass mark on the
/// checkout at `dir` (this test's pid) and the write lock, so a search's own
/// warm can neither finish the index nor see it finish.
fn hold_as_another_indexer(db: &Path, dir: &Path) -> (rusqlite::Connection, String) {
    let root = dir.canonicalize().unwrap();
    let mark = format!("pass:{}:{}", root.display(), std::process::id());
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, strftime('%s', 'now'))",
        rusqlite::params![mark],
    )
    .unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    (conn, mark)
}

#[test]
fn an_explicit_wait_bounds_an_interactive_search_while_another_process_indexes() {
    let (dir, db) = prefix_and_exact("wait-tty");
    rq(&db, &dir, &["--index", "app"]);
    let (conn, mark) = hold_as_another_indexer(&db, &dir);

    let start = std::time::Instant::now();
    let mut child = rq_cmd(&db, &dir)
        .args(["User", "--wait", "1s"])
        .env("RQ_ASSUME_INTERACTIVE", "1")
        // a warm child would wait out the held writer too; a detached one
        // does that after the search has exited, which is what's timed here
        .envs(NO_CHILD)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run rq");
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let waited = start.elapsed();
    conn.execute_batch("COMMIT").unwrap();
    conn.execute("DELETE FROM meta WHERE key = ?1", rusqlite::params![mark])
        .unwrap();
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // the writer's busy timeout bounds the join after the answer, not the wait
    assert_eq!(status.and_then(|s| s.code()), Some(2), "after {waited:?}");
}

/// A checkout indexed only under `a/`, where a prefix match for `Widget`
/// lives; the exact definition under `z/` is unread.
fn partly_indexed_elsewhere(label: &str) -> (Scratch, PathBuf) {
    let (dir, db) = scratch(label);
    fs::create_dir_all(dir.join("a")).unwrap();
    fs::create_dir_all(dir.join("z")).unwrap();
    fs::write(dir.join("a/widget.rb"), "class WidgetThing\nend\n").unwrap();
    fs::write(dir.join("z/widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index", "a"]);
    (dir, db)
}

/// A second repo, committed, sharing `db`'s index.
fn another_repo(label: &str) -> Scratch {
    let (dir, _) = scratch(label);
    fs::write(dir.join("other.rb"), "class Other\nend\n").unwrap();
    git_init_commit(&dir);
    dir
}

#[test]
fn across_checkouts_a_partial_one_nothing_is_indexing_answers_and_says_so() {
    let (r1, db) = partly_indexed_elsewhere("all-idle");
    let r2 = another_repo("all-idle-here");
    rq(&db, &r2, &["--index"]);

    // the warm this search leaves behind is for r2, not r1, and a retry from
    // here reads no more of r1: an answer, disclosed, not a request to retry
    let (code, out, _) = rq_full(
        &db,
        &r2,
        &["-a", "Widget", "--json"],
        &[("RQ_WARM_DETACH", "1")],
        None,
    );
    assert_eq!(code, 0, "{out}");
    let top = &json(&out)[0];
    assert_eq!(top["name"], "WidgetThing", "{out}");
    assert_eq!(top["warming"]["interrupted"], true, "{out}");
    let hint = top["warming"]["hint"].as_str().unwrap();
    let r1_root = r1.canonicalize().unwrap();
    assert!(
        hint.contains(&format!("rq --index {}", r1_root.display())),
        "names the checkout to index: {out}"
    );
}

#[test]
fn across_checkouts_this_ones_demand_walk_does_not_settle_another_still_indexing() {
    let (r1, db) = partly_indexed_elsewhere("all-busy");
    let r3 = another_repo("all-busy-here");
    let root = r1.canonicalize().unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mark = format!("pass:{}:{}", root.display(), std::process::id());
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, strftime('%s', 'now'))",
        rusqlite::params![mark],
    )
    .unwrap();

    // r3's own walk reads every file of r3 holding the name; r1's `z/` is
    // still to come from the process indexing it, which the search waits on
    let (code, out, _) = rq_full(
        &db,
        &r3,
        &["-a", "Widget", "--json", "--wait", "2"],
        &[],
        None,
    );
    assert_eq!(code, 2, "{out}");
    assert_eq!(json(&out)["provisional"][0]["name"], "WidgetThing", "{out}");
}

#[test]
fn files_read_and_the_tree_count_the_same_files() {
    // an explicit index reads untracked files too; they belong in `of` as
    // much as in `read`, or the unread tracked files vanish from the count
    let (dir, db) = scratch("span-untracked");
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join("models")).unwrap();
    fs::write(
        dir.join("app/fields.rb"),
        "class UserFieldsController\nend\n",
    )
    .unwrap();
    fs::write(dir.join("models/user.rb"), "class User\nend\n").unwrap();
    git_init_commit(&dir);
    for i in 0..3 {
        fs::write(
            dir.join(format!("app/u{i}.rb")),
            format!("class U{i}\nend\n"),
        )
        .unwrap();
    }
    rq(&db, &dir, &["--index", "app"]);

    let (code, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &[], None);
    assert_eq!(code, 2, "{out}");
    let w = &json(&out)["warming"];
    assert_eq!((&w["read"], &w["of"]), (&4.into(), &5.into()), "{out}");
}

#[test]
fn a_tree_never_counted_is_counted_when_asked_and_never_null() {
    // an index an older rq left half-built recorded no span
    let (dir, db) = prefix_and_exact("span-missing");
    rq(&db, &dir, &["--index", "models"]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM meta WHERE key LIKE 'span:%'", [])
        .unwrap();

    // --status counts it as a hit does
    let (_, out) = rq(&db, &dir, &["--status", "--json"]);
    assert_eq!(json(&out)[0]["of"], 2, "{out}");
    conn.execute("DELETE FROM meta WHERE key LIKE 'span:%'", [])
        .unwrap();

    let (code, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 0, "{out}");
    let w = &json(&out)[0]["warming"];
    assert_eq!((&w["read"], &w["of"]), (&1.into(), &2.into()), "{out}");
    // ...once: the count is kept for the next query
    let kept: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM meta WHERE key LIKE 'span:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1);
}

#[test]
fn a_tree_that_cannot_be_counted_backs_no_confidence() {
    // outside git nothing counts the tree short of walking it all
    let (dir, db) = scratch("span-non-git");
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join("models")).unwrap();
    fs::write(
        dir.join("app/fields.rb"),
        "class UserFieldsController\nend\n",
    )
    .unwrap();
    fs::write(dir.join("models/user.rb"), "class User\nend\n").unwrap();
    rq(&db, &dir, &["--index", ".", "--path", "models"]);

    let (code, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &[], None);
    assert_eq!(code, 0, "{out}");
    let top = &json(&out)[0];
    assert!(!out.contains("null"), "omitted, never null: {out}");
    assert!(top["warming"].get("of").is_none(), "{out}");
    assert_eq!(top["confidence"], 0.0, "{out}");
}

#[test]
fn status_reads_a_checkout_a_live_pass_is_filling_as_warming() {
    // a cold pass reads for the name before it writes a file
    let (dir, db) = prefix_and_exact("status-live-pass");
    rq(&db, &dir, &["--index", "app"]);
    let root = dir.canonicalize().unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DELETE FROM coverage; DELETE FROM checkout_files;")
        .unwrap();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, strftime('%s', 'now'))",
        rusqlite::params![format!("pass:{}:{}", root.display(), std::process::id())],
    )
    .unwrap();

    let (code, out, _) = rq_full(&db, &dir, &["--status", "--json"], &[], None);
    assert_eq!(code, 0, "{out}");
    let row = &json(&out)[0];
    assert_eq!(row["status"], "warming", "{out}");
    assert_eq!((&row["files"], &row["of"]), (&0.into(), &2.into()), "{out}");
    let (_, out, _) = rq_full(&db, &dir, &["--status"], &[], None);
    assert!(out.contains("0 of 2 files"), "{out}");
}

#[test]
fn a_warming_miss_says_how_far_the_index_got() {
    let (dir, db) = prefix_and_exact("miss-progress");
    rq(&db, &dir, &["--index", "app"]);

    let (code, out, _) = rq_full(
        &db,
        &dir,
        &["Gadget", "--json", "--no-wait"],
        &NO_CHILD,
        None,
    );
    assert_eq!(code, 2, "{out}");
    let v = json(&out);
    assert_eq!(v["status"], "warming", "{out}");
    assert_eq!(
        (&v["warming"]["read"], &v["warming"]["of"]),
        (&1.into(), &2.into()),
        "{out}"
    );

    let (_, _, err) = rq_full(&db, &dir, &["Gadget", "--no-wait"], &NO_CHILD, None);
    assert!(err.contains("1 of 2 files read"), "{err}");

    // keys in the order a result's `warming` lists them, as one -J stream mixes both
    let (_, out, _) = rq_full(
        &db,
        &dir,
        &["Gadget", "--ndjson", "--no-wait"],
        &NO_CHILD,
        None,
    );
    assert!(
        out.contains("\"warming\":{\"read\":1,\"of\":2,\"interrupted\":"),
        "{out}"
    );
}

#[test]
fn a_live_candidate_is_scaled_as_an_indexed_one_is() {
    // Mid first pass (files held, no coverage yet) `--no-wait` blends a live
    // scan into the index's answer: every candidate is on one scale, the
    // share of the tree the index has read, and says so.
    let (dir, db) = scratch("live-scaled");
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join("models")).unwrap();
    fs::write(dir.join("app/tool.rb"), "class AdminUserTool\nend\n").unwrap();
    fs::write(dir.join("models/user.rb"), "class User\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index", "app"]);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM coverage", [])
        .unwrap();

    for query in ["User", "Usr"] {
        let (_, out, _) = rq_full(
            &db,
            &dir,
            &[query, "--json", "--no-wait", "-l", "5"],
            &NO_CHILD,
            None,
        );
        let v = json(&out);
        let hits = v.as_array().or(v["provisional"].as_array()).expect("hits");
        assert!(
            hits.iter().any(|h| h["source"] == "live"),
            "{query}: the scan answered: {out}"
        );
        for hit in hits {
            let w = &hit["warming"];
            assert_eq!(
                (&w["read"], &w["of"]),
                (&1.into(), &2.into()),
                "{query}: {out}"
            );
            assert!(hit["confidence"].as_f64().unwrap() <= 0.5, "{query}: {out}");
        }
    }
}

#[test]
fn a_pass_past_its_reads_says_it_is_finishing() {
    // `read` stands still while a pass rebuilds its name index and reads
    // commit times: `phase` says why, wherever `warming` is reported
    let (dir, db) = prefix_and_exact("phase");
    rq(&db, &dir, &["--index", "app"]);
    let root = dir.canonicalize().unwrap();
    let me = std::process::id();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, strftime('%s', 'now'))",
        [format!("pass:{}:{me}", root.display())],
    )
    .unwrap();

    // a pass that records no phase (an older rq's) has none to report
    let (_, out, _) = rq_full(
        &db,
        &dir,
        &["Gadget", "--json", "--no-wait"],
        &NO_CHILD,
        None,
    );
    assert!(!out.contains("phase"), "{out}");

    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, 'finishing:' || (strftime('%s', 'now') - 7))",
        [format!("phase:{}:{me}", root.display())],
    )
    .unwrap();
    let finishing = |w: &serde_json::Value, what: &str| {
        assert_eq!(w["phase"], "finishing", "{what}: {w}");
        let secs = w["phase_secs"].as_i64().unwrap_or(-1);
        assert!((7..60).contains(&secs), "{what}: {w}");
    };
    let (code, out, _) = rq_full(
        &db,
        &dir,
        &["Gadget", "--json", "--no-wait"],
        &NO_CHILD,
        None,
    );
    assert_eq!(code, 2, "{out}");
    finishing(&json(&out)["warming"], "a miss");
    let (code, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &NO_CHILD, None);
    assert_eq!(code, 2, "{out}");
    let v = json(&out);
    finishing(&v["warming"], "provisional");
    finishing(&v["provisional"][0]["warming"], "a provisional hit");
    let (code, out, _) = rq_full(
        &db,
        &dir,
        &["UserFieldsController", "--json", "--no-wait"],
        &NO_CHILD,
        None,
    );
    assert_eq!(code, 0, "{out}");
    finishing(&json(&out)[0]["warming"], "a hit");
    let (_, out, _) = rq_full(&db, &dir, &["--status", "--json"], &NO_CHILD, None);
    finishing(&json(&out)[0], "--status");

    // text says what JSON says
    let (_, _, err) = rq_full(&db, &dir, &["Gadget", "--no-wait"], &NO_CHILD, None);
    assert!(err.contains("1 of 2 files read, finishing"), "{err}");
    let (_, out, _) = rq_full(&db, &dir, &["--status"], &NO_CHILD, None);
    assert!(out.contains("1 of 2 files, finishing"), "{out}");
}

#[test]
fn a_provisional_match_has_a_results_shape() {
    let (dir, db) = prefix_and_exact("provisional-shape");
    rq(&db, &dir, &["--index", "app"]);
    let (_, out, _) = rq_full(&db, &dir, &["User", "--json", "--no-wait"], &[], None);
    rq(&db, &dir, &["--index"]);
    let (_, hit, _) = rq_full(&db, &dir, &["UserFieldsController", "--json"], &[], None);

    // keys in the order a result lists them, not sorted
    let keys = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.starts_with("      \"") || l.starts_with("    \""))
            .filter_map(|l| l.trim().split('"').nth(1).map(str::to_string))
            .filter(|k| k != "warming" && k != "features")
            .take(5)
            .collect()
    };
    let provisional = out.split("\"provisional\": [").nth(1).unwrap_or_default();
    assert_eq!(keys(provisional), keys(&hit), "{out}\n{hit}");
}

/// A complete repo holding only a prefix match for `Widget`, and another
/// whose exact `class Widget` is unread while a live process (this test)
/// marks a pass over it.
fn complete_here_partial_elsewhere(label: &str) -> (Scratch, Scratch, PathBuf) {
    let (other, db) = scratch(label);
    fs::create_dir_all(other.join("x")).unwrap();
    fs::create_dir_all(other.join("y")).unwrap();
    fs::write(other.join("x/thing.rb"), "class Thing\nend\n").unwrap();
    fs::write(other.join("y/widget.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&other);
    rq(&db, &other, &["--index", "x"]);
    let here = another_repo(&format!("{label}-here"));
    fs::write(here.join("maker.rb"), "def widget_maker\nend\n").unwrap();
    git_init_commit(&here);
    rq(&db, &here, &["--index"]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mark = format!(
        "pass:{}:{}",
        other.canonicalize().unwrap().display(),
        std::process::id()
    );
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, strftime('%s', 'now'))",
        rusqlite::params![mark],
    )
    .unwrap();
    (other, here, db)
}

#[test]
fn across_checkouts_a_complete_ones_match_waits_on_another_still_indexing() {
    let (other, here, db) = complete_here_partial_elsewhere("all-complete-here");

    let (code, out, _) = rq_full(
        &db,
        &here,
        &["-a", "widget", "--json", "--no-wait"],
        &[],
        None,
    );
    assert_eq!(code, 2, "{out}");
    let v = json(&out);
    assert_eq!(v["provisional"][0]["name"], "widget_maker", "{out}");
    // the checkout that holds the answer back is the one named
    let root = other.canonicalize().unwrap();
    assert_eq!(v["warming"]["interrupted"], false, "{out}");
    let hint = v["warming"]["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("rq --index {}", root.display())),
        "{out}"
    );
    assert!(!hint.contains("this checkout"), "it's another one: {out}");

    let (code, _, err) = rq_full(&db, &here, &["-a", "widget", "--no-wait"], &[], None);
    assert_eq!(code, 2);
    assert!(
        err.contains(&format!("rq --index {}", root.display())),
        "text names it too: {err}"
    );
}

#[test]
fn across_checkouts_a_complete_ones_search_follows_another_indexer() {
    let (other, here, db) = complete_here_partial_elsewhere("all-follow");
    let indexer = {
        let (db, other) = (db.clone(), other.to_path_buf());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(800));
            rq(&db, &other, &["--index"]);
        })
    };

    let (code, out, _) = rq_full(
        &db,
        &here,
        &["-a", "Widget", "--json", "--wait", "20"],
        &[],
        None,
    );
    indexer.join().unwrap();
    assert_eq!(code, 0, "{out}");
    assert_eq!(json(&out)[0]["name"], "Widget", "{out}");
}

#[test]
fn a_warm_child_keeps_going_past_its_budget_while_it_makes_progress() {
    // one file a pass, and a budget a few passes fill: a child that stopped
    // at its budget would leave the checkout part-read with nobody indexing it
    let (dir, db) = scratch("warm-past-budget");
    for i in 0..60 {
        fs::write(dir.join(format!("w{i}.rb")), format!("class W{i}\nend\n")).unwrap();
    }
    git_init_commit(&dir);
    let root = dir.canonicalize().unwrap();
    let (code, _, _) = rq_full(
        &db,
        &dir,
        &["--warm", root.to_str().unwrap()],
        &[("RQ_WARM_BUDGET_MS", "200"), ("RQ_COLLECT_CAP", "1")],
        None,
    );
    assert_eq!(code, 0);
    let (_, out) = rq(&db, &dir, &["--status", "--json"]);
    let rows = json(&out);
    assert_eq!(rows[0]["status"], "complete", "{out}");
    assert_eq!(rows[0]["files"], 60, "{out}");
}

#[test]
fn a_sparse_checkout_spans_only_the_files_it_has() {
    let (dir, db) = scratch("sparse");
    for d in 0..5 {
        fs::create_dir_all(dir.join(format!("d{d}"))).unwrap();
        for f in 0..2 {
            fs::write(
                dir.join(format!("d{d}/w{f}.rb")),
                format!("class W{d}x{f}\nend\n"),
            )
            .unwrap();
        }
    }
    git_init_commit(&dir);
    let git = |args: &[&str]| {
        let out = git_cmd(&dir).args(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&["sparse-checkout", "set", "d0"]);
    // skip-worktree on a file still on disk (hiding local edits) keeps it
    git(&["update-index", "--skip-worktree", "d0/w0.rb"]);
    assert!(!dir.join("d1").exists(), "the cone leaves d1 out");
    rq(&db, &dir, &["--index", "--path", "d0"]);

    let (code, out, _) = rq_full(&db, &dir, &["W0x", "--json", "--no-wait"], &[], None);
    assert!(code == 0 || code == 2, "{out}");
    let v = json(&out);
    let w = if v.is_array() {
        &v[0]["warming"]
    } else {
        &v["warming"]
    };
    assert_eq!((&w["read"], &w["of"]), (&2.into(), &2.into()), "{out}");
}

#[test]
fn an_untracked_file_an_explicit_index_read_survives_a_warm_completing_the_checkout() {
    let (dir, db) = scratch("untracked-kept");
    fs::create_dir_all(dir.join("a")).unwrap();
    fs::create_dir_all(dir.join("b")).unwrap();
    fs::write(dir.join("a/tracked.rb"), "class Tracked\nend\n").unwrap();
    fs::write(dir.join("b/other.rb"), "class Other\nend\n").unwrap();
    git_init_commit(&dir);
    fs::write(dir.join("a/untracked1.rb"), "class Untracked1\nend\n").unwrap();
    fs::write(dir.join("a/gone.rb"), "class Gone\nend\n").unwrap();
    rq(&db, &dir, &["--index", "--path", "a"]);
    fs::remove_file(dir.join("a/gone.rb")).unwrap();

    // a search's warm reads the rest from git's index and completes the checkout
    let (code, out, _) = rq_full(&db, &dir, &["Other", "--json"], &[], None);
    assert_eq!(code, 0, "{out}");
    let (_, status) = rq(&db, &dir, &["--status", "--json"]);
    assert_eq!(json(&status)[0]["status"], "complete", "{status}");

    let (code, out, _) = rq_full(&db, &dir, &["Untracked1", "--json"], &[], None);
    assert_eq!(code, 0, "still on disk, still indexed: {out}");
    let (code, out, _) = rq_full(&db, &dir, &["Gone", "--json"], &[], None);
    assert_eq!(code, 1, "deleted, forgotten: {out}");
}

#[test]
fn an_explicit_index_waits_out_another_writer_holding_the_lock_a_while() {
    // a cold pass's end rebuilds the name index in one transaction, which on
    // a large repo holds the lock past a search's busy timeout
    let (dir, db) = prefix_and_exact("index-waits");
    rq(&db, &dir, &["--index", "app"]);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(4500));
        conn.execute_batch("COMMIT").unwrap();
    });

    let (code, out, err) = rq_full(&db, &dir, &["--index", "--json"], &[], None);
    holder.join().unwrap();
    assert_eq!(code, 0, "{out}{err}");
}

/// Run `rq` at a pretend terminal from `cwd`, sending SIGINT after `interrupt`
/// if given, and killing it past `cap`. (exit code, elapsed, stdout, stderr);
/// no code when it was killed.
fn rq_at_terminal(
    db: &Path,
    cwd: &Path,
    args: &[&str],
    interrupt: Option<std::time::Duration>,
    cap: std::time::Duration,
) -> (Option<i32>, std::time::Duration, String, String) {
    use std::io::Read;
    let start = std::time::Instant::now();
    let mut child = rq_cmd(db, cwd)
        .args(args)
        .env("RQ_ASSUME_INTERACTIVE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run rq");
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            s
        })
    };
    let out = drain(Box::new(child.stdout.take().unwrap()));
    let err = drain(Box::new(child.stderr.take().unwrap()));
    let mut signalled = false;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if !signalled && interrupt.is_some_and(|after| start.elapsed() >= after) {
            // SAFETY: SIGINT to our own child, which `try_wait` just saw running.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
            signalled = true;
        }
        if start.elapsed() > cap {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let elapsed = start.elapsed();
    (
        status.and_then(|s| s.code()),
        elapsed,
        out.join().unwrap(),
        err.join().unwrap(),
    )
}

#[test]
fn across_checkouts_a_terminal_search_shows_the_indexer_it_follows_and_leaves_one_that_stalls() {
    // another process marks a pass over `other` and writes nothing more —
    // stopped, or a crashed pass's mark on a reused pid
    let (other, here, db) = complete_here_partial_elsewhere("all-tty-stall");

    let (code, waited, out, err) = rq_at_terminal(
        &db,
        &here,
        &["-a", "widget"],
        None,
        std::time::Duration::from_secs(25),
    );
    assert_eq!(code, Some(2), "after {waited:?}: {err}");
    assert!(waited < std::time::Duration::from_secs(15), "{waited:?}");
    assert!(out.contains("widget_maker"), "{out}");
    let label = other.file_name().unwrap().to_string_lossy();
    assert!(err.contains(&format!("indexing {label}")), "{err}");
}

#[test]
fn across_checkouts_ctrl_c_at_a_terminal_prints_what_is_known() {
    let (_other, here, db) = complete_here_partial_elsewhere("all-tty-int");

    let (code, waited, out, err) = rq_at_terminal(
        &db,
        &here,
        &["-a", "widget"],
        Some(std::time::Duration::from_millis(1500)),
        std::time::Duration::from_secs(25),
    );
    assert_eq!(code, Some(2), "after {waited:?}: {err}");
    assert!(out.contains("widget_maker"), "{out}");
}

#[test]
fn an_index_blocked_by_another_writer_says_so_and_gives_up_after_one_wait() {
    let (dir, db) = scratch("index-blocked");
    fs::write(dir.join("a.rb"), "class Alpha\nend\n").unwrap();
    git_init_commit(&dir);
    rq(&db, &dir, &["--index"]);
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder
        .execute_batch("BEGIN IMMEDIATE; INSERT OR REPLACE INTO meta VALUES ('held', '1');")
        .unwrap();

    let start = std::time::Instant::now();
    let (code, out, err) = rq_full(
        &db,
        &dir,
        &["--index"],
        &[
            ("RQ_ASSUME_INTERACTIVE", "1"),
            ("RQ_WRITER_WAIT_MS", "2000"),
        ],
        None,
    );
    let waited = start.elapsed();
    holder.execute_batch("COMMIT").unwrap();
    assert_eq!(code, 74, "{out}{err}");
    assert!(err.contains("waiting for another rq"), "{err}");
    // one wait, not one per write
    assert!(waited < std::time::Duration::from_secs(4), "{waited:?}");
}

#[test]
fn every_mode_answers_json() {
    // Each mode in clap's exclusive `mode` group, run with --json: a mode
    // added without structured output fails here, not in a caller's parser.
    // `--warm` is the detached child (no output); `--completions` prints a
    // shell script.
    const NO_OUTPUT: [&str; 2] = ["warm", "completions"];
    let runs: [(&str, &[&str]); 5] = [
        ("index", &["--index"]),
        ("symbols", &["--symbols", "a.rb"]),
        ("status", &["--status"]),
        ("usage", &["--usage"]),
        ("drop", &["--drop"]),
    ];
    let source = include_str!("../src/cli/mod.rs");
    let group = source
        .split("ArgGroup::new(\"mode\")")
        .nth(1)
        .and_then(|rest| rest.split(".args([").nth(1))
        .and_then(|rest| rest.split("])").next())
        .expect("the mode ArgGroup's .args([...])");
    let mut modes: Vec<&str> = group
        .split(',')
        .map(|m| m.trim().trim_matches('"'))
        .filter(|m| !m.is_empty())
        .collect();
    modes.sort_unstable();
    let mut covered: Vec<&str> = runs.iter().map(|(m, _)| *m).chain(NO_OUTPUT).collect();
    covered.sort_unstable();
    assert_eq!(covered, modes, "a mode without a --json run here");

    let (dir, db) = scratch("mode-json");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    git_init_commit(&dir);
    for (mode, args) in runs {
        let (_, out, err) = rq_both(&db, &dir, &[args, &["--json"]].concat());
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap_or_else(|e| panic!("--{mode} --json: {e}: {out:?} {err}"));
    }
}
