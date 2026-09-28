//! SQLite schema and migrations.
//!
//! Kept in sync with the schema block in `docs/ARCHITECTURE.md`. The one
//! deviation: `symbols.parent` is the enclosing symbol's qualified *name*
//! (TEXT), not a `parent_id`, which avoids intra-file id resolution and maps
//! straight to [`crate::core::Symbol`].

/// Current schema version. Bump when adding a migration step.
pub(crate) const VERSION: i64 = 19;

/// Full schema for a fresh database (already at the current [`VERSION`]).
pub(crate) const SCHEMA: &str = r#"
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
  generated INTEGER NOT NULL DEFAULT 0, -- declares itself generated (a header marker)
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
  visibility TEXT                    -- public|crate|private|protected|local;
                                     -- NULL when unknown (pre-v9 rows
                                     -- backfill lazily)
);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_symbols_repo_name ON symbols(repository_id, name_lower);

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

-- usage counters, one row per (day, caller, flag set), incremented on write.
-- The only record of how rq is actually used. Read by `--usage`, never by
-- ranking.
CREATE TABLE usage_daily (
  day TEXT NOT NULL,                 -- local date, YYYY-MM-DD
  source TEXT NOT NULL,
  flags TEXT NOT NULL,
  searches INTEGER NOT NULL,
  misses INTEGER NOT NULL,           -- answered nothing, against a ready index
  warming INTEGER NOT NULL,          -- answered nothing because it wasn't ready
  on_complete INTEGER NOT NULL,      -- ran against a fully indexed repo
  live INTEGER NOT NULL DEFAULT 0,   -- answered from a live scan, not the index
  PRIMARY KEY (day, source, flags)
);

-- the name index (docs/NAME_INDEX.md): each repo's distinct symbol names and
-- file paths as fixed-size signatures, in append-order chunks. `keys` is each
-- key's end offset (u32), then the keys' bytes.
CREATE TABLE name_sigs (
  repository_id INTEGER NOT NULL,
  kind INTEGER NOT NULL,             -- 0 symbol names, 1 file paths (by stem)
  chunk INTEGER NOT NULL,
  n INTEGER NOT NULL,
  sigs BLOB NOT NULL,
  keys BLOB NOT NULL,
  PRIMARY KEY (repository_id, kind, chunk)
);

-- a repo's index is read only while it's current: built under this format,
-- and maintained since; -1 while a cold pass suspends it. `built` is how many
-- names the last rebuild wrote.
CREATE TABLE name_index (
  repository_id INTEGER PRIMARY KEY,
  format INTEGER NOT NULL,
  built INTEGER NOT NULL
);

-- small key/value store (indexed HEAD, warm lock, branch-file cache)
CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// Migration from v1 → v2: stabilize `selection_stats`, reshape `events` for
/// the behavioral-learning rollup, and add the `meta` table. The two tables
/// carried no data in v1, so they are simply recreated.
pub(crate) const MIGRATION_V2: &str = r#"
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
pub(crate) const MIGRATION_V3: &str = r#"
ALTER TABLE files ADD COLUMN git_ts INTEGER;
"#;

/// Migration v3 → v4: add the definition's end line, so a result carries the
/// full `line..=end_line` span. Existing rows read `NULL` and backfill lazily as
/// files change (or on an explicit `rq --drop` + `rq --index`) — the same
/// lazy-fill the v3 `git_ts` column uses. New/edited files get it immediately.
pub(crate) const MIGRATION_V4: &str = r#"
ALTER TABLE symbols ADD COLUMN end_line INTEGER;
"#;

/// Migration v4 → v5: indexes for repo-scoped scans. `symbols(repository_id)`
/// backs the per-repo totals/drop/coverage counts; `events(repository_id, id)`
/// backs the per-repo event scans (rollup, prune).
pub(crate) const MIGRATION_V5: &str = r#"
CREATE INDEX IF NOT EXISTS idx_symbols_repo ON symbols(repository_id);
CREATE INDEX IF NOT EXISTS idx_events_repo ON events(repository_id, id);
"#;

/// Migration v5 → v6: `files.mtime` moves from unix seconds to nanoseconds, so
/// two edits within the same second get distinct mtimes and the incremental
/// skip can't mistake the later one for "unchanged" (git's racy-mtime fix).
/// Existing second-resolution rows are scaled in place; the magnitude guard
/// keeps a re-run (or an already-converted row) from double-scaling.
pub(crate) const MIGRATION_V6: &str = r#"
UPDATE files SET mtime = mtime * 1000000000
  WHERE mtime IS NOT NULL AND mtime < 100000000000;
"#;

/// Migration v6 → v7: drop `repositories.display_name` — never written or read.
pub(crate) const MIGRATION_V7: &str = r#"
ALTER TABLE repositories DROP COLUMN display_name;
"#;

/// Migration v7 → v8: retire the `partial` coverage status. A subtree index
/// (`--index --path`) is now a *seed* rather than a fence — coverage stays
/// `warming` so normal warming continues over the rest of the repo.
pub(crate) const MIGRATION_V8: &str = r#"
UPDATE coverage SET status = 'warming' WHERE status = 'partial';
"#;

/// Migration v8 → v9: record each definition's visibility, a ranking hint
/// (private helpers below public API). Existing rows read `NULL` (no penalty)
/// and backfill lazily as files re-extract.
pub(crate) const MIGRATION_V9: &str = r#"
ALTER TABLE symbols ADD COLUMN visibility TEXT;
"#;

/// Migration v9 -> v10: usage observability. `events` gains the caller label,
/// the hit count, and the flag set for `search` rows; `usage_daily` accumulates
/// the same counts so they survive the rolling prune of the raw log. Existing
/// rows read `NULL` — they predate the columns and there is nothing to backfill
/// from.
pub(crate) const MIGRATION_V10: &str = r#"
ALTER TABLE events ADD COLUMN source TEXT;
ALTER TABLE events ADD COLUMN results INTEGER;
ALTER TABLE events ADD COLUMN flags TEXT;

CREATE TABLE IF NOT EXISTS usage_daily (
  day TEXT NOT NULL,
  source TEXT NOT NULL,
  flags TEXT NOT NULL,
  searches INTEGER NOT NULL,
  misses INTEGER NOT NULL,
  PRIMARY KEY (day, source, flags)
);
"#;

/// Migration v10 -> v11: split "found nothing" into a definitive miss and a
/// not-ready one, and record the index state a query arrived to. `misses` had
/// been counting both, which overstates real misses — rq distinguishes them in
/// its exit codes for the same reason. Counters are additive, so existing rows
/// keep their `searches`/`misses` and read zero for the new columns.
pub(crate) const MIGRATION_V11: &str = r#"
ALTER TABLE events ADD COLUMN status TEXT;
ALTER TABLE events ADD COLUMN coverage TEXT;
ALTER TABLE usage_daily ADD COLUMN warming INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_daily ADD COLUMN on_complete INTEGER NOT NULL DEFAULT 0;
"#;

/// Migration v11 -> v12: recall filters by repository, so index names within a
/// repository. The composite leads with `repository_id`, so it also serves
/// everything the single-column index it replaces did.
pub(crate) const MIGRATION_V12: &str = r#"
CREATE INDEX IF NOT EXISTS idx_symbols_repo_name ON symbols(repository_id, name_lower);
DROP INDEX IF EXISTS idx_symbols_repo;
"#;

/// Migration v12 -> v13: behavioral learning is removed. `selection_stats` was
/// its rollup, and `events` existed to feed it (search rows were only ever
/// pruned); `usage_daily` keeps the counts. `events_hwm` was the rollup's
/// position in the log.
pub(crate) const MIGRATION_V13: &str = r#"
DROP TABLE IF EXISTS selection_stats;
DROP TABLE IF EXISTS events;
DELETE FROM meta WHERE key = 'events_hwm';
"#;

/// Migration v13 -> v14: the Go, Python and TypeScript/JavaScript plugins now
/// extract constants, which files indexed before them lack. Forget those files'
/// stat and hash, since both the mtime skip and the write path's hash skip would
/// keep the old rows. The hash becomes `''`, not NULL: the write path reads it
/// as a `String`, and no real hash is empty. Then demote their repos' coverage
/// from `complete`, which is only ever staleness-checked against git, never
/// swept. Old symbols stay until each file is rewritten, so searches answer as
/// before in the meantime.
pub(crate) const MIGRATION_V14: &str = r#"
UPDATE coverage SET status = 'warming'
  WHERE scope = 'full' AND status = 'complete'
    AND repository_id IN (
      SELECT repository_id FROM files
      WHERE language IN ('go', 'python', 'typescript', 'javascript'));
UPDATE files SET mtime = NULL, content_hash = ''
  WHERE language IN ('go', 'python', 'typescript', 'javascript');
"#;

/// Migration v14 -> v15: the trigram table drops token positions
/// (`detail=none`). Recall only ever asks whether a name holds a trigram, and
/// positions were most of the table. FTS5 can't change `detail` in place, so
/// the table is recreated and rebuilt from `symbols`; no file is re-parsed.
pub(crate) const MIGRATION_V15: &str = r#"
DROP TABLE IF EXISTS symbols_fts;
CREATE VIRTUAL TABLE symbols_fts USING fts5(
  name,
  content='symbols',
  content_rowid='id',
  tokenize='trigram',
  detail=none
);
INSERT INTO symbols_fts(symbols_fts) VALUES ('rebuild');
"#;

/// Migration v15 -> v16: count the searches answered from a live scan of an
/// untracked directory rather than from the index. Existing rows read zero.
const MIGRATION_V16: Step = Step::AddColumn {
    table: "usage_daily",
    column: "live",
    decl: "INTEGER NOT NULL DEFAULT 0",
};

/// Migration v16 -> v17: the name index's tables, empty. Recall builds a
/// repo's index the first time it needs it.
pub(crate) const MIGRATION_V17: &str = r#"
CREATE TABLE IF NOT EXISTS name_sigs (
  repository_id INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  chunk INTEGER NOT NULL,
  n INTEGER NOT NULL,
  sigs BLOB NOT NULL,
  keys BLOB NOT NULL,
  PRIMARY KEY (repository_id, kind, chunk)
);
CREATE TABLE IF NOT EXISTS name_index (
  repository_id INTEGER PRIMARY KEY,
  format INTEGER NOT NULL,
  built INTEGER NOT NULL
);
"#;

/// Migration v17 -> v18: fuzzy recall reads only the name index, so the
/// trigram table and its sync triggers go, with the `name_lower` index only
/// its first-letter net read. Triggers first: one left behind would fail every
/// symbol write. Nothing is rebuilt; the freed pages are reused.
pub(crate) const MIGRATION_V18: &str = r#"
DROP TRIGGER IF EXISTS symbols_ai;
DROP TRIGGER IF EXISTS symbols_ad;
DROP TRIGGER IF EXISTS symbols_au;
DROP TABLE IF EXISTS symbols_fts;
DROP INDEX IF EXISTS idx_symbols_name_lower;
"#;

/// Migration v18 -> v19: files record whether they declare themselves
/// generated, which only reading them can tell. Every file is queued for
/// re-extraction the way v14 queued three languages: stat and hash forgotten,
/// complete repos demoted to warming. Old rows answer as before meanwhile.
const MIGRATION_V19: Step = Step::AddColumn {
    table: "files",
    column: "generated",
    decl: "INTEGER NOT NULL DEFAULT 0",
};
pub(crate) const MIGRATION_V19_REQUEUE: &str = r#"
UPDATE coverage SET status = 'warming' WHERE scope = 'full' AND status = 'complete';
UPDATE files SET mtime = NULL, content_hash = '';
"#;

/// One rung of the migration ladder.
pub(crate) enum Step {
    Sql(&'static str),
    /// `ALTER TABLE … ADD COLUMN`, skipped when the column is already there.
    /// SQL can't say `IF NOT EXISTS` here, and a step can run twice: rq before
    /// 0.54.1 reset `user_version` to its own when it opened a newer database.
    AddColumn {
        table: &'static str,
        column: &'static str,
        decl: &'static str,
    },
}

/// The cumulative migration ladder for existing databases: apply every step
/// whose version exceeds the database's `user_version`.
pub(crate) const MIGRATIONS: [(i64, Step); 19] = [
    (2, Step::Sql(MIGRATION_V2)),
    (3, Step::Sql(MIGRATION_V3)),
    (4, Step::Sql(MIGRATION_V4)),
    (5, Step::Sql(MIGRATION_V5)),
    (6, Step::Sql(MIGRATION_V6)),
    (7, Step::Sql(MIGRATION_V7)),
    (8, Step::Sql(MIGRATION_V8)),
    (9, Step::Sql(MIGRATION_V9)),
    (10, Step::Sql(MIGRATION_V10)),
    (11, Step::Sql(MIGRATION_V11)),
    (12, Step::Sql(MIGRATION_V12)),
    (13, Step::Sql(MIGRATION_V13)),
    (14, Step::Sql(MIGRATION_V14)),
    (15, Step::Sql(MIGRATION_V15)),
    (16, MIGRATION_V16),
    (17, Step::Sql(MIGRATION_V17)),
    (18, Step::Sql(MIGRATION_V18)),
    (19, MIGRATION_V19),
    (19, Step::Sql(MIGRATION_V19_REQUEUE)),
];
