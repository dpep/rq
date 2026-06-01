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

No daemon — instead of a resident process, deferred work is amortized across
interactions: each `rq` invocation prints results, then does a small bounded
chunk of background work (event rollup, opportunistic index warming) before
exiting. See "No daemon — amortized post-interaction work" in ARCHITECTURE.

Still open (only matters for a long-lived consumer; the CLI is sub-millisecond):

- [ ] streamed result tail (results arrive incrementally)
- [ ] proactive indexing of files adjacent to a result, in the deferred pass

Exit criteria met: search works at 0%, partial, and 100% coverage; the user
doesn't have to know which layer answered.

## Phase 3 — Behavioral learning

The differentiator.

- [x] `events` capture — `rq <query>` logs a search; the `rq record` hook logs
      open/select with query + file + line
- [x] rollup → `selection_stats`, amortized in the post-interaction pass; keyed
      by `(query_norm, file, name)` so it survives reindexing
- [x] learned boost as an additive feature with evidence-ramped weight
- [x] time-decay (recency, ~30-day half-life)
- [x] exploration via repeat-as-miss: a repeated search (nothing opened since)
      decays that query's learned boost, so a stale favorite stops dominating
- [x] prefix/related-query learning — a pick for `han` informs `handler`
- [x] bound the raw `events` log — the deferred pass prunes events already
      rolled up, keeping only the most recent few (for repeat detection)
- [ ] measure: does learned ranking beat static on real usage?

CLI shape: operations are flags (`--index`, `--status`, `--record`), not
subcommands, so no word is reserved — every term stays searchable, matching the
rg/fd feel.

## Phase 4 — Git awareness

Ranking hints, never hard filters. Added as fields on `search::Boosts` so each
signal slots into the scorer without threading new parameters.

- [x] recency boost — symbols in recently-active files rank higher, sourced
      from the more recent of file mtime (recent edit) and last git commit time
      (recent commit). Commit times are captured once per index via a single
      `git log` (parsed by the pure `parse_git_log`), never on the search path.
- [x] branch awareness — on a feature branch, files that differ from the trunk
      (committed + uncommitted) get a `branch` boost, and their directory
      neighbors a smaller one; computed at search time via a few git calls,
      gated so the trunk pays nothing
- [ ] use the active-file set for proactive (pre-)indexing in the deferred pass
- [ ] ownership / activity hints

## Phase 5 — Editor integration

- [x] ingestion point — `rq --record` (plus `-C` to target a workspace); no
      daemon, just CLI calls
- [x] result-opening protocol — every result is a `path:line`
- [x] reference shell wrapper — `script/rq-open` (search → pick → open → record)
- [x] integration guide — docs/EDITORS.md (VS Code task + extension sketch, Neovim)
- [ ] a packaged VS Code extension (the doc has the sketch; not yet shipped)

## Later — more languages

Each is a new `lang/` plugin implementing `LanguagePlugin`; no core change.

- [ ] Go
- [ ] TypeScript
- [ ] Python
- [ ] Java

## Shipped CLI affordances

- `-j/--json`, `-J/--ndjson` — structured output for editors, scripts, agents;
  each result carries a `signature` (the definition's source line)
- `-p/--path DIR` — restrict results to a subtree (repeatable)
- `-l/--limit N` — cap the number of results
- `-e/--explain` — per-result score breakdown
- `--completions <shell>` — shell completion scripts
- `rq --index --path DIR` — partial index of a subtree (for big monorepos);
  a later search won't silently full-index over it

## Explicit non-goals

Not in scope (revisit only with a strong reason):

- call graphs, type inference, reference tracking, inheritance analysis
- full LSP feature set
- being an exhaustive search engine — `rq` ranks aggressively and returns
  fewer, better results on purpose
