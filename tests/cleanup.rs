//! The store hands its free pages back after indexing, in the background,
//! and never on a query's path (DECISIONS D65).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Scratch, git_cmd, rq, rq_both, rq_cmd};

/// A directory holding `repos` git repos, each one class, and the database.
fn repos(label: &str, repos: &[&str]) -> Scratch {
    let dir = Scratch::new(&format!("cleanup-{label}"));
    for name in repos {
        let repo = dir.join(name);
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("widget.rb"), format!("class Widget{name}\nend\n")).unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "-A"],
            &[
                "-c",
                "user.email=t@e.st",
                "-c",
                "user.name=test",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            assert!(git_cmd(&repo).args(args).status().unwrap().success());
        }
    }
    dir
}

/// Leave 20 MB of free pages in the file, as a requeue does, and clear the
/// stamp a new store gets, so a look is due.
fn bloat(db: &Path) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE junk (b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20)
             INSERT INTO junk SELECT zeroblob(1048576) FROM n;
             DROP TABLE junk;
             UPDATE meta SET value = '0' WHERE key = 'cleanup:checked';
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
}

fn storage(db: &Path, cwd: &Path) -> serde_json::Value {
    let (code, out) = rq(db, cwd, &["--storage", "--json"]);
    assert_eq!(code, 0, "{out}");
    serde_json::from_str(&out).unwrap()
}

const MB: i64 = 1 << 20;

#[test]
fn an_index_pass_compacts_the_store_and_a_query_does_not() {
    let dir = repos("trigger", &["a", "b"]);
    let db = dir.db();
    assert_eq!(rq(&db, &dir.join("a"), &["--index"]).0, 0);
    bloat(&db);

    // a hit on a complete index, and a cold repo a query indexes in-process
    assert_eq!(rq(&db, &dir.join("a"), &["Widgeta"]).0, 0);
    assert_eq!(rq(&db, &dir.join("b"), &["Widgetb"]).0, 0);
    let before = storage(&db, &dir);
    assert!(
        before["free_bytes"].as_i64().unwrap() >= 20 * MB,
        "{before}"
    );
    assert!(before["cleanup"]["ran_at"].is_null(), "{before}");

    // the harness waits for the detached cleanup, as it does for a warm
    assert_eq!(rq(&db, &dir.join("a"), &["--index"]).0, 0);
    let after = storage(&db, &dir);
    assert_eq!(after["free_bytes"], 0, "{after}");
    assert!(after["cleanup"]["ran_at"].is_i64(), "{after}");
    let back = after["cleanup"]["reclaimed_bytes"].as_i64().unwrap();
    assert!(back >= 20 * MB, "{after}");
    let file = fs::metadata(&db).unwrap().len() as i64;
    assert!(
        file <= after["bytes"].as_i64().unwrap() + 8192,
        "{file} {after}"
    );

    let (_, status) = rq(&db, &dir.join("a"), &["--status"]);
    assert!(status.contains("MB back to the disk"), "{status}");
}

/// Beside a newer rq's store, this rq writes a side store, and the cleanup
/// compacts that one: the store the pass wrote, named, not looked up again.
#[test]
#[allow(clippy::disallowed_methods, reason = "a database file the test wrote")]
fn a_cleanup_compacts_the_side_store_the_pass_wrote() {
    let dir = repos("side", &["a"]);
    let db = dir.db();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "PRAGMA journal_mode = WAL; CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             PRAGMA user_version = 999;",
        )
        .unwrap();
    let newer = fs::read(&db).unwrap();
    let (code, _, err) = rq_both(&db, &dir.join("a"), &["--index"]);
    assert_eq!(code, 0, "{err}");
    let side = PathBuf::from(storage(&db, &dir)["file"].as_str().unwrap());
    assert_ne!(side, db, "a side store");
    bloat(&side);

    assert_eq!(rq(&db, &dir.join("a"), &["--index"]).0, 0);
    let after = storage(&db, &dir);
    assert_eq!(after["free_bytes"], 0, "{after}");
    assert!(after["cleanup"]["reclaimed_bytes"].as_i64().unwrap() >= 20 * MB);
    assert_eq!(
        fs::read(&db).unwrap(),
        newer,
        "the newer rq's store untouched"
    );
}

/// A cleanup holds no mark: `--drop` isn't refused for it, and `--status` and
/// `--storage` read the store as they always do while it runs — the run not
/// yet recorded, beside the pages not yet handed back.
#[test]
fn a_drop_and_a_status_mid_cleanup_answer_as_usual() {
    let dir = repos("mid", &["a", "b"]);
    let db = dir.db();
    for repo in ["a", "b"] {
        assert_eq!(rq(&db, &dir.join(repo), &["--index"]).0, 0);
    }
    bloat(&db);
    let gate = dir.join("gate");
    fs::write(&gate, "").unwrap();
    let mut cleanup = rq_cmd(&db, &dir)
        .args(["--cleanup", db.to_str().unwrap()])
        .env("RQ_TEST_CLEANUP_HOLD", &gate)
        .spawn()
        .unwrap();
    let held = dir.join("gate.held");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !held.try_exists().unwrap() {
        assert!(
            Instant::now() < deadline,
            "the cleanup never reached its hold"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let mid = storage(&db, &dir);
    assert!(mid["free_bytes"].as_i64().unwrap() > 0, "{mid}");
    assert!(mid["cleanup"]["ran_at"].is_null(), "{mid}");
    let (code, out) = rq(&db, &dir.join("b"), &["--drop", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["dropped"],
        true
    );
    let (code, out) = rq(&db, &dir, &["--status", "--json"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    fs::remove_file(&gate).unwrap();
    assert!(cleanup.wait().unwrap().success());
    let after = storage(&db, &dir);
    assert_eq!(
        after["free_bytes"], 0,
        "the dropped checkout's pages too: {after}"
    );
    assert!(after["cleanup"]["ran_at"].is_i64(), "{after}");
    assert_eq!(rq(&db, &dir.join("a"), &["Widgeta"]).0, 0);
}

/// An older store's one full rewrite holds the write lock for as long as it
/// takes, which on a large store outlasts a search's 3 s wait: a drop waits
/// it out, as `--index` does, rather than failing "locked".
#[test]
fn a_drop_waits_out_a_long_writer() {
    let dir = repos("long-writer", &["a"]);
    let db = dir.db();
    assert_eq!(rq(&db, &dir.join("a"), &["--index"]).0, 0);
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3500));
        writer.execute_batch("ROLLBACK").unwrap();
    });
    let (code, out) = rq(&db, &dir.join("a"), &["--drop", "--json"]);
    holder.join().unwrap();
    assert_eq!(code, 0, "{out}");
}
