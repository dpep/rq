//! A database rq can't use as it is — damaged, half-upgraded, or a newer rq's —
//! still leaves the command working (DECISIONS D51).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A repo with one class, and a database path in a directory of its own:
/// set-aside copies and side stores land beside the database.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("rq-recovery-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::write(dir.join("repo/widget.rb"), "class Widget\nend\n").unwrap();
        Scratch { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("rq.db")
    }

    fn rq(&self, args: &[&str]) -> Output {
        rq_command(&self.db(), &self.dir.join("repo"), args)
            .output()
            .expect("run rq")
    }

    /// Files in the database's directory whose names contain `part`.
    fn beside(&self, part: &str) -> Vec<String> {
        fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.contains(part) && !n.ends_with("-wal") && !n.ends_with("-shm"))
            .collect()
    }

    fn user_version(&self) -> i64 {
        rusqlite::Connection::open(self.db())
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn rq_command(db: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rq"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .current_dir(cwd)
        .env("RQ_DB", db)
        .env("RQ_WARM_DETACH", "0");
    cmd
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A search answers with its normal JSON, whatever the database went through.
fn finds_widget(s: &Scratch) -> Output {
    let out = s.rq(&["Widget", "--json"]);
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("stdout is JSON: {out:?}"));
    assert!(json.get("error").is_none(), "not an error: {json}");
    assert!(
        json.to_string().contains("widget.rb"),
        "the search answered: {json}"
    );
    out
}

fn indexed(s: &Scratch) {
    let out = s.rq(&["--index"]);
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn a_file_that_is_not_a_database_is_rebuilt() {
    let s = Scratch::new("garbage");
    fs::write(s.db(), "not a database ".repeat(1000)).unwrap();
    let out = finds_widget(&s);
    let err = stderr(&out);
    assert_eq!(err.matches("couldn't be read").count(), 1, "{err}");
    assert!(err.contains("rq.db.broken-"), "names the old file: {err}");
    assert_eq!(s.beside(".broken-").len(), 1);
    // recovered: the next run says nothing
    assert!(!stderr(&finds_widget(&s)).contains("couldn't"));
    let status = String::from_utf8_lossy(&s.rq(&["--status"]).stdout).into_owned();
    assert!(status.contains("rq.db.broken-"), "{status}");
}

#[test]
fn a_truncated_database_is_rebuilt() {
    let s = Scratch::new("truncated");
    indexed(&s);
    let len = fs::metadata(s.db()).unwrap().len();
    fs::File::options()
        .write(true)
        .open(s.db())
        .unwrap()
        .set_len(len / 2)
        .unwrap();
    let err = stderr(&finds_widget(&s));
    assert!(err.contains("couldn't be read"), "{err}");
}

#[test]
fn an_upgrade_that_fails_is_rebuilt() {
    let s = Scratch::new("upgrade");
    indexed(&s);
    // an old version number over a schema its steps can't apply to
    rusqlite::Connection::open(s.db())
        .unwrap()
        .execute_batch("DROP TABLE checkout_files; PRAGMA user_version = 21;")
        .unwrap();
    let err = stderr(&finds_widget(&s));
    assert!(err.contains("couldn't be upgraded (from v21"), "{err}");
    assert_eq!(s.beside(".broken-").len(), 1);
}

#[test]
fn a_newer_rqs_database_is_left_alone_and_both_keep_working() {
    let s = Scratch::new("newer");
    indexed(&s);
    rusqlite::Connection::open(s.db())
        .unwrap()
        .execute_batch(
            "PRAGMA user_version = 999; \
             INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_by', '9.9.9');",
        )
        .unwrap();
    let newer = fs::read(s.db()).unwrap();
    let err = stderr(&finds_widget(&s));
    assert!(err.contains("written by a newer rq (9.9.9)"), "{err}");
    assert!(err.contains("rq.v"), "names its own index: {err}");
    // said once; the newer rq's database is untouched throughout
    for _ in 0..2 {
        let err = stderr(&finds_widget(&s));
        assert!(!err.contains("newer"), "{err}");
    }
    assert_eq!(s.user_version(), 999);
    assert_eq!(fs::read(s.db()).unwrap(), newer);
    assert!(s.beside(".broken-").is_empty());
    let status = s.rq(&["--status", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(rows.is_array(), "the usual rows: {rows}");
    let status = String::from_utf8_lossy(&s.rq(&["--status"]).stdout).into_owned();
    assert!(status.contains("belongs to a newer rq"), "{status}");
}

#[test]
fn concurrent_commands_on_a_broken_database_rebuild_it_once() {
    let s = Scratch::new("concurrent");
    fs::write(s.db(), "not a database ".repeat(1000)).unwrap();
    let children: Vec<_> = (0..6)
        .map(|_| {
            rq_command(&s.db(), &s.dir.join("repo"), &["--status", "--json"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut told = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let _: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
        told += stderr(&out).matches("couldn't be read").count();
    }
    assert_eq!(told, 1, "one process set it aside and said so");
    assert_eq!(s.beside(".broken-").len(), 1);
    finds_widget(&s);
}
