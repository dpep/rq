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

- [ ] `store/` — SQLite schema + migrations, WAL mode
- [ ] `core/` — common `Symbol` model, repository identity normalization
      (git remote → `github.com/org/repo`, `local:/path` fallback)
- [ ] `lang/ruby/` — Tree-sitter Ruby plugin: classes, modules, methods
- [ ] `index/` — incremental walker (respects `.gitignore`), coverage tracking
- [ ] `search/` — Layers 1–3 (exact/prefix, fuzzy, path) + additive scorer
- [ ] abbreviation-aware fuzzy matcher (`refundproc → RefundProcessor`)
- [ ] `rq <query>` default command, `rq index`, `rq status`
- [ ] `--explain` score breakdown
- [ ] benchmark harness; verify < 50 ms on an indexed mid-size repo

Exit criteria: `rq refund` returns the right Ruby definition first, sub-50 ms,
on an indexed repo.

## Phase 2 — Partial indexing + streaming

Make `rq` useful before indexing finishes or when it never ran.

- [ ] streaming result API (results arrive incrementally)
- [ ] confidence gate / early-exit
- [ ] Layer 4 live scan as a streamed tail when coverage is low
- [ ] Layer 5 opportunistic extraction (persist symbols seen during a scan)
- [ ] staleness detection via `content_hash`; lazy top-N validation
- [ ] background indexer decoupled from search

Exit criteria: search works at 0%, 5%, and 100% coverage with graceful
degradation; the user can't tell which layer answered.

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
