//! Opening a database that can't be used as it is (DECISIONS D51).
//!
//! The index is a cache, so a damaged file, a failed upgrade or a schema this
//! rq can't read is set aside and rebuilt rather than reported until someone
//! deletes it by hand. A database a *newer* rq wrote is never touched: this rq
//! keeps an index of its own beside it, so two installed versions don't take
//! turns wiping each other's.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, ffi};

use super::schema::Ladder;
use super::{Result, Store, UPGRADE_WAIT};

/// Why [`Store::init`] couldn't hand back a store.
pub(super) enum Refusal {
    /// Written by a newer rq, at this schema version.
    Newer(i64),
    /// Damaged, or an upgrade that failed: set it aside and start over.
    Broken { upgrading: bool, reason: String },
    /// Anything a fresh file wouldn't fix (busy, disk full, permissions).
    Failed(rusqlite::Error),
}

impl From<rusqlite::Error> for Refusal {
    fn from(e: rusqlite::Error) -> Refusal {
        if damaged(&e) {
            Refusal::Broken {
                upgrading: false,
                reason: short(&e),
            }
        } else {
            Refusal::Failed(e)
        }
    }
}

impl Refusal {
    /// A failed upgrade step: whatever it hit, short of the environment.
    pub(super) fn upgrade(from: i64, e: rusqlite::Error) -> Refusal {
        if environmental(&e) {
            return Refusal::Failed(e);
        }
        Refusal::Broken {
            upgrading: true,
            reason: format!("from v{from}: {}", short(&e)),
        }
    }
}

/// The file itself is unreadable as a database.
fn damaged(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase))
}

/// A failure a new file would meet too, so moving this one aside can't help.
fn environmental(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(
        f.code,
        ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::DiskFull
            | ErrorCode::SystemIoFailure
            | ErrorCode::OutOfMemory
            | ErrorCode::ReadOnly
            | ErrorCode::CannotOpen
            | ErrorCode::PermissionDenied
            | ErrorCode::OperationInterrupted
            | ErrorCode::NoLargeFileSupport
    ))
}

/// SQLite's message without the SQL it came from.
fn short(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqliteFailure(_, Some(msg)) => msg.clone(),
        rusqlite::Error::SqliteFailure(f, None) => f.to_string(),
        rusqlite::Error::SqlInputError { msg, .. } => msg.clone(),
        other => other.to_string(),
    }
}

fn failure(code: std::os::raw::c_int, message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(ffi::Error::new(code), Some(message))
}

fn io_failure(what: &str, path: &Path, e: std::io::Error) -> rusqlite::Error {
    failure(ffi::SQLITE_IOERR, format!("{what} {}: {e}", path.display()))
}

/// Open `path` at `ladder`'s schema, recovering what can be recovered.
pub(super) fn open(path: &Path, ladder: &Ladder) -> Result<Store> {
    open_as(path, ladder, true)
}

fn open_as(path: &Path, ladder: &Ladder, main: bool) -> Result<Store> {
    let mut set_aside = false;
    loop {
        let before = identity(path);
        let refusal = {
            // Shared while opening: setting a file aside takes it exclusively,
            // so nothing opens the old file between its parts moving.
            let _open = Lock::take(path, false)?;
            match Store::init(Connection::open(path)?, ladder) {
                Ok((store, wrote_schema)) => {
                    if main && wrote_schema {
                        sweep_side_stores(path, ladder.version);
                    }
                    return Ok(store);
                }
                Err(refusal) => refusal,
            }
        };
        match refusal {
            Refusal::Failed(e) => return Err(e),
            Refusal::Newer(version) if main => return side(path, ladder, version),
            Refusal::Newer(version) => {
                return Err(failure(
                    ffi::SQLITE_MISMATCH,
                    format!(
                        "{} is schema v{version}, newer than this rq's v{}",
                        path.display(),
                        ladder.version
                    ),
                ));
            }
            Refusal::Broken { reason, .. } if set_aside => {
                return Err(failure(ffi::SQLITE_CORRUPT, reason));
            }
            Refusal::Broken { upgrading, reason } => {
                set_aside = true;
                let _only = Lock::take(path, true)?;
                // Another opener already did it: open whatever is there now.
                if identity(path) != before {
                    continue;
                }
                let to = quarantine(path)?;
                let what = if upgrading { "upgraded" } else { "read" };
                eprintln!(
                    "rq: the index couldn't be {what} ({reason}); rebuilding it — the old file is at {}",
                    to.display()
                );
            }
        }
    }
}

/// The index this rq keeps while a newer one owns `path`.
fn side(path: &Path, ladder: &Ladder, newer: i64) -> Result<Store> {
    let side = side_path(path, ladder.version);
    let first = !side.exists();
    let store = open_as(&side, ladder, false)?;
    if first {
        let by = written_by(path).unwrap_or_else(|| format!("schema v{newer}"));
        eprintln!(
            "rq: {} was written by a newer rq ({by}); using {} beside it for this version",
            path.display(),
            file_name(&side)
        );
    }
    Ok(store)
}

/// `rq.db` → `rq.v23.db`.
pub(crate) fn side_path(path: &Path, version: i64) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.v{version}.{}", ext.to_string_lossy()),
        None => format!("{stem}.v{version}"),
    };
    path.with_file_name(name)
}

/// Which rq laid down a database's schema, read without writing to it.
fn written_by(path: &Path) -> Option<String> {
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row("SELECT value FROM meta WHERE key = 'schema_by'", [], |r| {
        r.get(0)
    })
    .ok()
}

/// The file at `path` now, so a second opener can tell it was replaced.
fn identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Move the database and its WAL to `<name>.broken-<unix time>`, keeping only
/// the newest such copy. The shared-memory file is rebuilt from the WAL, so it
/// goes.
fn quarantine(path: &Path) -> Result<PathBuf> {
    let name = file_name(path);
    let prefix = format!("{name}.broken-");
    for old in siblings(path) {
        if file_name(&old).starts_with(&prefix) {
            let _ = std::fs::remove_file(&old);
        }
    }
    let to = path.with_file_name(format!("{prefix}{}", crate::core::now_unix()));
    // The WAL before the database: a new file must never find the old WAL.
    for (from, dest) in [
        (with_suffix(path, "-wal"), with_suffix(&to, "-wal")),
        (path.to_path_buf(), to.clone()),
    ] {
        match std::fs::rename(&from, &dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_failure("can't move aside", &from, e)),
        }
    }
    let _ = std::fs::remove_file(with_suffix(path, "-shm"));
    Ok(to)
}

/// How long an older rq's own index outlives its last use.
const SIDE_STORE_IDLE: Duration = Duration::from_secs(30 * 24 * 3600);

/// After laying down a schema at `path`: this version's own side store is
/// stale now, and an older one unused for a month is abandoned.
fn sweep_side_stores(path: &Path, version: i64) {
    for (side, v) in side_stores(path, 1..=version) {
        let idle = || {
            [side.clone(), with_suffix(&side, "-wal")]
                .iter()
                .filter_map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
                .max()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > SIDE_STORE_IDLE)
        };
        if v == version || (v < version && idle()) {
            for suffix in ["", "-wal", "-shm", ".lock"] {
                let _ = std::fs::remove_file(with_suffix(&side, suffix));
            }
        }
    }
}

/// Side stores beside `path` for schema `versions`: probed by name, since a
/// database may live in a directory of many thousands of files.
pub(crate) fn side_stores(
    path: &Path,
    versions: std::ops::RangeInclusive<i64>,
) -> Vec<(PathBuf, i64)> {
    versions
        .map(|v| (side_path(path, v), v))
        .filter(|(p, _)| p.exists())
        .collect()
}

/// The copy [`quarantine`] kept, if any.
pub(crate) fn broken_copy(path: &Path) -> Option<PathBuf> {
    let prefix = format!("{}.broken-", file_name(path));
    siblings(path).into_iter().find(|p| {
        let name = file_name(p);
        name.starts_with(&prefix) && !name.ends_with("-wal") && !name.ends_with("-shm")
    })
}

fn siblings(path: &Path) -> Vec<PathBuf> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// An advisory lock on `<db>.lock`: shared while opening, exclusive while
/// setting the database aside. Released on drop, or by the kernel when a
/// holder dies.
struct Lock(Option<File>);

impl Lock {
    fn take(path: &Path, exclusive: bool) -> Result<Lock> {
        let lock = with_suffix(path, ".lock");
        // A directory rq can't write to fails the open itself, and says so.
        let Ok(file) = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
        else {
            return Ok(Lock(None));
        };
        let deadline = Instant::now() + UPGRADE_WAIT;
        loop {
            let held = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            if held.is_ok() {
                return Ok(Lock(Some(file)));
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    ffi::SQLITE_BUSY,
                    format!("timed out waiting for {}", lock.display()),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            // closing the file would release it too
            let _ = file.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::{self, Step};

    /// A directory of its own: set-aside copies and side stores are siblings.
    struct Dir(PathBuf);

    impl Dir {
        fn new(label: &str) -> Dir {
            let dir =
                std::env::temp_dir().join(format!("rq-recover-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }
        fn db(&self) -> PathBuf {
            self.0.join("rq.db")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The rq after this one: its schema adds a table.
    fn newer() -> Ladder {
        let extra = "CREATE TABLE extra (x INTEGER);";
        Ladder {
            version: schema::VERSION + 1,
            fresh: Box::leak(format!("{}{extra}", schema::SCHEMA).into_boxed_str()),
            steps: Box::leak(Box::new([(schema::VERSION + 1, Step::Sql(extra))])),
        }
    }

    fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    fn put(store: &Store, key: &str) {
        store
            .conn
            .execute("INSERT INTO meta (key, value) VALUES (?1, 'v')", [key])
            .unwrap();
    }

    fn has(store: &Store, key: &str) -> bool {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM meta WHERE key = ?1", [key], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
            == 1
    }

    #[test]
    fn a_file_that_is_not_a_database_is_set_aside_and_rebuilt() {
        let dir = Dir::new("garbage");
        std::fs::write(dir.db(), "not a database ".repeat(1000)).unwrap();
        let store = Store::open(&dir.db()).expect("a working store");
        put(&store, "k");
        let copy = broken_copy(&dir.db()).expect("the old file is kept");
        assert!(std::fs::read(copy).unwrap().starts_with(b"not a database"));
    }

    #[test]
    fn a_truncated_file_is_set_aside_and_rebuilt() {
        let dir = Dir::new("truncated");
        {
            let store = Store::open(&dir.db()).unwrap();
            let rows = (0..5000)
                .map(|i| format!("('key{i}', '{}')", "x".repeat(200)))
                .collect::<Vec<_>>()
                .join(",");
            store
                .conn
                .execute_batch(&format!("INSERT INTO meta (key, value) VALUES {rows}"))
                .unwrap();
        }
        let len = std::fs::metadata(dir.db()).unwrap().len();
        File::options()
            .write(true)
            .open(dir.db())
            .unwrap()
            .set_len(len / 2)
            .unwrap();
        let store = Store::open(&dir.db()).expect("a working store");
        assert!(!has(&store, "key1"), "rebuilt, not the old file");
        assert!(broken_copy(&dir.db()).is_some());
    }

    #[test]
    fn a_failed_upgrade_is_set_aside_and_rebuilt_at_the_new_schema() {
        let dir = Dir::new("failed-upgrade");
        put(&Store::open(&dir.db()).unwrap(), "old");
        let mut failing = newer();
        // the first statement applies, the second fails: midway
        failing.steps = Box::leak(Box::new([(
            schema::VERSION + 1,
            Step::Sql("CREATE TABLE extra (x INTEGER); INSERT INTO nowhere VALUES (1);"),
        )]));
        let store = open(&dir.db(), &failing).expect("a working store");
        assert!(!has(&store, "old"));
        drop(store);
        assert_eq!(version(&dir.db()), schema::VERSION + 1);
        let copy = broken_copy(&dir.db()).unwrap();
        assert_eq!(
            version(&copy),
            schema::VERSION,
            "the failed upgrade rolled back"
        );
    }

    #[test]
    fn a_database_missing_a_table_is_rebuilt() {
        let dir = Dir::new("missing-table");
        drop(Store::open(&dir.db()).unwrap());
        Connection::open(dir.db())
            .unwrap()
            .execute_batch("DROP TABLE checkout_files;")
            .unwrap();
        let store = Store::open(&dir.db()).unwrap();
        assert!(store.conn.prepare("SELECT * FROM checkout_files").is_ok());
        assert!(broken_copy(&dir.db()).is_some());
    }

    #[test]
    fn only_the_newest_set_aside_copy_is_kept() {
        let dir = Dir::new("one-copy");
        for _ in 0..2 {
            std::fs::write(dir.db(), "garbage ".repeat(1000)).unwrap();
            let _ = std::fs::remove_file(with_suffix(&dir.db(), "-wal"));
            drop(Store::open(&dir.db()).unwrap());
            std::thread::sleep(Duration::from_millis(1100));
        }
        let copies = siblings(&dir.db())
            .into_iter()
            .filter(|p| file_name(p).contains(".broken-") && !file_name(p).ends_with("-wal"))
            .count();
        assert_eq!(copies, 1);
    }

    #[test]
    fn a_path_that_cannot_be_opened_is_an_error_and_nothing_moves() {
        let dir = Dir::new("cantopen");
        std::fs::create_dir(dir.db()).unwrap();
        assert!(Store::open(&dir.db()).is_err());
        assert!(dir.db().is_dir());
        assert!(broken_copy(&dir.db()).is_none());
    }

    #[test]
    fn a_newer_database_is_left_alone_and_this_rq_keeps_its_own() {
        let dir = Dir::new("newer");
        put(&open(&dir.db(), &newer()).unwrap(), "newer");
        let store = Store::open(&dir.db()).expect("a working store");
        put(&store, "older");
        drop(store);
        assert_eq!(version(&dir.db()), schema::VERSION + 1);
        let main = open(&dir.db(), &newer()).unwrap();
        assert!(has(&main, "newer") && !has(&main, "older"));
        assert_eq!(
            side_stores(&dir.db(), 1..=schema::VERSION + 1),
            [(side_path(&dir.db(), schema::VERSION), schema::VERSION)]
        );
    }

    #[test]
    fn two_versions_taking_turns_keep_their_indexes() {
        let dir = Dir::new("alternate");
        for turn in 0..3 {
            let newer_store = open(&dir.db(), &newer()).unwrap();
            let older_store = Store::open(&dir.db()).unwrap();
            if turn == 0 {
                put(&newer_store, "n");
                put(&older_store, "o");
            }
            assert!(
                has(&newer_store, "n"),
                "turn {turn}: the newer index survived"
            );
            assert!(
                has(&older_store, "o"),
                "turn {turn}: the older index survived"
            );
        }
        assert!(broken_copy(&dir.db()).is_none());
    }

    #[test]
    fn a_newer_rq_retires_its_own_stale_side_store() {
        let dir = Dir::new("retire");
        // this rq ran while an even newer one owned the path...
        drop(open(&dir.db(), &newer()).unwrap());
        drop(Store::open(&dir.db()).unwrap());
        let side = side_path(&dir.db(), schema::VERSION);
        assert!(side.exists());
        // ...then the path was rebuilt at this rq's own schema
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(with_suffix(&dir.db(), suffix));
        }
        drop(Store::open(&dir.db()).unwrap());
        assert!(!side.exists());
        // an older rq's side store in use stays
        let older = side_path(&dir.db(), schema::VERSION - 1);
        std::fs::write(&older, "").unwrap();
        drop(open(&dir.db(), &newer()).unwrap());
        assert!(older.exists());
    }

    #[test]
    fn concurrent_openers_of_a_broken_database_rebuild_it_once() {
        let dir = Dir::new("concurrent");
        for round in 0..5 {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(with_suffix(&dir.db(), suffix));
            }
            std::fs::write(dir.db(), "garbage ".repeat(1000)).unwrap();
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let opens: Vec<_> = (0..8)
                .map(|i| {
                    let (path, start) = (dir.db(), std::sync::Arc::clone(&start));
                    std::thread::spawn(move || {
                        start.wait();
                        let store = Store::open(&path).expect("every opener gets a store");
                        put(&store, &format!("t{i}"));
                    })
                })
                .collect();
            for open in opens {
                open.join().unwrap();
            }
            let store = Store::open(&dir.db()).unwrap();
            for i in 0..8 {
                assert!(
                    has(&store, &format!("t{i}")),
                    "round {round}: t{i}'s write survived"
                );
            }
            assert!(broken_copy(&dir.db()).is_some());
        }
    }
}
