//! The database file itself: its size, the free pages inside it, handing
//! them back to the disk, and what the background cleanup last did (D65).
//! The policy — when, and how much — is [`super::cleanup`]'s.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Result, Store, pid_alive, schema, stamp_fresh};
use crate::core::now_unix;

/// When a cleanup last measured the store (unix seconds). Also the claim one
/// pass holds: of several passes finishing at once, one moves it.
const CHECKED: &str = "cleanup:checked";
/// When a pass that found the store busy asks again, sooner than an interval
/// from [`CHECKED`]; gone with the next look.
const RETRY: &str = "cleanup:retry";
/// The last pass that gave something back: `<at> <bytes>`.
const RAN: &str = "cleanup:ran";
/// Why the last pass left free pages it meant to hand back: a [`Held`],
/// and after a failure, the error.
const SKIPPED: &str = "cleanup:skipped";

/// What the background cleanup last did. Each field absent until it applies.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Cleanup {
    /// When it last measured the store; a new store is stamped at creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// When it looks again, after a pass that found the store busy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// Its last pass that returned space to the disk, and how much.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaimed_bytes: Option<i64>,
    /// Why its last pass left free pages it meant to hand back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Held>,
    /// What failed, when its last pass did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Cleanup {
    /// When the next look is due: the retry a busy pass asked for, else an
    /// `interval` after the last.
    pub(crate) fn next_look(&self, interval: i64) -> Option<i64> {
        self.retry_at.or(self.checked_at.map(|at| at + interval))
    }
}

/// Why a pass left the file's free pages where they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Held {
    /// The one full rewrite an older store needs wouldn't fit on the disk.
    NoRoom,
    /// Another rq was indexing, or held the write lock past the wait.
    Busy,
    /// An error ended it, such as a disk filling mid-rewrite.
    Failed,
}

impl Held {
    fn as_str(self) -> &'static str {
        match self {
            Held::NoRoom => "no_room",
            Held::Busy => "busy",
            Held::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Held> {
        [Held::NoRoom, Held::Busy, Held::Failed]
            .into_iter()
            .find(|h| h.as_str() == s)
    }
}

/// The file's size and free pages with the cleanup record, as of one moment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Storage {
    /// The database's pages, as SQLite counts them.
    pub bytes: i64,
    /// Of those, the free ones: what compacting returns to the disk.
    pub free_bytes: i64,
    pub cleanup: Cleanup,
}

/// Stamp a store as it is laid down, so its first look is an interval away:
/// a new store has nothing to give back.
pub(super) fn created(conn: &Connection) -> Result<()> {
    set(conn, CHECKED, &now_unix().to_string())
}

fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
           ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(drop)
}

fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

fn unset(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])
        .map(drop)
}

/// Record a look at `at`: the next is an interval from it, whatever retry a
/// busy pass asked for.
fn looked(conn: &Connection, at: i64) -> Result<()> {
    set(conn, CHECKED, &at.to_string())?;
    unset(conn, RETRY)
}

#[cfg(test)]
thread_local! {
    /// Run between [`Store::storage`]'s reads: a test's write landing there.
    pub(super) static BETWEEN_READS: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

impl Store {
    /// What the background cleanup last did.
    pub(crate) fn cleanup(&self) -> Result<Cleanup> {
        let time =
            |key| Ok::<_, rusqlite::Error>(get(&self.conn, key)?.and_then(|v| v.parse().ok()));
        let skipped = get(&self.conn, SKIPPED)?.unwrap_or_default();
        let (held, error) = skipped.split_once(' ').unwrap_or((&skipped, ""));
        let ran = get(&self.conn, RAN)?;
        let mut ran = ran
            .iter()
            .flat_map(|v| v.split(" "))
            .map(|n| n.parse::<i64>().ok());
        Ok(Cleanup {
            checked_at: time(CHECKED)?,
            retry_at: time(RETRY)?,
            ran_at: ran.next().flatten(),
            reclaimed_bytes: ran.next().flatten(),
            skipped: Held::parse(held),
            error: (!error.is_empty()).then(|| error.to_string()),
        })
    }

    /// The size, the free pages and the cleanup record in one snapshot: read
    /// one by one, a pass finishing between them reports its run beside the
    /// pages it has since handed back.
    pub(crate) fn storage(&self) -> Result<Storage> {
        let tx = self.conn.unchecked_transaction()?;
        let free_bytes = self.free_bytes()?;
        #[cfg(test)]
        BETWEEN_READS.with_borrow_mut(|hook| hook.as_mut().map(|f| f()));
        let storage = Storage {
            bytes: self.db_bytes()?,
            free_bytes,
            cleanup: self.cleanup()?,
        };
        tx.commit()?;
        Ok(storage)
    }

    /// Record that a cleanup looked at `at`. Never waits long: the caller is
    /// a pass on its way out.
    pub(crate) fn cleanup_checked(&self, at: i64) -> Result<()> {
        self.bookkeeping(|| {
            let tx = self.conn.unchecked_transaction()?;
            looked(&tx, at)?;
            tx.commit()
        })
    }

    /// Take the cleanup at `at` unless its next look isn't due yet (see
    /// [`Cleanup::next_look`]): one immediate transaction, so of several
    /// asking at once one is told yes. Never waits for the lock — busy is an
    /// error, and the next pass asks again.
    pub(crate) fn claim_cleanup(&mut self, at: i64, interval: i64) -> Result<bool> {
        self.waiting_at_most(std::time::Duration::ZERO, || {
            let tx =
                rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            if self
                .cleanup()?
                .next_look(interval)
                .is_some_and(|next| at < next)
            {
                return Ok(false);
            }
            looked(&tx, at)?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Record a pass that left the free pages because the store was busy,
    /// and look again at `retry_at`. One write, never waiting long: the
    /// store is busy.
    pub(crate) fn cleanup_busy(&self, retry_at: i64) -> Result<()> {
        self.bookkeeping(|| {
            let tx = self.conn.unchecked_transaction()?;
            set(&tx, RETRY, &retry_at.to_string())?;
            set(&tx, SKIPPED, Held::Busy.as_str())?;
            tx.commit()
        })
    }

    /// Record a pass that `error` ended. Its claim stands, so the next look
    /// is an interval away, not the next pass's.
    pub(crate) fn cleanup_failed(&self, error: &str) -> Result<()> {
        // one line, as the record is one value
        let error = error.split_whitespace().collect::<Vec<_>>().join(" ");
        self.bookkeeping(|| {
            set(
                &self.conn,
                SKIPPED,
                &format!("{} {error}", Held::Failed.as_str()),
            )
        })
    }

    /// Record a pass at `at` that returned `reclaimed` bytes to the disk.
    pub(crate) fn cleanup_ran(&self, at: i64, reclaimed: i64) -> Result<()> {
        set(&self.conn, RAN, &format!("{at} {reclaimed}"))
    }

    /// Record why the last pass left the free pages, or that it didn't.
    pub(crate) fn cleanup_held(&self, held: Option<Held>) -> Result<()> {
        match held {
            Some(held) => set(&self.conn, SKIPPED, held.as_str()),
            None => unset(&self.conn, SKIPPED),
        }
    }

    /// Free pages inside the file, in bytes: what compacting returns.
    pub(crate) fn free_bytes(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT freelist_count * page_size FROM pragma_freelist_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )
    }

    /// The database's size in bytes, as SQLite counts its pages: with a
    /// reader holding the log, the file on disk shrinks later.
    pub(crate) fn db_bytes(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |r| r.get(0),
        )
    }

    /// Whether free pages can go back a chunk at a time, without a rewrite.
    pub(crate) fn incremental(&self) -> Result<bool> {
        // inside a read, so the header is the file's now, not as last cached
        let tx = self.conn.unchecked_transaction()?;
        let _: i64 = tx.query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))?;
        let mode: i64 = tx.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
        tx.commit()?;
        Ok(mode == 2)
    }

    /// Rewrite the file without its free pages, switching it to incremental
    /// vacuuming on the way, so this is the last full one. SQLite's copy of
    /// the live pages goes to a temporary file: in memory, as this
    /// connection's temporary tables live, it is the whole database.
    pub(crate) fn vacuum(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA temp_store = FILE;")?;
        let vacuumed = self.conn.execute_batch("VACUUM;");
        self.conn.execute_batch("PRAGMA temp_store = MEMORY;")?;
        vacuumed
    }

    /// Hand back up to `pages` free pages — pages from the file's end move
    /// into the holes — in one short write transaction, then fold the log
    /// back as far as readers let it.
    pub(crate) fn vacuum_step(&self, pages: i64) -> Result<()> {
        // each step of the statement frees a page: run it to the end
        let mut stmt = self
            .conn
            .prepare(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        self.conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
    }

    /// Fold the log into the file and empty it, so what a compaction gave
    /// back leaves the disk. Best effort and brief: a reader mid-transaction
    /// keeps it, and the next checkpoint finishes the job.
    pub(crate) fn truncate_wal(&self) {
        self.waiting_at_most(std::time::Duration::ZERO, || {
            for _ in 0..10 {
                let busy: rusqlite::Result<i64> =
                    self.conn
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0));
                if matches!(busy, Ok(0)) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
    }

    /// The indexes the schema gained without a version bump that this store,
    /// laid down before them, lacks.
    pub(crate) fn missing_indexes(&self) -> Result<Vec<&'static str>> {
        let mut have = self
            .conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1")?;
        let mut missing = Vec::new();
        for (name, _) in schema::ADDED_INDEXES {
            if !have.exists([name])? {
                missing.push(name);
            }
        }
        Ok(missing)
    }

    /// Build the indexes [`missing_indexes`](Self::missing_indexes) names, and
    /// say which.
    pub(crate) fn add_missing_indexes(&self) -> Result<Vec<&'static str>> {
        let missing = self.missing_indexes()?;
        for (name, create) in schema::ADDED_INDEXES {
            if missing.contains(&name) {
                self.conn.execute_batch(create)?;
            }
        }
        Ok(missing)
    }

    /// Whether a live rq other than this one is indexing any checkout: a
    /// pass mark or a warm lock, judged as `--drop` and search judge them.
    /// Its writes would reuse the free pages a cleanup would hand back.
    pub(crate) fn indexing_anywhere(&self) -> Result<bool> {
        let me = std::process::id();
        let mut rows = self.conn.prepare(
            "SELECT key, value FROM meta WHERE substr(key, 1, 5) = 'pass:' \
             OR substr(key, 1, 10) = 'warm_lock:'",
        )?;
        let marks = rows
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>>>()?;
        Ok(marks.iter().any(|(key, value)| {
            // `pass:<root>:<pid>` = stamp; `warm_lock:<root>` = `<pid>:<stamp>`
            let (pid, at) = match key.strip_prefix("pass:") {
                Some(rest) => (
                    rest.rsplit_once(':').map(|(_, pid)| pid),
                    Some(value.as_str()),
                ),
                None => match value.split_once(':') {
                    Some((pid, at)) => (Some(pid), Some(at)),
                    None => (None, None),
                },
            };
            let pid = pid.and_then(|p| p.parse::<u32>().ok());
            pid.is_some_and(|p| p != me && pid_alive(i64::from(p))) && at.is_some_and(stamp_fresh)
        }))
    }
}
