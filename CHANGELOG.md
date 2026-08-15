# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

## 0.40.0 — 2026-08-15

### Changed
- `--show` now records the definition it printed as a selection, so ranking
  learns from it. Printing a confident body is a pick in a way a ranked list
  isn't: the caller asked for one definition and consumed exactly that one, and
  rq observed it — no follow-up `rq --record` required.
- `--no-record` keeps its job but narrows to it: suppressing the signal from
  benchmark and CI loops, whose repeated queries would otherwise dominate the
  learned ranking. It is no longer the recommended default for agents — the
  reason it once was (a search itself mutated ranking state) was removed in
  0.39.0 and 0.39.1, and excluding the highest-volume users left the learned
  boost with no data at all.

Nothing to do on upgrade. If you script rq inside a benchmark or a loop that
repeats the same query, pass `--no-record` there.

## 0.39.1 — 2026-08-13

### Changed
- Searching no longer writes a `search` row. Its only reader was the
  repeat-query decay removed in 0.39.0 — the roll-up that feeds learned ranking
  selects `type IN ('select','open')` and always skipped them — so the producer
  outlived its consumer by a release. Recorded picks are unaffected; this only
  stops the store growing a row per query that nothing ever read.

### Added
- The Claude Code skill ships in the repo at `claude/rq-skill.md`, so a
  behaviour change and its documentation land in the same commit rather than
  drifting apart in a separate marketplace.

### Fixed
- The skill said rq indexes four languages. It indexes six — Ruby, Rust, Go,
  Python, TypeScript and JavaScript — and its own frontmatter already said so.
  The body is what Claude reads when deciding whether the tool applies, so the
  short list was talking it out of using rq on a `.ts` file.

## 0.39.0 — 2026-08-03

### Changed
- A repeated search no longer decays that query's learned boost. It read a
  repeat as "the last answer missed", but in practice repeats come from
  automation re-running a command, and the signal it was protecting had never
  been used — `selection_stats` was empty. It was also asymmetric with the
  learning it corrected (boosts generalise by prefix; the decay matched
  exactly) and unbounded in time. Searching no longer writes ranking state, so
  a query's answer depends only on the index and explicitly recorded picks.
- A learned pick now expires. The recency half-life was floored, so an old
  choice could be diminished but never forgotten; with the repeat-decay gone,
  time is the only forgetting left and it runs to zero.

## 0.38.0 — 2026-08-03

### Added
- Queries piped on stdin, one per line, are answered in a single run — the
  store, repo resolution, branch files, identity and the staleness check are
  paid once instead of per query. 10 queries on a 6042-file repo: 16ms/query as
  separate processes, 3ms/query batched. Each row carries the query it answers,
  a miss still reports, and a cold repo warms once up front rather than
  answering from a partial index. `--ndjson` (or text); `--json`, `--show` and
  `--open` don't apply to a stream.

### Fixed
- The same query now returns the same answer. Hits sharing a name, a length and
  a score tied every tiebreak, so a stable sort preserved whatever order the
  database returned — `rq Transaction` on a large repo picked a different file
  almost every run. Ties now break on location.

### Changed
- A query no longer waits on the staleness check. Asking whether the worktree
  moved forks `git status`, which scales with worktree size rather than with
  the query — 12.6ms of a 16.8ms query on a 6042-file repo. It runs alongside
  the search now and is collected once results are out: time-to-answer 16.8ms
  to 4.6ms, process exit 20ms to 16ms.
- An edited worktree is reindexed by the detached child rather than in the
  foreground. That sweep cost ~32ms on every query for as long as anything
  stayed uncommitted — 44ms/query to 13ms on a dirty 3000-file repo. A miss
  taken while that work is outstanding reports `warming` rather than a
  definitive `no_match`, since the symbol may be in an edit not yet indexed.

## 0.37.0 — 2026-08-03
- TypeScript / JavaScript plugin — `.ts/.tsx/.mts/.cts` and `.js/.jsx/.mjs/.cjs`.
  Indexes `class`, `interface`, `type`, `enum`, `namespace`, `function` (and
  `const f = () => …`, the modern spelling), plus the members a class, interface,
  or object type declares. They are two languages to `-x`: `-x ts` and `-x js`
  filter separately (aliases `tsx`, `jsx`).
- `-k` accepts TypeScript's vocabulary: `interface` means trait, `type` means
  struct. Both work as the leading kind keyword too (`rq interface Renderer`).
- A repo indexed before this release already reads `complete`, so warming skips
  it and its TypeScript/JavaScript files stay invisible. Run `rq --index` once to
  pick them up — or let the repo's next change do it incrementally.

## 0.36.0 — 2026-07-31
- `--profile` reports where a query's time went, phase by phase, to stderr or
  as JSON alongside `--json`. Free when off.
- Search latency: a query on a feature branch went ~66ms to ~13ms (p50, end to
  end against 0.35.2 on the same repo). Nearly all of it was setup — resolving
  the repo, deciding whether to warm — rather than searching, which `--profile`
  made visible for the first time. Fewer git forks, and the branch-file list is
  served from the store with its refresh started alongside the search.
- Ranking: a typed capital now counts, so `Symbol` and `symbol` no longer fall
  through to mtime-based recency.

## 0.35.2 — 2026-07-30
- Ruby: `field :name, …` declarations index as the methods they define,
  covering the schema DSLs (graphql-ruby, Mongoid) where the declaration *is*
  the definition. Tree-sitter sees a call rather than a `def`, so these were
  previously invisible to navigation.

## 0.35.1 — 2026-07-09
- A bare `--wait` number is seconds, not milliseconds.

## 0.34.0 — 2026-07-06
- Ruby metaprogramming recall, and visibility as a ranking signal.

## 0.33.0 — 2026-07-06
- Subtree seeds, detached background warm, live-scan merge.

## 0.32.0 — 2026-07-06
- Performance and correctness batch: repo-scoped indexes, incremental
  commit-time capture, parser reuse, namespace mtimes, FTS self-heal, and
  aligned JSON shapes.

## 0.31.3 — 2026-07-01
- Prune stale checkouts at index time rather than on every search.

## 0.31.2 — 2026-07-01
- Prune stale checkout rows, so a moved repo self-heals.

## 0.31.1 — 2026-07-01
- Fix signatures and `--show` for a moved repo (stale checkout root).

## 0.31.0 — 2026-07-01
- `--show`, normalized confidence, and restructured JSON scoring.

## 0.30.0 — 2026-07-01
- Fix a cross-repo leak: search is scoped to the current repo by default.

## 0.29.0 — 2026-07-01
- `end_line` spans and JSON status objects, for agent consumers.

## 0.28.0 — 2026-07-01
- Leading kind keyword: `rq class Foo`, `rq method zoom`.

## 0.27.0 — 2026-07-01
- Qualified-name lookup: `Foo::Bar` and `Foo::Bar#baz`.

## 0.26.2 — 2026-06-28
- Bias fuzzy highlights toward contiguous runs.
