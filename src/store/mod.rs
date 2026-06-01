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
    s.parent, s.repository_id, r.identity, fi.mtime, fi.git_ts";
const CANDIDATE_FROM: &str = "FROM symbols s \
    JOIN files fi ON fi.id = s.file_id \
    JOIN repositories r ON r.id = s.repository_id";

/// A handle to the rq database.
pub struct Store {
    conn: Connection,
}

/// One row of `rq status` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageRow {
    pub identity: String,
    pub status: String,
    pub files_indexed: i64,
    pub files_seen: i64,
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
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
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
                   (repository_id, file_id, name, name_lower, kind, language, line, parent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                    s.parent,
                ])?;
            }
        }
        tx.commit()
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

    /// All known repositories with their coverage and symbol count.
    pub fn coverage_overview(&self) -> Result<Vec<CoverageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.identity,
                    COALESCE(c.status, 'never'),
                    COALESCE(c.files_indexed, 0),
                    COALESCE(c.files_seen, 0),
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
                    files_indexed: r.get(2)?,
                    files_seen: r.get(3)?,
                    symbols: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
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

    /// Candidate symbols for a query, drawn from two cheap layers and merged:
    /// exact/prefix on `name_lower`, plus a trigram-FTS pass for fuzzy recall.
    /// Ranking happens in `crate::search`; this only narrows the field.
    pub fn search_candidates(&self, query: &str, limit: usize) -> Result<Vec<SymbolRow>> {
        let q = query.to_ascii_lowercase();
        let mut found: HashMap<i64, SymbolRow> = HashMap::new();

        // Layer 1: first-character anchor (index-backed prefix scan) plus the
        // exact name. Anchoring on the first character — rather than the whole
        // query as a prefix — is what lets short skip-abbreviations like
        // `usr → user` reach the scorer; the scorer filters and ranks them.
        if let Some(first) = q.chars().next() {
            let like = format!("{}%", escape_like(&first.to_string()));
            let sql = format!(
                "SELECT {CANDIDATE_COLS} {CANDIDATE_FROM} \
                 WHERE s.name_lower = ?1 OR s.name_lower LIKE ?2 ESCAPE '\\' LIMIT ?3"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![q, like, limit as i64], row_to_candidate)?;
            for row in rows {
                let (id, cand) = row?;
                found.insert(id, cand);
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
            parent: r.get(6)?,
            repository_id: r.get(7)?,
            repo_identity: r.get(8)?,
            mtime: r.get(9)?,
            git_ts: r.get(10)?,
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
            parent: parent.map(String::from),
        }
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

        let cands = store.search_candidates("foo", 10).unwrap();
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
