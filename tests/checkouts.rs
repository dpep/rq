//! Several checkouts of one repo — worktrees, clones of one remote, a detached
//! HEAD — share an identity but not their files. Each must answer from its own
//! tree, and indexing one must not overwrite another.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// A fresh base directory and database for one test.
fn scratch(label: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("rq-co-{}-{label}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();
    let db = base.join("rq.db");
    (base, db)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(["-c", "user.email=t@e.st", "-c", "user.name=test"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
}

/// Run rq with an isolated database, warming in-process. Returns (exit ok, stdout).
fn rq(db: &Path, cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rq"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .current_dir(cwd)
        .env("RQ_DB", db)
        .env("RQ_WARM_DETACH", "0")
        .output()
        .expect("run rq");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The hits of an `--ndjson` search, as `(name, file, root)`.
fn hits(db: &Path, cwd: &Path, query: &str, extra: &[&str]) -> Vec<(String, String, String)> {
    let mut args = vec![query, "--ndjson"];
    args.extend_from_slice(extra);
    let (_, out) = rq(db, cwd, &args);
    out.lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("ndjson"))
        .filter(|v| v.get("name").is_some())
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            (s("name"), s("file"), s("root"))
        })
        .collect()
}

fn names(hits: &[(String, String, String)]) -> Vec<&str> {
    hits.iter().map(|h| h.0.as_str()).collect()
}

const WIDGET: &str = "class Widget\n  def base\n  end\nend\n";
const GADGET: &str = "class Gadget\n  def old_name\n  end\nend\n";

/// A committed repo with a remote, so every checkout of it shares one identity.
fn origin(base: &Path) -> PathBuf {
    let main = base.join("main");
    fs::create_dir_all(&main).unwrap();
    fs::write(main.join("widget.rb"), WIDGET).unwrap();
    fs::write(main.join("gadget.rb"), GADGET).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(
        &main,
        &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
    );
    commit_all(&main, "init");
    main
}

/// Branch A adds `Widget#alpha`; branch B renames `old_name` to `new_name`.
fn diverge(a: &Path, b: &Path) {
    fs::write(
        a.join("widget.rb"),
        "class Widget\n  def base\n  end\n\n  def alpha\n  end\nend\n",
    )
    .unwrap();
    commit_all(a, "alpha");
    fs::write(b.join("gadget.rb"), GADGET.replace("old_name", "new_name")).unwrap();
    commit_all(b, "rename");
}

/// Each checkout answers from its own tree, whichever was indexed last.
fn assert_each_reads_its_own(db: &Path, a: &Path, b: &Path) {
    let root = |d: &Path| d.to_string_lossy().into_owned();
    for _round in 0..2 {
        let found = hits(db, a, "alpha", &["-k", "method"]);
        assert_eq!(names(&found), ["alpha"], "A has alpha: {found:?}");
        assert_eq!(found[0].2, root(a), "A's alpha is read from A");
        let found = hits(db, b, "alpha", &["-k", "method"]);
        assert!(found.is_empty(), "alpha must not leak into B: {found:?}");

        let found = hits(db, b, "new_name", &[]);
        assert_eq!(names(&found), ["new_name"], "B has new_name: {found:?}");
        assert_eq!(found[0].2, root(b));
        let found = hits(db, a, "new_name", &[]);
        assert!(found.is_empty(), "new_name must not leak into A: {found:?}");
        let found = hits(db, a, "old_name", &[]);
        assert_eq!(names(&found), ["old_name"], "A keeps old_name: {found:?}");
    }
}

#[test]
fn worktrees_each_answer_from_their_own_branch() {
    let (base, db) = scratch("worktrees");
    let main = origin(&base);
    let (a, b) = (base.join("wt-a"), base.join("wt-b"));
    git(
        &main,
        &["worktree", "add", "-q", "-b", "a", a.to_str().unwrap()],
    );
    git(
        &main,
        &["worktree", "add", "-q", "-b", "b", b.to_str().unwrap()],
    );
    diverge(&a, &b);

    // index A, then B: B's pass must not overwrite what A indexed
    assert!(rq(&db, &a, &["--index"]).0);
    assert!(rq(&db, &b, &["--index"]).0);
    assert_each_reads_its_own(&db, &a, &b);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn clones_of_one_remote_each_answer_from_their_own_files() {
    let (base, db) = scratch("clones");
    let main = origin(&base);
    let (a, b) = (base.join("clone-a"), base.join("clone-b"));
    for dir in [&a, &b] {
        git(
            &base,
            &["clone", "-q", main.to_str().unwrap(), dir.to_str().unwrap()],
        );
        // the clones' origin is `main`'s path; point both at the shared remote
        git(
            dir,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:acme/widgets.git",
            ],
        );
    }
    diverge(&a, &b);

    assert!(rq(&db, &a, &["--index"]).0);
    assert!(rq(&db, &b, &["--index"]).0);
    assert_each_reads_its_own(&db, &a, &b);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_detached_checkout_answers_from_its_commit() {
    let (base, db) = scratch("detached");
    let main = origin(&base);
    let init = git(&main, &["rev-parse", "HEAD"]).trim().to_string();
    fs::write(
        main.join("widget.rb"),
        "class Widget\n  def base\n  end\n\n  def alpha\n  end\nend\n",
    )
    .unwrap();
    commit_all(&main, "alpha");
    let old = base.join("old");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            old.to_str().unwrap(),
            &init,
        ],
    );

    assert!(rq(&db, &main, &["--index"]).0);
    assert!(rq(&db, &old, &["--index"]).0);
    let found = hits(&db, &main, "alpha", &["-k", "method"]);
    assert_eq!(names(&found), ["alpha"], "main has alpha: {found:?}");
    let found = hits(&db, &old, "alpha", &["-k", "method"]);
    assert!(found.is_empty(), "the old commit has no alpha: {found:?}");

    let _ = fs::remove_dir_all(&base);
}

/// Two diverged worktrees, both indexed, and a third on `main` holding only
/// content its siblings already hold.
fn three_worktrees(label: &str) -> (PathBuf, PathBuf, [PathBuf; 3]) {
    let (base, db) = scratch(label);
    let main = origin(&base);
    let wt = |name: &str, branch: &str| {
        let dir = base.join(name);
        git(
            &main,
            &["worktree", "add", "-q", "-b", branch, dir.to_str().unwrap()],
        );
        dir
    };
    let (a, b, c) = (wt("wt-a", "a"), wt("wt-b", "b"), wt("wt-c", "c"));
    diverge(&a, &b);
    assert!(rq(&db, &a, &["--index"]).0);
    assert!(rq(&db, &b, &["--index"]).0);
    (base, db, [a, b, c])
}

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

#[test]
fn a_new_worktree_parses_nothing_its_siblings_hold() {
    let (base, db, [a, _, c]) = three_worktrees("shared");
    // C's widget.rb is B's and its gadget.rb is A's: both versions are held
    let (ok, out) = rq(&db, &c, &["--index", "--json"]);
    assert!(ok, "{out}");
    let index = json(&out);
    assert_eq!(index["files"], 2, "{out}");
    assert_eq!(index["symbols_added"], 0, "nothing parsed: {out}");
    assert_eq!(index["root"], c.to_string_lossy().as_ref(), "{out}");
    assert_eq!(names(&hits(&db, &c, "old_name", &[])), ["old_name"]);
    assert!(hits(&db, &c, "alpha", &["-k", "method"]).is_empty());
    // and A is untouched by C's pass
    assert_eq!(names(&hits(&db, &a, "alpha", &["-k", "method"])), ["alpha"]);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn all_repos_folds_a_definition_the_checkouts_share() {
    let (base, db, [a, b, _]) = three_worktrees("all");
    let root = |d: &Path| d.to_string_lossy().into_owned();
    // `Widget` is at widget.rb:1 in both, though the files differ: one hit,
    // from the checkout asked in
    let found = hits(&db, &b, "Widget", &["-a", "-k", "class"]);
    assert_eq!(names(&found), ["Widget"], "{found:?}");
    assert_eq!(found[0].2, root(&b));
    let found = hits(&db, &a, "Widget", &["-a", "-k", "class"]);
    assert_eq!(found[0].2, root(&a));
    // what only one checkout defines, `-a` finds there
    let found = hits(&db, &b, "alpha", &["-a", "-k", "method"]);
    assert_eq!(names(&found), ["alpha"]);
    assert_eq!(found[0].2, root(&a));

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn status_and_drop_name_each_checkout() {
    let (base, db, [a, b, c]) = three_worktrees("status");
    assert!(rq(&db, &c, &["--index"]).0);
    let (ok, out) = rq(&db, &a, &["--status", "--json"]);
    assert!(ok, "{out}");
    let rows = json(&out);
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 3, "one row per checkout: {out}");
    for (row, dir) in rows.iter().zip([&a, &b, &c]) {
        assert_eq!(row["repo"], "github.com/acme/widgets");
        assert_eq!(row["root"], dir.to_string_lossy().as_ref());
        assert_eq!(row["status"], "complete");
        assert_eq!(row["files"], 2);
    }

    // dropping C leaves A and B, and every version they map
    let (ok, out) = rq(&db, &c, &["--drop", "--json"]);
    assert!(ok, "{out}");
    let dropped = json(&out);
    assert_eq!(dropped["dropped"], true);
    assert_eq!(dropped["root"], c.to_string_lossy().as_ref());
    let (_, out) = rq(&db, &a, &["--status", "--json"]);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(2), "{out}");
    assert_each_reads_its_own(&db, &a, &b);

    // dropping the repo by name drops every checkout of it
    let (ok, out) = rq(&db, &a, &["--drop", "github.com/acme/widgets", "--json"]);
    assert!(ok, "{out}");
    let (_, out) = rq(&db, &a, &["--status", "--json"]);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(0), "{out}");

    let _ = fs::remove_dir_all(&base);
}

/// `n` files each defining `perform_task`, in `k` worktrees, every one indexed.
fn crowded(label: &str, n: usize, k: usize) -> (PathBuf, PathBuf, Vec<PathBuf>) {
    let (base, db) = scratch(label);
    let main = base.join("main");
    fs::create_dir_all(main.join("jobs")).unwrap();
    for i in 0..n {
        fs::write(
            main.join(format!("jobs/job_{i}.rb")),
            format!("class Job{i}\n  def perform_task\n  end\nend\n"),
        )
        .unwrap();
    }
    git(&main, &["init", "-q", "-b", "main"]);
    git(
        &main,
        &["remote", "add", "origin", "git@github.com:acme/jobs.git"],
    );
    commit_all(&main, "init");
    let mut roots = vec![main.clone()];
    for i in 1..k {
        let dir = base.join(format!("wt-{i}"));
        git(
            &main,
            &["worktree", "add", "-q", "--detach", dir.to_str().unwrap()],
        );
        roots.push(dir);
    }
    for dir in &roots {
        assert!(rq(&db, dir, &["--index"]).0);
    }
    (base, db, roots)
}

#[test]
fn all_repos_counts_each_definition_once_however_many_checkouts_hold_it() {
    // more rows than the candidate cap once multiplied by the checkouts
    let (n, k) = (2000, 5);
    let (base, db, roots) = crowded("crowded", n, k);
    let (ok, out) = rq(
        &db,
        &roots[1],
        &["perform_task", "-a", "--limit", "0", "--ndjson"],
    );
    assert!(ok);
    let rows: Vec<Value> = out.lines().map(json).collect();
    assert_eq!(rows.len(), n, "every definition, once");
    assert_eq!(rows[0]["total"], n);
    let root = roots[1].to_string_lossy();
    assert!(
        rows.iter().all(|r| r["root"] == root.as_ref()),
        "read from the checkout asked in"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_checkout_gone_from_disk_is_forgotten() {
    let (base, db, [a, b, c]) = three_worktrees("gone");
    // C, the newest, shares A's gadget.rb; then it's deleted without a word
    assert!(rq(&db, &c, &["--index"]).0);
    fs::remove_dir_all(&c).unwrap();
    let elsewhere = base.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let root = |d: &Path| d.to_string_lossy().into_owned();

    let found = hits(&db, &elsewhere, "old_name", &["-a"]);
    assert_eq!(names(&found), ["old_name"], "{found:?}");
    assert_eq!(found[0].2, root(&a), "read from a checkout that exists");

    let (ok, out) = rq(&db, &elsewhere, &["--status", "--json"]);
    assert!(ok, "{out}");
    let rows = json(&out);
    let roots: Vec<&str> = rows
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["root"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(roots, [root(&a), root(&b)], "{out}");

    // the last checkout of a repo takes its name index with it
    fs::remove_dir_all(&a).unwrap();
    fs::remove_dir_all(&b).unwrap();
    let (_, out) = rq(&db, &elsewhere, &["--status", "--json"]);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(0), "{out}");
    assert!(hits(&db, &elsewhere, "old_name", &["-a"]).is_empty());
    let (ok, out) = rq(
        &db,
        &elsewhere,
        &["--drop", "github.com/acme/widgets", "--json"],
    );
    assert!(ok, "{out}");
    assert_eq!(json(&out)["dropped"], false, "nothing left to drop: {out}");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn all_repos_folds_a_definition_the_rest_of_its_file_moved() {
    let (base, db, [a, b, _]) = three_worktrees("moved");
    // A's `alpha` pushed `base` nowhere, but B grows a comment above `Gadget`
    fs::write(
        b.join("gadget.rb"),
        format!("# a note\n\n{}", GADGET.replace("old_name", "new_name")),
    )
    .unwrap();
    commit_all(&b, "note");
    assert!(rq(&db, &b, &["--index"]).0);
    let elsewhere = base.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let found = hits(&db, &elsewhere, "Gadget", &["-a", "-k", "class"]);
    assert_eq!(names(&found), ["Gadget"], "one Gadget: {found:?}");
    let found = hits(&db, &a, "Gadget", &["-a", "-k", "class"]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].2, a.to_string_lossy(), "the checkout asked in");

    let _ = fs::remove_dir_all(&base);
}
