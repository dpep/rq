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
- [x] time-bounded warming (`index::index_budgeted`) — the cold first query never
      blocks on a full walk of a large repo: a small inline budget indexes the
      branch's active files first and answers, then the deferred pass warms more
      per query until coverage is complete. A cheap mtime check skips unchanged
      files, so repeated sweeps converge and pick up added/changed/deleted files
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
- [x] detached background warming — after results print, a search re-execs a
      detached `rq --warm` child (null stdio, own process group, niced +
      throttled I/O) that sweeps until coverage completes on a seconds-scale
      budget (`RQ_WARM_BUDGET_MS`), single-flighted per repo via a pid-stamped
      lock in `meta`. The foreground only ever waits on the answer;
      `RQ_WARM_DETACH=0` keeps the warm in-process (tests, debugging)
- [x] fused walk→parse→write pipeline — `run_index` streams: one walk thread
      feeds parse workers, which feed a writer committing in batches *as results
      arrive*. Walk and parse overlap (indexing starts on the first file found),
      and a budget-cut pass persists everything it parsed rather than losing the
      lot. This replaced the collect-all-then-parse path, whose serial walk could
      eat the whole budget on a huge repo and parse zero. Query relevance is the
      content-scan's job (below), so the walk just streams in walk order — nothing
      is deferred, which is what guarantees progress when the walk can't finish
- [x] demand-first coverage — a warming repo content-scans for the query up front
      (and on an empty result), *persists* the matches (`index::scan_for_query` →
      `replace_files`), and searches; coverage grows toward what's actually
      searched, not just walk order
- [x] subtree index as a *seed* — `--index --path DIR` gets the named subtree in
      first and leaves coverage `warming`, so normal warming continues over the
      rest of the repo through use (it's an accelerator, not a permanent scope;
      the earlier `partial` fence status is retired). Untracked non-git dirs
      merge a bounded live scan instead of replacing index results
- [ ] best-first indexing scheduler — extend the fused pipeline with content/
      git-recency signals and a priority heap between walk and parse (so warming
      orders by relevance, not just walk order). Design:
      [PRIORITY_INDEXING.md](PRIORITY_INDEXING.md)
- [ ] cheaper fuzzy pre-filter — the substring pre-filter is blind to
      abbreviations (`usr`↛`user`). A loose, recall-preserving narrowing (even
      ~50%) would speed cold fuzzy scans without the full unfiltered fallback

Exit criteria met: search works at 0%, partial, and 100% coverage; the user
doesn't have to know which layer answered.

## Phase 3 — Behavioral learning

The differentiator.

- [x] `events` capture — `rq --open` records the pick; the `rq --record` hook
      logs open/select with query + file + line. A bare query logs nothing
- [x] rollup → `selection_stats`, amortized in the post-interaction pass; keyed
      by `(query_norm, file, name)` so it survives reindexing
- [x] learned boost as an additive feature with evidence-ramped weight
- [x] time-decay (recency, ~30-day half-life)
- [x] prefix/related-query learning — a pick for `han` informs `handler`
- [x] bound the raw `events` log — the deferred pass prunes events already
      rolled up, keeping only the most recent few
- [x] exploration via repeat-as-miss — built, then **removed**: it fired on
      machine re-runs rather than a human re-asking, so it decayed boosts that
      were fine. Time decay is the only forgetting left
- [ ] **feed the signal — the actual blocker.** The weighting is done; the
      inputs aren't. Only `--open`/`--record` report a pick, so agents (the
      heaviest users) and anyone typing a bare `rq foo` contribute nothing, and
      `selection_stats` stays empty. Refining the boost is wasted until a
      normal invocation produces evidence. Candidates: infer a pick when a
      single confident hit is returned; treat `--show`/`--open` alike; have the
      skill call `--record` after acting on a result
- [ ] measure: does learned ranking beat static on real usage? Blocked on the
      above — there's no usage data to measure with

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
- [x] use the active-file set for proactive (pre-)indexing — `index_budgeted`
      warms the branch's active files first, so the working set is indexed (and
      kept fresh) before the rest of the repo
- [ ] ownership / activity hints

## Phase 5 — Editor integration

- [x] ingestion point — `rq --record` (plus `-C` to target a workspace); no
      daemon, just CLI calls
- [x] result-opening protocol — every result is a `path:line`
- [x] native open-and-record — `rq -o/--open` jumps to the best match (prompting
      on a TTY with several), records the pick, and `exec`s the launcher
      (`RQ_OPEN` template → `code` → `$VISUAL`/`$EDITOR` → print). Bare `rq` stays
      a `path:line` printer; the model + record path are unchanged
- [x] reference shell wrapper — `script/rq-open` (search → pick → open → record),
      now for interactive fzf picking / custom flows; `rq -o` covers the default
- [x] integration guide — docs/EDITORS.md (VS Code task + extension sketch, Neovim)
- [ ] a packaged VS Code extension (the doc has the sketch; not yet shipped)

## Later — more languages

Each is a new `lang/` plugin implementing `LanguagePlugin`. The plugin stays
self-contained; the only shared change a language may need is extending the
`core::Kind` vocabulary (Rust added `struct`/`enum`/`trait`) — generalizing the
model, not leaking a language into `index`/`search`/scoring.

- [x] `--profile` — per-phase search timing to stderr (JSON alongside
      `--json`), free when off. `examples/bench.rs` measures whether a search
      is fast; this says which phase to look at
- [x] Ruby metaprogramming recall — `attr_accessor`/`attr_reader`/`attr_writer`,
      `define_method`, `alias_method`, `delegate`, `scope`, `has_many`/`has_one`/
      `belongs_to`, `field` emit the methods they define (literal names only,
      pointing at the macro's line) — the definitions Tree-sitter can't see as
      `def`s. `field` covers the schema DSLs (graphql-ruby, Mongoid), where the
      declaration *is* the definition
- [x] Rust — `lang/rust/` (`fn`/`struct`/`enum`/`trait`/`mod`, impl & trait
      methods). The dogfood language: rq indexes its own source (`make dogfood`)
- [x] Go — `lang/go/` (`func`/method, `struct`, `interface`→trait)
- [x] Python — `lang/python/` (`class`, `def` free/method, decorator-aware)
- [x] TypeScript / JavaScript — `lang/typescript/` (`class`, `interface`→trait,
      `type`→struct, `enum`, `namespace`→module, `function` and `const f = () =>`,
      class/interface members→method). Two tags off one grammar family, so
      `-x ts` and `-x js` each mean what they say; `.tsx`/`.jsx` parse as JSX
- [ ] Java

## Shipped CLI affordances

- `-j/--json`, `-J/--ndjson` — structured output for editors, scripts, agents;
  each result carries a `signature` (the definition's source line)
- path filters — trailing positionals (rg-style `rq query dir…`) or `-p/--path`
- `-k/--kind` — restrict to kind: class/module/method/function/struct/enum/trait
  (`interface`=trait, `type`=struct)
- `-x/--lang` — restrict to language: ruby/rust/go/python/typescript/javascript
  (prefix-matched; `r`=ruby+rust; aliases rb/rs/golang/ts/tsx/js/jsx)
- `-l/--limit N` — cap the number of results
- `--no-record` — search without recording a behavioral signal (for agents)
- `-o/--open` — open the best match in your editor and record the pick; prompts
  to choose on a TTY with several. Launcher: `RQ_OPEN` → `code` → `$VISUAL`/`$EDITOR`
- `-e/--explain` — per-result score breakdown
- match highlighting — text results color the matched chars (TTY-only; honors
  `NO_COLOR` and `GREP_COLORS`)
- `--completions <shell>` — shell completion scripts
- `rq --index --path DIR` — seed the index with a subtree first (for big
  monorepos: the part you care about answers immediately; warming fills in the
  rest through use)
- `rq --drop [PATH|IDENTITY]` — remove a repo's index (symbols, files, coverage,
  learned ranking); the inverse of `--index`. By path (or current repo), or by an
  identity string from `--status` to clear orphaned cruft
- `rq --symbols FILE` — outline one file's definitions in line order (kind,
  parent, signature); a structural read of a file you're already at, not a
  ranked search. Honors `-k`/`-x` and `--json`/`--ndjson`

## Exploratory — semantic / association layer

Speculative; not committed. The idea: surface *related* symbols, not just
lexically-matching ones — `refund` leading you toward `Chargeback` even with no
shared characters. The interesting bet is to mint associations **from the repo
itself** (distributional semantics — word2vec/GloVe/LSA from co-occurrence), not
a pretrained model, so it stays local, cheap, and in character with the rest.

- [ ] self-derived associations from symbol proximity. Signals rq can use, some
      already captured: same-file / same-scope / N-line-window co-occurrence;
      `parent` nesting; **git co-change** (files committed together — already
      sourced for recency); **behavioral co-selection** (the `events` /
      `selection_stats` already record which pick answered which query).
- [ ] two flavors, increasing in ambition:
  - sparse **association graph** — a `cooccurrence(a, b, count)` table
    accumulated during the parse walk. Query → top co-occurring symbols (query
    expansion / "related"). Cheap, incremental, and **explainable** ("related:
    co-occurs in 14 files, co-changed in 9 commits"), which a neural cosine is
    not. Compounds with the learned ranking already in place.
  - dense **self-derived vectors** — factorize the co-occurrence matrix
    (PMI + truncated SVD), store ~100-dim int8 per symbol (~100 B; cheaper than
    neural since the vocabulary is repo-scale), ANN-recall as a gated `semantic`
    feature in the additive scorer. The query embeds by averaging its
    in-vocabulary token vectors — pure arithmetic, no model in memory.
- why it fits: local, no model / network / **no resident daemon**, CPU-cheap,
  incremental, explainable, and degrades to today's behavior with no association
  table. It sidesteps the fatal flaw of pretrained embeddings for a fork-per-
  query CLI — loading a neural model per invocation (100 ms–1 s) blows the
  latency budget.
- limitation: associations are bound to *this repo's* vocabulary — great for
  "things this codebase uses together," but no generic cross-vocabulary synonyms
  (`authentication`↛`login` unless they co-occur here). Subword splitting widens
  it; lexical stays the fallback. Sparse stats cold-start on small/new repos.
- if pretrained **neural** embeddings are ever wanted (for cross-vocabulary
  semantics), the model + inference runtime + resident process belong in a
  sibling `rq-embed` binary in a Cargo workspace sharing `store`/`core` — never
  linked into the lean default binary. The self-derived approach above needs
  none of that.

## Explicit non-goals

Not in scope (revisit only with a strong reason):

- call graphs, type inference, reference tracking, inheritance analysis.
  Evaluated properly once (a `--refs` / reverse-lookup mode) and declined —
  the reasoning, so it isn't re-litigated:
  - **Accuracy is a ladder, and the useful rungs are unreachable.** Lexical
    name matching is free but can't tell `user.save` from `record.save`. Scope
    and import resolution (no types — roughly GitHub's stack-graphs) fixes
    qualified names only. Resolving an unqualified receiver needs a real type
    checker, per language.
  - **Ruby inverts the cost/benefit.** The dynamic languages where name
    matching is noisiest (`call`, `perform`, `save`; `send`, `define_method`,
    `method_missing`, ActiveRecord's generated methods) are exactly the ones
    where resolution is *unreachable*, not merely expensive. The static
    languages where inference works are the ones where a plain name match was
    already decent. Most effort, least payoff.
  - **The persisted version is the expensive one.** Reference rows run ~10x
    definitions (measured on this repo: 451 defs vs ~4,800 call-shaped tokens),
    which taxes the cold-warm write path the indexing design works hardest to
    protect, and doubles the cost of every future language plugin — the thing
    that actually differentiates rq.
  - **If it's ever revisited**, the cheap honest version is a scan for
    occurrences *attributed to their enclosing definition* (`symbols.end_line`
    + `idx_symbols_file` already make that a pure index lookup, no new
    extraction). That's the one thing `rg` structurally can't do. Ship that or
    nothing; don't build the analyzer.
- full LSP feature set — and note the protocol is not the hard part. Precision
  belongs to the analyzer behind it (rust-analyzer, gopls, tsserver, pyright),
  each a multi-person-year project that must run as a **resident, workspace-
  indexing daemon**. That's the same architectural block that exiles pretrained
  embeddings below: it can't live inside a fork-per-query CLI with a 50 ms
  budget. Anyone wanting precise references already has one in their editor.
  rq's edge is what those can't do — no project setup, no build graph, no
  warm-up, across repos you've never opened, on trees that don't compile, at 5%
  coverage, in a terminal.
  - The one daemon-free escape hatch, if precision is ever genuinely needed:
    consume a precomputed **SCIP** index (Sourcegraph's format; `scip-go`,
    `scip-typescript`, `rust-analyzer scip`, et al. emit precise defs *and*
    refs into a static file). rq would read it when present and fall back to
    ranking otherwise — real precision, zero analyzer code. The cost is that
    someone must run a CI-scale indexer per repo, which contradicts the
    zero-setup pitch. Filed as a path, not a plan.
  - **`scip-ruby` exists but doesn't rescue the Ruby case.** It is built on
    Sorbet, so its precision is Sorbet's precision: fine for constants and
    class/module references, bounded by annotations for method calls, and blind
    to metaprogrammed methods (ActiveRecord attributes, `has_many` accessors)
    unless RBIs are generated for them. It also needs the tree to be
    Sorbet-parseable with a sorbet config. So the SCIP escape hatch pays off in
    exactly the static languages that needed it least — the same inversion as
    above, one layer down.
- pretrained-model embedding search **in the core binary** — a model + inference
  runtime + resident daemon don't belong in the fork-per-query CLI (see the
  exploratory association layer above for the local, daemon-free alternative)
- being an exhaustive search engine — `rq` ranks aggressively and returns
  fewer, better results on purpose
