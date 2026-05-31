//! SQLite schema and migrations.
//!
//! Kept in sync with the schema block in `docs/ARCHITECTURE.md`. The one
//! deviation: `symbols.parent` is the enclosing symbol's qualified *name*
//! (TEXT), not a `parent_id`, which avoids intra-file id resolution and maps
//! straight to [`crate::core::Symbol`].

/// Current schema version. Bump when adding a migration step.
pub const VERSION: i64 = 1;

/// Full schema, applied when the database is at version 0.
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

CREATE TABLE events (
  id INTEGER PRIMARY KEY,
  type TEXT NOT NULL,
  query TEXT,
  repository_id INTEGER,
  file_id INTEGER,
  symbol_id INTEGER,
  branch TEXT,
  ts INTEGER NOT NULL
);

CREATE TABLE selection_stats (
  repository_id INTEGER NOT NULL,
  query_norm TEXT NOT NULL,
  symbol_id INTEGER NOT NULL,
  selections INTEGER NOT NULL,
  last_selected_at INTEGER,
  PRIMARY KEY (repository_id, query_norm, symbol_id)
);
"#;
