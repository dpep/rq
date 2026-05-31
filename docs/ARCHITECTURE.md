# rq architecture (target model)

This is the **design we are building toward**. No code exists yet; this
document is the contract the implementation should satisfy.
[ROADMAP.md](ROADMAP.md) tracks what ships in which phase.

## Core principle

`rq` is a **navigation engine**. It optimizes for reaching the one result a
developer most likely wants, fast — not for enumerating every match. Four
ranked priorities resolve every design tension:

1. relevance over completeness
2. navigation over discovery
3. speed over exhaustiveness
4. learned behavior over static ranking

The latency target is **< 50 ms perceived** for index-backed results, then
*progressive improvement* — slower layers stream in behind the fast first
answer. This forces one early commitment: **results are a stream, not a
synchronous list.** Everything below assumes that.

## Implementation language

Rust. The latency target effectively requires a compiled language with
near-zero startup cost; Rust also has first-class Tree-sitter bindings and
ships as a single static binary (the `rg`/`fd`/`fzf` feel we are matching). A
scripting-language runtime's startup alone would consume the whole 50 ms
budget.

## The common symbol model

Every language plugin emits the same shape. The core never sees a
language-specific concept.

```text
Symbol {
  repository   # which repo it belongs to
  language     # ruby, go, ts, ...
  name         # RefundProcessor, perform, User
  kind         # class | module | method | function
  file         # repo-relative path
  line         # 1-based
  parent       # enclosing symbol (cheap nesting, NOT a call graph)
}
```

`parent` records lexical nesting only (`Foo::Bar#baz`). It is **not** reference
tracking or inheritance — those are explicit non-goals for the MVP.

## Repository identity — two levels

Identity answers two different questions, so it is modeled at two levels:

- **Logical project** — `github.com/org/repo` (from the upstream remote) or
  `local:/abs/path` fallback, or an explicit name. Used to dedupe symbols and
  aggregate behavioral learning across checkouts. Robust to forks/clones being
  the "same" project.
- **Local checkout** — a root path plus current branch. Used for indexing
  coverage state and git-aware ranking. One project may have several checkouts.

The system is designed for **many** repositories and millions of symbols from
day one. It never assumes a single repository.

## Module layout

Language-agnostic core; language specifics quarantined under `lang/`.

```text
src/
  cli/        # `rq <query>` default command, arg parsing, output
  core/       # symbol model, repository identity, scoring — NO language specifics
  store/      # SQLite schema, migrations, queries (WAL mode)
  index/      # walker, incremental indexer, coverage tracking
  search/     # staged pipeline, scorer, --explain
  lang/       # Tree-sitter plugins
    ruby/     # the first plugin
  events/     # interaction capture + async rollup
  adapters/   # editor event ingestion (thin, decoupled)
```

A `LanguagePlugin` trait is the only seam languages plug into:

```rust
trait LanguagePlugin {
    fn extensions(&self) -> &[&str];
    fn extract(&self, source: &str) -> Vec<Symbol>;
}
```

A registry maps file extension → plugin. Adding Go/TS/Python/Java is a new
plugin, not a core change.

## SQLite schema

WAL mode is mandatory — the background indexer writes while searches read.

```sql
PRAGMA journal_mode = WAL;

-- a logical project
repositories (
  id INTEGER PRIMARY KEY,
  identity TEXT UNIQUE NOT NULL,     -- github.com/org/repo | local:/abs/path
  display_name TEXT,
  default_branch TEXT,
  created_at INTEGER, updated_at INTEGER
);

-- a local clone of a repository
checkouts (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  root_path TEXT NOT NULL UNIQUE,
  current_branch TEXT
);

files (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  path TEXT NOT NULL,                -- repo-relative
  language TEXT,
  mtime INTEGER,
  content_hash TEXT,                 -- staleness detection
  indexed_at INTEGER,
  UNIQUE(repository_id, path)
);

symbols (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  file_id INTEGER NOT NULL REFERENCES files(id),
  name TEXT NOT NULL,
  name_lower TEXT NOT NULL,          -- prefix / ranking
  kind TEXT NOT NULL,                -- class|module|method|function
  language TEXT NOT NULL,
  line INTEGER NOT NULL,
  parent_id INTEGER                  -- enclosing symbol (lexical nesting only)
);
CREATE INDEX idx_symbols_name_lower ON symbols(name_lower);

-- fuzzy candidate narrowing: trigram FTS over symbol names
CREATE VIRTUAL TABLE symbols_fts USING fts5(
  name, content='symbols', content_rowid='id', tokenize='trigram'
);

-- partial-indexing state, per repo (or directory scope)
coverage (
  id INTEGER PRIMARY KEY,
  repository_id INTEGER NOT NULL REFERENCES repositories(id),
  scope TEXT NOT NULL DEFAULT 'full',   -- 'full' or a directory prefix
  files_seen INTEGER, files_indexed INTEGER,
  status TEXT NOT NULL,                  -- never | partial | complete | stale
  last_indexed_at INTEGER,
  UNIQUE(repository_id, scope)
);

-- raw, append-only interaction log
events (
  id INTEGER PRIMARY KEY,
  type TEXT NOT NULL,                -- search | open | select | focus
  query TEXT, repository_id INTEGER, file_id INTEGER, symbol_id INTEGER,
  branch TEXT, ts INTEGER NOT NULL
);

-- rollup the hot path reads; never scan raw events at query time
selection_stats (
  repository_id INTEGER NOT NULL,
  query_norm TEXT NOT NULL,
  symbol_id INTEGER NOT NULL,
  selections INTEGER NOT NULL,
  last_selected_at INTEGER,
  PRIMARY KEY (repository_id, query_norm, symbol_id)
);
```

Decisions worth calling out:

- **Trigram FTS5** narrows millions of symbols to a small candidate set before
  any expensive scoring runs — the answer to "fuzzy + millions + 50 ms".
- **`content_hash`** detects staleness so partial/old indexes don't silently
  point at moved lines.
- **`coverage`** lets search know its own confidence and decide whether to
  append a live-scan tail.
- **`events` + `selection_stats`** separate the append-only truth from the
  aggregate the ranking path reads, so the hot path never scans the log.

## Indexing model

Indexing is **decoupled** from search — a background worker parses and writes;
search only reads.

- **Incremental** — compare `mtime` / `content_hash`; skip unchanged files. The
  walker respects `.gitignore`.
- **Opportunistic** — when a live scan parses a file (search Layer 5), persist
  its symbols. The index warms through normal use.
- **Prioritized** — current checkout first, then git-recent directories first.
- **Coverage-aware** — every walk updates `coverage`.
- **Language-isolated** — the indexer is blind to language; plugins emit the
  common symbol model.

Tree-sitter parsing is the expensive step and is kept **off the search critical
path**. Layer 5 extraction persists for the *next* query rather than blocking
the current one.

## Search / ranking pipeline

Staged, streaming, early-exit on confidence:

| Layer | What | Notes |
| ----- | ---- | ----- |
| 0 | parse query | case, separators, looks-like-a-path? |
| 1 | exact / prefix symbol | indexed `name_lower`; fastest, highest confidence |
| 2 | fuzzy symbol | trigram FTS candidate set → abbreviation-aware scorer |
| 3 | path / filename | |
| 4 | live scan | async, streamed when coverage is low |
| 5 | opportunistic extraction | parse newly-seen files, persist for next time |

**Confidence gate:** a strong exact match in the current repo returns
immediately and stops the pipeline. Otherwise return the top-N from layers 1–3
now and stream refinements from 4–5.

### Scoring — simple, additive, explainable

Ranking is an additive sum of named features so `--explain` can print exactly
why a result ranked where it did:

- **match quality** — exact > prefix > camel-hump abbreviation > subsequence
- **kind weight** — tunable (e.g. class/module slightly above method)
- **current-repo boost** — the repo you're in dominates other repos
- **recency** — recently modified / recently opened
- **learned boost** — from `selection_stats` (see below)
- **path proximity** — closeness to the working directory

Prefer understandable scoring over sophisticated algorithms. Tuning a weight
must never require re-indexing.

### Abbreviation matching

`refundproc → RefundProcessor`, `usr → User`, `perf → perform`:

1. Tokenize the candidate on camel-case / underscore boundaries
   (`RefundProcessor` and `refund_processor` both → `[refund, processor]`).
2. Greedily match the query against token prefixes and initials.
3. Score by contiguity and token-boundary alignment.

Intra-token fuzz (`paymnt → Payments`) falls back to subsequence matching with
a penalty. Quality of ranking matters more than the cleverness of the algorithm.

## Partial indexing

The index is **never assumed complete**.

- `coverage.status` tells search its own confidence (`never | partial |
  complete | stale`).
- When coverage is below threshold, search appends a **streamed live-scan tail**
  so missing symbols still surface — slower, but visible.
- **Opportunistic extraction** grows coverage through normal use.
- **Staleness:** a `content_hash` mismatch marks a file's symbols stale; search
  lazily validates only the **top-N** results (stat, re-parse if changed) before
  presenting — cheap because it touches a handful of files, not the index.

Degradation ladder:

```text
zero index      → pure live scan (works, slower)
partial index   → index results + streamed scan tail
complete + fresh → index only, sub-50 ms
```

The user never needs to know which layer a result came from.

## Behavioral learning

The long-term differentiator: ranking learns from what users actually choose.

- Every interaction appends to `events` — from the CLI and from editor adapters.
- An **async rollup** aggregates events into `selection_stats` keyed by
  `(repository_id, query_norm, symbol_id)`, so ranking does one indexed lookup
  and never scans the raw log.
- The **learned boost** is one additive feature whose weight **ramps with
  evidence** — few selections → low confidence → the static prior dominates.
  This is what solves cold start (new user / new repo / never indexed).
- **Time-decay** on selections so stale habits fade, plus a little
  **exploration** so results aren't frozen on past choices.

Git-awareness (current branch, recent commits, ownership, recently-modified
areas) enters later as additional **ranking hints — never hard filters**.

## Editor integration

Designed for early, decoupled editor integration. Editors POST a minimal event
to a thin local endpoint:

```text
{ type: "open" | "focus" | "select", file, repository, branch, ts }
```

No editor-specific coupling in the core. VS Code, Neovim, and JetBrains are all
just event sources and result openers. Result locations are `path:line` so any
editor can jump to them.

## Open risks (tracked, not yet resolved)

1. **Fuzzy-over-millions latency** — mitigated by trigram candidate narrowing;
   needs measurement against the 50 ms budget at scale.
2. **Cross-repo ranking** — repo-level priors (current-repo boost, recency)
   must keep distant repos from drowning the wanted result.
3. **Learning overfit** — decay + exploration are the guardrails; needs tuning.
4. **Ranking explainability** — `--explain` from day one is the mitigation.
5. **Scope creep** — Layers 4–5 are a streamed tail, not a second search engine;
   keep them lean for the MVP.
