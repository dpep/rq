//! Storage — SQLite (WAL mode), schema, and queries.
//!
//! The background indexer writes here; search reads. WAL mode lets those
//! happen concurrently. See `docs/ARCHITECTURE.md` for the schema.

mod schema;

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::core::{RepoIdentity, Symbol};

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

/// A learned selection signal for ranking: how often a `(file, name)` was
/// chosen for a query, and when it was last chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionStat {
    pub repository_id: i64,
    pub file: String,
    pub name: String,
    pub selections: i64,
    pub last_selected_at: i64,
}

/// Column projection shared by the candidate queries. Column order is consumed
/// by [`row_to_candidate`].
const CANDIDATE_COLS: &str = "s.id, s.name, s.kind, s.language, fi.path, s.line, \
    s.end_line, s.parent, s.repository_id, r.identity, fi.mtime, fi.git_ts, s.visibility";
const CANDIDATE_FROM: &str = "FROM symbols s \
    JOIN files fi ON fi.id = s.file_id \
    JOIN repositories r ON r.id = s.repository_id";

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
        // wait briefly instead of erroring with "database is locked".
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=3000; \
             PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-16384;",
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
        identity: &RepoIdentity,
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
    /// trigram tokenization. Pair with [`rebuild_fts`](Self::rebuild_fts), which
    /// rebuilds the index in one pass and restores the trigger. No-op safe to
    /// call when the trigger is already gone.
    pub(crate) fn defer_fts_insert(&self) -> Result<()> {
        self.conn
            .execute_batch("DROP TRIGGER IF EXISTS symbols_ai;")?;
        Ok(())
    }

    /// Rebuild the trigram FTS index from the symbols table in one bulk pass —
    /// far cheaper than the per-row trigger on a cold index — then recreate the
    /// `AFTER INSERT` trigger so later incremental writes stay in sync. The
    /// inverse of [`defer_fts_insert`](Self::defer_fts_insert). One transaction:
    /// a concurrent writer either lands before the rebuild (and is captured by
    /// it — the rebuild scans the whole symbols table) or after the trigger is
    /// back, never in between.
    pub(crate) fn rebuild_fts(&self) -> Result<()> {
        let sql = format!(
            "BEGIN IMMEDIATE;\nINSERT INTO symbols_fts(symbols_fts) VALUES('rebuild');\n{}\nCOMMIT;",
            schema::FTS_INSERT_TRIGGER
        );
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    /// Whether the `AFTER INSERT` FTS-sync trigger is currently absent — true
    /// only mid-bulk-index (see [`defer_fts_insert`](Self::defer_fts_insert))
    /// or after one crashed before its [`rebuild_fts`](Self::rebuild_fts).
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

    /// The on-disk root of a repository's checkout, used to resolve relative
    /// paths when validating staleness.
    pub(crate) fn checkout_root(&self, repository_id: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT root_path FROM checkouts WHERE repository_id = ?1 ORDER BY id LIMIT 1",
                params![repository_id],
                |r| r.get(0),
            )
            .optional()
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
    /// their FTS rows, via trigger), files, coverage, learned selections, events,
    /// checkout, and the repository row. Deleted in FK-safe order in one
    /// transaction.
    pub(crate) fn drop_repository(&mut self, repository_id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        for sql in [
            "DELETE FROM symbols WHERE repository_id = ?1",
            "DELETE FROM files WHERE repository_id = ?1",
            "DELETE FROM coverage WHERE repository_id = ?1",
            "DELETE FROM selection_stats WHERE repository_id = ?1",
            "DELETE FROM events WHERE repository_id = ?1",
            "DELETE FROM checkouts WHERE repository_id = ?1",
            "DELETE FROM repositories WHERE id = ?1",
        ] {
            tx.execute(sql, params![repository_id])?;
        }
        tx.commit()
    }

    // ----- behavioral learning -----

    /// Append a raw interaction event (the cheap write on the hot path; rollup
    /// happens later in [`Store::aggregate_events`]).
    pub(crate) fn record_event(
        &self,
        kind: &str,
        query: Option<&str>,
        repository_id: Option<i64>,
        path: Option<&str>,
        line: Option<i64>,
        branch: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (type, query, repository_id, path, line, branch, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, query, repository_id, path, line, branch, now_unix()],
        )?;
        Ok(())
    }

    /// Learned selections relevant to a query, read by ranking. Matches not just
    /// the exact query but any *shorter* query the user has selected for — a pick
    /// for `han` informs `handler` — so typing more keeps the benefit.
    pub(crate) fn selections_for(&self, query_norm: &str) -> Result<Vec<SelectionStat>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT repository_id, file, name, selections, last_selected_at
             FROM selection_stats WHERE ?1 LIKE query_norm || '%'",
        )?;
        let rows = stmt
            .query_map(params![query_norm], |r| {
                Ok(SelectionStat {
                    repository_id: r.get(0)?,
                    file: r.get(1)?,
                    name: r.get(2)?,
                    selections: r.get(3)?,
                    last_selected_at: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Roll up to `batch` new `open`/`select` events into `selection_stats`.
    /// Returns how many events were processed. Resolves the chosen symbol from
    /// `(repo, path, line)` at rollup time, turning a selection into a
    /// `(query, file, name)` signal. This is the amortized post-processing run
    /// after a user interaction.
    pub(crate) fn aggregate_events(&mut self, batch: usize) -> Result<usize> {
        let hwm = self.meta_get_i64("events_hwm")?.unwrap_or(0);

        type Pending = (
            i64,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<i64>,
            i64,
        );
        let pending: Vec<Pending> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, query, repository_id, path, line, ts FROM events
                 WHERE id > ?1 AND type IN ('select', 'open')
                 ORDER BY id LIMIT ?2",
            )?;
            stmt.query_map(params![hwm, batch as i64], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
        };

        if pending.is_empty() {
            // advance past trailing non-selection events so we don't rescan them
            let max_id: Option<i64> =
                self.conn
                    .query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))?;
            if let Some(m) = max_id.filter(|m| *m > hwm) {
                self.meta_set_i64("events_hwm", m)?;
            }
            return Ok(0);
        }

        let drained = pending.len() < batch;
        let max_pending_id = pending.iter().map(|p| p.0).max().unwrap_or(hwm);

        let tx = self.conn.transaction()?;
        let mut processed = 0;
        for (_id, query, repo, path, line, ts) in &pending {
            processed += 1;
            let (Some(query), Some(repo), Some(path)) = (query, repo, path) else {
                continue;
            };
            let name: Option<String> = match line {
                Some(line) => tx
                    .query_row(
                        "SELECT s.name FROM symbols s JOIN files fi ON fi.id = s.file_id
                         WHERE s.repository_id = ?1 AND fi.path = ?2 AND s.line <= ?3
                         ORDER BY s.line DESC LIMIT 1",
                        params![repo, path, line],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => tx
                    .query_row(
                        "SELECT s.name FROM symbols s JOIN files fi ON fi.id = s.file_id
                         WHERE s.repository_id = ?1 AND fi.path = ?2
                           AND s.kind IN ('class', 'module')
                         ORDER BY s.line ASC LIMIT 1",
                        params![repo, path],
                        |r| r.get(0),
                    )
                    .optional()?,
            };
            if let Some(name) = name {
                tx.execute(
                    "INSERT INTO selection_stats
                       (repository_id, query_norm, file, name, selections, last_selected_at)
                     VALUES (?1, ?2, ?3, ?4, 1, ?5)
                     ON CONFLICT(repository_id, query_norm, file, name) DO UPDATE SET
                       selections = selections + 1,
                       last_selected_at = max(last_selected_at, excluded.last_selected_at)",
                    params![repo, query, path, name, ts],
                )?;
            }
        }

        let new_hwm = if drained {
            tx.query_row("SELECT MAX(id) FROM events", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .unwrap_or(max_pending_id)
        } else {
            max_pending_id
        };
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('events_hwm', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![new_hwm.to_string()],
        )?;
        tx.commit()?;
        Ok(processed)
    }

    /// Keep the raw `events` log bounded. Deletes only events that have already
    /// been rolled up (id ≤ the aggregation high-water mark) and are not among
    /// the most recent `keep_recent` rows. Returns the number deleted.
    pub(crate) fn prune_events(&self, keep_recent: i64) -> Result<usize> {
        let hwm = self.meta_get_i64("events_hwm")?.unwrap_or(0);
        let max_id: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))?;
        let Some(max_id) = max_id else {
            return Ok(0);
        };
        let cutoff = hwm.min(max_id - keep_recent);
        if cutoff <= 0 {
            return Ok(0);
        }
        let n = self
            .conn
            .execute("DELETE FROM events WHERE id <= ?1", params![cutoff])?;
        Ok(n)
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

    /// The cached branch-changed file list for a repo: `(stamp, computed_at,
    /// files)`. Stored rather than recomputed because the git diff behind it is
    /// O(tracked files) and runs on the search path.
    pub(crate) fn branch_files_get(
        &self,
        identity: &str,
    ) -> Result<Option<(String, i64, Vec<String>)>> {
        let Some(raw) = self.meta_get(&format!("branch_files:{identity}"))? else {
            return Ok(None);
        };
        let mut lines = raw.lines();
        let (Some(stamp), Some(at)) = (lines.next(), lines.next()) else {
            return Ok(None);
        };
        let Ok(at) = at.parse::<i64>() else {
            return Ok(None);
        };
        Ok(Some((
            stamp.to_string(),
            at,
            lines.map(str::to_string).collect(),
        )))
    }

    pub(crate) fn branch_files_set(
        &self,
        identity: &str,
        stamp: &str,
        at: i64,
        files: &[String],
    ) -> Result<()> {
        // Newline-delimited: git paths can't contain one, and it beats pulling
        // in a serializer for three fields.
        let mut value = format!("{stamp}\n{at}");
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

    fn meta_get_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.meta_get(key)?.and_then(|s| s.parse().ok()))
    }

    fn meta_set_i64(&self, key: &str, value: i64) -> Result<()> {
        self.meta_set(key, &value.to_string())
    }

    /// Candidate symbols for a query, drawn from cheap layers and merged:
    /// exact/prefix on `name_lower`, then broad fuzzy recall (first-char anchor,
    /// trigram FTS, path). Ranking happens in `crate::search`; this only narrows
    /// the field.
    ///
    /// When `force_fuzzy` is false and exact/prefix already matched, the broad
    /// fuzzy layers are skipped: the relevance gate drops every fuzzy candidate
    /// once a strong (exact/prefix) hit exists, so fetching and scoring them is
    /// wasted. A wildcard query passes `force_fuzzy = true` — it isn't gated and
    /// always needs the trigram recall.
    pub(crate) fn search_candidates(
        &self,
        query: &str,
        limit: usize,
        force_fuzzy: bool,
    ) -> Result<Vec<SymbolRow>> {
        let q = query.to_ascii_lowercase();
        let mut found: HashMap<i64, SymbolRow> = HashMap::new();

        // exact name — always included, never subject to the cap. The
        // match we most want must reach the scorer no matter how large the index
        // is (a broad capped scan could otherwise truncate it away).
        {
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} WHERE s.name_lower = ?1 LIMIT ?2"
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![q, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.insert(id, cand);
            }
        }

        // query as a prefix — selective, so prefix matches always
        // surface even on a huge repo (unlike the broad first-char anchor below,
        // which the cap can truncate).
        {
            let like = format!("{}%", escape_like(&q));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
                 WHERE s.name_lower LIKE ?1 ESCAPE '\\' LIMIT ?2"
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![like, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
        }

        // Fast path: a strong (exact/prefix) match exists, so the relevance gate
        // will discard everything the broad layers below would add. Skip them —
        // identical results, no wasted fetch/score. (Wildcard queries force the
        // fuzzy layers; they aren't gated.)
        if !force_fuzzy && !found.is_empty() {
            return Ok(found.into_values().collect());
        }

        // fuzzy recall (a): first-character anchor (index-backed scan) for short
        // skip-abbreviations like `usr → user` that prefix matching can't reach;
        // the scorer filters and ranks. Best-effort under the cap — exact and
        // prefix are already guaranteed above.
        if let Some(first) = q.chars().next() {
            let like = format!("{}%", escape_like(&first.to_string()));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
                 WHERE s.name_lower LIKE ?1 ESCAPE '\\' LIMIT ?2"
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![like, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
        }

        // fuzzy recall (b): trigram FTS (OR of the query's trigrams).
        if let Some(match_expr) = trigram_or_query(&q) {
            let sql = format!(
                "SELECT {CANDIDATE_COLS} FROM symbols_fts f \
                 JOIN symbols s ON s.id = f.rowid \
                 JOIN files fi ON fi.id = s.file_id \
                 JOIN repositories r ON r.id = s.repository_id \
                 WHERE symbols_fts MATCH ?1 LIMIT ?2"
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![match_expr, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
        }

        // path recall: primary definitions in files whose path matches the query,
        // so `billing` can surface the class defined in `billing.rb`.
        let path_like = format!("%{}%", escape_like(&q));
        let sql = format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE fi.path LIKE ?1 ESCAPE '\\' AND s.kind IN ('class', 'module') LIMIT ?2"
        );
        {
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![path_like, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
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

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Kind;

    #[test]
    fn branch_files_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.branch_files_get("repo").unwrap().is_none());

        let files = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        store
            .branch_files_set("repo", "123:456", 99, &files)
            .unwrap();
        let (stamp, at, got) = store.branch_files_get("repo").unwrap().unwrap();
        assert_eq!(stamp, "123:456");
        assert_eq!(at, 99);
        assert_eq!(got, files);

        // a later write replaces the entry rather than accumulating
        store.branch_files_set("repo", "789:1", 100, &[]).unwrap();
        let (stamp, _, got) = store.branch_files_get("repo").unwrap().unwrap();
        assert_eq!(stamp, "789:1");
        assert!(got.is_empty(), "an empty list is a real answer, not a miss");

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
            // simulate a pre-v5 database: no repo-scoped indexes, and the
            // (since-dropped) display_name column still present
            let store = Store::open(&path).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP INDEX idx_symbols_repo; DROP INDEX idx_events_repo; \
                     ALTER TABLE repositories ADD COLUMN display_name TEXT; \
                     ALTER TABLE symbols DROP COLUMN visibility; \
                     PRAGMA user_version=4;",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let n: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND name IN ('idx_symbols_repo','idx_events_repo')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        drop(store);
        let _ = std::fs::remove_file(&path);
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
    fn prune_events_drops_aggregated_but_keeps_recent() {
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

        // a select, then a run of searches (so the newest event is a search)
        store
            .record_event(
                "select",
                Some("foo"),
                Some(repo),
                Some("a.rb"),
                Some(1),
                None,
            )
            .unwrap();
        for _ in 0..10 {
            store
                .record_event("search", Some("foo"), Some(repo), None, None, None)
                .unwrap();
        }
        store.aggregate_events(100).unwrap(); // hwm advances to the last id (11)

        // 11 events, all aggregated; keep the 3 newest → drop ids 1..=8
        assert_eq!(store.prune_events(3).unwrap(), 8);
        // idempotent: nothing left to prune
        assert_eq!(store.prune_events(3).unwrap(), 0);
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

        let cands = store.search_candidates("foo", 10, false).unwrap();
        assert_eq!(cands[0].git_ts, Some(1_700_000_000));
    }

    #[test]
    fn aggregates_a_selection_and_decays_on_repeat() {
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

        // a selection for "foo" rolls up into one learned stat
        store
            .record_event(
                "select",
                Some("foo"),
                Some(repo),
                Some("a.rb"),
                Some(1),
                None,
            )
            .unwrap();
        assert_eq!(store.aggregate_events(10).unwrap(), 1);
        assert_eq!(store.selections_for("foo").unwrap().len(), 1);
        // ...and a longer query still benefits (prefix learning)
        assert_eq!(store.selections_for("foobar").unwrap().len(), 1);
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
}
