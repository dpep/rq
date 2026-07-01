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
  lang/       # Tree-sitter plugins: ruby, rust, go, python
    ruby/     # the first plugin
    rust/     # what rq dogfoods on its own source
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
plugin. The one shared thing a language may extend is the `core::Kind`
vocabulary — Rust added `struct`/`enum`/`trait` — which generalizes the model
rather than leaking a language into `index`/`search`/scoring.

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
  kind TEXT NOT NULL,                -- class|module|method|function|struct|enum|trait
  language TEXT NOT NULL,
  line INTEGER NOT NULL,
  end_line INTEGER,                  -- 1-based last line of the definition body
                                     -- (NULL for rows indexed before v4)
  parent TEXT                        -- enclosing symbol's qualified NAME
                                     -- (lexical nesting only), e.g. Foo::Bar
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
  type TEXT NOT NULL,                -- search | open | select
  query TEXT,                       -- normalized query, when applicable
  repository_id INTEGER,
  path TEXT, line INTEGER,          -- the file/line for open/select
  branch TEXT, ts INTEGER NOT NULL
);

-- rollup the hot path reads; never scan raw events at query time.
-- Keyed by (file, name), NOT symbol_id: symbol ids are recreated whenever a
-- file is re-extracted, so keying on the stable file+name keeps learning across
-- reindexing.
selection_stats (
  repository_id INTEGER NOT NULL,
  query_norm TEXT NOT NULL,
  file TEXT NOT NULL,
  name TEXT NOT NULL,
  selections INTEGER NOT NULL,
  last_selected_at INTEGER,
  PRIMARY KEY (repository_id, query_norm, file, name)
);

-- small key/value store (e.g. the event-rollup high-water mark)
meta ( key TEXT PRIMARY KEY, value TEXT NOT NULL );
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

- **One core, two entry points** — explicit (`index_under`, unbounded) and
  opportunistic (`index_budgeted`, time-bounded) both call `run_index`, which
  differs only by parameters (active files, subtrees, deadline): collect
  candidates serially → parse the changed/new ones → write a batch.
- **Incremental** — a cheap `mtime` match short-circuits before any read; the
  content `hash` then guards the write. The walker respects `.gitignore`.
- **Parallel parse, batched write** — parsing (the expensive Tree-sitter step)
  fans out across CPUs; the parsed files are written in **one** transaction (one
  `fsync` per batch, not per file). Writes stay serialized; parsing doesn't.
- **Opportunistic + time-bounded** (`index_budgeted`) — the first query warms the
  index without blocking on a full walk: a small inline budget indexes the active
  (branch) files first and answers, then the deferred pass warms more per query
  until a full sweep marks coverage `complete` (reconciling deletions + capturing
  commit times). Explicit `rq --index` is the same path, unbounded.
- **Block-until-answered (cold start)** — the time-boxed warm exists so a query
  never hangs, but on a *huge, cold* repo it can expire before the symbol is
  indexed, turning a real hit into a false "no matches". Correctness beats the
  first query's latency (and once warm the repo answers fast), so a query against
  a genuinely warming repo keeps indexing until the answer appears or the sweep
  completes — for humans **and** programs alike. Small/medium repos finish inside
  the normal budget and are unaffected; only a large cold repo waits, once.
  - **Humans** (a TTY, plain text) also get a one-line "indexing…" progress
    heads-up on stderr after ~500 ms and a graceful **Ctrl-C** (a `SIGINT` handler
    over `libc`, installed only on this path) that aborts and prints the best
    partial results. Interactive waits are unbounded — Ctrl-C is the escape.
  - **Programs** (`--json`/`--ndjson` or any pipe) block silently, bounded by a
    wait budget (`RQ_WAIT_BUDGET_MS`, default 1 min; `0` = non-blocking) since
    there's no one to interrupt.
  - A miss distinguishes **definitive** (index `complete` → exit 1) from
    **indeterminate** (still `warming`, e.g. the wait budget was hit on a huge
    repo → exit 2 + a one-line stderr note), so a caller isn't misled into
    treating "not yet" as "absent". Both are non-zero, so `rq … && …` is
    unchanged. Committed batches persist, so a re-run resumes.
  - `index_budgeted_cancellable` carries the abort flag (Ctrl-C, a wait timeout,
    or an early answer) down into the walk so the pass stops promptly without
    losing committed work.
- **Discovery vs tracking** — a *git work tree* is auto-discovered (a stray query
  may warm it); a *non-git* dir is only indexed when asked (`rq --index`), after
  which it's **tracked** (has coverage) and treated like any repo. Git-ness gates
  auto-discovery and branch-awareness; tracking gates the current-repo boost and
  self-healing warm.
- **Prioritized** — active (branch) files first, so the working set is indexed
  and kept fresh ahead of the rest of the repo.
- **Coverage-aware** — every walk updates `coverage` (`warming` until a full
  sweep completes, then `complete`; a deliberate `--index --path` subset is
  `partial` and never auto-warmed over).
- **Git off the hot path** — `is_git_repo` is native (walk up for `.git`),
  identity is cached by checkout root, and the `git log` for commit-time recency
  runs only when a sweep actually (re)indexed something — so a search of a clean,
  indexed repo forks no `git` at all.
- **Language-isolated** — the indexer is blind to language; plugins emit the
  common symbol model.

Tree-sitter parsing is the expensive step and is kept **off the search critical
path**: the inline warm is time-boxed, and the bulk of extraction persists for
the *next* query rather than blocking the current one.

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
- **qualifier** — a scoped query (`Foo::Bar`, `Foo::Bar#baz`) matches its leaf
  against the name and rewards a candidate whose `parent` ends with the named
  scope chain (`Bar` inside `Foo`). The qualifier reorders, it doesn't filter —
  an unscoped match still surfaces, just lower
- **path** — query also matches the file's name (Layer 3)
- **current-repo scope + boost** — results are restricted to the repo you're in
  by default (a search there answers about *that* repo, never leaking another
  indexed one; `--all-repos` opts into cross-repo), and within it the current
  repo's rows still carry the boost
- **learned boost** — behavioral signal from `selection_stats` (see below)
- **recency** — symbols in recently-active files (~14-day half-life), sourced
  from the more recent of file mtime and last git commit time (captured once per
  index, not on the search path)
- **branch** — on a feature branch, symbols in files that differ from the trunk
  (committed since divergence + uncommitted) get a strong boost; symbols in
  those files' directories a smaller one. This is the one signal computed *at
  search time* (a few `git diff --name-only` calls) because it tracks live
  working state; it's gated to feature branches, so the trunk pays nothing.
  The active-file set also drives proactive pre-indexing — `index_budgeted`
  warms those files first.

Match quality and the static features live in the pure `score()` function. The
dynamic, context-dependent signals (`learned`, `recency`) are computed by the
search layer — which owns the clock and store lookups — and passed in via a
`Boosts` struct, so a new git signal (recent commit, branch, ownership) is a new
field, not a new parameter. Prefer understandable scoring over sophisticated
algorithms; tuning a weight must never require re-indexing.

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

- `coverage.status` tells search its own confidence (`never | warming | partial
  | complete`). `warming` is opportunistic indexing in progress; `partial` is a
  deliberate `--index --path` subset that auto-warming won't clobber.
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

- Every interaction appends to `events`. `rq <query>` logs a `search`; the
  `rq record` hook logs an `open`/`select` with the query, file, and line — the
  decoupled ingestion point editors and shells call.
- A rollup aggregates events into `selection_stats`. It resolves the chosen
  symbol from `(repo, path, line)` at rollup time and keys on `(query_norm,
  file, name)`, so ranking does one indexed lookup and never scans the raw log.
- The **learned boost** is one additive feature whose weight **ramps with
  evidence** (saturates ~5 selections) and **decays** with recency (~30-day
  half-life, floored). Few selections → low weight → the static prior dominates,
  which solves cold start (new user / new repo / never indexed).
- **Prefix learning:** a pick for a shorter query (`han`) informs longer ones
  (`handler`) — `selections_for` matches any stored query that is a prefix of
  the current one, so typing more keeps the benefit.
- **Repeat-as-miss (exploration):** if the most recent event for a repo is a
  `search` for the same query (nothing opened since), the query was repeated —
  a signal the last results missed. That query's learned boost is decayed
  before ranking, so a stale favorite stops dominating and alternatives
  resurface. Opening a result reinforces it again.

### No daemon — amortized post-interaction work

Aggregation (and other proactive work like warming the index) is **not** a
resident daemon. Each `rq` invocation prints results first, then does a small,
bounded chunk of deferred work before exiting — rolling a batch of events into
`selection_stats`, warming the index opportunistically. Cost amortizes across
interactions, with no process to manage. A high-water mark in `meta` tracks
which events have been rolled up so each pass only touches new ones, and the
same pass prunes already-rolled-up events (keeping a small recent window for
repeat detection) so the raw log stays bounded. The same pass also warms the
index a little (`index_budgeted`) — bounded, so the process still exits
promptly. Making that warm a *detached* child (so it can run longer without
delaying the foreground) is a future addition; see the roadmap.

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
2. **Cross-repo ranking** — resolved for the common case by scoping to the
   current repo by default (`--all-repos` opts out); cross-repo ranking priors
   (recency) still matter under `--all-repos`.
3. **Learning overfit** — decay + exploration are the guardrails; needs tuning.
4. **Ranking explainability** — `--explain` from day one is the mitigation.
5. **Scope creep** — Layers 4–5 are a streamed tail, not a second search engine;
   keep them lean for the MVP.
