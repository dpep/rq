//! SQLite schema and migrations.
//!
//! Kept in sync with the schema block in `docs/ARCHITECTURE.md`. The one
//! deviation: `symbols.parent` is the enclosing symbol's qualified *name*
//! (TEXT), not a `parent_id`, which avoids intra-file id resolution and maps
//! straight to [`crate::core::Symbol`].

/// Current schema version. Bump when adding a migration step.
pub const VERSION: i64 = 3;

/// Full schema for a fresh database (already at the current [`VERSION`]).
pub const SCHEMA: &str = r#"
CREATE TABLE repositories (
  id INTEGER PRIMARY KEY,
  identity TEXT UNIQUE NOT NULL,
  display_name TEXT,
  default_branch TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE checkouts (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  root_path TEXT NOT NULL UNIQUE,
  current_branch TEXT
);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  path TEXT NOT NULL,
  language TEXT,
  mtime INTEGER,
  git_ts INTEGER,                    -- last git commit time touching this file
  content_hash TEXT,
  indexed_at INTEGER,
  UNIQUE(repository_id, path)
);

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  file_id INTEGER NOT NULL REFERENCES files(id),
  name TEXT NOT NULL,
  name_lower TEXT NOT NULL,
  kind TEXT NOT NULL,
  language TEXT NOT NULL,
  line INTEGER NOT NULL,
  parent TEXT
);
CREATE INDEX idx_symbols_name_lower ON symbols(name_lower);
CREATE INDEX idx_symbols_file ON symbols(file_id);

-- fuzzy candidate narrowing: trigram FTS over symbol names
CREATE VIRTUAL TABLE symbols_fts USING fts5(
  name,
  content='symbols',
  content_rowid='id',
  tokenize='trigram'
);

-- keep the external-content FTS index in sync with symbols
CREATE TRIGGER symbols_ai AFTER INSERT ON symbols BEGIN
  INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER symbols_ad AFTER DELETE ON symbols BEGIN
  INSERT INTO symbols_fts(symbols_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TRIGGER symbols_au AFTER UPDATE ON symbols BEGIN
  INSERT INTO symbols_fts(symbols_fts, rowid, name) VALUES ('delete', old.id, old.name);
  INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name);
END;

CREATE TABLE coverage (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  scope TEXT NOT NULL DEFAULT 'full',
  files_seen INTEGER,
  files_indexed INTEGER,
  status TEXT NOT NULL,
  last_indexed_at INTEGER,
  UNIQUE(repository_id, scope)
);

-- raw, append-only interaction log
CREATE TABLE events (
  id INTEGER PRIMARY KEY,
  type TEXT NOT NULL,                 -- search | open | select
  query TEXT,                        -- normalized query, when applicable
  repository_id INTEGER,
  path TEXT,                         -- repo-relative file, for open/select
  line INTEGER,
  branch TEXT,
  ts INTEGER NOT NULL
);

-- rollup the ranking hot path reads. Keyed by (file, name) rather than
-- symbol_id so learning survives reindexing (symbol ids are recreated on every
-- file re-extract; file+name is stable).
CREATE TABLE selection_stats (
  repository_id INTEGER NOT NULL,
  query_norm TEXT NOT NULL,
  file TEXT NOT NULL,
  name TEXT NOT NULL,
  selections INTEGER NOT NULL,
  last_selected_at INTEGER,
  PRIMARY KEY (repository_id, query_norm, file, name)
);

-- small key/value store (e.g. the event-rollup high-water mark)
CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// Migration from v1 → v2: stabilize `selection_stats`, reshape `events` for
/// the behavioral-learning rollup, and add the `meta` table. The two tables
/// carried no data in v1, so they are simply recreated.
pub const MIGRATION_V2: &str = r#"
DROP TABLE IF EXISTS selection_stats;
DROP TABLE IF EXISTS events;

CREATE TABLE events (
  id INTEGER PRIMARY KEY,
  type TEXT NOT NULL,
  query TEXT,
  repository_id INTEGER,
  path TEXT,
  line INTEGER,
  branch TEXT,
  ts INTEGER NOT NULL
);

CREATE TABLE selection_stats (
  repository_id INTEGER NOT NULL,
  query_norm TEXT NOT NULL,
  file TEXT NOT NULL,
  name TEXT NOT NULL,
  selections INTEGER NOT NULL,
  last_selected_at INTEGER,
  PRIMARY KEY (repository_id, query_norm, file, name)
);

CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// Migration v2 → v3: add the per-file git last-commit time used by the recency
/// ranking signal.
pub const MIGRATION_V3: &str = r#"
ALTER TABLE files ADD COLUMN git_ts INTEGER;
"#;
