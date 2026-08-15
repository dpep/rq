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
    let _ = Command::new("git").args(args).current_dir(dir).output();
}

fn rev_parse(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
