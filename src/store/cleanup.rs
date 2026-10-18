//! The store cleans up after itself (D65).
//!
//! Deletes leave their pages free inside the file — a requeue migration, a
//! `--drop`, a checkout pruned — and SQLite reuses them but never returns
//! them. So a pass that has finished indexing measures the free pages, at
//! most every [`INTERVAL`], and when they pass the threshold claims the
//! store's cleanup for a detached process (the CLI's `--cleanup`) that hands
//! them back. The claim comes first, so of several passes finishing together
//! one starts it, and one that fails is not restarted by every pass after.

use std::path::Path;
use std::time::Duration;

use super::{Held, Result, Store};

/// The least time between two looks at the store, in seconds: a busy day of
/// indexing measures a handful of times, and what an upgrade left is gone
/// within the session that upgraded.
pub(crate) const INTERVAL: i64 = 2 * 3600;

/// How soon a pass that found the store busy is asked for again, in seconds:
/// its claim would otherwise hold off the next for a whole interval.
const RETRY: i64 = 10 * 60;

/// Less than this is not worth a pass, however large a share of the file.
const MIN_WASTE: i64 = 16 << 20;

/// Free pages handed back per write transaction (4 MB at rq's page size), so
/// a writer waits that long at most, not the whole compaction.
const CHUNK_PAGES: i64 = 1024;

/// Between chunks: longer than a waiting writer's longest backoff (SQLite's
/// own handler sleeps up to 100 ms), so a queued write goes first.
const CHUNK_PAUSE: Duration = Duration::from_millis(150);

/// Worth a pass: at least [`MIN_WASTE`], and a quarter of the file. The
/// share keeps the churn every index leaves, which the next one reuses,
/// from rewriting a store.
fn worth(waste: i64, file: i64) -> bool {
    waste >= MIN_WASTE && waste * 4 >= file
}

/// Whether to start a cleanup now: one is [`due`], and this process took the
/// claim for it, which another asking at once then finds taken.
pub(crate) fn start(store: &mut Store, now: i64) -> Result<bool> {
    if !due(store, now)? {
        return Ok(false);
    }
    match store.claim_cleanup(now, INTERVAL) {
        Err(e) if super::is_busy(&e) => Ok(false),
        claimed => claimed,
    }
}

/// Whether a cleanup should run: nobody looked within [`INTERVAL`], nobody
/// is indexing (their writes would reuse the pages), and there's an index to
/// add or waste worth a pass. A look that finds nothing to do is recorded,
/// so the next is an interval away.
fn due(store: &Store, now: i64) -> Result<bool> {
    let next = store.cleanup()?.next_look(INTERVAL);
    if next.is_some_and(|next| now < next) || store.indexing_anywhere()? {
        return Ok(false);
    }
    if !store.missing_indexes()?.is_empty() || worth(store.free_bytes()?, store.db_bytes()?) {
        return Ok(true);
    }
    match store.cleanup_checked(now) {
        Err(e) if super::is_busy(&e) => Ok(false),
        checked => checked.map(|()| false),
    }
}

/// What a pass did with the file's free pages.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compacted {
    /// Too few to be worth it.
    #[default]
    NotWorth,
    /// Handed back a chunk at a time.
    Incremental,
    /// Rewritten whole: an older store, which the rewrite switches.
    Full,
    /// The rewrite wouldn't fit on the disk.
    NoRoom,
    /// Another rq was indexing, or held the lock past the wait.
    Busy,
}

/// What one cleanup pass did.
#[derive(Debug, Default)]
pub(crate) struct Done {
    pub(crate) compacted: Compacted,
    /// Indexes the schema gained that this store lacked.
    pub(crate) indexes: Vec<&'static str>,
    /// What left the file, counted in pages.
    pub(crate) reclaimed_bytes: i64,
}

/// Clean up `store`, claimed by [`start`]. Its writes wait as a search's do,
/// never as an index's; `between_chunks` runs after each chunk handed back.
pub(crate) fn run(store: &mut Store, now: i64, between_chunks: &mut dyn FnMut()) -> Result<Done> {
    let done = match pass(store, between_chunks) {
        Err(e) if super::is_busy(&e) => Ok(Done {
            compacted: Compacted::Busy,
            ..Done::default()
        }),
        done => done,
    };
    let recorded = record(store, now, &done);
    let done = done?;
    match recorded {
        // a writer still holding the lock leaves the record as it was
        Err(e) if super::is_busy(&e) => {}
        recorded => recorded?,
    }
    Ok(done)
}

fn pass(store: &mut Store, between_chunks: &mut dyn FnMut()) -> Result<Done> {
    if store.indexing_anywhere()? {
        return Ok(Done {
            compacted: Compacted::Busy,
            ..Done::default()
        });
    }
    // in pages: a reader can hold off the file's own shrink past this pass
    let before = store.db_bytes()?;
    let indexes = store.add_missing_indexes()?;
    let (free, file) = (store.free_bytes()?, store.db_bytes()?);
    let compacted = if !worth(free, file) {
        Compacted::NotWorth
    } else if store.incremental()? {
        in_chunks(store, between_chunks)?
    } else if !room_for(store.file(), file - free) {
        Compacted::NoRoom
    } else {
        store.vacuum()?;
        store.truncate_wal();
        Compacted::Full
    };
    Ok(Done {
        compacted,
        indexes,
        reclaimed_bytes: (before - store.db_bytes()?).max(0),
    })
}

/// What `--storage` says of a pass: a run, when it gave something back, and
/// why it left the free pages, when it did. The run last, so a reader that
/// sees it sees the pages already gone.
fn record(store: &Store, now: i64, done: &Result<Done>) -> Result<()> {
    let done = match done {
        Err(e) => return store.cleanup_failed(&e.to_string()),
        Ok(done) => done,
    };
    if done.compacted == Compacted::Busy {
        // the next pass asks again in `RETRY`, not a whole interval
        return store.cleanup_busy(now + RETRY);
    }
    store.cleanup_held(match done.compacted {
        Compacted::NoRoom => Some(Held::NoRoom),
        Compacted::Busy => Some(Held::Busy),
        Compacted::NotWorth | Compacted::Incremental | Compacted::Full => None,
    })?;
    if done.reclaimed_bytes > 0 {
        store.cleanup_ran(now, done.reclaimed_bytes)?;
    }
    Ok(())
}

/// Hand every free page back, a chunk per write transaction, pausing between
/// them so a waiting writer goes first. A writer holding the lock past the
/// wait ends the pass where it is.
fn in_chunks(store: &Store, between_chunks: &mut dyn FnMut()) -> Result<Compacted> {
    while store.free_bytes()? > 0 {
        match store.vacuum_step(CHUNK_PAGES) {
            Err(e) if super::is_busy(&e) => return Ok(Compacted::Busy),
            stepped => stepped?,
        }
        between_chunks();
        std::thread::sleep(CHUNK_PAUSE);
    }
    store.truncate_wal();
    Ok(Compacted::Incremental)
}

/// Whether the disks hold what a full rewrite writes on the way: the live
/// pages twice — SQLite's temporary copy, and the rewrite into the log
/// beside the store — and a quarter of each to spare.
fn room_for(store: Option<&Path>, live: i64) -> bool {
    let Some(dir) = store.and_then(Path::parent) else {
        return true;
    };
    let temp = sqlite_temp_dir();
    let (Some(beside), Some(spare)) = (available(dir), available(&temp)) else {
        return false;
    };
    fits(live.max(0) as u64, beside, spare, same_disk(dir, &temp))
}

/// `live` bytes written beside the store and to the temporary directory,
/// each with a quarter to spare.
fn fits(live: u64, beside: u64, temp: u64, same_disk: bool) -> bool {
    let need = live + live / 4;
    if same_disk {
        beside >= 2 * need
    } else {
        beside >= need && temp >= need
    }
}

/// Where SQLite puts a temporary file: the first of these that is a
/// directory, as `unixTempFileDir` in its os_unix.c looks.
fn sqlite_temp_dir() -> std::path::PathBuf {
    ["SQLITE_TMPDIR", "TMPDIR"]
        .iter()
        .filter_map(std::env::var_os)
        .map(std::path::PathBuf::from)
        .chain(["/var/tmp", "/usr/tmp", "/tmp", "."].map(std::path::PathBuf::from))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| ".".into())
}

fn same_disk(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => true,
    }
}

/// Bytes an unprivileged process may still write on `dir`'s filesystem.
#[allow(
    clippy::useless_conversion,
    reason = "statvfs field widths differ: u32 on macOS, u64 on Linux"
)]
fn available(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and outlives the call; `stat` is
    // written by `statvfs` and read only after it reports success.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    Some(u64::from(stat.f_bavail) * u64::from(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::Scratch;

    const NOW: i64 = 1_800_000_000;

    /// A store on disk, stamped long ago, so a look is due.
    fn store(label: &str) -> (Scratch, Store) {
        let dir = Scratch::new(&format!("cleanup-{label}"));
        let store = Store::open(&dir.join("rq.db")).unwrap();
        store.cleanup_checked(0).unwrap();
        (dir, store)
    }

    /// Leave `mb` megabytes of free pages in the file, as a requeue does.
    fn bloat(dir: &Path, mb: u32) {
        let conn = rusqlite::Connection::open(dir.join("rq.db")).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE junk (b BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {mb})
             INSERT INTO junk SELECT zeroblob(1048576) FROM n;
             DROP TABLE junk;
             PRAGMA wal_checkpoint(TRUNCATE);"
        ))
        .unwrap();
    }

    fn quietly() -> impl FnMut() {
        || {}
    }

    #[test]
    fn a_new_store_waits_an_interval_before_its_first_look() {
        let dir = Scratch::new("cleanup-new");
        let store = Store::open(&dir.join("rq.db")).unwrap();
        bloat(&dir, 20);
        let created = store.cleanup().unwrap().checked_at.expect("stamped");
        assert!(!due(&store, created + 60).unwrap(), "looked at just now");
        assert!(due(&store, created + INTERVAL).unwrap());
        assert!(store.incremental().unwrap(), "laid down incremental");
    }

    #[test]
    fn only_waste_past_the_threshold_is_worth_a_pass() {
        let (dir, store) = store("threshold");
        assert!(!due(&store, NOW).unwrap(), "a new store's churn");
        assert_eq!(
            store.cleanup().unwrap().checked_at,
            Some(NOW),
            "a look that found nothing waits an interval"
        );
        assert!(!due(&store, NOW + INTERVAL - 1).unwrap());
        bloat(&dir, 20);
        assert!(due(&store, NOW + INTERVAL).unwrap());
        assert_eq!(
            store.cleanup().unwrap().checked_at,
            Some(NOW),
            "a due look leaves the claim to the cleanup"
        );
        // plenty of bytes but a small share of the file, or a large share of little
        assert!(!worth(MIN_WASTE * 2, MIN_WASTE * 9));
        assert!(!worth(MIN_WASTE - 1, MIN_WASTE));
        assert!(worth(MIN_WASTE, MIN_WASTE * 4));
    }

    #[test]
    fn a_pass_hands_a_new_stores_pages_back_in_chunks() {
        let (dir, mut store) = store("chunks");
        bloat(&dir, 20);
        let before = store.db_bytes().unwrap();
        assert!(start(&mut store, NOW).unwrap());
        let mut chunks = 0;
        let done = run(&mut store, NOW, &mut || chunks += 1).unwrap();
        assert_eq!(done.compacted, Compacted::Incremental);
        assert!(chunks >= 5, "{chunks} chunks");
        assert_eq!(store.free_bytes().unwrap(), 0);
        let after = store.db_bytes().unwrap();
        assert!(after < before - (19 << 20), "{before} -> {after}");
        assert!(
            done.reclaimed_bytes.abs_diff(before - after) <= 8192,
            "{} vs {}",
            done.reclaimed_bytes,
            before - after
        );
        let file = std::fs::metadata(dir.join("rq.db")).unwrap().len() as i64;
        assert!(file <= after + 8192, "the file shrank too: {file}");
        let cleanup = store.cleanup().unwrap();
        assert_eq!(
            (cleanup.checked_at, cleanup.ran_at, cleanup.skipped),
            (Some(NOW), Some(NOW), None)
        );
        assert_eq!(cleanup.reclaimed_bytes, Some(done.reclaimed_bytes));
    }

    /// A store from before incremental vacuuming is rewritten once, which
    /// switches it: the next pass goes a chunk at a time.
    #[test]
    fn an_older_store_is_switched_by_one_full_vacuum() {
        let (dir, mut store) = store("switch");
        rusqlite::Connection::open(dir.join("rq.db"))
            .unwrap()
            .execute_batch("PRAGMA auto_vacuum = NONE; VACUUM;")
            .unwrap();
        assert!(!store.incremental().unwrap());
        bloat(&dir, 20);
        assert!(start(&mut store, NOW).unwrap());
        let done = run(&mut store, NOW, &mut quietly()).unwrap();
        assert_eq!(done.compacted, Compacted::Full);
        assert!(store.incremental().unwrap());
        assert_eq!(store.free_bytes().unwrap(), 0);

        bloat(&dir, 20);
        assert!(start(&mut store, NOW + INTERVAL).unwrap());
        let done = run(&mut store, NOW + INTERVAL, &mut quietly()).unwrap();
        assert_eq!(done.compacted, Compacted::Incremental);
        assert!(done.reclaimed_bytes >= 20 << 20, "{}", done.reclaimed_bytes);
    }

    #[test]
    fn a_full_vacuum_needs_the_live_pages_twice_and_a_quarter_spare() {
        const GB: u64 = 1 << 30;
        let cases = [
            // live, beside the store, temporary dir, one disk, fits
            (GB, 3 * GB, 3 * GB, true, true),
            (GB, 2 * GB, 2 * GB, true, false),
            (GB, 2 * GB, 2 * GB, false, true),
            (GB, 2 * GB, GB, false, false),
            (GB, GB, 2 * GB, false, false),
        ];
        for (live, beside, temp, one, fit) in cases {
            assert_eq!(
                fits(live, beside, temp, one),
                fit,
                "{live} {beside} {temp} {one}"
            );
        }
        assert!(!room_for(Some(Path::new("/nonexistent/rq.db")), 1));
    }

    #[test]
    fn a_pass_that_reclaims_nothing_is_not_recorded_as_a_run() {
        let (_dir, mut store) = store("nothing");
        let done = run(&mut store, NOW, &mut quietly()).unwrap();
        assert_eq!(
            (done.compacted, done.reclaimed_bytes),
            (Compacted::NotWorth, 0)
        );
        assert_eq!(store.cleanup().unwrap().ran_at, None);
    }

    #[test]
    fn a_failed_pass_says_why_and_holds_its_claim() {
        let (dir, mut store) = store("failed");
        bloat(&dir, 20);
        assert!(start(&mut store, NOW).unwrap());
        let full = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk\nis full".into()),
        );
        record(&store, NOW, &Err(full)).unwrap();
        let cleanup = store.cleanup().unwrap();
        assert_eq!(
            (
                cleanup.checked_at,
                cleanup.skipped,
                cleanup.error.as_deref()
            ),
            (
                Some(NOW),
                Some(Held::Failed),
                Some("database or disk is full")
            )
        );
        assert!(!start(&mut store, NOW + INTERVAL - 1).unwrap(), "claimed");
        assert!(start(&mut store, NOW + INTERVAL).unwrap());
        run(&mut store, NOW + INTERVAL, &mut quietly()).unwrap();
        let cleanup = store.cleanup().unwrap();
        assert_eq!(
            (cleanup.skipped, cleanup.error),
            (None, None),
            "a pass that works clears it"
        );
    }

    #[test]
    fn a_pass_is_started_at_most_once_an_interval() {
        let (dir, mut store) = store("rate");
        bloat(&dir, 20);
        assert!(start(&mut store, NOW).unwrap());
        assert!(
            !start(&mut store, NOW + 1).unwrap(),
            "claimed: a pass that then fails isn't restarted by the next index"
        );
        // a process that found it due just before the claim landed
        assert!(!store.claim_cleanup(NOW + 1, INTERVAL).unwrap());
        run(&mut store, NOW, &mut quietly()).unwrap();
        bloat(&dir, 20);
        assert!(!start(&mut store, NOW + INTERVAL - 1).unwrap());
        assert!(start(&mut store, NOW + INTERVAL).unwrap());
    }

    #[test]
    fn of_ten_passes_finishing_at_once_one_starts_the_cleanup() {
        let (dir, store) = store("race");
        bloat(&dir, 20);
        drop(store);
        let path = dir.join("rq.db");
        let gate = std::sync::Arc::new(std::sync::Barrier::new(10));
        let started = (0..10)
            .map(|_| {
                let (path, gate) = (path.clone(), std::sync::Arc::clone(&gate));
                std::thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    gate.wait();
                    start(&mut store, NOW).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(started, 1);
    }

    /// Another rq indexing, or holding the lock past the wait: the pass skips,
    /// says why, and is asked for again in `RETRY`, not an interval.
    #[test]
    fn a_busy_store_is_left_for_a_pass_soon_after() {
        let (dir, mut store) = store("busy");
        bloat(&dir, 20);
        // pid 1 is alive (another user's), so its mark is a live pass
        store.begin_pass("/other", 1, None).unwrap();
        assert!(
            !start(&mut store, NOW).unwrap(),
            "nobody claims under a pass"
        );
        let done = run(&mut store, NOW, &mut quietly()).unwrap();
        assert_eq!(done.compacted, Compacted::Busy);
        assert_eq!(store.cleanup().unwrap().skipped, Some(Held::Busy));
        store.end_pass("/other", 1, None).unwrap();
        assert!(!start(&mut store, NOW + RETRY - 1).unwrap());
        assert!(start(&mut store, NOW + RETRY).unwrap());

        // a writer holding the lock: no claim without it, and the pass gives up
        let writer = rusqlite::Connection::open(dir.join("rq.db")).unwrap();
        store.cleanup_checked(0).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(
            !start(&mut store, NOW).unwrap(),
            "no claim without the lock"
        );
        store.conn.busy_timeout(Duration::from_millis(50)).unwrap();
        let done = run(&mut store, NOW, &mut quietly()).unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(done.compacted, Compacted::Busy);
        assert_eq!(
            store.cleanup().unwrap().checked_at,
            Some(0),
            "nothing written under the lock: the next index asks again"
        );
        run(&mut store, NOW, &mut quietly()).unwrap();
        assert_eq!(
            store.cleanup().unwrap().skipped,
            None,
            "a pass that compacts clears it"
        );
    }

    /// A claim never waits for the lock, and gives the connection back its
    /// own wait: a pass's connection waits out writers longer than a search's.
    #[test]
    fn a_claim_gives_the_connection_back_its_own_wait() {
        let (dir, mut store) = store("waits");
        store
            .wait_out_writers(Duration::from_secs(20), None)
            .unwrap();
        assert!(store.claim_cleanup(NOW, INTERVAL).unwrap());
        let writer = rusqlite::Connection::open(dir.join("rq.db")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let holder = std::thread::spawn(move || {
            // past BUSY_WAIT, inside the pass's wait
            std::thread::sleep(super::super::BUSY_WAIT + Duration::from_millis(500));
            writer.execute_batch("ROLLBACK").unwrap();
        });
        let wrote = store.conn.execute("INSERT INTO meta VALUES ('k', 'v')", []);
        holder.join().unwrap();
        wrote.expect("the pass's wait, not a search's");
    }

    /// A store laid down before an index joined the schema is due for a pass
    /// with nothing else to do, and the pass adds it.
    #[test]
    fn a_pass_adds_an_index_the_store_lacks() {
        let (dir, mut store) = store("index");
        rusqlite::Connection::open(dir.join("rq.db"))
            .unwrap()
            .execute_batch("DROP INDEX idx_checkouts_repo")
            .unwrap();
        assert!(start(&mut store, NOW).unwrap(), "due for the index alone");
        let done = run(&mut store, NOW, &mut quietly()).unwrap();
        assert_eq!(done.indexes, ["idx_checkouts_repo"]);
        assert!(store.missing_indexes().unwrap().is_empty());
        assert!(!due(&store, NOW + INTERVAL).unwrap(), "and nothing after");
    }

    #[test]
    fn a_fresh_store_has_every_added_index() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.missing_indexes().unwrap().is_empty());
    }

    /// `--storage` reads in one snapshot: a pass recording its run between
    /// the reads isn't reported beside the pages it had not yet handed back.
    #[test]
    fn storage_is_read_in_one_snapshot() {
        let (dir, store) = store("snapshot");
        bloat(&dir, 20);
        let path = dir.join("rq.db");
        super::super::storage::BETWEEN_READS.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                let mut other = Store::open(&path).unwrap();
                run(&mut other, NOW, &mut || {}).unwrap();
            }));
        });
        let seen = store.storage().unwrap();
        super::super::storage::BETWEEN_READS.with_borrow_mut(|hook| *hook = None);
        assert!(seen.free_bytes >= 20 << 20);
        assert_eq!(seen.cleanup.ran_at, None, "{seen:?}");
        assert!(seen.bytes >= seen.free_bytes, "{seen:?}");
        let now = store.storage().unwrap();
        assert_eq!((now.free_bytes, now.cleanup.ran_at), (0, Some(NOW)));
    }

    #[test]
    fn a_store_that_is_gone_is_not_opened_again() {
        let dir = Scratch::new("cleanup-gone");
        let path = dir.join("rq.db");
        assert!(Store::open_existing(&path).is_err());
        assert!(!path.try_exists().unwrap(), "no new store");
        assert!(!dir.join("rq.db.lock").try_exists().unwrap());
    }
}
