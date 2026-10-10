//! Storage — SQLite (WAL mode), schema, and queries.
//!
//! The background indexer writes here; search reads. WAL mode lets those
//! happen concurrently. See `docs/ARCHITECTURE.md` for the schema.

mod names;
mod recover;
mod schema;

pub(crate) use recover::{broken_copy, side_path, side_stores};
pub(crate) use schema::VERSION;

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::core::{Symbol, now_unix};
use crate::search::{Probe, Verdict};

pub(crate) type Result<T> = rusqlite::Result<T>;

/// A symbol as returned by search candidate queries (joined with its file and
/// repository for display and ranking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolRow {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file: String,
    pub line: i64,
    /// 1-based last line of the definition body; `None` for rows indexed before
    /// end-line tracking (they backfill on the next re-extract).
    pub end_line: Option<i64>,
    pub parent: Option<String>,
    pub repository_id: i64,
    pub repo_identity: String,
    /// The checkout the row was read from, and its root: `file` is relative to
    /// it. One version can be mapped by several checkouts, one row each.
    pub checkout_id: i64,
    pub root: String,
    /// File mtime (unix *nanoseconds*) — a recency signal.
    pub mtime: Option<i64>,
    /// Last git commit time touching the file — the stronger recency signal.
    pub git_ts: Option<i64>,
    /// Access level (`public`/`crate`/`private`/`protected`/`local`) when the language
    /// expresses one; `None` for unknown (or pre-v9 rows). A ranking hint.
    pub visibility: Option<String>,
    /// The file declares itself generated. A ranking hint.
    pub generated: bool,
    /// Declares a definition whose body lives elsewhere ([`Symbol::stub`]).
    pub stub: bool,
    /// Belongs to the type, not its instances ([`Symbol::singleton`]).
    pub singleton: bool,
}

impl SymbolRow {
    /// A row for a symbol parsed live rather than read from the index — the
    /// same shape, with no file times, from no stored checkout ([`LIVE`]).
    pub(crate) fn live(s: Symbol, repo_identity: &str, root: &str, generated: bool) -> Self {
        SymbolRow {
            name: s.name,
            kind: s.kind.as_str().to_string(),
            language: s.language,
            file: s.file,
            line: s.line as i64,
            end_line: Some(s.end_line as i64),
            parent: s.parent,
            repository_id: LIVE,
            repo_identity: repo_identity.to_string(),
            checkout_id: LIVE,
            root: root.to_string(),
            mtime: None,
            git_ts: None,
            visibility: s.visibility.map(str::to_string),
            stub: s.stub,
            singleton: s.singleton,
            generated,
        }
    }
}

/// The repository and checkout id of a row parsed live: no stored one. A live
/// scan passes it as the current checkout, so its rows take the boost.
pub(crate) const LIVE: i64 = -1;

/// Column projection shared by the candidate queries. Column order is consumed
/// by [`row_to_candidate`].
const CANDIDATE_COLS: &str = "s.id, s.name, s.kind, s.language, fi.path, s.line, \
    s.end_line, s.parent, s.repository_id, r.identity, cf.mtime, cf.git_ts, s.visibility, \
    fi.generated, s.stub, cf.checkout_id, co.root_path, s.singleton";
/// A symbol row once per checkout that maps its version: the checkout's map is
/// what scopes a search to the tree it's asked from.
const CANDIDATE_FROM: &str = "FROM symbols s \
    JOIN files fi ON fi.id = s.file_id \
    JOIN checkout_files cf ON cf.file_id = s.file_id \
    JOIN checkouts co ON co.id = cf.checkout_id \
    JOIN repositories r ON r.id = s.repository_id";

/// Which checkout a candidate row is read from, bound to `?{n}`. Scoped, the
/// checkout `?{n}` names. Unscoped (`?{n}` the checkout to prefer, or NULL),
/// one per version: the preferred checkout when it maps the version, else the
/// newest that does. A definition k checkouts share then costs the cap one
/// row rather than k, and [`fold_checkouts`] applies the same preference.
pub(super) fn read_from(scoped: bool, n: usize) -> String {
    if scoped {
        return format!("cf.checkout_id = ?{n}");
    }
    format!(
        "cf.checkout_id = COALESCE(\
           (SELECT x.checkout_id FROM checkout_files x WHERE x.file_id = s.file_id AND x.checkout_id = ?{n}), \
           (SELECT MAX(x.checkout_id) FROM checkout_files x WHERE x.file_id = s.file_id))"
    )
}

/// A checkout as the index knows it: the unit coverage, file stats and caches
/// are kept for, and the repository whose file versions and names it shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Checkout {
    pub id: i64,
    pub repo: i64,
}

/// A handle to the rq database.
pub(crate) struct Store {
    conn: Connection,
}

impl Drop for Store {
    fn drop(&mut self) {
        // SQLite's recommended pre-close hygiene: refreshes planner statistics
        // for the query shapes this connection actually ran. Cheap, best-effort.
        let _ = self.conn.execute_batch("PRAGMA optimize;");
    }
}

/// A parsed file ready to persist — the unit the indexer produces (in parallel)
/// and [`Store::replace_files`] writes in one batched transaction.
#[derive(Debug, Clone)]
pub(crate) struct FileSymbols {
    pub path: String,
    pub language: String,
    pub mtime: Option<i64>,
    pub content_hash: String,
    /// The file declares itself generated (`crate::index::is_generated`).
    pub generated: bool,
    /// `None` when the repo already held this version and the parse was
    /// skipped: the write maps it rather than storing it again.
    pub symbols: Option<Vec<Symbol>>,
}

/// What a [`Store::replace_files`] wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Written {
    /// Files newly mapped into the checkout.
    pub files: usize,
    /// Of those, the ones parsed: a version the repo didn't hold.
    pub versions: usize,
    /// Symbols of the versions newly stored.
    pub symbols: usize,
    /// Files sent unparsed for a version another checkout let go of since:
    /// the caller parses them and writes them again.
    pub unmapped: Vec<String>,
}

/// How much of a checkout its index covers. Stored and printed as its
/// lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Coverage {
    /// Registered, nothing read: only `--status` reports it (a checkout an
    /// upgrade registered, which only a search there indexes).
    Unindexed,
    /// A pass is filling it, or was cut short.
    Warming,
    /// A pass read the whole tree.
    Complete,
}

impl Coverage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Coverage::Unindexed => "unindexed",
            Coverage::Warming => "warming",
            Coverage::Complete => "complete",
        }
    }
}

impl std::fmt::Display for Coverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl rusqlite::types::ToSql for Coverage {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl rusqlite::types::FromSql for Coverage {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "unindexed" => Ok(Coverage::Unindexed),
            "warming" => Ok(Coverage::Warming),
            "complete" => Ok(Coverage::Complete),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown coverage status {other:?}").into(),
            )),
        }
    }
}

/// A coverage row's `(status, last_indexed_at)` as a pass found it when it
/// began, or `None` before any pass finished (see `set_coverage_since`).
pub(crate) type CoverageMark = Option<(Coverage, i64)>;

/// One row of `rq status` output — the current indexed totals for a checkout
/// (not any single run's incremental counts).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CoverageRow {
    /// Repository identity (`github.com/org/repo` or `local:/path`). Named `repo`
    /// in JSON, matching the search result field.
    #[serde(rename = "repo")]
    pub identity: String,
    /// The checkout's root, named as a search hit names it.
    pub root: String,
    pub status: Coverage,
    pub files: i64,
    /// Files the tree spans, while it isn't complete and a pass has counted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<i64>,
    /// What a live pass over it is doing, and for how many whole seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_secs: Option<i64>,
    pub symbols: i64,
}

/// One search, as recorded for usage. A struct rather than a positional
/// argument list — every field here is a label, and mixing them up silently
/// would corrupt the counters.
pub(crate) struct SearchRecord<'a> {
    /// Caller label from [`crate::origin`].
    pub source: &'a str,
    /// Canonical flag set, comma-joined; empty for a bare search.
    pub flags: &'a str,
    /// A miss (the symbol isn't there) and a not-yet (the index wasn't ready
    /// to say) are counted apart, as their exit codes are: conflating them
    /// would overstate how often rq truly finds nothing.
    pub status: Verdict,
    /// Index state when the query arrived; `None` before any pass finished.
    pub coverage: Option<Coverage>,
    /// Answered from a live scan of an untracked directory, not the index.
    pub live: bool,
}

/// A cached list of the files this branch is changing, plus the two things
/// that decide whether it's still worth serving: when it was written, and what
/// it cost to build.
pub(crate) struct BranchFiles {
    /// Git state fingerprint — mtimes of `.git/HEAD` and `.git/index`. Catches
    /// every git operation; catches no bare working-tree edit, which is what
    /// the freshness window is for.
    pub stamp: String,
    pub written_at: i64,
    /// How long the git diffs behind it took. `None` for an entry written
    /// before this was recorded.
    pub cost_ms: Option<u64>,
    pub files: Vec<String>,
}

/// One row of `rq --usage` output: how rq was called on a given day, and what
/// came back.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct UsageRow {
    pub day: String,
    /// Caller label from `crate::origin` — `claude-code`, `human`, `ci`, ...
    pub source: String,
    /// Canonical flag set for the call, comma-joined; empty for a bare search.
    pub flags: String,
    pub searches: i64,
    /// Answered nothing against a ready index — the symbol isn't there.
    pub misses: i64,
    /// Answered nothing because the index wasn't ready yet.
    pub warming: i64,
    /// Ran against a fully indexed repo, whatever the outcome.
    pub on_complete: i64,
    /// Answered from a live scan of an untracked directory, not the index.
    pub live: i64,
}

/// Record which rq did something to the schema, e.g. `schema_by = 0.61.0`.
fn stamp(conn: &Connection, key: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, env!("CARGO_PKG_VERSION")],
    )
    .map(drop)
}

/// A table `schema` creates that the database doesn't have.
fn missing_table(conn: &Connection, schema: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let have = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<_>>>()?;
    Ok(schema
        .split("CREATE TABLE ")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .find(|table| !have.contains(*table))
        .map(String::from))
}

/// Run one migration step.
fn apply(conn: &Connection, step: &schema::Step) -> Result<()> {
    match *step {
        schema::Step::Sql(sql) => conn.execute_batch(sql),
        schema::Step::Run(run) => run(conn),
        schema::Step::AddColumn {
            table,
            column,
            decl,
        } => {
            let present: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                params![table, column],
                |r| r.get(0),
            )?;
            if present {
                return Ok(());
            }
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))
        }
    }
}

/// Switch to WAL. Two openers switching one fresh file at once can fail this
/// with SQLITE_BUSY without busy_timeout ever being consulted; the mode is
/// persistent, so a short retry finds it done by whoever won.
fn wal(conn: &Connection) -> Result<()> {
    let mut tries = 0;
    loop {
        match conn.execute_batch("PRAGMA journal_mode=WAL;") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy && tries < 50 =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            done => return done,
        }
    }
}

/// Is the process a pid in the database names still running? A pid of 0 is a
/// marker from before the pid was kept: its pass can't be told from a killed
/// one. A pid on another machine sharing the database reads as dead, which
/// costs that pass speed, not correctness.
pub(crate) fn pid_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // EPERM: alive, but another user's
    pid > 0
        // SAFETY: signal 0 sends nothing, only checks; `pid > 0` keeps it off
        // the process-group forms (0, -1, -pgid).
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// How long a warm lock or a pass mark is trusted since it was stamped — past
/// this, it is a crashed process's leftover whose pid may have been reused,
/// and a new warmer takes over. A pass renews its mark as it writes.
pub(crate) const WARM_LOCK_TTL_SECS: i64 = 600;

/// Whether a warm lock's holder, `pid` stamped at `ts`, still has it.
pub(crate) fn warm_lock_held(pid: u32, ts: i64) -> bool {
    pid_alive(i64::from(pid)) && now_unix() - ts < WARM_LOCK_TTL_SECS
}

/// How often a pass renews its mark, well inside the TTL.
pub(crate) const PASS_RENEWAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Whole seconds since `since` (unix seconds), never negative.
pub(crate) fn phase_secs(since: i64) -> i64 {
    (now_unix() - since).max(0)
}

/// Whether a pass mark's stamp is inside the TTL.
fn stamp_fresh(at: &str) -> bool {
    at.parse::<i64>()
        .is_ok_and(|at| now_unix() - at < WARM_LOCK_TTL_SECS)
}

/// A pass's phases, as `warming.phase` names them.
pub(crate) const READING: &str = "reading";
pub(crate) const FINISHING: &str = "finishing";

/// How long a write waits out another writer before failing "locked".
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a bookkeeping write (a usage count, a cache a later call can
/// rebuild) waits out another writer before it's skipped: a pass's batch
/// commit, not a cold pass's name-index rebuild, which holds the lock for
/// seconds while a search's caller waits on its exit.
const BOOKKEEPING_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a pass nobody waits on — `rq --index`, a warm child — waits out
/// another writer, in ms (see [`Store::wait_out_writers`]).
static WRITER_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Called while a pass has waited on another writer past [`WAIT_NOTICE`].
static ON_LONG_WAIT: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();
/// How long a pass waits on another writer before saying so.
const WAIT_NOTICE: std::time::Duration = std::time::Duration::from_secs(1);

/// The busy handler [`Store::wait_out_writers`] installs: each wait for the
/// lock is bounded on its own, and past [`WAIT_NOTICE`] the hook hears of it.
fn pass_busy(attempt: i32) -> bool {
    use std::time::{Duration, Instant};
    thread_local!(static SINCE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });
    let since = SINCE.with(|s| {
        if attempt == 0 || s.get().is_none() {
            s.set(Some(Instant::now()));
        }
        s.get().expect("set above")
    });
    let wait = Duration::from_millis(WRITER_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed));
    let waited = since.elapsed();
    if waited >= wait {
        return false;
    }
    if waited >= WAIT_NOTICE
        && let Some(hook) = ON_LONG_WAIT.get()
    {
        hook();
    }
    std::thread::sleep((wait - waited).min(Duration::from_millis(20)));
    true
}
/// ...and how long an opener waits out another's schema upgrade.
const UPGRADE_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

impl Store {
    /// Open (creating if needed) the database at `path`, enabling WAL and
    /// applying the schema. A database this rq can't use is set aside and
    /// rebuilt, or kept for a newer rq (see [`recover`]).
    pub(crate) fn open(path: &Path) -> Result<Store> {
        recover::open(path, &schema::LADDER)
    }

    /// The file this store reads, which is not the path asked for when a newer
    /// rq owns that one.
    pub(crate) fn file(&self) -> Option<&Path> {
        self.conn.path().filter(|p| !p.is_empty()).map(Path::new)
    }

    /// Open an in-memory database — used by tests.
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        match Self::init(conn, &schema::LADDER) {
            Ok((store, _)) => Ok(store),
            Err(recover::Refusal::Failed(e)) => Err(e),
            Err(_) => unreachable!("a new in-memory database is always usable"),
        }
    }

    /// The store, and whether this call laid down or upgraded its schema.
    fn init(
        conn: Connection,
        ladder: &schema::Ladder,
    ) -> std::result::Result<(Store, bool), recover::Refusal> {
        use recover::Refusal;
        // WAL lets one writer and many readers coexist; busy_timeout makes a
        // second writer (e.g. two `rq` processes in two terminals, both warming)
        // wait briefly instead of erroring with "database is locked". mmap reads
        // pages in place rather than copying them through read(): fuzzy recall
        // materializes thousands of rows (see DECISIONS D8).
        // busy_timeout first: switching to WAL takes a lock of its own.
        conn.execute_batch(&format!("PRAGMA busy_timeout={};", BUSY_WAIT.as_millis()))?;
        wal(&conn)?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; \
             PRAGMA cache_size=-16384; PRAGMA mmap_size=268435456;",
        )?;
        let user_version = |conn: &Connection| -> Result<i64> {
            conn.pragma_query_value(None, "user_version", |r| r.get(0))
        };
        let mut version = user_version(&conn)?;
        // Schema work runs under the write lock, re-reading the version once it
        // holds it: another opener may have laid the schema down meanwhile.
        // Foreign keys are off for it: rebuilding a table (v23) drops one that
        // others reference, and the pragma can't change inside a transaction.
        let upgrading = (0..ladder.version).contains(&version);
        if upgrading {
            // An upgrade holds the write lock as long as it takes, seconds on a
            // store of a million rows, and every other opener queues here
            // behind it: past the usual timeout they'd fail "locked".
            conn.execute_batch(&format!(
                "PRAGMA busy_timeout={}; PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE",
                UPGRADE_WAIT.as_millis()
            ))?;
            version = user_version(&conn)?;
            if version > 0 && version < ladder.version {
                eprintln!("rq: upgrading the index (one-time)…");
            }
            // v23 reshapes tables the steps before it write to, so a database
            // that already has its shape resumes there: an rq before 0.54.1
            // lowered the version of any newer database it opened.
            let reshaped: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'checkout_files')",
                [],
                |r| r.get(0),
            )?;
            if reshaped && version > 0 {
                version = version.max(23);
            }
        }
        // Never lowered, never migrated: a newer rq's database stays as it is.
        if version > ladder.version {
            return Err(Refusal::Newer(version));
        }
        if version < 0 {
            return Err(Refusal::Broken {
                upgrading: false,
                reason: format!("schema version {version}"),
            });
        }
        if version == 0 {
            // fresh database — SCHEMA is already at the current version
            conn.execute_batch(ladder.fresh)?;
            stamp(&conn, "created_by")?;
        } else {
            // cumulative migrations for existing databases
            for (v, step) in ladder.steps {
                if version < *v {
                    apply(&conn, step).map_err(|e| Refusal::upgrade(version, e))?;
                }
            }
        }
        let wrote = version < ladder.version;
        if wrote {
            stamp(&conn, "schema_by")?;
            conn.pragma_update(None, "user_version", ladder.version)?;
        }
        if upgrading {
            conn.execute_batch(&format!(
                "COMMIT; PRAGMA foreign_keys=ON; PRAGMA busy_timeout={}",
                BUSY_WAIT.as_millis()
            ))?;
        }
        // Also the first read of the schema, which is where a damaged or
        // truncated file shows itself.
        if let Some(table) = missing_table(&conn, ladder.fresh)? {
            return Err(Refusal::Broken {
                upgrading: false,
                reason: format!("no {table} table"),
            });
        }
        Ok((Store { conn }, wrote))
    }

    /// Insert or update a repository, returning its id.
    pub(crate) fn upsert_repository(
        &self,
        identity: &impl std::fmt::Display,
        default_branch: Option<&str>,
    ) -> Result<i64> {
        let now = now_unix();
        let id = self.conn.query_row(
            "INSERT INTO repositories (identity, default_branch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(identity) DO UPDATE SET
               default_branch = COALESCE(excluded.default_branch, repositories.default_branch),
               updated_at = excluded.updated_at
             RETURNING id",
            params![identity.to_string(), default_branch, now],
            |r| r.get(0),
        )?;
        names::start(&self.conn, id)?;
        Ok(id)
    }

    /// Record (or update) a local checkout of a repository. A root that now
    /// answers to another identity (its remote changed) moves with its map.
    pub(crate) fn upsert_checkout(
        &self,
        repository_id: i64,
        root_path: &str,
        branch: Option<&str>,
    ) -> Result<Checkout> {
        let prior = self.checkout(root_path)?;
        if prior.is_some_and(|c| c.repo != repository_id) {
            self.forget_checkout(root_path)?;
        }
        let id = self.conn.query_row(
            "INSERT INTO checkouts (repository_id, root_path, current_branch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(root_path) DO UPDATE SET current_branch = excluded.current_branch
             RETURNING id",
            params![repository_id, root_path, branch],
            |r| r.get(0),
        )?;
        Ok(Checkout {
            id,
            repo: repository_id,
        })
    }

    /// Record the repository and its checkout at `root_path` as a pass starts:
    /// its first writes, under the write lock taken up front, so a busy writer
    /// costs one wait rather than one per statement.
    pub(crate) fn register_checkout(
        &self,
        identity: &impl std::fmt::Display,
        branch: Option<&str>,
        root_path: &str,
    ) -> Result<Checkout> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let repo = self.upsert_repository(identity, branch)?;
        // a root that moved to another identity is forgotten in its own transaction
        if self.checkout(root_path)?.is_some_and(|c| c.repo != repo) {
            tx.commit()?;
            return self.upsert_checkout(repo, root_path, branch);
        }
        let checkout = self.upsert_checkout(repo, root_path, branch)?;
        tx.commit()?;
        Ok(checkout)
    }

    /// A repository with one checkout, rooted at its identity's text — the
    /// unit most tests write to.
    #[cfg(test)]
    pub(crate) fn test_checkout(&self, identity: &impl std::fmt::Display) -> Checkout {
        let repo = self.upsert_repository(identity, None).unwrap();
        self.upsert_checkout(repo, &identity.to_string(), None)
            .unwrap()
    }

    /// The checkout rooted at `root` (its canonical path), if one was indexed.
    pub(crate) fn checkout(&self, root: &str) -> Result<Option<Checkout>> {
        self.conn
            .prepare_cached("SELECT id, repository_id FROM checkouts WHERE root_path = ?1")?
            .query_row(params![root], |r| {
                Ok(Checkout {
                    id: r.get(0)?,
                    repo: r.get(1)?,
                })
            })
            .optional()
    }

    /// True if `path` is already indexed at this exact content hash — the
    /// incremental-skip check.
    pub(crate) fn file_unchanged(
        &self,
        checkout: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<bool> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT fi.content_hash FROM checkout_files cf JOIN files fi ON fi.id = cf.file_id
                 WHERE cf.checkout_id = ?1 AND cf.path = ?2",
                params![checkout, path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(content_hash))
    }

    /// One file's stored mtime: `None` when the file isn't indexed, `Some(None)`
    /// when it is but was stored without one.
    pub(crate) fn file_mtime(&self, checkout: i64, path: &str) -> Result<Option<Option<i64>>> {
        self.conn
            .prepare_cached(
                "SELECT mtime FROM checkout_files WHERE checkout_id = ?1 AND path = ?2",
            )?
            .query_row(params![checkout, path], |r| r.get(0))
            .optional()
    }

    /// Record a file's current mtime without touching its symbols — for a file
    /// whose content was confirmed unchanged.
    pub(crate) fn set_file_mtime(
        &self,
        checkout: i64,
        path: &str,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE checkout_files SET mtime = ?3 WHERE checkout_id = ?1 AND path = ?2",
            )?
            .execute(params![checkout, path, mtime])?;
        Ok(())
    }

    /// Indexed path → stored mtime for a checkout. The budgeted warm pass uses
    /// this to skip unchanged files with a cheap `stat` (no read or re-hash).
    pub(crate) fn file_mtimes(&self, checkout: i64) -> Result<HashMap<String, Option<i64>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime FROM checkout_files WHERE checkout_id = ?1")?;
        let rows = stmt.query_map(params![checkout], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, mtime) = row?;
            map.insert(path, mtime);
        }
        Ok(map)
    }

    /// Path → the content hashes of every version the repo holds at it: a
    /// file whose hash is here needs no parse, only a map row.
    pub(crate) fn versions(&self, repository_id: i64) -> Result<HashMap<String, Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, content_hash FROM files WHERE repository_id = ?1")?;
        let rows = stmt.query_map(params![repository_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (path, hash) = row?;
            map.entry(path).or_default().push(hash);
        }
        Ok(map)
    }

    /// Replace all symbols for one hand-written file — the single-file form of
    /// [`Store::replace_files`] (same upsert, hash-skip, and batching).
    #[cfg(test)]
    pub(crate) fn replace_file_symbols(
        &mut self,
        checkout: Checkout,
        path: &str,
        language: &str,
        mtime: Option<i64>,
        content_hash: &str,
        symbols: &[Symbol],
    ) -> Result<()> {
        self.replace_files(
            checkout,
            &[FileSymbols {
                path: path.to_string(),
                language: language.to_string(),
                mtime,
                content_hash: content_hash.to_string(),
                generated: false,
                symbols: Some(symbols.to_vec()),
            }],
        )?;
        Ok(())
    }

    /// Write many parsed files, one transaction per chunk — a batched `fsync`
    /// instead of one per file, while bounding how much a single transaction
    /// holds (a cold index of a huge repo would otherwise be one enormous txn).
    /// A file whose content hash already matches the checkout's is skipped (not
    /// rewritten); one whose version the repo already holds is mapped, not
    /// stored again. The version a path stops using is deleted once no
    /// checkout maps it.
    pub(crate) fn replace_files(
        &mut self,
        checkout: Checkout,
        files: &[FileSymbols],
    ) -> Result<Written> {
        /// Files per transaction — bounds memory and WAL frame size on a big index.
        const BATCH: usize = 512;

        let repository_id = checkout.repo;
        let mut written = Written::default();
        for chunk in files.chunks(BATCH) {
            // Immediate: this reads before it writes, and a deferred upgrade
            // fails at once when another writer commits first.
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (mut fresh, mut fresh_files) = (Vec::new(), Vec::new());
            {
                let mut current = tx.prepare(
                    "SELECT cf.file_id, fi.content_hash FROM checkout_files cf
                     JOIN files fi ON fi.id = cf.file_id
                     WHERE cf.checkout_id = ?1 AND cf.path = ?2",
                )?;
                let mut touch = tx.prepare(
                    "UPDATE checkout_files SET mtime = ?3 WHERE checkout_id = ?1 AND path = ?2",
                )?;
                let mut version = tx.prepare(
                    "SELECT id FROM files WHERE repository_id = ?1 AND path = ?2 AND content_hash = ?3",
                )?;
                let mut path_known = tx.prepare(
                    "SELECT 1 FROM files WHERE repository_id = ?1 AND path = ?2 LIMIT 1",
                )?;
                let mut add_version = tx.prepare(
                    "INSERT INTO files (repository_id, path, language, content_hash, generated)
                     VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                )?;
                let mut map = tx.prepare(
                    "INSERT INTO checkout_files (checkout_id, path, file_id, mtime)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(checkout_id, path) DO UPDATE SET
                       file_id = excluded.file_id,
                       mtime = excluded.mtime",
                )?;
                let mut insert = tx.prepare(
                    "INSERT INTO symbols
                       (repository_id, file_id, name, name_lower, kind, language, line, end_line,
                        parent, visibility, stub, singleton)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )?;
                // names new to the repo, for its name index
                let indexing = names::current(&tx, repository_id)?;
                let mut checked: std::collections::HashSet<&str> = Default::default();
                for f in chunk {
                    // content unchanged (e.g. mtime moved but bytes didn't): skip
                    // the rewrite, but refresh the stat columns — otherwise a
                    // touched (or racily-indexed) file re-parses on every warm
                    let stored: Option<(i64, String)> = current
                        .query_row(params![checkout.id, f.path], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    if stored.as_ref().is_some_and(|(_, h)| *h == f.content_hash) {
                        touch.execute(params![checkout.id, f.path, f.mtime])?;
                        continue;
                    }
                    let held: Option<i64> = version
                        .query_row(params![repository_id, f.path, f.content_hash], |r| r.get(0))
                        .optional()?;
                    let file_id = match (held, &f.symbols) {
                        (Some(id), _) => id,
                        (None, None) => {
                            written.unmapped.push(f.path.clone());
                            continue;
                        }
                        (None, Some(symbols)) => {
                            if indexing
                                && path_known
                                    .query_row(params![repository_id, f.path], |_| Ok(()))
                                    .optional()?
                                    .is_none()
                            {
                                fresh_files.push(f.path.clone());
                            }
                            if indexing {
                                for s in symbols {
                                    if checked.insert(&s.name)
                                        && !names::known(&tx, repository_id, &s.name)?
                                    {
                                        fresh.push(s.name.clone());
                                    }
                                }
                            }
                            let id: i64 = add_version.query_row(
                                params![
                                    repository_id,
                                    f.path,
                                    f.language,
                                    f.content_hash,
                                    f.generated
                                ],
                                |r| r.get(0),
                            )?;
                            for s in symbols {
                                insert.execute(params![
                                    repository_id,
                                    id,
                                    s.name,
                                    s.name.to_lowercase(),
                                    s.kind.as_str(),
                                    s.language,
                                    s.line,
                                    s.end_line,
                                    s.parent,
                                    s.visibility,
                                    s.stub,
                                    s.singleton,
                                ])?;
                            }
                            written.symbols += symbols.len();
                            written.versions += 1;
                            id
                        }
                    };
                    map.execute(params![checkout.id, f.path, file_id, f.mtime])?;
                    if let Some((old, _)) = stored {
                        release(&tx, old)?;
                    }
                    written.files += 1;
                }
            }
            names::append(&tx, repository_id, names::Keys::Names, &fresh)?;
            names::append(&tx, repository_id, names::Keys::Files, &fresh_files)?;
            tx.commit()?;
        }
        Ok(written)
    }

    /// Record indexing coverage for a checkout (scope `full`).
    #[cfg(test)]
    pub(crate) fn set_coverage(
        &self,
        checkout: i64,
        files_seen: i64,
        files_indexed: i64,
        status: Coverage,
    ) -> Result<()> {
        let mark = self.coverage_mark(checkout)?;
        self.set_coverage_since(checkout, files_seen, files_indexed, status, &mark)?;
        Ok(())
    }

    /// The coverage row as a pass found it when it began: `(status,
    /// last_indexed_at)`, or `None` before any pass finished.
    pub(crate) fn coverage_mark(&self, checkout: i64) -> Result<CoverageMark> {
        self.conn
            .query_row(
                "SELECT status, last_indexed_at FROM coverage
                 WHERE checkout_id = ?1 AND scope = 'full'",
                params![checkout],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Record coverage at the end of a pass that began at `mark`. A pass that
    /// didn't complete leaves alone a `complete` another writer recorded since
    /// `mark`: it would strand the checkout at `warming` with nothing indexing.
    /// Compare-and-set in the one upsert, so no writer lands in between.
    /// Returns whether the row was written.
    pub(crate) fn set_coverage_since(
        &self,
        checkout: i64,
        files_seen: i64,
        files_indexed: i64,
        status: Coverage,
        mark: &CoverageMark,
    ) -> Result<bool> {
        let now = now_unix();
        let (mark_status, mark_at) = match mark {
            Some((s, at)) => (Some(*s), Some(*at)),
            None => (None, None),
        };
        let written = self.conn.execute(
            "INSERT INTO coverage
               (checkout_id, scope, files_seen, files_indexed, status, last_indexed_at)
             VALUES (?1, 'full', ?2, ?3, ?4, ?5)
             ON CONFLICT(checkout_id, scope) DO UPDATE SET
               files_seen = excluded.files_seen,
               files_indexed = excluded.files_indexed,
               status = excluded.status,
               last_indexed_at = excluded.last_indexed_at
             WHERE excluded.status = 'complete'
                OR coverage.status <> 'complete'
                OR (coverage.status IS ?6 AND coverage.last_indexed_at IS ?7)",
            params![
                checkout,
                files_seen,
                files_indexed,
                status,
                now,
                mark_status,
                mark_at
            ],
        )?;
        Ok(written > 0)
    }

    /// Set the last-commit time for a checkout's files, from a path → unix-ts
    /// map (git log). Files not in the map are left untouched.
    pub(crate) fn set_file_git_ts(
        &mut self,
        checkout: i64,
        times: &HashMap<String, i64>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE checkout_files SET git_ts = ?3 WHERE checkout_id = ?1 AND path = ?2",
            )?;
            for (path, ts) in times {
                stmt.execute(params![checkout, path, ts])?;
            }
        }
        tx.commit()
    }

    /// Every checkout with its repository, coverage status and current totals.
    /// Every finished pass writes coverage, so one without it is mid-way
    /// through (or was cut short in) its first pass, `warming`, or holds
    /// nothing at all, `unindexed`: an upgrade registers a repo's other
    /// checkouts that way, and only a search in one indexes it.
    pub(crate) fn coverage_overview(&self) -> Result<Vec<CoverageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.identity, co.root_path,
                    COALESCE(c.status, CASE WHEN EXISTS
                      (SELECT 1 FROM checkout_files cf WHERE cf.checkout_id = co.id)
                      THEN 'warming' ELSE 'unindexed' END),
                    (SELECT COUNT(*) FROM checkout_files cf WHERE cf.checkout_id = co.id),
                    (SELECT COUNT(*) FROM checkout_files cf
                       JOIN symbols s ON s.file_id = cf.file_id
                       WHERE cf.checkout_id = co.id)
             FROM checkouts co
             JOIN repositories r ON r.id = co.repository_id
             LEFT JOIN coverage c ON c.checkout_id = co.id AND c.scope = 'full'
             ORDER BY r.identity, co.root_path",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CoverageRow {
                    identity: r.get(0)?,
                    root: r.get(1)?,
                    status: r.get(2)?,
                    files: r.get(3)?,
                    of: None,
                    phase: None,
                    phase_secs: None,
                    symbols: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<CoverageRow>>>()?;
        // A pass reads for a query's name before it writes a file, so a
        // checkout a live pass is filling may hold none yet.
        Ok(rows
            .into_iter()
            .map(|mut row| {
                if row.status != Coverage::Complete {
                    let (pids, span) = self.passes(&row.root).unwrap_or_default();
                    if row.status == Coverage::Unindexed
                        && pids.iter().any(|&p| pid_alive(i64::from(p)))
                    {
                        row.status = Coverage::Warming;
                    }
                    row.of = span.map(|s| s.max(row.files));
                    if let Some((phase, since)) = self.pass_phase(&row.root) {
                        row.phase = Some(phase);
                        row.phase_secs = Some(phase_secs(since));
                    }
                }
                row
            })
            .collect())
    }

    /// The roots of every checkout whose index isn't complete, as `--status`
    /// tells them: what a miss across checkouts can't vouch for.
    pub(crate) fn incomplete_roots(&self) -> Result<Vec<String>> {
        self.conn
            .prepare(
                "SELECT co.root_path FROM checkouts co \
                 LEFT JOIN coverage c ON c.checkout_id = co.id AND c.scope = 'full' \
                 WHERE c.status IS NOT 'complete' ORDER BY co.root_path",
            )?
            .query_map([], |r| r.get(0))?
            .collect()
    }

    /// The normalized identity of a repository by one of its checkout roots, if
    /// known — lets the hot path resolve identity from the cache instead of
    /// forking `git remote`. `root` should be the canonical work-tree path.
    pub(crate) fn identity_for_root(&self, root: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT r.identity FROM repositories r
                 JOIN checkouts c ON c.repository_id = r.id
                 WHERE c.root_path = ?1",
                params![root],
                |r| r.get(0),
            )
            .optional()
    }

    /// The id of a repository by its normalized identity, if known.
    pub(crate) fn repository_id(&self, identity: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM repositories WHERE identity = ?1",
                params![identity],
                |r| r.get(0),
            )
            .optional()
    }

    /// Coverage status for the checkout at `root` (`warming`/`complete`), or
    /// `None` if no index pass has finished for it.
    pub(crate) fn coverage_status(&self, root: &str) -> Result<Option<Coverage>> {
        self.conn
            .query_row(
                "SELECT c.status FROM coverage c
                 JOIN checkouts co ON co.id = c.checkout_id
                 WHERE co.root_path = ?1 AND c.scope = 'full'",
                params![root],
                |r| r.get(0),
            )
            .optional()
    }

    /// Whether a checkout has any indexed file — an existence check, where
    /// [`checkout_totals`](Self::checkout_totals) would count every symbol.
    pub(crate) fn checkout_has_files(&self, checkout: i64) -> Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM checkout_files WHERE checkout_id = ?1)",
            params![checkout],
            |r| r.get(0),
        )
    }

    /// Current indexed totals for a checkout: (files, symbols).
    pub(crate) fn checkout_totals(&self, checkout: i64) -> Result<(i64, i64)> {
        self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM checkout_files WHERE checkout_id = ?1),
                    (SELECT COUNT(*) FROM checkout_files cf
                       JOIN symbols s ON s.file_id = cf.file_id
                       WHERE cf.checkout_id = ?1)",
            params![checkout],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// A repository's totals as `--status` shows them, summed over its
    /// checkouts: (files, symbols). Versions would count a file once per edit
    /// any checkout holds.
    pub(crate) fn repo_totals(&self, repository_id: i64) -> Result<(i64, i64)> {
        self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM checkout_files cf
                       JOIN checkouts co ON co.id = cf.checkout_id
                       WHERE co.repository_id = ?1),
                    (SELECT COUNT(*) FROM checkout_files cf
                       JOIN checkouts co ON co.id = cf.checkout_id
                       JOIN symbols s ON s.file_id = cf.file_id
                       WHERE co.repository_id = ?1)",
            params![repository_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// Every symbol defined in one file (repo-relative path), in line order — a
    /// structural outline rather than a ranked search. Backed by `idx_symbols_file`.
    pub(crate) fn symbols_in_file(&self, checkout: i64, path: &str) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE cf.checkout_id = ?1 AND cf.path = ?2 \
             ORDER BY s.line, s.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![checkout, path], row_to_candidate)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.1);
        }
        Ok(out)
    }

    /// Every checkout root recorded for a repository, newest first.
    pub(crate) fn checkout_roots(&self, repository_id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT root_path FROM checkouts WHERE repository_id = ?1 ORDER BY id DESC")?;
        let rows = stmt.query_map(params![repository_id], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every checkout root recorded, of any repository.
    pub(crate) fn all_checkout_roots(&self) -> Result<Vec<String>> {
        self.conn
            .prepare("SELECT root_path FROM checkouts ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect()
    }

    /// Forget the checkout at `root_path` — a stale binding (the repo moved),
    /// a root gone from disk, or a `--drop`: its map, coverage and caches, and
    /// every version only it mapped. Its other checkouts are untouched; with
    /// none left, the repository goes too.
    pub(crate) fn forget_checkout(&self, root_path: &str) -> Result<()> {
        let Some(checkout) = self.checkout(root_path)? else {
            return Ok(());
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mapped: Vec<i64> = tx
            .prepare("SELECT DISTINCT file_id FROM checkout_files WHERE checkout_id = ?1")?
            .query_map(params![checkout.id], |r| r.get(0))?
            .collect::<Result<_>>()?;
        tx.execute(
            "DELETE FROM checkout_files WHERE checkout_id = ?1",
            params![checkout.id],
        )?;
        for id in mapped {
            release(&tx, id)?;
        }
        tx.execute(
            "DELETE FROM coverage WHERE checkout_id = ?1",
            params![checkout.id],
        )?;
        for key in checkout_meta_keys(checkout.id, root_path) {
            tx.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        }
        for kind in ["pass", "phase"] {
            let marks = format!("{kind}:{root_path}:");
            tx.execute(
                "DELETE FROM meta WHERE substr(key, 1, ?2) = ?1",
                params![marks, marks.len() as i64],
            )?;
        }
        tx.execute("DELETE FROM checkouts WHERE id = ?1", params![checkout.id])?;
        drop_if_unchecked_out(&tx, checkout.repo)?;
        tx.commit()
    }

    /// Drop a file from a checkout — deleted on disk — and its version once no
    /// checkout maps it.
    pub(crate) fn forget_file(&mut self, checkout: i64, path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let file_id: Option<i64> = tx
            .query_row(
                "DELETE FROM checkout_files WHERE checkout_id = ?1 AND path = ?2 RETURNING file_id",
                params![checkout, path],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = file_id {
            release(&tx, id)?;
        }
        tx.commit()
    }

    /// Drop a repository entirely — the inverse of indexing it: every checkout
    /// of it, its versions, symbols, name index and the repository row.
    pub(crate) fn drop_repository(&mut self, repository_id: i64) -> Result<()> {
        for root in self.checkout_roots(repository_id)? {
            self.forget_checkout(&root)?;
        }
        // a repo no checkout held (an upgrade left it) never saw one go
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        drop_if_unchecked_out(&tx, repository_id)?;
        tx.commit()
    }

    // ----- usage observability -----

    /// Count a search in `usage_daily`. Observability, so a busy store skips
    /// it rather than hold up the exit.
    pub(crate) fn record_search(&self, rec: &SearchRecord) -> Result<()> {
        let (miss, warming) = match rec.status {
            Verdict::Hit => (0, 0),
            Verdict::Miss => (1, 0),
            Verdict::Warming => (0, 1),
        };
        let on_complete = i64::from(rec.coverage == Some(Coverage::Complete));
        let live = i64::from(rec.live);
        // Local date, not UTC: an evening search on the US west coast would
        // otherwise be filed under tomorrow, which makes a per-day report
        // quietly wrong for a third of the waking day.
        self.waiting_at_most(BOOKKEEPING_WAIT, || self.conn.execute(
            "INSERT INTO usage_daily (day, source, flags, searches, misses, warming, on_complete, live)
             VALUES (date(?1, 'unixepoch', 'localtime'), ?2, ?3, 1, ?4, ?5, ?6, ?7)
             ON CONFLICT(day, source, flags) DO UPDATE SET
               searches = searches + 1,
               misses = misses + excluded.misses,
               warming = warming + excluded.warming,
               on_complete = on_complete + excluded.on_complete,
               live = live + excluded.live",
            params![
                now_unix(),
                rec.source,
                rec.flags,
                miss,
                warming,
                on_complete,
                live
            ],
        ))?;
        Ok(())
    }

    /// Usage counts, newest day first.
    pub(crate) fn usage_overview(&self) -> Result<Vec<UsageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, source, flags, searches, misses, warming, on_complete, live
             FROM usage_daily ORDER BY day DESC, searches DESC, source, flags",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UsageRow {
                    day: r.get(0)?,
                    source: r.get(1)?,
                    flags: r.get(2)?,
                    searches: r.get(3)?,
                    misses: r.get(4)?,
                    warming: r.get(5)?,
                    on_complete: r.get(6)?,
                    live: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The git HEAD sha recorded at the last complete index of a repo, if any —
    /// used to detect that the committed tree is unchanged since indexing.
    pub(crate) fn indexed_head(&self, checkout: i64) -> Result<Option<String>> {
        self.meta_get(&format!("head:{checkout}"))
    }

    /// Record the git HEAD sha at a complete index.
    pub(crate) fn set_indexed_head(&self, checkout: i64, head: &str) -> Result<()> {
        self.meta_set(&format!("head:{checkout}"), head)
    }

    /// Files the index may hold in a state other than HEAD's: the edits it has
    /// indexed. A discarded edit (`git checkout -- f`) leaves `f` clean, so
    /// `git status` stops naming it while the index still holds the edit; this
    /// is how the staleness check still knows to look at it.
    pub(crate) fn edited_files(&self, checkout: i64) -> Result<Vec<String>> {
        Ok(self
            .meta_get(&format!("edited:{checkout}"))?
            .map(|v| v.lines().map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Replace the edited-file set (newline-delimited: git paths can't hold one).
    pub(crate) fn set_edited_files(&self, checkout: i64, files: &[String]) -> Result<()> {
        self.meta_set(&format!("edited:{checkout}"), &files.join("\n"))
    }

    /// Add one file to the edited-file set.
    pub(crate) fn note_edited_file(&self, checkout: i64, path: &str) -> Result<()> {
        let mut files = self.edited_files(checkout)?;
        if files.iter().any(|f| f == path) {
            return Ok(());
        }
        files.push(path.to_string());
        self.set_edited_files(checkout, &files)
    }

    /// The git HEAD sha at the last commit-times capture (recency signal), if
    /// any — lets the next capture read only the commits since, or skip the
    /// `git log` entirely when HEAD hasn't moved.
    pub(crate) fn git_ts_head(&self, checkout: i64) -> Result<Option<String>> {
        self.meta_get(&format!("git_ts_head:{checkout}"))
    }

    /// Record the git HEAD sha a commit-times capture ran at.
    pub(crate) fn set_git_ts_head(&self, checkout: i64, head: &str) -> Result<()> {
        self.meta_set(&format!("git_ts_head:{checkout}"), head)
    }

    /// Claim the detached-warm lock for this process unless `held` says the
    /// current holder (pid, claimed-at) still has it. Read and write share one
    /// write transaction, so of two warmers racing for it only one wins.
    pub(crate) fn claim_warm_lock(
        &mut self,
        root: &str,
        pid: u32,
        held: impl Fn(u32, i64) -> bool,
    ) -> Result<bool> {
        let key = format!("warm_lock:{root}");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holder: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?;
        let holder = holder.and_then(|v| {
            let (pid, ts) = v.split_once(':')?;
            Some((pid.parse().ok()?, ts.parse().ok()?))
        });
        if holder.is_some_and(|(p, ts)| p != pid && held(p, ts)) {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, format!("{pid}:{}", now_unix())],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Release the detached-warm lock.
    pub(crate) fn clear_warm_lock(&self, root: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM meta WHERE key = ?1",
            params![format!("warm_lock:{root}")],
        )?;
        Ok(())
    }

    /// When a warm child last found the worktree unchanged since indexing, and
    /// the git-state stamp it saw: `(stamp, checked_at)`.
    pub(crate) fn warm_verified(&self, root: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .meta_get(&format!("warm_verified:{root}"))?
            .and_then(|v| {
                let (at, stamp) = v.split_once('\n')?;
                Some((stamp.to_string(), at.parse().ok()?))
            }))
    }

    /// Record that the worktree matched the index as of `checked_at`.
    pub(crate) fn set_warm_verified(&self, root: &str, stamp: &str, checked_at: i64) -> Result<()> {
        self.meta_set(
            &format!("warm_verified:{root}"),
            &format!("{checked_at}\n{stamp}"),
        )
    }

    /// The cached branch-changed file list for a repo: `(stamp, computed_at,
    /// files)`. Stored rather than recomputed because the git diff behind it is
    /// O(tracked files) and runs on the search path.
    /// The cached branch-file list, with what decides whether it's still good.
    pub(crate) fn branch_files_get(&self, root: &str) -> Result<Option<BranchFiles>> {
        let Some(raw) = self.meta_get(&format!("branch_files:{root}"))? else {
            return Ok(None);
        };
        let mut lines = raw.lines();
        let (Some(stamp), Some(second)) = (lines.next(), lines.next()) else {
            return Ok(None);
        };
        // `at`, or `at:cost_ms` — appended rather than given its own line so
        // entries written before the cost was recorded still parse.
        let (at, cost) = match second.split_once(':') {
            Some((at, cost)) => (at, cost.parse::<u64>().ok()),
            None => (second, None),
        };
        let Ok(at) = at.parse::<i64>() else {
            return Ok(None);
        };
        Ok(Some(BranchFiles {
            stamp: stamp.to_string(),
            written_at: at,
            cost_ms: cost,
            files: lines.map(str::to_string).collect(),
        }))
    }

    pub(crate) fn branch_files_set(
        &self,
        root: &str,
        stamp: &str,
        at: i64,
        cost_ms: u64,
        files: &[String],
    ) -> Result<()> {
        // Newline-delimited: git paths can't contain one, and it beats pulling
        // in a serializer for four fields.
        let mut value = format!("{stamp}\n{at}:{cost_ms}");
        for f in files {
            value.push('\n');
            value.push_str(f);
        }
        // a cache: a busy store leaves it for the next search to rebuild
        self.waiting_at_most(BOOKKEEPING_WAIT, || {
            self.meta_set(&format!("branch_files:{root}"), &value)
        })
    }

    /// Mark a pass over the checkout at `root` as running in `pid`, and record
    /// how many source files the tree spans when the pass enumerated them. A
    /// mark whose process is gone, or that went unrenewed past the TTL, is a
    /// crashed pass's leftover, cleared here so the marks never outgrow the
    /// live writers.
    pub(crate) fn begin_pass(&mut self, root: &str, pid: u32, span: Option<usize>) -> Result<()> {
        // it reads before it writes: deferred, a busy writer fails it at once
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // another pid's mark, or phase, left by a pass that died or (a mark)
        // went unrenewed; a phase counts only beside a live mark or lock
        let mut stale: Vec<(String, u32)> = Vec::new();
        for (kind, renewed) in [("pass", true), ("phase", false)] {
            let prefix = format!("{kind}:{root}:");
            let rows = tx
                .prepare("SELECT key, value FROM meta WHERE substr(key, 1, ?2) = ?1")?
                .query_map(params![prefix, prefix.len() as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>>>()?;
            stale.extend(rows.into_iter().filter_map(|(k, at)| {
                let p = k[prefix.len()..].parse::<u32>().ok()?;
                let gone = !pid_alive(i64::from(p)) || renewed && !stamp_fresh(&at);
                (p != pid && gone).then_some((k, p))
            }));
        }
        for (key, p) in stale {
            tx.execute(
                "DELETE FROM meta WHERE key IN (?1, ?2)",
                params![key, format!("phase:{root}:{p}")],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![format!("pass:{root}:{pid}"), now_unix().to_string()],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![
                format!("phase:{root}:{pid}"),
                format!("{READING}:{}", now_unix())
            ],
        )?;
        if let Some(span) = span {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![format!("span:{root}"), span.to_string()],
            )?;
        }
        tx.commit()
    }

    /// Restamp `pid`'s mark from [`begin_pass`](Self::begin_pass), so a pass
    /// that runs past the TTL isn't read as a crashed one.
    pub(crate) fn renew_pass(&self, root: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = ?1",
            params![format!("pass:{root}:{pid}"), now_unix().to_string()],
        )?;
        Ok(())
    }

    /// Record what `pid`'s pass over the checkout at `root` is doing:
    /// [`READING`] from its start, [`FINISHING`] once past its reads, when
    /// what remains (the name index, commit times) holds `read` still.
    pub(crate) fn set_pass_phase(&self, root: &str, pid: u32, phase: &'static str) -> Result<()> {
        self.meta_set(
            &format!("phase:{root}:{pid}"),
            &format!("{phase}:{}", now_unix()),
        )
    }

    /// What the passes indexing the checkout at `root` are doing, and since
    /// when (unix seconds): `reading` while any is, as `read` then moves.
    /// `None` when no live pass records one — an older rq's records none.
    pub(crate) fn pass_phase(&self, root: &str) -> Option<(&'static str, i64)> {
        let (pids, _) = self.passes(root).ok()?;
        pids.into_iter()
            .filter(|&p| pid_alive(i64::from(p)))
            .filter_map(|p| self.meta_get(&format!("phase:{root}:{p}")).ok().flatten())
            .filter_map(|v| {
                let (phase, since) = v.split_once(':')?;
                let phase = [READING, FINISHING].into_iter().find(|&p| p == phase)?;
                Some((phase, since.parse().ok()?))
            })
            .max_by_key(|&(phase, since)| (phase == READING, since))
    }

    /// Clear `pid`'s mark from [`begin_pass`](Self::begin_pass), and record
    /// the tree's span as the pass leaves it, or forget it (a complete
    /// checkout discloses none).
    pub(crate) fn end_pass(&self, root: &str, pid: u32, span: Option<usize>) -> Result<()> {
        self.conn.execute(
            "DELETE FROM meta WHERE key IN (?1, ?2)",
            params![format!("pass:{root}:{pid}"), format!("phase:{root}:{pid}")],
        )?;
        match span {
            Some(span) => self.meta_set(&format!("span:{root}"), &span.to_string()),
            None => {
                self.conn.execute(
                    "DELETE FROM meta WHERE key = ?1",
                    params![format!("span:{root}")],
                )?;
                Ok(())
            }
        }
    }

    /// A number that changes whenever another connection commits to the
    /// database — how a waiting search tells an indexer at work from one
    /// that has stopped writing.
    pub(crate) fn data_version(&self) -> Result<i64> {
        self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))
    }

    /// Let this connection's writes wait out another writer for `wait` each
    /// time, instead of the default sized for a search's latency, calling
    /// `on_long_wait` while a wait runs past a second. For a process that runs
    /// one pass: the bound and the hook are the process's.
    pub(crate) fn wait_out_writers(
        &self,
        wait: std::time::Duration,
        on_long_wait: Option<fn()>,
    ) -> Result<()> {
        WRITER_WAIT_MS.store(
            wait.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Some(hook) = on_long_wait {
            let _ = ON_LONG_WAIT.set(hook);
        }
        self.conn.busy_handler(Some(pass_busy))
    }

    /// Keep a span counted outside a pass (an older rq's partial index), unless
    /// a pass recorded one meanwhile. Best-effort and without waiting: a busy
    /// writer means a pass, which records its own.
    pub(crate) fn keep_span(&self, root: &str, span: usize) {
        let _ = self.waiting_at_most(std::time::Duration::ZERO, || {
            self.conn.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
                params![format!("span:{root}"), span.to_string()],
            )
        });
    }

    /// Run `write` waiting out another writer for at most `wait`, then go
    /// back to [`BUSY_WAIT`]. For a search's connection, which has no
    /// [`wait_out_writers`](Self::wait_out_writers) handler to lose.
    fn waiting_at_most<T>(&self, wait: std::time::Duration, write: impl FnOnce() -> T) -> T {
        let _ = self.conn.busy_timeout(wait);
        let out = write();
        let _ = self.conn.busy_timeout(BUSY_WAIT);
        out
    }

    /// The processes marked as indexing the checkout at `root` (live or not —
    /// the caller asks), and the files its tree spanned when last enumerated.
    /// A warm child counts from its lock, which it holds across the passes it
    /// runs, not only during one. A mark or lock past its TTL is a crashed
    /// process's, whose pid may be reused, and doesn't count.
    pub(crate) fn passes(&self, root: &str) -> Result<(Vec<u32>, Option<i64>)> {
        let prefix = format!("pass:{root}:");
        let mut pids: Vec<u32> = self
            .conn
            .prepare_cached("SELECT key, value FROM meta WHERE substr(key, 1, ?2) = ?1")?
            .query_map(params![prefix, prefix.len() as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .filter_map(|row| {
                let (k, at) = row.ok()?;
                stamp_fresh(&at).then(|| k[prefix.len()..].parse().ok())?
            })
            .collect();
        if let Some(pid) = self.meta_get(&format!("warm_lock:{root}"))?.and_then(|v| {
            let (pid, at) = v.split_once(':')?;
            stamp_fresh(at).then(|| pid.parse().ok())?
        }) {
            pids.push(pid);
        }
        let span = self
            .meta_get(&format!("span:{root}"))?
            .and_then(|v| v.parse().ok());
        Ok((pids, span))
    }

    /// Whether a live process other than this one holds a pass mark or the
    /// warm lock on the checkout at `root`.
    pub(crate) fn indexed_by_others(&self, root: &str) -> bool {
        let me = std::process::id();
        self.passes(root)
            .is_ok_and(|(pids, _)| pids.iter().any(|&p| p != me && pid_alive(i64::from(p))))
    }

    /// How many files a checkout holds — [`checkout_totals`](Self::checkout_totals)
    /// without the symbol count, which on a large checkout is the slow half.
    pub(crate) fn checkout_file_count(&self, checkout: i64) -> Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM checkout_files WHERE checkout_id = ?1",
            params![checkout],
            |r| r.get(0),
        )
    }

    fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
    }

    fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Candidate symbols for a query: exact and prefix matches on
    /// `name_lower`, then every name and file stem the name index says
    /// `probe` accepts. Ranking happens in `crate::search`; this only narrows
    /// the field.
    ///
    /// When `force_fuzzy` is false and exact/prefix already matched, the name
    /// index is skipped: the relevance gate drops every fuzzy candidate once a
    /// strong (exact/prefix) hit exists, so fetching and scoring them is
    /// wasted. A wildcard query passes `force_fuzzy = true` — it isn't gated.
    ///
    /// `only` scopes every layer to one checkout. It has to be applied here,
    /// not after: each layer's `limit` is otherwise shared with every other
    /// indexed checkout, and another's exact match would trip the fast path
    /// below and skip the fuzzy recall this one needed. Unscoped, every
    /// checkout answers, and a definition several of them hold is kept once:
    /// `prefer`'s copy when it has one (see [`fold_checkouts`]).
    pub(crate) fn search_candidates(
        &self,
        query: &str,
        limit: usize,
        force_fuzzy: bool,
        only: Option<Checkout>,
        prefer: Option<i64>,
        probe: &Probe,
    ) -> Result<Vec<SymbolRow>> {
        use rusqlite::types::Value;
        // folded as `name_lower` is at index time, or a non-ASCII name misses
        let q = query.to_lowercase();
        let mut found: HashMap<(i64, i64), SymbolRow> = HashMap::new();
        let mut keep = |rows: Vec<(i64, SymbolRow)>| {
            for (id, row) in rows {
                found.entry((id, row.checkout_id)).or_insert(row);
            }
        };
        // Run one layer. `filter` holds the layer's own placeholders; the
        // scope and the cap are bound after them as the next numbered ones.
        let fetch = |filter: &str, mut args: Vec<Value>| -> Result<Vec<(i64, SymbolRow)>> {
            let scope = match only {
                Some(c) => {
                    args.push(Value::Integer(c.repo));
                    args.push(Value::Integer(c.id));
                    format!(
                        " AND s.repository_id = ?{} AND {}",
                        args.len() - 1,
                        read_from(true, args.len())
                    )
                }
                None => {
                    args.push(prefer.map_or(Value::Null, Value::Integer));
                    format!(" AND {}", read_from(false, args.len()))
                }
            };
            args.push(Value::Integer(limit as i64));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} WHERE {filter}{scope} LIMIT ?{}",
                args.len()
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_candidate)?;
            rows.collect()
        };
        let text = |s: &str| Value::Text(s.to_string());

        // exact name — always included, never displaced by the fuzzy cap
        keep(fetch("s.name_lower = ?1", vec![text(&q)])?);

        // query as a prefix — selective, so prefix matches always surface
        keep(fetch(
            "s.name_lower >= ?1 AND s.name_lower < ?2",
            vec![text(&q), text(&prefix_upper_bound(&q))],
        )?);

        // Fast path: a strong (exact/prefix) match exists, so the relevance gate
        // will discard everything fuzzy recall would add. (Wildcard queries force
        // it; they aren't gated.)
        // Except for the same identifier in another case convention
        // (`AbortHandle` for `abort_handle`): that scores as exact too, and no
        // literal layer can fetch it.
        let repo = only.map(|c| c.repo);
        if !force_fuzzy && !found.is_empty() {
            let suspended = self.ensure_name_index(repo)?;
            let mut rows = self.respelled_candidates(only, prefer, &suspended, probe, &q)?;
            for (id, row) in rows.drain(..) {
                found.entry((id, row.checkout_id)).or_insert(row);
            }
            return Ok(fold_checkouts(found.into_values().collect(), only, prefer));
        }

        // The name index holds exactly the names and file stems the scorer
        // accepts.
        let suspended = self.ensure_name_index(repo)?;
        for (id, row) in self.named_candidates(only, prefer, &suspended, probe, limit)? {
            found.entry((id, row.checkout_id)).or_insert(row);
        }
        for (id, row) in self.filed_candidates(only, prefer, &suspended, probe, limit)? {
            found.entry((id, row.checkout_id)).or_insert(row);
        }
        Ok(fold_checkouts(found.into_values().collect(), only, prefer))
    }
}

/// Drop a version, and its symbols, once no checkout maps it: the one place
/// versions go, so an unreferenced one can't outlive the write that let go.
fn release(conn: &Connection, file_id: i64) -> Result<()> {
    let mapped: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM checkout_files WHERE file_id = ?1)")?
        .query_row(params![file_id], |r| r.get(0))?;
    if !mapped {
        conn.prepare_cached("DELETE FROM symbols WHERE file_id = ?1")?
            .execute(params![file_id])?;
        conn.prepare_cached("DELETE FROM files WHERE id = ?1")?
            .execute(params![file_id])?;
    }
    Ok(())
}

/// A repository is the checkouts that hold it: once none do, its versions
/// and name index are unreachable, so they go with the repository row.
fn drop_if_unchecked_out(conn: &Connection, repository_id: i64) -> Result<()> {
    let held: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM checkouts WHERE repository_id = ?1)")?
        .query_row(params![repository_id], |r| r.get(0))?;
    if held {
        return Ok(());
    }
    for sql in [
        "DELETE FROM name_sigs WHERE repository_id = ?1",
        "DELETE FROM name_index WHERE repository_id = ?1",
        "DELETE FROM symbols WHERE repository_id = ?1",
        "DELETE FROM files WHERE repository_id = ?1",
        "DELETE FROM repositories WHERE id = ?1",
    ] {
        conn.prepare_cached(sql)?.execute(params![repository_id])?;
    }
    Ok(())
}

/// The `meta` keys one checkout's caches live under: by id where the index
/// writes them, by root where a search reads them before any pass registered
/// the checkout.
fn checkout_meta_keys(id: i64, root: &str) -> [String; 7] {
    [
        format!("span:{root}"),
        format!("head:{id}"),
        format!("edited:{id}"),
        format!("git_ts_head:{id}"),
        format!("warm_lock:{root}"),
        format!("warm_verified:{root}"),
        format!("branch_files:{root}"),
    ]
}

/// Which definition a row is, across checkouts: its repo, path, name, kind
/// and parent. Not its line, which an edit above it moves.
pub(super) type Def<'a> = (i64, &'a str, &'a str, &'a str, Option<&'a str>);

pub(super) fn def_key(r: &SymbolRow) -> Def<'_> {
    let (file, name, kind) = (r.file.as_str(), r.name.as_str(), r.kind.as_str());
    (r.repository_id, file, name, kind, r.parent.as_deref())
}

/// Unscoped rows fold across checkouts: a definition several checkouts hold —
/// the same name, kind and parent at the same path, wherever the rest of the
/// file moved it — is kept from one of them, `prefer` when it holds it, else
/// the newest, as [`read_from`] picks for one version. Within that checkout
/// every row stays.
fn fold_checkouts(
    rows: Vec<SymbolRow>,
    only: Option<Checkout>,
    prefer: Option<i64>,
) -> Vec<SymbolRow> {
    if only.is_some() {
        return rows;
    }
    let rank = |checkout: i64| (Some(checkout) != prefer, std::cmp::Reverse(checkout));
    let mut winner: HashMap<Def, i64> = HashMap::new();
    for r in &rows {
        winner
            .entry(def_key(r))
            .and_modify(|w| {
                if rank(r.checkout_id) < rank(*w) {
                    *w = r.checkout_id;
                }
            })
            .or_insert(r.checkout_id);
    }
    let keep: Vec<bool> = rows
        .iter()
        .map(|r| winner[&def_key(r)] == r.checkout_id)
        .collect();
    rows.into_iter()
        .zip(keep)
        .filter_map(|(r, keep)| keep.then_some(r))
        .collect()
}

fn row_to_candidate(r: &rusqlite::Row) -> Result<(i64, SymbolRow)> {
    Ok((
        r.get(0)?,
        SymbolRow {
            name: r.get(1)?,
            kind: r.get(2)?,
            language: r.get(3)?,
            file: r.get(4)?,
            line: r.get(5)?,
            end_line: r.get(6)?,
            parent: r.get(7)?,
            repository_id: r.get(8)?,
            repo_identity: r.get(9)?,
            mtime: r.get(10)?,
            git_ts: r.get(11)?,
            visibility: r.get(12)?,
            stub: r.get(14)?,
            generated: r.get(13)?,
            checkout_id: r.get(15)?,
            root: r.get(16)?,
            singleton: r.get(17)?,
        },
    ))
}

/// The exclusive upper bound of everything starting with `prefix`, for a range
/// seek on `name_lower`.
///
/// `LIKE 'x%'` looks like it should use the index and doesn't: SQLite turns
/// `LIKE` into a range scan only when the index collation matches the
/// operator's case sensitivity, and case-insensitive `LIKE` against a `BINARY`
/// index falls back to scanning every row. `EXPLAIN QUERY PLAN` says `SCAN` for
/// both `LIKE 'x%'` and `LIKE 'x%' ESCAPE '\'`, and `SEARCH` for this. A range
/// is also exact: `_` in `connection_pool` is a `LIKE` wildcard that had to be
/// escaped, and a comparison reads it literally.
fn prefix_upper_bound(prefix: &str) -> String {
    // every string starting with `prefix` sorts below this one, since nothing
    // sorts above the maximum code point
    let mut upper = prefix.to_string();
    upper.push(char::MAX);
    upper
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Kind, RepoIdentity};
    use crate::tests::support::Scratch;
    use std::collections::BTreeMap;

    /// Each table's columns in docs/ARCHITECTURE.md's schema block.
    fn documented_schema() -> BTreeMap<String, Vec<String>> {
        let doc = include_str!("../../docs/ARCHITECTURE.md");
        let block = doc
            .split("```sql\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("ARCHITECTURE.md has a ```sql schema block");
        let mut tables = BTreeMap::new();
        let mut current: Option<(String, String)> = None;
        for line in block.lines() {
            let line = line.split("--").next().unwrap().trim();
            if let Some((name, body)) = &mut current {
                body.push_str(line);
                body.push(' ');
                if line.starts_with(')') {
                    tables.insert(std::mem::take(name), columns(body));
                    current = None;
                }
            } else if let Some((name, rest)) = line.split_once(" (")
                && !name.contains(' ')
            {
                match rest.strip_suffix(");") {
                    Some(body) => {
                        tables.insert(name.to_string(), columns(body));
                    }
                    None => current = Some((name.to_string(), rest.to_string())),
                }
            }
        }
        tables
    }

    /// The column names in a table body: the first word of each top-level
    /// comma-separated entry that isn't a table constraint.
    fn columns(body: &str) -> Vec<String> {
        let mut depth = 0;
        let mut entries = vec![String::new()];
        for c in body.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    entries.push(String::new());
                    continue;
                }
                _ => {}
            }
            if depth >= 0 {
                entries.last_mut().unwrap().push(c);
            }
        }
        entries
            .iter()
            .filter_map(|e| {
                e.split(|c: char| c.is_whitespace() || c == '(')
                    .find(|w| !w.is_empty())
            })
            .filter(|w| !matches!(*w, "PRIMARY" | "UNIQUE" | "FOREIGN" | "CHECK"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_documented_schema_is_the_schema() {
        let store = Store::open_in_memory().unwrap();
        let mut stmt = store
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let actual: BTreeMap<String, Vec<String>> = names
            .into_iter()
            .map(|table| {
                let mut info = store
                    .conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap();
                let cols = info
                    .query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                (table, cols)
            })
            .collect();
        assert_eq!(
            documented_schema(),
            actual,
            "docs/ARCHITECTURE.md's schema block has drifted from the schema"
        );
    }

    #[test]
    fn branch_files_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.branch_files_get("repo").unwrap().is_none());

        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        store
            .branch_files_set("repo", "123:456", 99, 42, &files)
            .unwrap();
        let hit = store.branch_files_get("repo").unwrap().unwrap();
        assert_eq!(hit.stamp, "123:456");
        assert_eq!(hit.written_at, 99);
        assert_eq!(
            hit.cost_ms,
            Some(42),
            "what it cost decides how long it's good for"
        );
        assert_eq!(hit.files, files);

        // an entry written before the cost was recorded still parses — the
        // timestamp line gained a suffix rather than a new line for exactly this
        store
            .meta_set("branch_files:old", "123:456\n99\nsrc/a.rs")
            .unwrap();
        let old = store.branch_files_get("old").unwrap().unwrap();
        assert_eq!((old.written_at, old.cost_ms), (99, None));
        assert_eq!(old.files, vec!["src/a.rs".to_string()]);

        // a later write replaces the entry rather than accumulating
        store
            .branch_files_set("repo", "789:1", 100, 0, &[])
            .unwrap();
        let hit = store.branch_files_get("repo").unwrap().unwrap();
        assert_eq!(hit.stamp, "789:1");
        assert!(
            hit.files.is_empty(),
            "an empty list is a real answer, not a miss"
        );

        // repos don't share an entry
        assert!(store.branch_files_get("other").unwrap().is_none());
    }

    fn sym(name: &str, kind: Kind, line: u32, parent: Option<&str>) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            language: "ruby".into(),
            file: "app/models/user.rb".into(),
            line,
            end_line: line,
            parent: parent.map(String::from),
            visibility: None,
            stub: false,
            singleton: false,
        }
    }

    #[test]
    fn a_batch_write_waits_out_a_concurrent_writer() {
        // A deferred batch reads first, so a writer that commits while it waits
        // for the lock invalidates its snapshot: SQLite then fails it with
        // SQLITE_BUSY at once, busy_timeout or not.
        let dir = Scratch::new("busy-store");
        let path = dir.join("rq.db");
        let mut store = Store::open(&path).unwrap();
        let repo = store.test_checkout(&RepoIdentity::Local("/tmp/busy".into()));
        let other = Store::open(&path).unwrap();
        other
            .conn
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO meta VALUES ('k', 'v');")
            .unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.conn.execute_batch("COMMIT").unwrap();
        });
        let file = FileSymbols {
            path: "app/models/user.rb".into(),
            language: "ruby".into(),
            mtime: Some(1),
            content_hash: "h".into(),
            generated: false,
            symbols: Some(vec![sym("User", Kind::Class, 1, None)]),
        };
        let written = store.replace_files(repo, &[file]).unwrap();
        holder.join().unwrap();
        assert_eq!((written.files, written.symbols), (1, 1));
    }

    #[test]
    fn a_warm_lock_is_held_only_by_a_live_pid_inside_the_ttl() {
        let me = std::process::id();
        let fresh = now_unix();
        let stale = fresh - WARM_LOCK_TTL_SECS;
        // pid 1 is another user's: kill() says EPERM, which is still alive
        let cases = [
            (me, fresh, true),
            (1, fresh, true),
            (me, stale, false),
            (0, fresh, false),
            (u32::MAX, fresh, false),
        ];
        for (pid, ts, held) in cases {
            assert_eq!(warm_lock_held(pid, ts), held, "pid {pid} at {ts}");
        }
    }

    #[test]
    fn one_of_several_racing_warmers_claims_the_lock() {
        let dir = Scratch::new("warm-lock");
        let path = dir.join("rq.db");
        drop(Store::open(&path).unwrap());
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let claims: Vec<_> = (0..8u32)
            .map(|pid| {
                let (path, start) = (path.clone(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    start.wait();
                    // every holder counts as live: only an empty lock is claimable
                    store.claim_warm_lock("repo", pid + 1, |_, _| true).unwrap()
                })
            })
            .collect();
        let won = claims
            .into_iter()
            .filter_map(|h| h.join().unwrap().then_some(()))
            .count();
        assert_eq!(won, 1);

        // a holder the caller judges gone (dead pid, stale stamp) is taken over
        let mut store = Store::open(&path).unwrap();
        assert!(store.claim_warm_lock("repo", 99, |_, _| false).unwrap());
        assert!(!store.claim_warm_lock("repo", 100, |_, _| true).unwrap());
    }

    #[test]
    fn a_pass_mark_waits_out_a_concurrent_writer() {
        // read-then-write in a deferred transaction can't wait for the lock:
        // SQLite fails the upgrade with SQLITE_BUSY, busy_timeout or not
        let dir = Scratch::new("busy-pass");
        let path = dir.join("rq.db");
        let mut store = Store::open(&path).unwrap();
        let other = Store::open(&path).unwrap();
        other
            .conn
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO meta VALUES ('k', 'v');")
            .unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.conn.execute_batch("COMMIT").unwrap();
        });
        let marked = store.begin_pass("/repo", 7, Some(3));
        holder.join().unwrap();
        marked.unwrap();
        assert_eq!(store.passes("/repo").unwrap(), (vec![7], Some(3)));
    }

    #[test]
    fn a_pass_clears_the_marks_of_passes_that_died() {
        let mut store = Store::open_in_memory().unwrap();
        let dead = 4_000_000; // above any pid the OS hands out
        store.begin_pass("/repo", dead, Some(10)).unwrap();
        store.begin_pass("/repo", 7, None).unwrap();
        let (pids, span) = store.passes("/repo").unwrap();
        assert_eq!(pids, vec![7], "the dead pass's mark is gone");
        assert_eq!(
            span,
            Some(10),
            "a pass that didn't count keeps the last span"
        );

        store.end_pass("/repo", 7, Some(12)).unwrap();
        store.claim_warm_lock("/repo", 8, |_, _| false).unwrap();
        assert_eq!(
            store.passes("/repo").unwrap(),
            (vec![8], Some(12)),
            "a warm child counts, and the pass's recount stands"
        );

        store.end_pass("/repo", 8, None).unwrap();
        assert_eq!(store.passes("/repo").unwrap().1, None, "complete: no span");

        // a live pid on a lock past its TTL is a reused pid, not a warmer
        store
            .meta_set("warm_lock:/old", &format!("{}:0", std::process::id()))
            .unwrap();
        assert!(store.passes("/old").unwrap().0.is_empty());
    }

    #[test]
    fn a_pass_says_whether_it_is_reading_or_finishing() {
        let mut store = Store::open_in_memory().unwrap();
        let me = std::process::id();
        let dead = 4_000_000; // above any pid the OS hands out
        store.begin_pass("/repo", dead, None).unwrap();
        store.set_pass_phase("/repo", dead, FINISHING).unwrap();
        assert_eq!(store.pass_phase("/repo"), None, "a dead pass says nothing");

        store.begin_pass("/repo", me, None).unwrap();
        assert!(
            store
                .meta_get(&format!("phase:/repo:{dead}"))
                .unwrap()
                .is_none(),
            "cleared with the dead pass's mark"
        );
        assert_eq!(store.pass_phase("/repo").map(|p| p.0), Some(READING));
        store.set_pass_phase("/repo", me, FINISHING).unwrap();
        assert_eq!(store.pass_phase("/repo").map(|p| p.0), Some(FINISHING));
        store.end_pass("/repo", me, None).unwrap();
        assert_eq!(store.pass_phase("/repo"), None);
        assert!(
            store
                .meta_get(&format!("phase:/repo:{me}"))
                .unwrap()
                .is_none(),
            "a finished pass leaves no phase behind"
        );
    }

    #[test]
    fn a_pass_mark_unrenewed_past_its_ttl_is_a_crashed_passs() {
        // a crashed pass whose pid was reused by a live process
        let mut store = Store::open_in_memory().unwrap();
        let me = std::process::id();
        let stale = now_unix() - WARM_LOCK_TTL_SECS - 1;
        store
            .meta_set(&format!("pass:/repo:{me}"), &stale.to_string())
            .unwrap();
        assert!(store.passes("/repo").unwrap().0.is_empty());
        assert!(!store.indexed_by_others("/repo"));

        // a running pass renews its mark, and a stale one is cleared
        store.renew_pass("/repo", me).unwrap();
        assert_eq!(store.passes("/repo").unwrap().0, vec![me]);
        store
            .meta_set(&format!("pass:/repo:{me}"), &stale.to_string())
            .unwrap();
        store.begin_pass("/repo", 7, None).unwrap();
        assert_eq!(store.passes("/repo").unwrap().0, vec![7]);
    }

    #[test]
    fn a_burst_of_openers_shares_a_fresh_database() {
        let dir = Scratch::new("burst");
        let path = dir.join("rq.db");
        let clean = || remove(&path);
        for _ in 0..10 {
            clean();
            let start = std::sync::Arc::new(std::sync::Barrier::new(16));
            let opens: Vec<_> = (0..16)
                .map(|_| {
                    let (path, start) = (path.clone(), std::sync::Arc::clone(&start));
                    std::thread::spawn(move || {
                        start.wait();
                        Store::open(&path).map(drop)
                    })
                })
                .collect();
            for open in opens {
                open.join()
                    .unwrap()
                    .expect("every opener gets a working store");
            }
        }
        clean();
    }

    /// A database file as an older rq left it: v22's schema, `rows` inserted
    /// with SQL, and `user_version` set to `version`.
    #[test]
    fn an_opener_waits_out_another_openers_upgrade() {
        let (_dir, path) = legacy("upgrade-wait", 22, "");
        // stands in for an upgrade outlasting the usual busy timeout
        let holder = Connection::open(&path).unwrap();
        holder
            .execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE")
            .unwrap();
        let opener = {
            let path = path.clone();
            std::thread::spawn(move || Store::open(&path).map(drop))
        };
        std::thread::sleep(BUSY_WAIT + std::time::Duration::from_millis(500));
        holder.execute_batch("COMMIT").unwrap();
        opener.join().unwrap().expect("waits, then upgrades");
    }

    /// A database at schema `version` holding `rows`, in a dir of its own.
    fn legacy(label: &str, version: i64, rows: &str) -> (Scratch, std::path::PathBuf) {
        let dir = Scratch::new(label);
        let path = dir.join("rq.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(schema::SCHEMA_V22).unwrap();
        conn.execute_batch(rows).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        (dir, path)
    }

    /// Open `path` as the rq whose schema was `version` would: the ladder's
    /// later steps unapplied, so a test can see what one step did.
    fn open_at(path: &std::path::Path, version: i64) -> Result<Store> {
        let steps: Vec<_> = schema::MIGRATIONS
            .iter()
            .copied()
            .filter(|(v, _)| *v <= version)
            .collect();
        let ladder = schema::Ladder {
            version,
            fresh: schema::SCHEMA,
            steps: Box::leak(steps.into_boxed_slice()),
        };
        recover::open(path, &ladder)
    }

    fn remove(path: &std::path::Path) {
        for file in recover::db_files(path) {
            let _ = std::fs::remove_file(file);
        }
    }

    /// Two repos, each with a checkout rooted at its name: `/mixed` holds a
    /// file of each `languages`, `/ruby` one Ruby file. Every file hashes to
    /// `h` at mtime 1, and both are complete.
    fn two_repos(languages: &[(&str, &str)]) -> String {
        let files: Vec<String> = languages
            .iter()
            .map(|(path, lang)| format!("(1, '{path}', '{lang}', 1, 'h')"))
            .chain(["(2, 'z.rb', 'ruby', 1, 'h')".to_string()])
            .collect();
        format!(
            "INSERT INTO repositories (id, identity, created_at, updated_at) \
               VALUES (1, 'local:/mixed', 0, 0), (2, 'local:/ruby', 0, 0); \
             INSERT INTO checkouts (repository_id, root_path) VALUES (1, '/mixed'), (2, '/ruby'); \
             INSERT INTO files (repository_id, path, language, mtime, content_hash) VALUES {}; \
             INSERT INTO coverage (repository_id, scope, status) \
               VALUES (1, 'full', 'complete'), (2, 'full', 'complete');",
            files.join(", ")
        )
    }

    /// A file's `(mtime, content_hash)` in whichever checkout maps it.
    fn stat(store: &Store, path: &str) -> (Option<i64>, String) {
        store
            .conn
            .query_row(
                "SELECT cf.mtime, fi.content_hash FROM checkout_files cf \
                 JOIN files fi ON fi.id = cf.file_id WHERE cf.path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn migration_adds_repo_indexes_to_an_existing_db() {
        // a pre-v5 database: no repo-scoped indexes, the (since-dropped)
        // display_name column and learning tables still present, and none of
        // the columns/tables later migrations add
        let (_dir, path) = legacy(
            "migrate",
            4,
            "DROP INDEX idx_symbols_repo_name; \
             ALTER TABLE repositories ADD COLUMN display_name TEXT; \
             ALTER TABLE symbols DROP COLUMN visibility; \
             CREATE TABLE events (id INTEGER PRIMARY KEY, type TEXT NOT NULL, \
               query TEXT, repository_id INTEGER, path TEXT, line INTEGER, \
               branch TEXT, ts INTEGER NOT NULL); \
             CREATE TABLE selection_stats (repository_id INTEGER NOT NULL, \
               query_norm TEXT NOT NULL, file TEXT NOT NULL, name TEXT NOT NULL, \
               selections INTEGER NOT NULL, last_selected_at INTEGER); \
             INSERT INTO meta (key, value) VALUES ('events_hwm', '7'); \
             DROP TABLE usage_daily;",
        );
        let store = Store::open(&path).unwrap();
        let indexes: Vec<String> = store
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' \
                 AND name IN ('idx_symbols_repo','idx_symbols_repo_name','idx_events_repo') \
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        // v5 added both; v12 replaced idx_symbols_repo with the composite, and
        // v13 dropped the events table (and its index) with learning
        assert_eq!(indexes, ["idx_symbols_repo_name"]);
        let learning: i64 = store
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sqlite_master \
                   WHERE name IN ('events', 'selection_stats')) \
                 + (SELECT COUNT(*) FROM meta WHERE key = 'events_hwm')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            learning, 0,
            "v13 removes the learning tables and rollup mark"
        );
        // the ladder ran to the top: v10 recreated the usage table and v11
        // added its warming counter
        let usage: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('usage_daily') WHERE name='warming'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(usage, 1);
        drop(store);
    }

    #[test]
    fn an_added_column_survives_an_older_rq_resetting_the_version() {
        let live_columns = |store: &Store| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('usage_daily') WHERE name='live'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        // a v15 database gains the column
        let (_dir, path) = legacy(
            "migrate-v16",
            15,
            "ALTER TABLE usage_daily DROP COLUMN live;",
        );
        let store = Store::open(&path).unwrap();
        assert_eq!(live_columns(&store), 1);
        // an older rq opens it and writes its own version back
        store.conn.execute_batch("PRAGMA user_version=15;").unwrap();
        drop(store);
        let store = Store::open(&path).expect("the step re-runs as a no-op");
        assert_eq!(live_columns(&store), 1);
        drop(store);
    }

    #[test]
    fn an_upgrade_queues_every_file_for_re_extraction() {
        // v14 queued the constant languages' files; v19 queues every file, to
        // read its header, so an upgrade from either side re-parses them all
        for from in [13, 18] {
            let (_dir, path) = legacy(
                &format!("migrate-v19-{from}"),
                from,
                &two_repos(&[("a.go", "go"), ("b.rb", "ruby")]),
            );
            let store = open_at(&path, 23).unwrap();
            // both skips forgotten, so the next warm re-parses each file
            for p in ["a.go", "b.rb", "z.rb"] {
                assert_eq!(stat(&store, p), (None, String::new()), "{p} from v{from}");
            }
            let status = |root: &str| store.coverage_status(root).unwrap().unwrap();
            assert_eq!(status("/mixed"), Coverage::Warming);
            assert_eq!(status("/ruby"), Coverage::Warming);
            drop(store);
        }
    }

    #[test]
    fn v20_queues_only_the_languages_whose_extraction_changed() {
        let (_dir, path) = legacy(
            "migrate-v20",
            19,
            &two_repos(&[("a.py", "python"), ("b.rb", "ruby")]),
        );
        let store = open_at(&path, 23).unwrap();
        assert_eq!(stat(&store, "a.py").1, "");
        assert_eq!(stat(&store, "b.rb").1, "h");
        let status = |root: &str| store.coverage_status(root).unwrap().unwrap();
        assert_eq!(status("/mixed"), Coverage::Warming);
        assert_eq!(status("/ruby"), Coverage::Complete);
        drop(store);
    }

    #[test]
    fn v21_queues_python_and_typescript_and_keeps_stubs() {
        let (_dir, path) = legacy(
            "migrate-v21",
            20,
            &format!(
                "{} INSERT INTO symbols (repository_id, file_id, name, name_lower, kind, \
                   language, line, stub) \
                 SELECT 1, id, 'readFile', 'readfile', 'function', 'typescript', 1, 1 \
                 FROM files WHERE path = 'a.d.ts';",
                two_repos(&[("a.d.ts", "typescript"), ("b.rb", "ruby")])
            ),
        );
        let store = open_at(&path, 23).unwrap();
        assert_eq!(stat(&store, "a.d.ts").1, "");
        assert_eq!(stat(&store, "b.rb").1, "h");
        assert_eq!(
            store.coverage_status("/mixed").unwrap().unwrap(),
            Coverage::Warming
        );
        let stub: bool = store
            .conn
            .query_row(
                "SELECT stub FROM symbols WHERE name = 'readFile'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stub);
        drop(store);
    }

    #[test]
    fn v22_queues_every_language_that_emits_fields_but_ruby() {
        let (_dir, path) = legacy(
            "migrate-v22",
            21,
            &two_repos(&[
                ("a.rs", "rust"),
                ("b.go", "go"),
                ("c.py", "python"),
                ("d.ts", "typescript"),
                ("e.rb", "ruby"),
            ]),
        );
        let store = open_at(&path, 23).unwrap();
        for p in ["a.rs", "b.go", "c.py", "d.ts"] {
            assert_eq!(stat(&store, p).1, "", "{p}");
        }
        assert_eq!(stat(&store, "e.rb").1, "h");
        assert_eq!(
            store.coverage_status("/mixed").unwrap().unwrap(),
            Coverage::Warming
        );
        drop(store);
    }

    #[test]
    fn v24_queues_every_language_that_emits_singleton_but_go() {
        let (_dir, path) = legacy(
            "migrate-v24",
            22,
            &two_repos(&[
                ("a.rs", "rust"),
                ("b.go", "go"),
                ("c.py", "python"),
                ("d.ts", "typescript"),
                ("e.js", "javascript"),
                ("f.rb", "ruby"),
            ]),
        );
        let store = Store::open(&path).unwrap();
        for p in ["a.rs", "c.py", "d.ts", "e.js", "f.rb", "z.rb"] {
            assert_eq!(stat(&store, p), (None, "stale:h".to_string()), "{p}");
        }
        assert_eq!(stat(&store, "b.go"), (Some(1), "h".to_string()));
        assert_eq!(
            store.coverage_status("/mixed").unwrap().unwrap(),
            Coverage::Warming
        );
        assert_eq!(
            store.coverage_status("/ruby").unwrap().unwrap(),
            Coverage::Warming
        );
        drop(store);
    }

    #[test]
    fn a_go_only_checkout_stays_complete_through_v24() {
        let (_dir, path) = legacy("migrate-v24-go", 22, &two_repos(&[("b.go", "go")]));
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.coverage_status("/mixed").unwrap().unwrap(),
            Coverage::Complete
        );
        drop(store);
    }

    #[test]
    fn v25_and_v26_queue_only_typescript_and_javascript() {
        // each step alone, from a database read in full at the version before it
        for from in [24, 25] {
            let (_dir, path) = legacy(
                &format!("migrate-v{}", from + 1),
                22,
                &two_repos(&[
                    ("a.rs", "rust"),
                    ("d.ts", "typescript"),
                    ("e.js", "javascript"),
                ]),
            );
            drop(open_at(&path, from).unwrap());
            Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "UPDATE files SET content_hash = 'h'; UPDATE checkout_files SET mtime = 1; \
                     UPDATE coverage SET status = 'complete';",
                )
                .unwrap();
            let store = Store::open(&path).unwrap();
            for p in ["d.ts", "e.js"] {
                assert_eq!(
                    stat(&store, p),
                    (None, "stale:h".to_string()),
                    "v{from}: {p}"
                );
            }
            for p in ["a.rs", "z.rb"] {
                assert_eq!(stat(&store, p), (Some(1), "h".to_string()), "v{from}: {p}");
            }
            assert_eq!(
                store.coverage_status("/mixed").unwrap().unwrap(),
                Coverage::Warming
            );
            assert_eq!(
                store.coverage_status("/ruby").unwrap().unwrap(),
                Coverage::Complete
            );
            drop(store);
        }
    }

    #[test]
    fn v23_maps_each_repo_to_its_newest_checkout_when_none_is_on_disk() {
        // one repo, two checkouts (the older one stale), one repo with none
        let (_dir, path) = legacy(
            "migrate-v23",
            22,
            "INSERT INTO repositories (id, identity, created_at, updated_at) \
               VALUES (1, 'github.com/acme/widgets', 0, 0), (2, 'local:/gone', 0, 0); \
             INSERT INTO checkouts (id, repository_id, root_path) \
               VALUES (1, 1, '/old'), (2, 1, '/new'); \
             INSERT INTO files (id, repository_id, path, language, mtime, git_ts, content_hash) \
               VALUES (1, 1, 'w.rb', 'ruby', 5, 7, 'h'), (2, 2, 'x.rb', 'ruby', 5, 7, 'h'); \
             INSERT INTO symbols (repository_id, file_id, name, name_lower, kind, language, line) \
               VALUES (1, 1, 'Widget', 'widget', 'class', 'ruby', 1), \
                      (2, 2, 'Gone', 'gone', 'class', 'ruby', 1); \
             INSERT INTO coverage (repository_id, scope, status) \
               VALUES (1, 'full', 'complete'), (2, 'full', 'complete'); \
             INSERT INTO meta (key, value) VALUES ('head:1', 'abc'), ('warm_lock:x', '1:2');",
        );
        let store = open_at(&path, 23).unwrap();
        type Mapped = (i64, String, i64, Option<i64>, Option<i64>);
        let mapped: Vec<Mapped> = store
            .conn
            .prepare("SELECT checkout_id, path, file_id, mtime, git_ts FROM checkout_files")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(mapped, [(2, "w.rb".to_string(), 1, Some(5), Some(7))]);
        assert_eq!(
            store.coverage_status("/new").unwrap(),
            Some(Coverage::Complete)
        );
        assert_eq!(
            store.coverage_status("/old").unwrap(),
            None,
            "indexes on need"
        );
        // the rest reads through this rq's queries, which need its schema
        drop(store);
        let store = Store::open(&path).unwrap();
        let left = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            left("SELECT COUNT(*) FROM files"),
            1,
            "no checkout could map x.rb"
        );
        assert_eq!(left("SELECT COUNT(*) FROM symbols"), 1);
        assert_eq!(
            left("SELECT COUNT(*) FROM meta WHERE key NOT IN ('created_by', 'schema_by')"),
            0,
            "per-repo caches dropped"
        );
        let found = store
            .search_candidates("Widget", 10, false, None, None, &Probe::new("Widget"))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, "/new");
        drop(store);
    }

    #[test]
    fn v23_maps_each_repo_to_a_checkout_still_on_disk() {
        let dir = Scratch::new("v23-roots");
        let (verified, newer) = (dir.join("verified"), dir.join("newer"));
        for d in [&verified, &newer] {
            std::fs::create_dir_all(d).unwrap();
        }
        let (verified, newer) = (verified.display(), newer.display());
        // repo 1: a live root verified last, a newer live one, a newest gone
        // one; repo 2: a live root and a newer gone one; repo 3: no checkout
        let (_dir, path) = legacy(
            "migrate-v23-live",
            22,
            &format!(
                "INSERT INTO repositories (id, identity, created_at, updated_at) \
                   VALUES (1, 'r1', 0, 0), (2, 'r2', 0, 0), (3, 'r3', 0, 0); \
                 INSERT INTO checkouts (id, repository_id, root_path) \
                   VALUES (1, 1, '{verified}'), (2, 1, '{newer}'), (3, 1, '/gone/a'), \
                          (4, 2, '{newer}/r2'), (5, 2, '/gone/b'); \
                 INSERT INTO files (id, repository_id, path, language, content_hash) \
                   VALUES (1, 1, 'w.rb', 'ruby', 'h'), (2, 2, 'x.rb', 'ruby', 'h'); \
                 INSERT INTO name_index (repository_id, format, built) VALUES (3, 1, 0); \
                 INSERT INTO meta (key, value) VALUES \
                   ('warm_verified:{verified}', '200\nstamp'), \
                   ('warm_verified:{newer}', '100\nstamp');"
            ),
        );
        std::fs::create_dir_all(format!("{newer}/r2")).unwrap();
        let store = Store::open(&path).unwrap();
        let mapped: Vec<(i64, i64)> = store
            .conn
            .prepare("SELECT file_id, checkout_id FROM checkout_files ORDER BY file_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(mapped, [(1, 1), (2, 4)]);
        assert_eq!(store.repository_id("r3").unwrap(), None, "held by nothing");
        assert_eq!(count(&store, "name_index"), 0);
        drop(store);
    }

    #[test]
    fn v18_drops_the_trigram_table_and_recall_still_answers() {
        // 0.54.1 shipped v16; v17 only ever reached unreleased builds
        for from in [16, 17] {
            let drop_index = if from == 16 {
                "DROP TABLE name_sigs; DROP TABLE name_index;"
            } else {
                ""
            };
            // v17's FTS objects, as it created them
            let (_dir, path) = legacy(
                &format!("migrate-v18-{from}"),
                from,
                &format!(
                    "INSERT INTO repositories (id, identity, created_at, updated_at) \
                       VALUES (1, 'local:/r', 0, 0); \
                     INSERT INTO checkouts (repository_id, root_path) VALUES (1, '/r'); \
                     INSERT INTO files (id, repository_id, path, language, content_hash) \
                       VALUES (1, 1, 'a.rb', 'ruby', 'h'); \
                     INSERT INTO symbols (repository_id, file_id, name, name_lower, kind, \
                       language, line) VALUES (1, 1, 'AlphaWidget', 'alphawidget', 'class', \
                       'ruby', 1); \
                     {drop_index} \
                     CREATE VIRTUAL TABLE symbols_fts USING fts5(name, content='symbols', \
                       content_rowid='id', tokenize='trigram', detail=none); \
                     INSERT INTO symbols_fts(symbols_fts) VALUES ('rebuild'); \
                     CREATE TRIGGER symbols_ai AFTER INSERT ON symbols BEGIN \
                       INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name); END; \
                     CREATE TRIGGER symbols_ad AFTER DELETE ON symbols BEGIN \
                       INSERT INTO symbols_fts(symbols_fts, rowid, name) \
                         VALUES ('delete', old.id, old.name); END; \
                     CREATE TRIGGER symbols_au AFTER UPDATE ON symbols BEGIN \
                       INSERT INTO symbols_fts(symbols_fts, rowid, name) \
                         VALUES ('delete', old.id, old.name); \
                       INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name); END; \
                     CREATE INDEX idx_symbols_name_lower ON symbols(name_lower);"
                ),
            );
            let mut store = Store::open(&path).unwrap();
            let left: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE name LIKE 'symbols_fts%' OR type = 'trigger' \
                       OR name = 'idx_symbols_name_lower'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "from v{from}: FTS objects remain");
            let checkout = store.checkout("/r").unwrap().unwrap();
            // a write needs no trigger, and recall builds the index it reads
            let gadget = [sym("BetaGadget", Kind::Class, 1, None)];
            store
                .replace_file_symbols(checkout, "b.rb", "ruby", None, "h", &gadget)
                .unwrap();
            for (query, name) in [("alwdg", "AlphaWidget"), ("btgdg", "BetaGadget")] {
                let found = store
                    .search_candidates(query, 10, false, Some(checkout), None, &Probe::new(query))
                    .unwrap();
                assert!(
                    found.iter().any(|c| c.name == name),
                    "from v{from}: {query} recalls {name}"
                );
            }
            drop(store);
        }
    }

    #[test]
    fn a_prefix_range_covers_the_prefix_and_stops_after_it() {
        let upper = prefix_upper_bound("conn");
        let inside = |name: &str| name < upper.as_str();
        assert!(inside("conn"), "the bare prefix is inside");
        assert!(inside("connection_pool"), "a longer name is inside");
        // `_` is a LIKE wildcard; a range compares it literally
        assert!(inside("connx"));
        // the next name up is outside — that's what makes this a range seek
        // rather than a scan of every row
        assert!(!inside("cono"));
    }

    #[test]
    fn checkout_roots_returns_all_paths_newest_first() {
        let store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        // a repo indexed at an old path, then moved to a new one (same identity)
        store.upsert_checkout(repo, "/old/path", None).unwrap();
        store.upsert_checkout(repo, "/new/path", None).unwrap();
        // newest (most-recently inserted) first
        assert_eq!(
            store.checkout_roots(repo).unwrap(),
            vec!["/new/path", "/old/path"]
        );
    }

    fn write(
        store: &mut Store,
        checkout: Checkout,
        hash: &str,
        symbols: Option<Vec<Symbol>>,
    ) -> Written {
        let file = FileSymbols {
            path: "w.rb".into(),
            language: "ruby".into(),
            mtime: Some(1),
            content_hash: hash.into(),
            generated: false,
            symbols,
        };
        store.replace_files(checkout, &[file]).unwrap()
    }

    fn names_in(store: &Store, checkout: Checkout) -> Vec<String> {
        store
            .symbols_in_file(checkout.id, "w.rb")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect()
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn checkouts_share_a_version_and_each_keeps_its_own() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        let a = store.upsert_checkout(repo, "/a", None).unwrap();
        let b = store.upsert_checkout(repo, "/b", None).unwrap();
        let widget = || Some(vec![sym("Widget", Kind::Class, 1, None)]);
        let gadget = || Some(vec![sym("Gadget", Kind::Class, 1, None)]);

        write(&mut store, a, "h1", widget());
        // unparsed: the version is already held
        write(&mut store, b, "h1", None);
        assert_eq!((count(&store, "files"), count(&store, "symbols")), (1, 1));
        assert_eq!(names_in(&store, b), ["Widget"]);

        // B moves on; A keeps what it has
        write(&mut store, b, "h2", gadget());
        assert_eq!(names_in(&store, a), ["Widget"]);
        assert_eq!(names_in(&store, b), ["Gadget"]);
        assert_eq!(count(&store, "files"), 2);

        // A catches up: the version nobody maps any more goes
        write(&mut store, a, "h2", None);
        assert_eq!(names_in(&store, a), ["Gadget"]);
        assert_eq!((count(&store, "files"), count(&store, "symbols")), (1, 1));

        // a version gone since the pass began is handed back to be parsed
        let written = write(&mut store, a, "h3", None);
        assert_eq!(written.unmapped, ["w.rb"]);
        assert_eq!(names_in(&store, a), ["Gadget"], "unmapped, not emptied");
    }

    #[test]
    fn forgetting_a_checkout_drops_only_what_it_alone_held() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        let a = store.upsert_checkout(repo, "/a", None).unwrap();
        let b = store.upsert_checkout(repo, "/b", None).unwrap();
        write(
            &mut store,
            a,
            "h1",
            Some(vec![sym("Widget", Kind::Class, 1, None)]),
        );
        write(
            &mut store,
            b,
            "h2",
            Some(vec![sym("Gadget", Kind::Class, 1, None)]),
        );
        store.set_coverage(a.id, 1, 1, Coverage::Complete).unwrap();
        store.set_indexed_head(a.id, "abc").unwrap();
        store.branch_files_set("/a", "s", 1, 1, &[]).unwrap();
        store.begin_pass("/a", 4_000_000, Some(3)).unwrap();

        store.forget_checkout("/a").unwrap();
        assert_eq!(store.checkout_roots(repo).unwrap(), vec!["/b"]);
        assert_eq!(names_in(&store, b), ["Gadget"]);
        assert_eq!((count(&store, "files"), count(&store, "symbols")), (1, 1));
        assert_eq!(store.coverage_status("/a").unwrap(), None);
        assert_eq!(
            count(&store, "meta WHERE key NOT IN ('created_by', 'schema_by')"),
            0,
            "its caches go with it"
        );
        assert_eq!(store.repository_id("local:/x").unwrap(), Some(repo));
    }

    #[test]
    fn unscoped_candidates_fold_a_definition_several_checkouts_hold() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        let a = store.upsert_checkout(repo, "/a", None).unwrap();
        let b = store.upsert_checkout(repo, "/b", None).unwrap();
        // two versions of the file, both defining Widget at line 1; only B's
        // defines `alpha`
        write(
            &mut store,
            a,
            "h1",
            Some(vec![sym("Widget", Kind::Class, 1, None)]),
        );
        let alpha = sym("alpha", Kind::Method, 3, Some("Widget"));
        write(
            &mut store,
            b,
            "h2",
            Some(vec![sym("Widget", Kind::Class, 1, None), alpha]),
        );
        let roots = |query: &str, prefer: Option<i64>| -> Vec<String> {
            let mut roots: Vec<String> = store
                .search_candidates(query, 10, false, None, prefer, &Probe::new(query))
                .unwrap()
                .into_iter()
                .map(|r| r.root)
                .collect();
            roots.sort();
            roots
        };
        assert_eq!(
            roots("Widget", Some(b.id)),
            ["/b"],
            "the current checkout's"
        );
        assert_eq!(roots("Widget", Some(a.id)), ["/a"]);
        assert_eq!(roots("Widget", None), ["/b"], "else the newest checkout's");
        assert_eq!(roots("alpha", Some(a.id)), ["/b"], "what only B holds");
        // scoped, each checkout answers for itself
        let scoped = store
            .search_candidates("alpha", 10, false, Some(a), None, &Probe::new("alpha"))
            .unwrap();
        assert!(scoped.is_empty());
    }

    #[test]
    fn a_version_many_checkouts_share_costs_the_cap_one_row() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        let widgets = || {
            Some(
                (1..=3)
                    .map(|i| sym(&format!("widget_{i}"), Kind::Method, i, None))
                    .collect(),
            )
        };
        let checkouts: Vec<Checkout> = ["/a", "/b", "/c"]
            .iter()
            .map(|root| store.upsert_checkout(repo, root, None).unwrap())
            .collect();
        for &c in &checkouts {
            write(&mut store, c, "h", widgets());
        }
        let mut names: Vec<String> = store
            .search_candidates("widget", 3, false, None, None, &Probe::new("widget"))
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(names, ["widget_1", "widget_2", "widget_3"]);
    }

    #[test]
    fn git_ts_is_stored_and_surfaced_on_candidates() {
        let mut store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/x"));
        store
            .replace_file_symbols(
                checkout,
                "a.rb",
                "ruby",
                None,
                "h",
                &[sym("Foo", Kind::Class, 1, None)],
            )
            .unwrap();

        let times = HashMap::from([("a.rb".to_string(), 1_700_000_000_i64)]);
        store.set_file_git_ts(checkout.id, &times).unwrap();

        let cands = store
            .search_candidates("foo", 10, false, None, None, &Probe::new("foo"))
            .unwrap();
        assert_eq!(cands[0].git_ts, Some(1_700_000_000));
    }

    #[test]
    fn indexes_and_reports_coverage() {
        let mut store = Store::open_in_memory().unwrap();
        let id = RepoIdentity::Remote("github.com/dpep/rq".into());
        let repo = store.upsert_repository(&id, Some("main")).unwrap();
        let checkout = store
            .upsert_checkout(repo, "/tmp/rq", Some("main"))
            .unwrap();

        let symbols = vec![
            sym("User", Kind::Class, 1, None),
            sym("save", Kind::Method, 5, Some("User")),
        ];
        store
            .replace_file_symbols(
                checkout,
                "app/models/user.rb",
                "ruby",
                Some(100),
                "h1",
                &symbols,
            )
            .unwrap();
        store
            .set_coverage(checkout.id, 10, 1, Coverage::Warming)
            .unwrap();

        let overview = store.coverage_overview().unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].identity, "github.com/dpep/rq");
        assert_eq!(overview[0].root, "/tmp/rq");
        assert_eq!(overview[0].status, Coverage::Warming);
        assert_eq!(overview[0].symbols, 2);
    }

    #[test]
    fn a_cut_short_pass_keeps_a_complete_recorded_during_it() {
        let store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/tmp/rq")).id;
        let status = |store: &Store| store.coverage_overview().unwrap()[0].status;

        // first pass ever: nothing to protect
        let mark = store.coverage_mark(checkout).unwrap();
        assert!(
            store
                .set_coverage_since(checkout, 5, 2, Coverage::Warming, &mark)
                .unwrap()
        );

        // a warm begins; a concurrent full index completes; the warm runs out of time
        let mark = store.coverage_mark(checkout).unwrap();
        store
            .set_coverage(checkout, 5, 5, Coverage::Complete)
            .unwrap();
        assert!(
            !store
                .set_coverage_since(checkout, 5, 1, Coverage::Warming, &mark)
                .unwrap()
        );
        assert_eq!(status(&store), Coverage::Complete);

        // a pass that began on that `complete` still records its own outcome
        let mark = store.coverage_mark(checkout).unwrap();
        assert!(
            store
                .set_coverage_since(checkout, 5, 1, Coverage::Warming, &mark)
                .unwrap()
        );
        assert_eq!(status(&store), Coverage::Warming);
    }

    #[test]
    fn reindexing_a_file_replaces_its_symbols() {
        let mut store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/tmp/rq"));

        store
            .replace_file_symbols(
                checkout,
                "a.rb",
                "ruby",
                None,
                "h1",
                &[sym("Old", Kind::Class, 1, None)],
            )
            .unwrap();
        store
            .replace_file_symbols(
                checkout,
                "a.rb",
                "ruby",
                None,
                "h2",
                &[sym("New", Kind::Class, 1, None)],
            )
            .unwrap();

        store
            .set_coverage(checkout.id, 1, 1, Coverage::Complete)
            .unwrap();
        let overview = store.coverage_overview().unwrap();
        // old symbol gone, new one present → still exactly one symbol
        assert_eq!(overview[0].symbols, 1);
    }

    #[test]
    fn file_unchanged_detects_matching_hash() {
        let mut store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/tmp/rq"));
        store
            .replace_file_symbols(checkout, "a.rb", "ruby", None, "abc", &[])
            .unwrap();

        assert!(store.file_unchanged(checkout.id, "a.rb", "abc").unwrap());
        assert!(!store.file_unchanged(checkout.id, "a.rb", "xyz").unwrap());
        assert!(
            !store
                .file_unchanged(checkout.id, "missing.rb", "abc")
                .unwrap()
        );
    }
}
