# rq roadmap

Phased plan. Each phase is independently useful and ends in something you can
actually run. Earlier phases must not assume later ones exist.

## Phase 0 — Design (current)

- [x] Product vision and priorities ([README](../README.md))
- [x] Architecture: symbol model, repo identity, schema, indexing, search,
      partial indexing, behavioral learning ([ARCHITECTURE](ARCHITECTURE.md))
- [x] Implementation language decided: Rust
- [ ] Crate scaffold (`cargo init`, module skeleton, CI)

## Phase 1 — MVP: index + search Ruby definitions

The smallest thing that delivers the core promise. Layers 1–3 done well.

- [x] `store/` — SQLite schema + migrations, WAL mode, trigram FTS
- [x] `core/` — common `Symbol` model, repository identity normalization
      (git remote → `github.com/org/repo`, `local:/path` fallback)
- [x] `lang/ruby/` — Tree-sitter Ruby plugin: classes, modules, methods
- [x] `index/` — incremental walker (respects `.gitignore`), coverage tracking
- [x] `search/` — Layers 1–3 (exact/prefix, fuzzy, path / filename) + scorer
- [x] abbreviation-aware fuzzy matcher (`refundproc → RefundProcessor`)
- [x] current-repo boost in ranking
- [x] `rq <query>` default command, `rq index`, `rq status`
- [x] `--explain` score breakdown
- [x] benchmark harness; verify < 50 ms on an indexed mid-size repo
      (`make bench`: iriq, 412 symbols — p50 ~160 µs, max < 0.25 ms)

Exit criteria met: `rq corpus` returns the Corpus class first, sub-millisecond,
on an indexed repo.

## Phase 2 — Partial indexing + streaming

Make `rq` useful before indexing finishes or when it never ran.

- [x] Layer 4 live scan (`search::live_search`) — search answers at 0% coverage;
      the CLI uses it for non-git directories it won't persist
- [x] Layer 5 opportunistic indexing — the first query in a git repo warms the
      index (gated to git work trees so a stray query never walks a random dir)
- [x] staleness detection via `content_hash` + lazy top-N validation — the files
      behind the top hits are revalidated; changed files re-extracted, deleted
      files forgotten, results re-ranked
- [x] indexing decoupled from search — `rq index` is explicit, and search never
      requires a prior full index (Layers 4/5 cover the cold path)

Deferred to Phase 5 (editor / daemon), where they actually pay off — a one-shot
CLI completes faster than these would help:

- [ ] streamed result tail (results arrive incrementally) — only matters for a
      long-lived consumer; the CLI search is sub-millisecond
- [ ] background indexing daemon — a resident process watching for changes

Exit criteria met: search works at 0%, partial, and 100% coverage; the user
doesn't have to know which layer answered.

## Phase 3 — Behavioral learning

The differentiator.

- [ ] `events` capture from the CLI (search / open / select)
- [ ] async rollup → `selection_stats`
- [ ] learned boost as an additive feature with evidence-ramped weight
- [ ] time-decay + exploration
- [ ] measure: does learned ranking beat static on real usage?

## Phase 4 — Git awareness

Ranking hints, never hard filters.

- [ ] current-branch, recent-commit, recently-modified signals
- [ ] ownership / activity hints

## Phase 5 — Editor integration

- [ ] thin local event-ingestion endpoint
- [ ] result-opening protocol (`path:line`)
- [ ] reference adapters (VS Code, Neovim)

## Later — more languages

Each is a new `lang/` plugin implementing `LanguagePlugin`; no core change.

- [ ] Go
- [ ] TypeScript
- [ ] Python
- [ ] Java

## Explicit non-goals

Not in scope (revisit only with a strong reason):

- call graphs, type inference, reference tracking, inheritance analysis
- full LSP feature set
- being an exhaustive search engine — `rq` ranks aggressively and returns
  fewer, better results on purpose
