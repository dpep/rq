//! The harness every integration test shares: the built `rq`, run hermetically
//! against a database and a directory of the test's own.

#![allow(dead_code, reason = "each test binary uses its own subset")]

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The built binary, run from `cwd` against the database at `db`. Warming
/// stays in-process so no detached child races a test's asserts or cleanup;
/// a test of the production default removes `RQ_WARM_DETACH`.
///
/// The wall-clock budgets are generous: a verdict must not depend on how
/// loaded the machine is. A test that probes a budget sets it again.
pub(crate) fn rq_cmd(db: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rq"));
    scrub_git(&mut cmd);
    cmd.current_dir(cwd)
        .env("RQ_DB", db)
        .env("RQ_WARM_DETACH", "0")
        .env("RQ_FALLBACK_BUDGET_MS", GENEROUS_MS)
        .env("RQ_ANSWER_BUDGET_MS", GENEROUS_MS)
        .env("RQ_DEFERRED_BUDGET_MS", GENEROUS_MS);
    cmd
}

/// A budget no test's tree comes near, but short of hanging a broken run.
const GENEROUS_MS: &str = "20000";

/// `git` in `cwd`, scrubbed like [`rq_cmd`].
pub(crate) fn git_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    scrub_git(&mut cmd);
    cmd.current_dir(cwd);
    cmd
}

/// An inherited `GIT_DIR` and friends (a hook, `git rebase -x`) would point
/// every git call at the outer repo instead of the test's.
fn scrub_git(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
}

/// The exit code: 1 (a miss), 2 (warming) and 64 (usage) are the contract.
pub(crate) fn code(out: &Output) -> i32 {
    out.status.code().expect("rq exited on a signal")
}

/// Run rq: its exit code and stdout.
pub(crate) fn rq(db: &Path, cwd: &Path, args: &[&str]) -> (i32, String) {
    let (code, out, _) = rq_both(db, cwd, args);
    (code, out)
}

/// Run rq: its exit code, stdout and stderr — `--profile` and `-v` report to
/// stderr so stdout stays exactly the machine-readable result.
pub(crate) fn rq_both(db: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = rq_cmd(db, cwd).args(args).output().expect("run rq");
    (
        code(&out),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A fresh temp directory, removed on drop with the database beside it — so
/// a failing assert leaks nothing.
pub(crate) struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// `rq-{label}-{pid}` under the temp dir, emptied first.
    pub(crate) fn new(label: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("rq-{label}-{}", std::process::id()));
        let scratch = Scratch { dir };
        scratch.clean();
        fs::create_dir_all(&scratch.dir).unwrap();
        scratch
    }

    /// The same directory by its real path: macOS's temp dir is behind a
    /// symlink, and git reports roots resolved.
    pub(crate) fn canonical(mut self) -> Scratch {
        self.dir = self.dir.canonicalize().unwrap();
        self
    }

    /// A database path beside the directory, outside any repo in it.
    pub(crate) fn db(&self) -> PathBuf {
        PathBuf::from(format!("{}.db", self.dir.display()))
    }

    fn clean(&self) {
        let _ = fs::remove_dir_all(&self.dir);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.db().display()));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.clean();
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.dir
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.dir
    }
}
