//! Storage — SQLite (WAL mode), schema, and queries.
//!
//! The background indexer writes here; search reads. WAL mode lets those
//! happen concurrently. See `docs/ARCHITECTURE.md` for the schema.

mod schema;

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::{Symbol, now_unix};

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
    /// File mtime (unix *nanoseconds*) — a recency signal.
    pub mtime: Option<i64>,
    /// Last git commit time touching the file — the stronger recency signal.
    pub git_ts: Option<i64>,
    /// Access level (`public`/`crate`/`private`/`protected`) when the language
    /// expresses one; `None` for unknown (or pre-v9 rows). A ranking hint.
    pub visibility: Option<String>,
}

impl SymbolRow {
    /// A row for a symbol parsed live rather than read from the index — the
    /// same shape, with no file times.
    pub(crate) fn live(s: Symbol, repository_id: i64, repo_identity: &str) -> Self {
        SymbolRow {
            name: s.name,
            kind: s.kind.as_str().to_string(),
            language: s.language,
            file: s.file,
            line: s.line as i64,
            end_line: Some(s.end_line as i64),
            parent: s.parent,
            repository_id,
            repo_identity: repo_identity.to_string(),
            mtime: None,
            git_ts: None,
            visibility: s.visibility.map(str::to_string),
        }
    }
}

/// Column projection shared by the candidate queries. Column order is consumed
/// by [`row_to_candidate`].
/// Longest query that still gets the first-character anchor pass.
const FIRST_CHAR_ANCHOR_MAX: usize = 6;

const CANDIDATE_COLS: &str = "s.id, s.name, s.kind, s.language, fi.path, s.line, \
    s.end_line, s.parent, s.repository_id, r.identity, fi.mtime, fi.git_ts, s.visibility";
const CANDIDATE_FROM: &str = "FROM symbols s \
    JOIN files fi ON fi.id = s.file_id \
    JOIN repositories r ON r.id = s.repository_id";

/// How many rows a filtered fuzzy net reads for each one its cap can keep. A
/// rejected row costs little once the filter runs in SQLite, so the net looks
/// further before the cap cuts it off; 4× covered the largest nets measured
/// (D12).
const NET_WINDOW: usize = 4;

/// Given a candidate row's name, kind and file, could it match at all? Recall
/// runs this inside SQLite so the rows that can't are never decoded.
pub(crate) type CandidateFilter = Box<dyn Fn(&str, &str, &str) -> bool + Send>;

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
    pub symbols: Vec<Symbol>,
}

/// One row of `rq status` output — the current indexed totals for a repo (not
/// any single run's incremental counts).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CoverageRow {
    /// Repository identity (`github.com/org/repo` or `local:/path`). Named `repo`
    /// in JSON, matching the search result field.
    #[serde(rename = "repo")]
    pub identity: String,
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
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=3000; \
             PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-16384; \
             PRAGMA mmap_size=268435456;",
        )?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            // fresh database — SCHEMA is already at the current version
            conn.execute_batch(schema::SCHEMA)?;
            conn.execute_batch(schema::FTS_INSERT_TRIGGER)?;
        } else {
            // cumulative migrations for existing databases
            for (v, sql) in schema::MIGRATIONS {
                if version < v {
                    conn.execute_batch(sql)?;
                }
            }
        }
        if version != schema::VERSION {
            conn.pragma_update(None, "user_version", schema::VERSION)?;
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
        self.conn.query_row(
            "INSERT INTO repositories (identity, default_branch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(identity) DO UPDATE SET
               default_branch = COALESCE(excluded.default_branch, repositories.default_branch),
               updated_at = excluded.updated_at
             RETURNING id",
            params![identity.to_string(), default_branch, now],
            |r| r.get(0),
        )
    }

    /// Record (or update) a local checkout of a repository.
    pub(crate) fn upsert_checkout(
        &self,
        repository_id: i64,
        root_path: &str,
        branch: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO checkouts (repository_id, root_path, current_branch)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(root_path) DO UPDATE SET
               repository_id = excluded.repository_id,
               current_branch = excluded.current_branch",
            params![repository_id, root_path, branch],
        )?;
        Ok(())
    }

    /// True if `path` is already indexed at this exact content hash — the
    /// incremental-skip check.
    pub(crate) fn file_unchanged(
        &self,
        repository_id: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<bool> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT content_hash FROM files WHERE repository_id = ?1 AND path = ?2",
                params![repository_id, path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(content_hash))
    }

    /// One file's stored mtime: `None` when the file isn't indexed, `Some(None)`
    /// when it is but was stored without one.
    pub(crate) fn file_mtime(&self, repository_id: i64, path: &str) -> Result<Option<Option<i64>>> {
        self.conn
            .prepare_cached("SELECT mtime FROM files WHERE repository_id = ?1 AND path = ?2")?
            .query_row(params![repository_id, path], |r| r.get(0))
            .optional()
    }

    /// Record a file's current mtime without touching its symbols — for a file
    /// whose content was confirmed unchanged.
    pub(crate) fn set_file_mtime(
        &self,
        repository_id: i64,
        path: &str,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE files SET mtime = ?3 WHERE repository_id = ?1 AND path = ?2")?
            .execute(params![repository_id, path, mtime])?;
        Ok(())
    }

    /// Indexed path → stored mtime for a repository. The budgeted warm pass uses
    /// this to skip unchanged files with a cheap `stat` (no read or re-hash).
    pub(crate) fn file_mtimes(&self, repository_id: i64) -> Result<HashMap<String, Option<i64>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime FROM files WHERE repository_id = ?1")?;
        let rows = stmt.query_map(params![repository_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, mtime) = row?;
            map.insert(path, mtime);
        }
        Ok(map)
    }

    /// Replace all symbols for one file — the single-file form of
    /// [`Store::replace_files`] (same upsert, hash-skip, and batching).
    pub(crate) fn replace_file_symbols(
        &mut self,
        repository_id: i64,
        path: &str,
        language: &str,
        mtime: Option<i64>,
        content_hash: &str,
        symbols: &[Symbol],
    ) -> Result<()> {
        self.replace_files(
            repository_id,
            &[FileSymbols {
                path: path.to_string(),
                language: language.to_string(),
                mtime,
                content_hash: content_hash.to_string(),
                symbols: symbols.to_vec(),
            }],
        )?;
        Ok(())
    }

    /// Write many parsed files, one transaction per chunk — a batched `fsync`
    /// instead of one per file, while bounding how much a single transaction
    /// holds (a cold index of a huge repo would otherwise be one enormous txn).
    /// A file whose content hash already matches the index is skipped (not
    /// rewritten). Returns `(files_written, symbols_written)`; skips don't count.
    pub(crate) fn replace_files(
        &mut self,
        repository_id: i64,
        files: &[FileSymbols],
    ) -> Result<(usize, usize)> {
        /// Files per transaction — bounds memory and WAL frame size on a big index.
        const BATCH: usize = 512;

        let now = now_unix();
        let mut files_written = 0;
        let mut symbols_written = 0;
        for chunk in files.chunks(BATCH) {
            let tx = self.conn.transaction()?;
            {
                let mut upsert = tx.prepare(
                    "INSERT INTO files (repository_id, path, language, mtime, content_hash, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(repository_id, path) DO UPDATE SET
                       language = excluded.language,
                       mtime = excluded.mtime,
                       content_hash = excluded.content_hash,
                       indexed_at = excluded.indexed_at
                     RETURNING id",
                )?;
                let mut current = tx.prepare(
                    "SELECT content_hash FROM files WHERE repository_id = ?1 AND path = ?2",
                )?;
                let mut touch = tx.prepare(
                    "UPDATE files SET mtime = ?3, indexed_at = ?4
                     WHERE repository_id = ?1 AND path = ?2",
                )?;
                let mut clear = tx.prepare("DELETE FROM symbols WHERE file_id = ?1")?;
                let mut insert = tx.prepare(
                    "INSERT INTO symbols
                       (repository_id, file_id, name, name_lower, kind, language, line, end_line,
                        parent, visibility)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )?;
                for f in chunk {
                    // content unchanged (e.g. mtime moved but bytes didn't): skip
                    // the rewrite, but refresh the stat columns — otherwise a
                    // touched (or racily-indexed) file re-parses on every warm
                    let stored: Option<String> = current
                        .query_row(params![repository_id, f.path], |r| r.get(0))
                        .optional()?;
                    if stored.as_deref() == Some(f.content_hash.as_str()) {
                        touch.execute(params![repository_id, f.path, f.mtime, now])?;
                        continue;
                    }
                    let file_id: i64 = upsert.query_row(
                        params![
                            repository_id,
                            f.path,
                            f.language,
                            f.mtime,
                            f.content_hash,
                            now
                        ],
                        |r| r.get(0),
                    )?;
                    clear.execute(params![file_id])?;
                    for s in &f.symbols {
                        insert.execute(params![
                            repository_id,
                            file_id,
                            s.name,
                            s.name.to_lowercase(),
                            s.kind.as_str(),
                            s.language,
                            s.line,
                            s.end_line,
                            s.parent,
                            s.visibility,
                        ])?;
                    }
                    files_written += 1;
                    symbols_written += f.symbols.len();
                }
            }
            tx.commit()?;
        }
        Ok((files_written, symbols_written))
    }

    /// Suspend per-row FTS maintenance for a cold bulk index: drop the
    /// `AFTER INSERT` trigger so symbol inserts skip the expensive per-row
    /// trigram tokenization. Pair with [`sync_fts`](Self::sync_fts), which
    /// indexes the skipped rows in one pass and restores the trigger. No-op safe
    /// to call when the trigger is already gone.
    pub(crate) fn defer_fts_insert(&self) -> Result<()> {
        self.conn
            .execute_batch("DROP TRIGGER IF EXISTS symbols_ai;")?;
        Ok(())
    }

    /// Index every symbol the FTS index is missing — the rows written while the
    /// `AFTER INSERT` trigger was absent, by this writer or any other — then
    /// recreate the trigger. The inverse of
    /// [`defer_fts_insert`](Self::defer_fts_insert).
    ///
    /// "Missing" is read from FTS5's `_docsize` table, which holds one row per
    /// indexed rowid, so this is exact without a watermark and costs an
    /// anti-join over `symbols` plus the new rows' tokenization. A full
    /// `'rebuild'` re-tokenizes every repo in the database to add one: 293 ms
    /// against 34 ms for the anti-join at 176k symbols. One transaction: a
    /// concurrent writer lands before (and is caught up) or after the trigger
    /// is back, never in between.
    pub(crate) fn sync_fts(&self) -> Result<()> {
        let sql = format!(
            "BEGIN IMMEDIATE;
             INSERT INTO symbols_fts(rowid, name)
               SELECT s.id, s.name FROM symbols s
               WHERE NOT EXISTS (SELECT 1 FROM symbols_fts_docsize d WHERE d.id = s.id);
             {}
             COMMIT;",
            schema::FTS_INSERT_TRIGGER
        );
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    /// Whether the `AFTER INSERT` FTS-sync trigger is currently absent — true
    /// only mid-bulk-index (see [`defer_fts_insert`](Self::defer_fts_insert))
    /// or after one crashed before its [`sync_fts`](Self::sync_fts).
    pub(crate) fn fts_trigger_missing(&self) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='symbols_ai'",
            [],
            |r| r.get(0),
        )?;
        Ok(n == 0)
    }

    /// Record indexing coverage for a repository (scope `full`).
    pub(crate) fn set_coverage(
        &self,
        repository_id: i64,
        files_seen: i64,
        files_indexed: i64,
        status: &str,
    ) -> Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO coverage
               (repository_id, scope, files_seen, files_indexed, status, last_indexed_at)
             VALUES (?1, 'full', ?2, ?3, ?4, ?5)
             ON CONFLICT(repository_id, scope) DO UPDATE SET
               files_seen = excluded.files_seen,
               files_indexed = excluded.files_indexed,
               status = excluded.status,
               last_indexed_at = excluded.last_indexed_at",
            params![repository_id, files_seen, files_indexed, status, now],
        )?;
        Ok(())
    }

    /// Set the last-commit time for files in a repository, from a path → unix-ts
    /// map (git log). Files not in the map are left untouched.
    pub(crate) fn set_file_git_ts(
        &mut self,
        repository_id: i64,
        times: &HashMap<String, i64>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE files SET git_ts = ?3 WHERE repository_id = ?1 AND path = ?2")?;
            for (path, ts) in times {
                stmt.execute(params![repository_id, path, ts])?;
            }
        }
        tx.commit()
    }

    /// All known repositories with their coverage status and current totals.
    pub(crate) fn coverage_overview(&self) -> Result<Vec<CoverageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.identity,
                    COALESCE(c.status, 'never'),
                    (SELECT COUNT(*) FROM files fi WHERE fi.repository_id = r.id),
                    (SELECT COUNT(*) FROM symbols s WHERE s.repository_id = r.id)
             FROM repositories r
             LEFT JOIN coverage c ON c.repository_id = r.id AND c.scope = 'full'
             ORDER BY r.identity",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CoverageRow {
                    identity: r.get(0)?,
                    status: r.get(1)?,
                    files: r.get(2)?,
                    symbols: r.get(3)?,
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

    /// Coverage status for a repository's full scope (`never`/`warming`/
    /// `complete`), or `None` if the repository is unknown.
    pub(crate) fn coverage_status(&self, identity: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT c.status FROM coverage c
                 JOIN repositories r ON r.id = c.repository_id
                 WHERE r.identity = ?1 AND c.scope = 'full'",
                params![identity],
                |r| r.get(0),
            )
            .optional()
    }

    /// Current indexed totals for a repository: (files, symbols).
    /// Whether a repository has any indexed file — an existence check, where
    /// [`repo_totals`](Self::repo_totals) would count every symbol to answer it.
    pub(crate) fn repo_has_files(&self, repository_id: i64) -> Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM files WHERE repository_id = ?1)",
            params![repository_id],
            |r| r.get(0),
        )
    }

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
    pub(crate) fn symbols_in_file(&self, repository_id: i64, path: &str) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE s.repository_id = ?1 AND fi.path = ?2 \
             ORDER BY s.line, s.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![repository_id, path], row_to_candidate)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.1);
        }
        Ok(out)
    }

    /// Every checkout root recorded for a repository, newest first. A repo can
    /// have more than one (it was moved or cloned twice, both under the same
    /// remote identity), and an old row may be stale — so callers that read files
    /// try these in order (current checkout before a stale one).
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

    /// Drop a checkout row — used to prune a stale binding (a repo moved away
    /// from `root_path`). Symbols/coverage are keyed by repo identity, not this
    /// row, so forgetting a checkout only forgets *where* the repo was on disk.
    pub(crate) fn forget_checkout(&mut self, root_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM checkouts WHERE root_path = ?1",
            params![root_path],
        )?;
        Ok(())
    }

    /// Drop a file and its symbols — used when a file has been deleted on disk.
    pub(crate) fn forget_file(&mut self, repository_id: i64, path: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let file_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM files WHERE repository_id = ?1 AND path = ?2",
                params![repository_id, path],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(fid) = file_id {
            tx.execute("DELETE FROM symbols WHERE file_id = ?1", params![fid])?;
            tx.execute("DELETE FROM files WHERE id = ?1", params![fid])?;
        }
        tx.commit()
    }

    /// Drop a repository entirely — the inverse of indexing it: its symbols (and
    /// their FTS rows, via trigger), files, coverage, checkout, and the
    /// repository row. Deleted in FK-safe order in one
    /// transaction.
    pub(crate) fn drop_repository(&mut self, repository_id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        for sql in [
            "DELETE FROM symbols WHERE repository_id = ?1",
            "DELETE FROM files WHERE repository_id = ?1",
            "DELETE FROM coverage WHERE repository_id = ?1",
            "DELETE FROM checkouts WHERE repository_id = ?1",
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
        // Local date, not UTC: an evening search on the US west coast would
        // otherwise be filed under tomorrow, which makes a per-day report
        // quietly wrong for a third of the waking day.
        self.conn.execute(
            "INSERT INTO usage_daily (day, source, flags, searches, misses, warming, on_complete)
             VALUES (date(?1, 'unixepoch', 'localtime'), ?2, ?3, 1, ?4, ?5, ?6)
             ON CONFLICT(day, source, flags) DO UPDATE SET
               searches = searches + 1,
               misses = misses + excluded.misses,
               warming = warming + excluded.warming,
               on_complete = on_complete + excluded.on_complete",
            params![
                now_unix(),
                rec.source,
                rec.flags,
                miss,
                warming,
                on_complete
            ],
        )?;
        Ok(())
    }

    /// Usage counts, newest day first.
    pub(crate) fn usage_overview(&self) -> Result<Vec<UsageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT day, source, flags, searches, misses, warming, on_complete
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
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The git HEAD sha recorded at the last complete index of a repo, if any —
    /// used to detect that the committed tree is unchanged since indexing.
    pub(crate) fn indexed_head(&self, repository_id: i64) -> Result<Option<String>> {
        self.meta_get(&format!("head:{repository_id}"))
    }

    /// Record the git HEAD sha at a complete index.
    pub(crate) fn set_indexed_head(&self, repository_id: i64, head: &str) -> Result<()> {
        self.meta_set(&format!("head:{repository_id}"), head)
    }

    /// Files the index may hold in a state other than HEAD's: the edits it has
    /// indexed. A discarded edit (`git checkout -- f`) leaves `f` clean, so
    /// `git status` stops naming it while the index still holds the edit; this
    /// is how the staleness check still knows to look at it.
    pub(crate) fn edited_files(&self, repository_id: i64) -> Result<Vec<String>> {
        Ok(self
            .meta_get(&format!("edited:{repository_id}"))?
            .map(|v| v.lines().map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Replace the edited-file set (newline-delimited: git paths can't hold one).
    pub(crate) fn set_edited_files(&self, repository_id: i64, files: &[String]) -> Result<()> {
        self.meta_set(&format!("edited:{repository_id}"), &files.join("\n"))
    }

    /// Add one file to the edited-file set.
    pub(crate) fn note_edited_file(&self, repository_id: i64, path: &str) -> Result<()> {
        let mut files = self.edited_files(repository_id)?;
        if files.iter().any(|f| f == path) {
            return Ok(());
        }
        files.push(path.to_string());
        self.set_edited_files(repository_id, &files)
    }

    /// The git HEAD sha at the last commit-times capture (recency signal), if
    /// any — lets the next capture read only the commits since, or skip the
    /// `git log` entirely when HEAD hasn't moved.
    pub(crate) fn git_ts_head(&self, repository_id: i64) -> Result<Option<String>> {
        self.meta_get(&format!("git_ts_head:{repository_id}"))
    }

    /// Record the git HEAD sha a commit-times capture ran at.
    pub(crate) fn set_git_ts_head(&self, repository_id: i64, head: &str) -> Result<()> {
        self.meta_set(&format!("git_ts_head:{repository_id}"), head)
    }

    /// The detached-warm single-flight lock for a repo: `(pid, stamped_at)` of
    /// the process that claimed it, if any. Liveness/staleness policy is the
    /// caller's (the store just holds the record).
    pub(crate) fn warm_lock(&self, identity: &str) -> Result<Option<(u32, i64)>> {
        Ok(self
            .meta_get(&format!("warm_lock:{identity}"))?
            .and_then(|v| {
                let (pid, ts) = v.split_once(':')?;
                Some((pid.parse().ok()?, ts.parse().ok()?))
            }))
    }

    /// Claim the detached-warm lock for this process.
    pub(crate) fn set_warm_lock(&self, identity: &str, pid: u32) -> Result<()> {
        self.meta_set(
            &format!("warm_lock:{identity}"),
            &format!("{pid}:{}", now_unix()),
        )
    }

    /// Release the detached-warm lock.
    pub(crate) fn clear_warm_lock(&self, identity: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM meta WHERE key = ?1",
            params![format!("warm_lock:{identity}")],
        )?;
        Ok(())
    }

    /// When a warm child last found the worktree unchanged since indexing, and
    /// the git-state stamp it saw: `(stamp, checked_at)`.
    pub(crate) fn warm_verified(&self, identity: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .meta_get(&format!("warm_verified:{identity}"))?
            .and_then(|v| {
                let (at, stamp) = v.split_once('\n')?;
                Some((stamp.to_string(), at.parse().ok()?))
            }))
    }

    /// Record that the worktree matched the index as of `checked_at`.
    pub(crate) fn set_warm_verified(
        &self,
        identity: &str,
        stamp: &str,
        checked_at: i64,
    ) -> Result<()> {
        self.meta_set(
            &format!("warm_verified:{identity}"),
            &format!("{checked_at}\n{stamp}"),
        )
    }

    /// The cached branch-changed file list for a repo: `(stamp, computed_at,
    /// files)`. Stored rather than recomputed because the git diff behind it is
    /// O(tracked files) and runs on the search path.
    /// The cached branch-file list, with what decides whether it's still good.
    pub(crate) fn branch_files_get(&self, identity: &str) -> Result<Option<BranchFiles>> {
        let Some(raw) = self.meta_get(&format!("branch_files:{identity}"))? else {
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
        identity: &str,
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
        self.meta_set(&format!("branch_files:{identity}"), &value)
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

    /// Candidate symbols for a query, drawn from cheap layers and merged:
    /// exact/prefix on `name_lower`, then broad fuzzy recall (first-char anchor,
    /// trigram FTS, path). Ranking happens in `crate::search`; this only narrows
    /// the field.
    ///
    /// `filter` narrows the broad fuzzy layers. They are loose nets — a shared
    /// first letter, a shared trigram — and most of what they catch can't
    /// match; rejecting those inside SQLite means they're never decoded. That
    /// makes a read row cheap enough for the net to read [`NET_WINDOW`] times
    /// past its cap, so the cap falls on rows that could match rather than on
    /// whichever the net met first.
    ///
    /// When `force_fuzzy` is false and exact/prefix already matched, the broad
    /// fuzzy layers are skipped: the relevance gate drops every fuzzy candidate
    /// once a strong (exact/prefix) hit exists, so fetching and scoring them is
    /// wasted. A wildcard query passes `force_fuzzy = true` — it isn't gated and
    /// always needs the trigram recall.
    ///
    /// `repo` scopes every layer to one repository. It has to be applied here,
    /// not after: each layer's `limit` is otherwise shared with every other
    /// indexed repo, and another repo's exact match would trip the fast path
    /// below and skip the fuzzy layers this repo needed.
    pub(crate) fn search_candidates(
        &self,
        query: &str,
        limit: usize,
        force_fuzzy: bool,
        repo: Option<i64>,
        filter: Option<CandidateFilter>,
    ) -> Result<Vec<SymbolRow>> {
        use rusqlite::types::Value;
        let q = query.to_ascii_lowercase();
        let mut found: HashMap<i64, SymbolRow> = HashMap::new();
        let window = Value::Integer((limit * if filter.is_some() { NET_WINDOW } else { 1 }) as i64);
        let limit = Value::Integer(limit as i64);
        // The repo scope, bound after a layer's own placeholders as the next
        // numbered one.
        let scope = |args: &mut Vec<Value>| match repo {
            Some(id) => {
                args.push(Value::Integer(id));
                format!(" AND s.repository_id = ?{}", args.len())
            }
            None => String::new(),
        };
        // Run one layer. `filter` holds the layer's own placeholders.
        let fetch =
            |from: &str, filter: &str, mut args: Vec<Value>| -> Result<Vec<(i64, SymbolRow)>> {
                let scope = scope(&mut args);
                args.push(limit.clone());
                let sql = format!(
                    "SELECT {CANDIDATE_COLS} {from} WHERE {filter}{scope} LIMIT ?{}",
                    args.len()
                );
                let mut stmt = self.conn.prepare_cached(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_candidate)?;
                rows.collect()
            };
        let text = |s: &str| Value::Text(s.to_string());

        // exact name — always included, never subject to the cap. The
        // match we most want must reach the scorer no matter how large the index
        // is (a broad capped scan could otherwise truncate it away).
        for (id, cand) in fetch(CANDIDATE_FROM, "s.name_lower = ?1", vec![text(&q)])? {
            found.insert(id, cand);
        }

        // query as a prefix — selective, so prefix matches always
        // surface even on a huge repo (unlike the broad first-char anchor below,
        // which the cap can truncate).
        for (id, cand) in fetch(
            CANDIDATE_FROM,
            "s.name_lower >= ?1 AND s.name_lower < ?2",
            vec![text(&q), text(&prefix_upper_bound(&q))],
        )? {
            found.entry(id).or_insert(cand);
        }

        // Fast path: a strong (exact/prefix) match exists, so the relevance gate
        // will discard everything the broad layers below would add. Skip them —
        // identical results, no wasted fetch/score. (Wildcard queries force the
        // fuzzy layers; they aren't gated.)
        if !force_fuzzy && !found.is_empty() {
            return Ok(found.into_values().collect());
        }

        // Registered per search: the filter closes over this query. Redefining
        // a function expires the statements that use it, so the cached ones
        // re-prepare — cheap next to the rows it saves decoding.
        let keep = match filter {
            Some(filter) => {
                use rusqlite::functions::FunctionFlags;
                self.conn.create_scalar_function(
                    "rq_keep",
                    3,
                    FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                    move |ctx| {
                        let text = |i| ctx.get_raw(i).as_str().unwrap_or_default();
                        Ok(filter(text(0), text(1), text(2)))
                    },
                )?;
                " WHERE rq_keep(s.name, s.kind, fi.path)"
            }
            None => "",
        };
        // Run one broad net: read up to `window` of its rows, decode the ones
        // `keep` passes, and cap those.
        let fetch_net =
            |net: &str, filter: &str, mut args: Vec<Value>| -> Result<Vec<(i64, SymbolRow)>> {
                let scope = scope(&mut args);
                args.push(window.clone());
                let w = args.len();
                args.push(limit.clone());
                let sql = format!(
                    "SELECT {CANDIDATE_COLS} FROM \
                       (SELECT s.id FROM {net} WHERE {filter}{scope} LIMIT ?{w}) w \
                     JOIN symbols s ON s.id = w.id \
                     JOIN files fi ON fi.id = s.file_id \
                     JOIN repositories r ON r.id = s.repository_id{keep} LIMIT ?{}",
                    args.len()
                );
                let mut stmt = self.conn.prepare_cached(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_candidate)?;
                rows.collect()
            };

        // fuzzy recall (a): first-character anchor (index-backed scan) for short
        // skip-abbreviations like `usr → user` that prefix matching can't reach;
        // the scorer filters and ranks. Best-effort under the cap — exact and
        // prefix are already guaranteed above.
        // Only for a short query. This exists to reach skip-abbreviations
        // (`usr` -> `user`) that prefix matching can't, and a short query yields
        // too few trigrams for the FTS layer below to be much of a net. A long
        // query gets a good trigram net, so anchoring on one letter just drags
        // in thousands of rows the scorer will reject.
        if let Some(first) = q
            .chars()
            .next()
            .filter(|_| q.chars().count() <= FIRST_CHAR_ANCHOR_MAX)
        {
            let anchor = first.to_string();
            for (id, cand) in fetch_net(
                "symbols s",
                "s.name_lower >= ?1 AND s.name_lower < ?2",
                vec![text(&anchor), text(&prefix_upper_bound(&anchor))],
            )? {
                found.entry(id).or_insert(cand);
            }
        }

        // fuzzy recall (b): trigram FTS (OR of the query's trigrams).
        if let Some(match_expr) = trigram_or_query(&q) {
            for (id, cand) in fetch_net(
                "symbols_fts f JOIN symbols s ON s.id = f.rowid",
                "symbols_fts MATCH ?1",
                vec![text(&match_expr)],
            )? {
                found.entry(id).or_insert(cand);
            }
        }

        // path recall: primary definitions in files whose path matches the query,
        // so `billing` can surface the class defined in `billing.rb`.
        let path_like = format!("%{}%", escape_like(&q));
        // Narrow on `files` first. A leading `%` can't use an index either way,
        // but scanning 3k file rows and then seeking their symbols beats
        // scanning 49k symbol rows to test a column on the joined table —
        // measured at 29 ms against 0.4 ms on rq's own Rails index.
        for (id, cand) in fetch(
            CANDIDATE_FROM,
            "s.file_id IN (SELECT id FROM files WHERE path LIKE ?1 ESCAPE '\\') \
             AND s.kind IN ('class', 'module')",
            vec![text(&path_like)],
        )? {
            found.entry(id).or_insert(cand);
        }

        Ok(found.into_values().collect())
    }
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

/// Escape LIKE wildcards so identifier characters (`_`) are matched literally.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Build an FTS5 `MATCH` expression that ORs the query's trigrams, giving broad
/// recall (any shared trigram makes a candidate). `None` if the query is too
/// short to form a trigram.
fn trigram_or_query(q: &str) -> Option<String> {
    let cleaned: Vec<char> = q
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if cleaned.len() < 3 {
        return None;
    }
    let mut grams: Vec<String> = Vec::new();
    for w in cleaned.windows(3) {
        let gram: String = w.iter().collect();
        let quoted = format!("\"{gram}\"");
        if !grams.contains(&quoted) {
            grams.push(quoted);
        }
    }
    Some(grams.join(" OR "))
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
        }
    }

    #[test]
    fn migration_adds_repo_indexes_to_an_existing_db() {
        let path = std::env::temp_dir().join(format!("rq-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            // simulate a pre-v5 database: no repo-scoped indexes, the
            // (since-dropped) display_name column and learning tables still
            // present, and none of the columns/tables later migrations add
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
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
                     DROP TABLE usage_daily; \
                     PRAGMA user_version=4;",
                )
                .unwrap();
        }
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
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v14_queues_constant_languages_for_re_extraction() {
        let path = std::env::temp_dir().join(format!("rq-migrate-v14-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let file = |path: &str, language: &str| FileSymbols {
            path: path.into(),
            language: language.into(),
            mtime: Some(1),
            content_hash: "h".into(),
            symbols: Vec::new(),
        };
        {
            // a v13 index: one repo with Go beside Ruby, one Ruby-only
            let mut store = Store::open(&path).unwrap();
            let mixed = store.upsert_repository(&"local:/mixed", None).unwrap();
            let ruby = store.upsert_repository(&"local:/ruby", None).unwrap();
            store
                .replace_files(mixed, &[file("a.go", "go"), file("b.rb", "ruby")])
                .unwrap();
            store.replace_files(ruby, &[file("c.rb", "ruby")]).unwrap();
            store.set_coverage(mixed, 2, 2, "complete").unwrap();
            store.set_coverage(ruby, 1, 1, "complete").unwrap();
            store.conn.execute_batch("PRAGMA user_version=13;").unwrap();
        }
        let store = Store::open(&path).unwrap();
        let stat = |p: &str| -> (Option<i64>, Option<String>) {
            store
                .conn
                .query_row(
                    "SELECT mtime, content_hash FROM files WHERE path = ?1",
                    [p],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        // both skips forgotten for the Go file, so the next warm re-parses it
        assert_eq!(stat("a.go"), (None, Some(String::new())));
        assert_eq!(stat("b.rb"), (Some(1), Some("h".into())));
        // only the repo holding such files is swept again
        let status = |id: &str| store.coverage_status(id).unwrap().unwrap();
        assert_eq!(status("local:/mixed"), "warming");
        assert_eq!(status("local:/ruby"), "complete");
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v15_rebuilds_the_trigram_table_without_positions() {
        let path = std::env::temp_dir().join(format!("rq-migrate-v15-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            // a v14 index: positional trigram table, one indexed symbol
            let mut store = Store::open(&path).unwrap();
            let repo = store.upsert_repository(&"local:/r", None).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TABLE symbols_fts; \
                     CREATE VIRTUAL TABLE symbols_fts USING fts5(name, content='symbols', \
                       content_rowid='id', tokenize='trigram'); \
                     PRAGMA user_version=14;",
                )
                .unwrap();
            let widget = [sym("AlphaWidget", Kind::Class, 1, None)];
            store
                .replace_file_symbols(repo, "a.rb", "ruby", None, "h", &widget)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'symbols_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("detail=none"), "{sql}");
        let hits: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'idg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "existing names stay searchable without a re-index");
        drop(store);
        let _ = std::fs::remove_file(&path);
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
        let roots = store.checkout_roots(repo).unwrap();
        // both are returned, newest (most-recently inserted) first so a reader
        // tries the current checkout before a stale one
        assert_eq!(roots, vec!["/new/path", "/old/path"]);
    }

    #[test]
    fn forget_checkout_prunes_a_stale_binding() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        store.upsert_checkout(repo, "/old/path", None).unwrap();
        store.upsert_checkout(repo, "/new/path", None).unwrap();
        store.forget_checkout("/old/path").unwrap();
        // only the live binding remains; the repo (and its symbols) is untouched
        assert_eq!(store.checkout_roots(repo).unwrap(), vec!["/new/path"]);
        assert_eq!(store.repository_id("local:/x").unwrap(), Some(repo));
    }

    #[test]
    fn git_ts_is_stored_and_surfaced_on_candidates() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/x"), None)
            .unwrap();
        store
            .replace_file_symbols(
                repo,
                "a.rb",
                "ruby",
                None,
                "h",
                &[sym("Foo", Kind::Class, 1, None)],
            )
            .unwrap();

        let times = HashMap::from([("a.rb".to_string(), 1_700_000_000_i64)]);
        store.set_file_git_ts(repo, &times).unwrap();

        let cands = store
            .search_candidates("foo", 10, false, None, None)
            .unwrap();
        assert_eq!(cands[0].git_ts, Some(1_700_000_000));
    }

    #[test]
    fn indexes_and_reports_coverage() {
        let mut store = Store::open_in_memory().unwrap();
        let id = RepoIdentity::Remote("github.com/dpep/rq".into());
        let repo = store.upsert_repository(&id, Some("main")).unwrap();
        store
            .upsert_checkout(repo, "/tmp/rq", Some("main"))
            .unwrap();

        let symbols = vec![
            sym("User", Kind::Class, 1, None),
            sym("save", Kind::Method, 5, Some("User")),
        ];
        store
            .replace_file_symbols(
                repo,
                "app/models/user.rb",
                "ruby",
                Some(100),
                "h1",
                &symbols,
            )
            .unwrap();
        store.set_coverage(repo, 10, 1, "warming").unwrap();

        let overview = store.coverage_overview().unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].identity, "github.com/dpep/rq");
        assert_eq!(overview[0].status, "warming");
        assert_eq!(overview[0].symbols, 2);
    }

    #[test]
    fn reindexing_a_file_replaces_its_symbols() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/tmp/rq"), None)
            .unwrap();

        store
            .replace_file_symbols(
                repo,
                "a.rb",
                "ruby",
                None,
                "h1",
                &[sym("Old", Kind::Class, 1, None)],
            )
            .unwrap();
        store
            .replace_file_symbols(
                repo,
                "a.rb",
                "ruby",
                None,
                "h2",
                &[sym("New", Kind::Class, 1, None)],
            )
            .unwrap();

        store.set_coverage(repo, 1, 1, "complete").unwrap();
        let overview = store.coverage_overview().unwrap();
        // old symbol gone, new one present → still exactly one symbol
        assert_eq!(overview[0].symbols, 1);
    }

    #[test]
    fn file_unchanged_detects_matching_hash() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&RepoIdentity::local("/tmp/rq"), None)
            .unwrap();
        store
            .replace_file_symbols(repo, "a.rb", "ruby", None, "abc", &[])
            .unwrap();

        assert!(store.file_unchanged(repo, "a.rb", "abc").unwrap());
        assert!(!store.file_unchanged(repo, "a.rb", "xyz").unwrap());
        assert!(!store.file_unchanged(repo, "missing.rb", "abc").unwrap());
    }

    #[test]
    fn fts_sync_indexes_exactly_the_rows_the_trigger_missed() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store
            .upsert_repository(&RepoIdentity::local("/tmp/first"), None)
            .unwrap();
        let second = store
            .upsert_repository(&RepoIdentity::local("/tmp/second"), None)
            .unwrap();
        let widget = [sym("AlphaWidget", Kind::Class, 1, None)];
        store
            .replace_file_symbols(first, "a.rb", "ruby", None, "h", &widget)
            .unwrap();

        // a bulk index of another repo, with per-row FTS suspended
        store.defer_fts_insert().unwrap();
        let gadget = [sym("BetaWidget", Kind::Class, 1, None)];
        store
            .replace_file_symbols(second, "b.rb", "ruby", None, "h", &gadget)
            .unwrap();
        store.sync_fts().unwrap();

        let count = |sql: &str| -> i64 { store.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'dge'"),
            2,
            "both repos searchable, neither indexed twice"
        );
        assert!(!store.fts_trigger_missing().unwrap(), "trigger restored");
        store.sync_fts().unwrap();
        assert_eq!(
            count("SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'dge'"),
            2,
            "a second sync finds nothing left to add"
        );
    }
}
