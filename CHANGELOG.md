# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

## Unreleased

### Changed
- **A typo now finds the tight match, not a longer name containing it.**
  Searching Rails for `Validaton` returned `ValidationError`, and `Assocations`
  returned `AssociationScope` — the fuzzy score counts matched *query*
  characters, so a candidate's extra characters were free. Fuzzy matches now
  take the same unmatched-tail penalty prefix matches already took. Gentle and
  capped, so an abbreviation still reaches a long name it barely covers
  (`apc` → `ApplicationController`).
- **Shallower definitions win a tie.** A new `depth` signal penalizes each
  level of enclosing scope, slightly. Searching Rails for `save` used to return
  `ActiveRecord::Middleware::DatabaseSelector::Resolver::Session#save` first
  and `ActiveRecord::Persistence#save` second, for no better reason than
  "middleware" sorting before "persistence" — every candidate scored
  identically, so alphabetical file order decided it.

  It is deliberately small, below every other signal: on a large repo whole
  result sets score the same (20 of the top 20 for `perform`), and this exists
  to order those, not to outweigh how well a name matched.

## 0.43.0 — 2026-08-21

### Changed
- **Definitions under test and spec paths rank below source.** Searching Rails
  for `save` returned eight fake models from `test/` fixtures and never reached
  `ActiveRecord::Persistence#save` — all of them scored identically, so the tie
  fell through to alphabetical path order. `--explain` shows the new
  `test_path` feature like any other.

  It's a penalty, not a filter: when a name only lives in tests, every
  candidate takes it equally and the order among them is unchanged, so you can
  still navigate to a test.

  Matching is by whole directory segment (`test/`, `tests/`, `spec/`, `specs/`,
  `__tests__/`, `__mocks__/`, `testdata/`, `fixtures/`) plus filename suffixes
  (`*_test.*`, `*_spec.*`, `*.test.*`, `*.spec.*`) and `conftest.py`. A `test_*`
  prefix rule was tried and dropped — it wrongly demoted genuine public API
  like `ActiveSupport::TestCase` and `ActionView::TestCase`.

## 0.42.1 — 2026-08-20

### Fixed
- **`--usage` counted a not-yet-ready index as a miss.** rq distinguishes "the
  symbol isn't there" (exit 1) from "the index hasn't reached it yet" (exit 2),
  but the counts netted both into `misses` — on the first real day of data that
  overstated genuine misses by 2x. They're separate columns now, and the two
  call for opposite responses: index more, versus the symbol isn't there.
- **`day` was UTC, so evening searches were filed under tomorrow.** For anyone
  west of Greenwich a per-day report was quietly shifted for part of the day.
  It's the local date now. Rows written before this keep the UTC day they were
  recorded under — history isn't rewritten, so a day either side of the upgrade
  may be split across two rows.

### Added
- **`--usage` records the index state each query arrived to** — `on_complete`
  counts the searches that ran against a fully indexed repo, so a miss rate can
  be read against how ready rq actually was.

## 0.42.0 — 2026-08-20

### Added
- **`rq --usage` — how rq is actually being called.** Searches per day, broken
  down by caller and by which flags the call used, with `--json`/`--ndjson`
  like every other command. Answers the questions the index couldn't: how much
  traffic is there, how much of it is agents, and how often does a search find
  nothing.
- **Searches are counted, and the caller is recorded with them.** rq labels the
  invocation from its environment — `claude-code` (and `claude-code:mcp` when
  the entrypoint isn't a shell), `cursor`, `ci`, `human` for a terminal, or
  `piped` when nothing identifies it. Skill identity isn't exposed by any
  environment, so it isn't recorded.

  Counts live in a new `usage_daily` table and are incremented on write, so
  they survive the rolling prune that bounds the raw event log — previously the
  log capped at 200 rows, which made it a ceiling rather than a count.

  **This is observability, not learning.** The rollup that feeds ranking reads
  only `open`/`select` rows, so counting a search can never move a result.

### Changed
- **`--no-record` now also keeps a call out of `--usage`.** It already meant
  "this isn't real usage"; benchmark and CI loops shouldn't skew the counts any
  more than they should skew ranking.

### Internal
- Schema v10 adds `events.source`, `events.results`, `events.flags`, and the
  `usage_daily` table. Existing databases migrate in place; rows written before
  the upgrade read `NULL` for the new columns.
- The two `profile` unit tests could interleave — they drive process-global
  state and cargo runs tests as threads in one process, so the "off" test could
  observe the "on" test's flag and fail. They're serialized now.

## 0.41.0 — 2026-08-19

### Added
- **`-a` is short for `--all-repos`**, for the cross-repo search you reach for
  interactively.
- **`--limit 0` means no limit** — every ranked match instead of the top 10.
  Previously `0` asked for nothing and got nothing.

### Changed
- **The library API is `cli::run()` — 146 public items down to one module.**
  `rq` publishes as `reference-query`, and `lib.rs` re-exported all eight
  modules, so that surface was public by default rather than by decision.
  `core`, `index`, `lang`, `profile`, `search`, `store` and `trace` are all
  crate-private now; rustdoc exports `cli` and nothing else.

  What kept them public was the test suite, not any caller: an integration test
  in `tests/` is a separate crate, so every internal it touches has to be
  `pub`. Those tests now live inside the lib, where `#[cfg(test)]` code can
  reach crate-private items, and the ones that belong at the CLI boundary drive
  the built binary instead. Same 185 tests either way.

Nothing to do on upgrade: the `rq` binary and its CLI surface are unchanged.
This only affects code that depends on `reference-query` as a library.

### Internal
- `make bench` runs the search-latency benchmark as an `#[ignore]`d test rather
  than `cargo run --example`. `REPO=` still selects the repository to index. It
  had to move: an example is a separate crate, so timing `index`, `search` and
  `store` from one meant publishing all three, and measuring in-process is the
  point of the benchmark.

### Fixed
- `script/check.sh` could report "all green" while `cargo clippy` was failing,
  so a red tree looked shippable. A shell function on the left of a pipe runs
  with `set -e` suppressed: a failing step didn't stop the run, and the
  pipeline reported the *last* step's status rather than the first failure's.
  The file header warns about exactly this for `| tail`; the fix for that
  reintroduced it one line lower.

### Internal
- `unreachable_pub` is on. A `pub` item inside a private module is reachable
  from nowhere, and `pub` is precisely what makes `dead_code` skip an item — so
  the two together were hiding unused code. Nothing turned out to be dead in
  rq once they were visible, which is the point: the lint can now answer.

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
