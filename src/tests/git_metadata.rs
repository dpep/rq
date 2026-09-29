//! `index::git_head` / `index::branch_changed_files` read `.git` directly
//! instead of forking git — worth ~30 ms a query, and only correct if they
//! agree with git itself. `git rev-parse` is the oracle.
//!
//! Moved here from `tests/cli_e2e.rs`: it never ran the binary, so it was an
//! integration test only in the sense that it lived in `tests/` — and being
//! there was the last reason `index` had to be public.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::index;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rq-{}-{label}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
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
    let _ = fs::remove_dir_all(&dir);
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
