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
}

/// Column projection shared by the candidate queries. Column order is consumed
/// by [`row_to_candidate`].
const CANDIDATE_COLS: &str = "s.id, s.name, s.kind, s.language, fi.path, s.line, \
    s.parent, s.repository_id, r.identity";
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
        if version < schema::VERSION {
            conn.execute_batch(schema::SCHEMA)?;
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
