//! A `--drop` fired while an `--index` is still enumerating the tree sees the
//! pass and refuses (D64). A `git` shim on the index's `PATH` holds it inside
//! its enumeration (`git ls-files`) until the drop has answered, so the
//! window is deterministic rather than a few milliseconds wide.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Scratch, code, git_cmd, rq_both, rq_cmd};

/// Lets the held `git ls-files` go on however the test ends.
struct Gate(PathBuf);

impl Gate {
    fn entered(&self) -> bool {
        self.0.join("entered").try_exists().unwrap()
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release"), "");
    }
}

fn shim(bin: &Path) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join("git");
    fs::write(
        &path,
        r#"#!/bin/sh
case " $* " in
  *" ls-files "*)
    if [ ! -e "$RQ_TEST_GATE/entered" ]; then
      : > "$RQ_TEST_GATE/entered"
      while [ ! -e "$RQ_TEST_GATE/release" ]; do sleep 0.01; done
    fi ;;
esac
PATH="$RQ_TEST_REAL_PATH" exec git "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_drop_while_an_index_enumerates_is_refused() {
    let scratch = Scratch::new("drop-enum").canonical();
    let dir = scratch.join("repo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.rb"), "class Widget\nend\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "c",
        ],
    ] {
        assert!(git_cmd(&dir).args(args).status().unwrap().success());
    }
    let db = scratch.db();
    let bin = scratch.join("bin");
    shim(&bin);
    let gate = Gate(scratch.join("gate"));
    fs::create_dir_all(&gate.0).unwrap();
    let real = std::env::var("PATH").unwrap_or_default();

    let mut index = rq_cmd(&db, &dir)
        .args(["--index"])
        .env("PATH", format!("{}:{real}", bin.display()))
        .env("RQ_TEST_REAL_PATH", &real)
        .env("RQ_TEST_GATE", &gate.0)
        .env("RQ_WARM_DETACH", "0")
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !gate.entered() {
        assert!(
            index.try_wait().unwrap().is_none(),
            "the index finished without enumerating through git"
        );
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "never enumerated"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let (dropped, out, err) = rq_both(&db, &dir, &["--drop"]);
    drop(gate);
    let status = index.wait().unwrap();
    assert_eq!(
        dropped, 75,
        "the enumerating pass holds the checkout: {out}{err}"
    );
    assert!(status.success(), "the index finishes undisturbed");

    let out = rq_cmd(&db, &dir).args(["--drop"]).output().unwrap();
    assert_eq!(code(&out), 0, "and once it has, the drop goes ahead");
}
