//! The harness every integration test shares: the built `rq`, run hermetically
//! against a database and a directory of the test's own.

#![allow(dead_code, reason = "each test binary uses its own subset")]

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The built binary, run from `cwd` against the database at `db`. Warming
/// runs as in production, detached child and all, except that rq waits for
/// the child (`RQ_WARM_DETACH=wait`), so it never races a test's asserts or
/// cleanup. A test staging an index nobody is filling sets `=0` (no child);
/// one of a truly detached child removes the variable.
///
/// The wall-clock budgets are generous: a verdict must not depend on how
/// loaded the machine is. A test that probes a budget sets it again.
pub(crate) fn rq_cmd(db: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rq"));
    scrub_git(&mut cmd);
    cmd.current_dir(cwd)
        .env("RQ_DB", db)
        .env("RQ_WARM_DETACH", "wait")
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

/// Run rq with extra env and optional piped stdin, killing it after `limit`:
/// its exit code and stdout, or `None` when it hung.
pub(crate) fn rq_bounded(
    db: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&str>,
    limit: std::time::Duration,
) -> Option<(i32, String)> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = rq_cmd(db, cwd)
        .args(args)
        .envs(env.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run rq");
    if let Some(input) = stdin {
        // dropped at the end of the statement: the batch sees EOF
        let _ = child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(input.as_bytes());
    }
    let deadline = std::time::Instant::now() + limit;
    let hung = loop {
        if child.try_wait().expect("poll rq").is_some() {
            break false;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = child.wait_with_output().expect("rq exits");
    (!hung).then(|| {
        (
            code(&out),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
}

/// A FIFO at `path`. On drop it releases any reader still blocked opening it,
/// so a hang the test caught doesn't outlive the test.
pub(crate) struct Fifo(PathBuf);

impl Fifo {
    pub(crate) fn new(path: PathBuf) -> Fifo {
        let ok = Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(ok.success(), "mkfifo {}", path.display());
        Fifo(path)
    }
}

impl Drop for Fifo {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt;
        // non-blocking: succeeds only while a reader waits, which it releases
        for _ in 0..25 {
            let _ = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.0);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// A fresh temp directory, removed on drop with the database beside it — so
/// a failing assert leaks nothing.
pub(crate) struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// `rq-{label}-{pid}-{n}` under the temp dir, emptied first: `n` counts
    /// up per process, so two tests that share a label never share a dir.
    pub(crate) fn new(label: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("rq-{label}-{}-{n}", std::process::id());
        let dir = std::env::temp_dir().join(name);
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
