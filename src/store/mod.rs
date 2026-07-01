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

pub type Result<T> = rusqlite::Result<T>;

/// A symbol as returned by search candidate queries (joined with its file and
/// repository for display and ranking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
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
    /// File mtime (unix seconds) — a recency signal.
    pub mtime: Option<i64>,
    /// Last git commit time touching the file — the stronger recency signal.
    pub git_ts: Option<i64>,
}

/// A learned selection signal for ranking: how often a `(file, name)` was
/// chosen for a query, and when it was last chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionStat {
    pub repository_id: i64,
    pub file: String,
    pub name: String,
    pub selections: i64,
    pub last_selected_at: i64,
}

/// Column projection shared by the candidate queries. Column order is consumed
/// by [`row_to_candidate`].
const CANDIDATE_COLS: &str = "s.id, s.name, s.kind, s.language, fi.path, s.line, \
    s.end_line, s.parent, s.repository_id, r.identity, fi.mtime, fi.git_ts";
const CANDIDATE_FROM: &str = "FROM symbols s \
    JOIN files fi ON fi.id = s.file_id \
    JOIN repositories r ON r.id = s.repository_id";

/// A handle to the rq database.
pub struct Store {
    conn: Connection,
}

/// A parsed file ready to persist — the unit the indexer produces (in parallel)
/// and [`Store::replace_files`] writes in one batched transaction.
#[derive(Debug, Clone)]
pub struct FileSymbols {
    pub path: String,
    pub language: String,
    pub mtime: Option<i64>,
    pub content_hash: String,
    pub symbols: Vec<Symbol>,
}

/// One row of `rq status` output — the current indexed totals for a repo (not
/// any single run's incremental counts).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CoverageRow {
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
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory database — used by tests.
    pub fn open_in_memory() -> Result<Store> {
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
        }
        // cumulative migrations for existing databases (each guards its version)
        if version != 0 && version < 2 {
            conn.execute_batch(schema::MIGRATION_V2)?;
        }
        if version != 0 && version < 3 {
            conn.execute_batch(schema::MIGRATION_V3)?;
        }
        if version != 0 && version < 4 {
            conn.execute_batch(schema::MIGRATION_V4)?;
        }
        if version != schema::VERSION {
            conn.pragma_update(None, "user_version", schema::VERSION)?;
        }
        Ok(Store { conn })
    }

    /// Insert or update a repository, returning its id.
    pub fn upsert_repository(
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
    pub fn upsert_checkout(
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
    pub fn file_unchanged(
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
    pub fn file_mtimes(&self, repository_id: i64) -> Result<HashMap<String, Option<i64>>> {
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

    /// Replace all symbols for one file in a single transaction: upsert the
    /// file row, drop its old symbols, insert the new ones.
    pub fn replace_file_symbols(
        &mut self,
        repository_id: i64,
        path: &str,
        language: &str,
        mtime: Option<i64>,
        content_hash: &str,
        symbols: &[Symbol],
    ) -> Result<()> {
        let now = now_unix();
        let tx = self.conn.transaction()?;
        let file_id: i64 = tx.query_row(
            "INSERT INTO files (repository_id, path, language, mtime, content_hash, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(repository_id, path) DO UPDATE SET
               language = excluded.language,
               mtime = excluded.mtime,
               content_hash = excluded.content_hash,
               indexed_at = excluded.indexed_at
             RETURNING id",
            params![repository_id, path, language, mtime, content_hash, now],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols
                   (repository_id, file_id, name, name_lower, kind, language, line, end_line, parent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for s in symbols {
                stmt.execute(params![
                    repository_id,
                    file_id,
                    s.name,
                    s.name.to_lowercase(),
                    s.kind.as_str(),
                    s.language,
                    s.line,
                    s.end_line,
                    s.parent,
                ])?;
            }
        }
        tx.commit()
    }

    /// Write many parsed files, one transaction per chunk — a batched `fsync`
    /// instead of one per file, while bounding how much a single transaction
    /// holds (a cold index of a huge repo would otherwise be one enormous txn).
    /// A file whose content hash already matches the index is skipped (not
    /// rewritten). Returns `(files_written, symbols_written)`; skips don't count.
    pub fn replace_files(
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
                let mut clear = tx.prepare("DELETE FROM symbols WHERE file_id = ?1")?;
                let mut insert = tx.prepare(
                    "INSERT INTO symbols
                       (repository_id, file_id, name, name_lower, kind, language, line, end_line, parent)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for f in chunk {
                    // content unchanged (e.g. mtime moved but bytes didn't): skip
                    let stored: Option<String> = current
                        .query_row(params![repository_id, f.path], |r| r.get(0))
                        .optional()?;
                    if stored.as_deref() == Some(f.content_hash.as_str()) {
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
    pub fn defer_fts_insert(&self) -> Result<()> {
        self.conn
            .execute_batch("DROP TRIGGER IF EXISTS symbols_ai;")?;
        Ok(())
    }

    /// Rebuild the trigram FTS index from the symbols table in one bulk pass —
    /// far cheaper than the per-row trigger on a cold index — then recreate the
    /// `AFTER INSERT` trigger so later incremental writes stay in sync. The
    /// inverse of [`defer_fts_insert`](Self::defer_fts_insert).
    pub fn rebuild_fts(&self) -> Result<()> {
        let sql = format!(
            "INSERT INTO symbols_fts(symbols_fts) VALUES('rebuild');\n{}",
            schema::FTS_INSERT_TRIGGER
        );
        self.conn.execute_batch(&sql)?;
        Ok(())
    }

    /// Record indexing coverage for a repository (scope `full`).
    pub fn set_coverage(
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
    pub fn set_file_git_ts(
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
    pub fn coverage_overview(&self) -> Result<Vec<CoverageRow>> {
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
    pub fn identity_for_root(&self, root: &str) -> Result<Option<String>> {
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
    pub fn repository_id(&self, identity: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM repositories WHERE identity = ?1",
                params![identity],
                |r| r.get(0),
            )
            .optional()
    }

    /// Coverage status for a repository's full scope (`never`/`partial`/
    /// `complete`), or `None` if the repository is unknown.
    pub fn coverage_status(&self, identity: &str) -> Result<Option<String>> {
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
    pub fn repo_totals(&self, repository_id: i64) -> Result<(i64, i64)> {
        let files = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE repository_id = ?1",
            params![repository_id],
            |r| r.get(0),
        )?;
        let symbols = self.conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE repository_id = ?1",
            params![repository_id],
            |r| r.get(0),
        )?;
        Ok((files, symbols))
    }

    /// Every symbol defined in one file (repo-relative path), in line order — a
    /// structural outline rather than a ranked search. Backed by `idx_symbols_file`.
    pub fn symbols_in_file(&self, repository_id: i64, path: &str) -> Result<Vec<SymbolRow>> {
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
    pub fn checkout_root(&self, repository_id: i64) -> Result<Option<String>> {
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
    pub fn checkout_roots(&self, repository_id: i64) -> Result<Vec<String>> {
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
    pub fn forget_checkout(&mut self, root_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM checkouts WHERE root_path = ?1",
            params![root_path],
        )?;
        Ok(())
    }

    /// Drop a file and its symbols — used when a file has been deleted on disk.
    pub fn forget_file(&mut self, repository_id: i64, path: &str) -> Result<()> {
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
    pub fn drop_repository(&mut self, repository_id: i64) -> Result<()> {
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
    pub fn record_event(
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
    pub fn selections_for(&self, query_norm: &str) -> Result<Vec<SelectionStat>> {
        let mut stmt = self.conn.prepare(
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

    /// Whether the most recent event for this repo is a `search` for the same
    /// query — i.e. the query was repeated with no selection in between, a
    /// signal that the last results missed.
    pub fn is_repeat_search(&self, repository_id: i64, query_norm: &str) -> Result<bool> {
        let last: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT type, query FROM events WHERE repository_id = ?1 ORDER BY id DESC LIMIT 1",
                params![repository_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(matches!(last, Some((kind, Some(q))) if kind == "search" && q == query_norm))
    }

    /// Decay the learned boost for a query — a repeated search signals the
    /// learned pick didn't satisfy. Rows that reach zero are dropped.
    pub fn decay_selections(&self, repository_id: i64, query_norm: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE selection_stats SET selections = selections - 1
             WHERE repository_id = ?1 AND query_norm = ?2",
            params![repository_id, query_norm],
        )?;
        self.conn.execute(
            "DELETE FROM selection_stats
             WHERE repository_id = ?1 AND query_norm = ?2 AND selections <= 0",
            params![repository_id, query_norm],
        )?;
        Ok(())
    }

    /// Roll up to `batch` new `open`/`select` events into `selection_stats`.
    /// Returns how many events were processed. Resolves the chosen symbol from
    /// `(repo, path, line)` at rollup time, turning a selection into a
    /// `(query, file, name)` signal. This is the amortized post-processing run
    /// after a user interaction.
    pub fn aggregate_events(&mut self, batch: usize) -> Result<usize> {
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
    /// the most recent `keep_recent` rows (which `is_repeat_search` needs).
    /// Returns the number deleted.
    pub fn prune_events(&self, keep_recent: i64) -> Result<usize> {
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
    pub fn indexed_head(&self, repository_id: i64) -> Result<Option<String>> {
        self.meta_get(&format!("head:{repository_id}"))
    }

    /// Record the git HEAD sha at a complete index.
    pub fn set_indexed_head(&self, repository_id: i64, head: &str) -> Result<()> {
        self.meta_set(&format!("head:{repository_id}"), head)
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
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(raw.and_then(|s| s.parse().ok()))
    }

    fn meta_set_i64(&self, key: &str, value: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
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
    pub fn search_candidates(
        &self,
        query: &str,
        limit: usize,
        force_fuzzy: bool,
    ) -> Result<Vec<SymbolRow>> {
        let q = query.to_ascii_lowercase();
        let mut found: HashMap<i64, SymbolRow> = HashMap::new();

        // Layer 0: exact name — always included, never subject to the cap. The
        // match we most want must reach the scorer no matter how large the index
        // is (a broad capped scan could otherwise truncate it away).
        {
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} WHERE s.name_lower = ?1 LIMIT ?2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![q, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.insert(id, cand);
            }
        }

        // Layer 1: query as a prefix — selective, so prefix matches always
        // surface even on a huge repo (unlike the broad first-char anchor below,
        // which the cap can truncate).
        {
            let like = format!("{}%", escape_like(&q));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
                 WHERE s.name_lower LIKE ?1 ESCAPE '\\' LIMIT ?2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
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

        // Layer 2: first-character anchor (index-backed scan) for short
        // skip-abbreviations like `usr → user` that prefix matching can't reach;
        // the scorer filters and ranks. Best-effort under the cap — exact and
        // prefix are already guaranteed above.
        if let Some(first) = q.chars().next() {
            let like = format!("{}%", escape_like(&first.to_string()));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
                 WHERE s.name_lower LIKE ?1 ESCAPE '\\' LIMIT ?2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![like, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
        }

        // Layer 2: trigram FTS (OR of the query's trigrams) for fuzzy recall.
        if let Some(match_expr) = trigram_or_query(&q) {
            let sql = format!(
                "SELECT {CANDIDATE_COLS} FROM symbols_fts f \
                 JOIN symbols s ON s.id = f.rowid \
                 JOIN files fi ON fi.id = s.file_id \
                 JOIN repositories r ON r.id = s.repository_id \
                 WHERE symbols_fts MATCH ?1 LIMIT ?2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![match_expr, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.entry(id).or_insert(cand);
            }
        }

        // Layer 3: primary definitions in files whose path matches the query,
        // so `billing` can surface the class defined in `billing.rb`.
        let path_like = format!("%{}%", escape_like(&q));
        let sql = format!(
            "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
             WHERE fi.path LIKE ?1 ESCAPE '\\' AND s.kind IN ('class', 'module') LIMIT ?2"
        );
        {
            let mut stmt = self.conn.prepare(&sql)?;
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

    fn sym(name: &str, kind: Kind, line: u32, parent: Option<&str>) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            language: "ruby".into(),
            file: "app/models/user.rb".into(),
            line,
            end_line: line,
            parent: parent.map(String::from),
        }
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
        // the most recent event still drives repeat detection
        assert!(store.is_repeat_search(repo, "foo").unwrap());
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

        // last event is the select → not a repeat
        assert!(!store.is_repeat_search(repo, "foo").unwrap());
        // re-search "foo" without opening anything → repeat
        store
            .record_event("search", Some("foo"), Some(repo), None, None, None)
            .unwrap();
        assert!(store.is_repeat_search(repo, "foo").unwrap());

        // decaying the lone selection drops it
        store.decay_selections(repo, "foo").unwrap();
        assert!(store.selections_for("foo").unwrap().is_empty());
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
        store.set_coverage(repo, 10, 1, "partial").unwrap();

        let overview = store.coverage_overview().unwrap();
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].identity, "github.com/dpep/rq");
        assert_eq!(overview[0].status, "partial");
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
