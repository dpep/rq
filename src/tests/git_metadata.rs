//! `index::git_head` / `index::branch_changed_files` read `.git` directly
//! instead of forking git — worth ~30 ms a query, and only correct if they
//! agree with git itself. `git rev-parse` is the oracle.
//!
//! Moved here from `tests/cli_e2e.rs`: it never ran the binary, so it was an
//! integration test only in the sense that it lived in `tests/` — and being
//! there was the last reason `index` had to be public.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::index;
use crate::tests::support::Scratch;

fn scratch(label: &str) -> Scratch {
    Scratch::new(label)
}

fn git(dir: &Path, args: &[&str]) {
    let _ = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .current_dir(dir)
        .output();
}

fn rev_parse(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn git_metadata_read_from_disk_matches_git() {
    let dir = scratch("gitmeta");
    fs::write(dir.join("a.rb"), "class A\nend\n").unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "init",
        ],
    );

    // loose ref: .git/refs/heads/<branch>
    let expected = rev_parse(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        index::git_head(&dir).as_deref(),
        Some(expected.as_str()),
        "loose ref"
    );

    // packed: `git pack-refs` moves it into .git/packed-refs
    git(&dir, &["pack-refs", "--all"]);
    assert_eq!(
        index::git_head(&dir).as_deref(),
        Some(expected.as_str()),
        "packed ref"
    );

    // detached HEAD holds the commit itself
    git(&dir, &["checkout", "-q", "--detach"]);
    assert_eq!(
        index::git_head(&dir).as_deref(),
        Some(expected.as_str()),
        "detached HEAD"
    );
    // and a detached HEAD has no branch, so there are no branch files
    assert!(index::branch_changed_files(&dir).is_empty());
}

#[test]
fn an_unborn_head_is_a_state_of_its_own() {
    let dir = scratch("unborn");
    assert_eq!(index::head_state(&dir), None, "not a repo yet");
    git(&dir, &["init", "-q"]);
    let unborn = index::head_state(&dir);
    assert!(
        unborn.is_some(),
        "a repo with no commits still has a HEAD state"
    );
    assert_eq!(index::git_head(&dir), None, "but no commit to hand git");

    fs::write(dir.join("a.rb"), "class A\nend\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "init",
        ],
    );
    assert_eq!(index::head_state(&dir), index::git_head(&dir));
    assert_ne!(index::head_state(&dir), unborn, "the first commit moves it");
}

#[test]
fn a_linked_worktree_reads_its_git_state_from_disk_too() {
    // `.git` is a file there, naming a dir that holds HEAD and the index; refs
    // live in the common dir. Git is the oracle.
    let base = scratch("gitmeta-worktree");
    let main = base.join("main");
    fs::create_dir_all(&main).unwrap();
    fs::write(main.join("a.rb"), "class A\nend\n").unwrap();
    let commit = |dir: &Path| {
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@e.st",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "c",
            ],
        );
    };
    git(&main, &["init", "-q", "-b", "main"]);
    commit(&main);
    let wt = base.join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    fs::write(wt.join("b.rb"), "class B\nend\n").unwrap();
    commit(&wt);

    let expected = rev_parse(&wt, &["rev-parse", "HEAD"]);
    assert_eq!(
        index::git_head(&wt).as_deref(),
        Some(expected.as_str()),
        "loose ref"
    );
    git(&main, &["pack-refs", "--all"]);
    assert_eq!(
        index::git_head(&wt).as_deref(),
        Some(expected.as_str()),
        "packed ref"
    );
    assert_ne!(
        index::git_head(&main),
        index::git_head(&wt),
        "each its own HEAD"
    );

    // its branch diffs against the trunk the common dir holds, and its state
    // can be stamped, so the caches keyed on it can hit
    assert_eq!(index::branch_changed_files(&wt), ["b.rb"]);
    assert!(index::branch_files_stamp(&wt).is_some());
    assert!(index::git_state_stamp(&wt, &expected).is_some());
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn branch_files_are_named_as_the_index_keys_them() {
    // git quotes a non-ASCII name unless asked for `-z`
    let dir = scratch("branch-names");
    let commit = |dir: &Path| {
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@e.st",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "c",
            ],
        );
    };
    fs::write(dir.join("a.rb"), "class A\nend\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    commit(&dir);
    git(&dir, &["checkout", "-qb", "feature"]);
    fs::write(dir.join("café.rb"), "class Cafe\nend\n").unwrap();
    commit(&dir);
    fs::write(dir.join("a.rb"), "class A\n  def b; end\nend\n").unwrap();

    let mut files = index::branch_changed_files(&dir);
    files.sort();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(files, ["a.rb", "café.rb"]);
}

#[test]
fn a_pass_its_search_cut_short_leaves_commit_times_to_the_warm() {
    let dir = scratch("cut-short-git-ts");
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    fs::write(dir.join("b.rb"), "class Gadget\nend\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=test",
            "commit",
            "-qm",
            "init",
        ],
    );
    let mut store = crate::store::Store::open_in_memory().unwrap();
    let key = dir.canonicalize().unwrap().to_string_lossy().into_owned();
    let budget = std::time::Duration::from_secs(5);
    // the names whose definitions carry a commit time
    let dated = |store: &crate::store::Store| -> Vec<String> {
        ["Gadget", "Widget"]
            .into_iter()
            .filter(|&name| {
                let probe = crate::search::Probe::new(name);
                store
                    .search_candidates(name, 10, false, None, None, &probe)
                    .unwrap()
                    .iter()
                    .any(|c| c.git_ts.is_some())
            })
            .map(String::from)
            .collect()
    };

    // the search answered before the walk began: only the active file is read
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let demanded = std::sync::atomic::AtomicBool::new(false);
    let stats = index::index_budgeted_cancellable(
        &mut store,
        &dir,
        &["a.rb".to_string()],
        budget,
        Some("Widget"),
        &cancel,
        &demanded,
    )
    .unwrap();
    assert_eq!(stats.files_indexed, 1, "{stats:?}");
    let checkout = store.checkout(&key).unwrap().unwrap();
    assert_eq!(store.git_ts_head(checkout.id).unwrap(), None);
    assert!(dated(&store).is_empty());

    // the warm that completes the checkout records them
    index::index_budgeted(&mut store, &dir, &[], budget, None).unwrap();
    let head = rev_parse(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(store.git_ts_head(checkout.id).unwrap(), Some(head));
    assert_eq!(dated(&store), ["Gadget", "Widget"]);
}
