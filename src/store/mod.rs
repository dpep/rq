//! Storage — SQLite (WAL mode), schema, and queries.
//!
//! The background indexer writes here; search reads. WAL mode lets those
//! happen concurrently. See `docs/ARCHITECTURE.md` for the schema.

mod names;
mod schema;

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::core::{Symbol, now_unix};
use crate::search::Probe;

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
    fi.generated, s.stub, cf.checkout_id, co.root_path";
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
    /// Symbols of the versions newly stored.
    pub symbols: usize,
    /// Files sent unparsed for a version another checkout let go of since:
    /// the caller parses them and writes them again.
    pub unmapped: Vec<String>,
}

/// A coverage row's `(status, last_indexed_at)` as a pass found it when it
/// began, or `None` before any pass finished (see `set_coverage_since`).
pub(crate) type CoverageMark = Option<(String, i64)>;

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
    pub status: String,
    pub files: i64,
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
    /// `hit`, `miss` (the symbol isn't there), or `warming` (the index wasn't
    /// ready to say). rq separates the last two in its exit codes; conflating
    /// them in the counts would overstate how often it truly finds nothing.
    pub status: &'a str,
    /// Index state when the query arrived: `complete`, `warming`, or `none`.
    pub coverage: &'a str,
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

/// Run one migration step.
fn apply(conn: &Connection, step: &schema::Step) -> Result<()> {
    match *step {
        schema::Step::Sql(sql) => conn.execute_batch(sql),
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

impl Store {
    /// Open (creating if needed) the database at `path`, enabling WAL and
    /// applying the schema.
    pub(crate) fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory database — used by tests.
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Store> {
        // WAL lets one writer and many readers coexist; busy_timeout makes a
        // second writer (e.g. two `rq` processes in two terminals, both warming)
        // wait briefly instead of erroring with "database is locked". mmap reads
        // pages in place rather than copying them through read(): fuzzy recall
        // materializes thousands of rows (see DECISIONS D8).
        // busy_timeout first: switching to WAL takes a lock of its own.
        conn.execute_batch("PRAGMA busy_timeout=3000;")?;
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
        let upgrading = version < schema::VERSION;
        if upgrading {
            conn.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE")?;
            version = user_version(&conn)?;
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
        if version == 0 {
            // fresh database — SCHEMA is already at the current version
            conn.execute_batch(schema::SCHEMA)?;
        } else {
            // cumulative migrations for existing databases
            for (v, step) in schema::MIGRATIONS {
                if version < v {
                    apply(&conn, &step)?;
                }
            }
        }
        // Only ever raise it: an older rq that lowered it would make a newer one
        // re-run migrations it had already applied.
        if version < schema::VERSION {
            conn.pragma_update(None, "user_version", schema::VERSION)?;
        }
        if upgrading {
            conn.execute_batch("COMMIT; PRAGMA foreign_keys=ON")?;
        }
        Ok(Store { conn })
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
                        parent, visibility, stub)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                                ])?;
                            }
                            written.symbols += symbols.len();
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
        status: &str,
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
        status: &str,
        mark: &CoverageMark,
    ) -> Result<bool> {
        let now = now_unix();
        let (mark_status, mark_at) = match mark {
            Some((s, at)) => (Some(s.as_str()), Some(*at)),
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
    /// Only an index pass registers a checkout and every finished pass writes
    /// coverage, so one without it is mid-way through (or was cut short in)
    /// its first pass: partially indexed, i.e. `warming`.
    pub(crate) fn coverage_overview(&self) -> Result<Vec<CoverageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.identity, co.root_path,
                    COALESCE(c.status, 'warming'),
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
                    symbols: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
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
    pub(crate) fn coverage_status(&self, root: &str) -> Result<Option<String>> {
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

    /// A repository's stored totals across its checkouts: (versions, symbols).
    pub(crate) fn repo_totals(&self, repository_id: i64) -> Result<(i64, i64)> {
        self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files WHERE repository_id = ?1),
                    (SELECT COUNT(*) FROM symbols WHERE repository_id = ?1)",
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

    /// Forget the checkout at `root_path` — a stale binding (the repo moved) or
    /// a `--drop`: its map, coverage and caches, and every version only it
    /// mapped. The repository and its other checkouts are untouched.
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
        tx.execute("DELETE FROM checkouts WHERE id = ?1", params![checkout.id])?;
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
        let tx = self.conn.transaction()?;
        for sql in [
            "DELETE FROM name_sigs WHERE repository_id = ?1",
            "DELETE FROM name_index WHERE repository_id = ?1",
            "DELETE FROM symbols WHERE repository_id = ?1",
            "DELETE FROM files WHERE repository_id = ?1",
            "DELETE FROM repositories WHERE id = ?1",
        ] {
            tx.execute(sql, params![repository_id])?;
        }
        tx.commit()
    }

    // ----- usage observability -----

    /// Count a search in `usage_daily`.
    pub(crate) fn record_search(&self, rec: &SearchRecord) -> Result<()> {
        let miss = i64::from(rec.status == "miss");
        let warming = i64::from(rec.status == "warming");
        let on_complete = i64::from(rec.coverage == "complete");
        let live = i64::from(rec.live);
        // Local date, not UTC: an evening search on the US west coast would
        // otherwise be filed under tomorrow, which makes a per-day report
        // quietly wrong for a third of the waking day.
        self.conn.execute(
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
        )?;
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
        self.meta_set(&format!("branch_files:{root}"), &value)
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

/// The `meta` keys one checkout's caches live under: by id where the index
/// writes them, by root where a search reads them before any pass registered
/// the checkout.
fn checkout_meta_keys(id: i64, root: &str) -> [String; 6] {
    [
        format!("head:{id}"),
        format!("edited:{id}"),
        format!("git_ts_head:{id}"),
        format!("warm_lock:{root}"),
        format!("warm_verified:{root}"),
        format!("branch_files:{root}"),
    ]
}

/// Unscoped rows fold across checkouts: a definition several checkouts hold —
/// the same name, kind and parent at the same path and line, whether or not
/// the rest of the file agrees — is kept from one of them, `prefer` when it
/// holds it, else the newest, as [`read_from`] picks for one version. Within
/// that checkout every row stays.
fn fold_checkouts(
    rows: Vec<SymbolRow>,
    only: Option<Checkout>,
    prefer: Option<i64>,
) -> Vec<SymbolRow> {
    if only.is_some() {
        return rows;
    }
    type Def = (i64, String, String, String, Option<String>, i64);
    let def = |r: &SymbolRow| -> Def {
        let (file, name, kind) = (r.file.clone(), r.name.clone(), r.kind.clone());
        (r.repository_id, file, name, kind, r.parent.clone(), r.line)
    };
    let rank = |checkout: i64| (Some(checkout) != prefer, std::cmp::Reverse(checkout));
    let mut winner: HashMap<Def, i64> = HashMap::new();
    for r in &rows {
        winner
            .entry(def(r))
            .and_modify(|w| {
                if rank(r.checkout_id) < rank(*w) {
                    *w = r.checkout_id;
                }
            })
            .or_insert(r.checkout_id);
    }
    rows.into_iter()
        .filter(|r| winner[&def(r)] == r.checkout_id)
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
        }
    }

    #[test]
    fn a_batch_write_waits_out_a_concurrent_writer() {
        // A deferred batch reads first, so a writer that commits while it waits
        // for the lock invalidates its snapshot: SQLite then fails it with
        // SQLITE_BUSY at once, busy_timeout or not.
        let path = std::env::temp_dir().join(format!("rq-busy-store-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
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
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn one_of_several_racing_warmers_claims_the_lock() {
        let path = std::env::temp_dir().join(format!("rq-warm-lock-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
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
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn a_burst_of_openers_shares_a_fresh_database() {
        let path = std::env::temp_dir().join(format!("rq-burst-{}.db", std::process::id()));
        let clean = || {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        };
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
    fn legacy(label: &str, version: i64, rows: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("rq-{label}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(schema::SCHEMA_V22).unwrap();
        conn.execute_batch(rows).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        path
    }

    fn remove(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
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
        let path = legacy(
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
        remove(&path);
    }

    #[test]
    fn opening_a_newer_database_leaves_its_version_alone() {
        let path = std::env::temp_dir().join(format!("rq-newer-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let newer = schema::VERSION + 1;
        drop(Store::open(&path).unwrap());
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .pragma_update(None, "user_version", newer)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, newer, "an older rq must not lower the version");
        drop(store);
        let _ = std::fs::remove_file(&path);
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
        let path = legacy(
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
        remove(&path);
    }

    #[test]
    fn an_upgrade_queues_every_file_for_re_extraction() {
        // v14 queued the constant languages' files; v19 queues every file, to
        // read its header, so an upgrade from either side re-parses them all
        for from in [13, 18] {
            let path = legacy(
                &format!("migrate-v19-{from}"),
                from,
                &two_repos(&[("a.go", "go"), ("b.rb", "ruby")]),
            );
            let store = Store::open(&path).unwrap();
            // both skips forgotten, so the next warm re-parses each file
            for p in ["a.go", "b.rb", "z.rb"] {
                assert_eq!(stat(&store, p), (None, String::new()), "{p} from v{from}");
            }
            let status = |root: &str| store.coverage_status(root).unwrap().unwrap();
            assert_eq!(status("/mixed"), "warming");
            assert_eq!(status("/ruby"), "warming");
            drop(store);
            remove(&path);
        }
    }

    #[test]
    fn v20_queues_only_the_languages_whose_extraction_changed() {
        let path = legacy(
            "migrate-v20",
            19,
            &two_repos(&[("a.py", "python"), ("b.rb", "ruby")]),
        );
        let store = Store::open(&path).unwrap();
        assert_eq!(stat(&store, "a.py").1, "");
        assert_eq!(stat(&store, "b.rb").1, "h");
        let status = |root: &str| store.coverage_status(root).unwrap().unwrap();
        assert_eq!(status("/mixed"), "warming");
        assert_eq!(status("/ruby"), "complete");
        drop(store);
        remove(&path);
    }

    #[test]
    fn v21_queues_python_and_typescript_and_keeps_stubs() {
        let path = legacy(
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
        let store = Store::open(&path).unwrap();
        assert_eq!(stat(&store, "a.d.ts").1, "");
        assert_eq!(stat(&store, "b.rb").1, "h");
        assert_eq!(store.coverage_status("/mixed").unwrap().unwrap(), "warming");
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
        remove(&path);
    }

    #[test]
    fn v22_queues_every_language_that_emits_fields_but_ruby() {
        let path = legacy(
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
        let store = Store::open(&path).unwrap();
        for p in ["a.rs", "b.go", "c.py", "d.ts"] {
            assert_eq!(stat(&store, p).1, "", "{p}");
        }
        assert_eq!(stat(&store, "e.rb").1, "h");
        assert_eq!(store.coverage_status("/mixed").unwrap().unwrap(), "warming");
        drop(store);
        remove(&path);
    }

    #[test]
    fn v23_maps_each_repo_to_its_newest_checkout() {
        // one repo, two checkouts (the older one stale), one repo with none
        let path = legacy(
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
        let store = Store::open(&path).unwrap();
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
            store.coverage_status("/new").unwrap().as_deref(),
            Some("complete")
        );
        assert_eq!(
            store.coverage_status("/old").unwrap(),
            None,
            "indexes on need"
        );
        let left = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            left("SELECT COUNT(*) FROM files"),
            1,
            "no checkout could map x.rb"
        );
        assert_eq!(left("SELECT COUNT(*) FROM symbols"), 1);
        assert_eq!(
            left("SELECT COUNT(*) FROM meta"),
            0,
            "per-repo caches dropped"
        );
        let found = store
            .search_candidates("Widget", 10, false, None, None, &Probe::new("Widget"))
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, "/new");
        drop(store);
        remove(&path);
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
            let path = legacy(
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
            remove(&path);
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
        store.set_coverage(a.id, 1, 1, "complete").unwrap();
        store.set_indexed_head(a.id, "abc").unwrap();
        store.branch_files_set("/a", "s", 1, 1, &[]).unwrap();

        store.forget_checkout("/a").unwrap();
        assert_eq!(store.checkout_roots(repo).unwrap(), vec!["/b"]);
        assert_eq!(names_in(&store, b), ["Gadget"]);
        assert_eq!((count(&store, "files"), count(&store, "symbols")), (1, 1));
        assert_eq!(store.coverage_status("/a").unwrap(), None);
        assert_eq!(count(&store, "meta"), 0, "its caches go with it");
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
        store.set_coverage(checkout.id, 10, 1, "warming").unwrap();

        let overview = store.coverage_overview().unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].identity, "github.com/dpep/rq");
        assert_eq!(overview[0].root, "/tmp/rq");
        assert_eq!(overview[0].status, "warming");
        assert_eq!(overview[0].symbols, 2);
    }

    #[test]
    fn a_cut_short_pass_keeps_a_complete_recorded_during_it() {
        let store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/tmp/rq")).id;
        let status = |store: &Store| store.coverage_overview().unwrap()[0].status.clone();

        // first pass ever: nothing to protect
        let mark = store.coverage_mark(checkout).unwrap();
        assert!(
            store
                .set_coverage_since(checkout, 5, 2, "warming", &mark)
                .unwrap()
        );

        // a warm begins; a concurrent full index completes; the warm runs out of time
        let mark = store.coverage_mark(checkout).unwrap();
        store.set_coverage(checkout, 5, 5, "complete").unwrap();
        assert!(
            !store
                .set_coverage_since(checkout, 5, 1, "warming", &mark)
                .unwrap()
        );
        assert_eq!(status(&store), "complete");

        // a pass that began on that `complete` still records its own outcome
        let mark = store.coverage_mark(checkout).unwrap();
        assert!(
            store
                .set_coverage_since(checkout, 5, 1, "warming", &mark)
                .unwrap()
        );
        assert_eq!(status(&store), "warming");
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

        store.set_coverage(checkout.id, 1, 1, "complete").unwrap();
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
