//! SQLite schema and migrations.
//!
//! Kept in sync with the schema block in `docs/ARCHITECTURE.md`. The one
//! deviation: `symbols.parent` is the enclosing symbol's qualified *name*
//! (TEXT), not a `parent_id`, which avoids intra-file id resolution and maps
//! straight to [`crate::core::Symbol`].

/// Current schema version. Bump when adding a migration step.
pub const VERSION: i64 = 9;

/// Full schema for a fresh database (already at the current [`VERSION`]).
/// The `symbols_ai` FTS-sync trigger lives in [`FTS_INSERT_TRIGGER`] (a cold
/// bulk index drops and recreates it around a rebuild) and is applied alongside
/// this on a fresh database.
pub const SCHEMA: &str = r#"
CREATE TABLE repositories (
  id INTEGER PRIMARY KEY,
  identity TEXT UNIQUE NOT NULL,
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
  mtime INTEGER,                     -- last-modified time, unix *nanoseconds*
                                     -- (git-style racy-edit protection)
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
  end_line INTEGER,                  -- 1-based last line of the definition body
  parent TEXT,
  visibility TEXT                    -- public|crate|private|protected; NULL when
                                     -- unknown (pre-v9 rows backfill lazily)
);
CREATE INDEX idx_symbols_name_lower ON symbols(name_lower);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_symbols_repo ON symbols(repository_id);

-- fuzzy candidate narrowing: trigram FTS over symbol names
CREATE VIRTUAL TABLE symbols_fts USING fts5(
  name,
  content='symbols',
  content_rowid='id',
  tokenize='trigram'
);

-- keep the external-content FTS index in sync with symbols
-- (the AFTER INSERT trigger is FTS_INSERT_TRIGGER, defined once below)
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
CREATE INDEX idx_events_repo ON events(repository_id, id);

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

/// Migration v3 → v4: add the definition's end line, so a result carries the
/// full `line..=end_line` span. Existing rows read `NULL` and backfill lazily as
/// files change (or on an explicit `rq --drop` + `rq --index`) — the same
/// lazy-fill the v3 `git_ts` column uses. New/edited files get it immediately.
pub const MIGRATION_V4: &str = r#"
ALTER TABLE symbols ADD COLUMN end_line INTEGER;
"#;

/// Migration v4 → v5: indexes for repo-scoped scans. `symbols(repository_id)`
/// backs the per-repo totals/drop/coverage counts; `events(repository_id, id)`
/// backs the newest-event probe (`is_repeat_search`) that runs on every search.
pub const MIGRATION_V5: &str = r#"
CREATE INDEX IF NOT EXISTS idx_symbols_repo ON symbols(repository_id);
CREATE INDEX IF NOT EXISTS idx_events_repo ON events(repository_id, id);
"#;

/// Migration v5 → v6: `files.mtime` moves from unix seconds to nanoseconds, so
/// two edits within the same second get distinct mtimes and the incremental
/// skip can't mistake the later one for "unchanged" (git's racy-mtime fix).
/// Existing second-resolution rows are scaled in place; the magnitude guard
/// keeps a re-run (or an already-converted row) from double-scaling.
pub const MIGRATION_V6: &str = r#"
UPDATE files SET mtime = mtime * 1000000000
  WHERE mtime IS NOT NULL AND mtime < 100000000000;
"#;

/// Migration v6 → v7: drop `repositories.display_name` — never written or read.
pub const MIGRATION_V7: &str = r#"
ALTER TABLE repositories DROP COLUMN display_name;
"#;

/// Migration v7 → v8: retire the `partial` coverage status. A subtree index
/// (`--index --path`) is now a *seed* rather than a fence — coverage stays
/// `warming` so normal warming continues over the rest of the repo.
pub const MIGRATION_V8: &str = r#"
UPDATE coverage SET status = 'warming' WHERE status = 'partial';
"#;

/// Migration v8 → v9: record each definition's visibility, a ranking hint
/// (private helpers below public API). Existing rows read `NULL` (no penalty)
/// and backfill lazily as files re-extract.
pub const MIGRATION_V9: &str = r#"
ALTER TABLE symbols ADD COLUMN visibility TEXT;
"#;

/// The cumulative migration ladder for existing databases: apply every step
/// whose version exceeds the database's `user_version`.
pub const MIGRATIONS: [(i64, &str); 8] = [
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
    (6, MIGRATION_V6),
    (7, MIGRATION_V7),
    (8, MIGRATION_V8),
    (9, MIGRATION_V9),
];

/// The `AFTER INSERT` FTS-sync trigger — defined once, applied with [`SCHEMA`]
/// on a fresh database. A cold bulk index drops this trigger, inserts symbols
/// without per-row FTS maintenance, rebuilds the FTS index in one pass, then
/// recreates it from here.
pub const FTS_INSERT_TRIGGER: &str = r#"
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
  INSERT INTO symbols_fts(rowid, name) VALUES (new.id, new.name);
END;
"#;
