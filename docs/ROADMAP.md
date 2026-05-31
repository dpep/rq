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

- [x] confidence gate — skip the fallback when the index has a strong answer
- [x] Layer 4 live scan when coverage is thin/absent (search works at 0%)
- [ ] make the live scan a *streamed* tail (results arrive incrementally)
      rather than the current synchronous fallback
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
