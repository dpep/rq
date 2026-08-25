# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

## Unreleased

### Changed
- **Indexing uses every available core by default.** The automatic parse-job
  count was capped at 8, on the theory that writes serialize behind the single
  SQLite writer so extra workers could not pay. Measured, they do: writes
  overlap parsing rather than queueing behind it, and the walk+parse+write phase
  keeps scaling to the core count. A machine with more than 8 cores was leaving
  them idle — one monorepo report put that at ~25% of indexing time. The default
  is now `available_parallelism()`, which additionally respects a container's
  CPU budget instead of the host's core count. `--jobs`/`RQ_JOBS` still override
  it, and nothing changes on an 8-core machine. See docs/DECISIONS.md D5.

## 0.50.1 — 2026-08-24

### Added
- **Constants are indexed.** `rq SOME_LIMIT` finds `SOME_LIMIT = …` in Ruby —
  including qualified (`Foo::BAR = …`), rooted (`::BAR = …`), multi-assignment,
  and `||=` forms — and `const`/`static` items in Rust. A new `constant` kind
  joins the model: `--kind constant` (or `const`) and the leading-keyword form
  (`rq constant FOO`) scope to it, and it appears as `"kind":"constant"` in
  JSON output. Constants land as files reindex on edit; `rq --drop` + a reindex
  picks them up everywhere at once. Other languages follow later.

### Fixed
- **Ruby: the `alias` keyword, bare `attr`, and rooted `class ::Foo`.** Three
  extraction gaps surfaced by reading Shopify's rubydex
  [ruby-behaviors](https://github.com/Shopify/rubydex/blob/main/docs/ruby-behaviors.md)
  catalog against the plugin: `alias baz bar` now indexes `baz` (only
  `alias_method` was covered before), the legacy `attr :a, :b` form indexes its
  readers, and a rooted definition (`class ::Bar` inside a module) is owned by
  the top level instead of the enclosing namespace. New symbols land as files
  reindex on edit; `rq --drop` + a reindex picks them up everywhere at once.

## 0.50.0 — 2026-08-22

### Changed
- **A slow branch-file refresh no longer competes with the search it decorates.**
  rq caches the list of files your branch is changing and, once the list ages
  out, recomputes it on a thread *alongside* the query — which hides the cost on
  a small repo and very much doesn't on a large one. Reported on a 90k-file
  monorepo: recall latency spiking from 40-115 ms to ~700 ms whenever the
  profile said `refreshing alongside`. The refresh forks two `git diff`s over
  the whole worktree, so it competes with recall for disk; the fixed 15-second
  window then re-paid that every fifteen seconds of active searching.

  The window is now derived from what the rebuild actually costs — about a
  hundred times its own cost, so the refresh never eats more than ~1% of the
  time between searches — floored at the previous 15 seconds and capped at 5
  minutes. Below ~150 ms the floor binds, so small and mid-size repos behave
  exactly as before.

  The ranking cost is small and bounded: the cache is invalidated by a git-state
  stamp that catches every commit, checkout, stage, merge and rebase regardless
  of the window, so a longer window only delays noticing an **unstaged** edit.
  And it delays a ranking *boost*, never recall — nothing drops out of results.

  `--profile` now prints the window in force next to the branch-files line, so a
  repo that has backed off says so instead of looking like rq ignoring stale
  state.

## 0.49.0 — 2026-08-21

### Fixed
- **A scope that matched nothing was silently ignored.** `Foo#bar` and
  `Foo::Bar` are documented as the surest way past an ambiguous name, but the
  scope only *reordered* candidates — it never excluded any. When the leaf name
  happened to be unique in the index, a completely made-up owner returned the
  real definition at **confidence 1.0**, identical to querying the correct
  owner, with only a missing `parent` entry under `--explain` to tell them
  apart. A caller scoping a query precisely to catch a wrong owner got rq's
  strongest signal of certainty on the one query whose constraint had been
  discarded.

  A named scope is now a constraint: a candidate outside it isn't an answer.
  That includes a candidate with no recorded parent, since `Foo::Bar` asserts
  `Bar` sits inside `Foo` and a top-level `Bar` does not.

  The two failures are also distinguishable, because "wrong owner" is much more
  useful than "no such name". A scope that matches nothing reports
  `scope_not_found` with a `found_in` field naming where the symbol actually
  lives, and says so in text output too — not only under `--explain`:

  ```
  rq: nothing matching "CompletelyMadeUpClass#__getobj__" — that name is
      defined under ActionMailer::MessageDelivery (…/message_delivery.rb:31)
  ```

  A name that genuinely isn't there still reports `no_match`. Both exit 1.

### Changed
- The skill's batch-mode latency figures were roughly 20x optimistic and
  understated the ratio, which misled in the direction of skipping batching on
  exactly the large repos where it pays most. Replaced with measured numbers at
  two repo sizes, and the note that the ratio is the durable part.

## 0.48.0 — 2026-08-21

### Changed
- **Recall is 2-6x faster on a query with no exact match.** Two passes were
  doing far more work than they returned:

  The path pass — the one that lets `billing` find the class in `billing.rb` —
  tested a `LIKE '%query%'` against the joined `files` table while scanning
  every symbol row. It now narrows on `files` first and seeks the symbols of
  whatever matched: 29 ms to 0.4 ms on a 49,000-symbol index, same results. A
  leading `%` can't use an index either way, but scanning 3,000 file rows beats
  scanning 49,000 symbol rows to test a column on a join.

  The first-character anchor now runs only for short queries. It exists to reach
  skip-abbreviations (`usr` → `user`) that prefix matching can't, and a short
  query yields too few trigrams for the FTS layer to be much of a net. A long
  query gets a good trigram net already, so anchoring on one letter only dragged
  in thousands of rows the scorer then rejected.

  Measured on Rails: `usr` 7 ms → 2 ms, `midleware` 9 ms → 3 ms,
  `connectoin_pool` 19 ms → 10 ms. Top-3 results are unchanged across a
  20-query battery, and long abbreviations (`connpool`, `actctrl`, `midstack`)
  still resolve to the same definitions.

## 0.47.0 — 2026-08-21

### Changed
- **Every search was scanning the whole symbol index.** The prefix pass asked
  for `name_lower LIKE 'query%'`, which looks like it should use the index and
  doesn't — SQLite only turns `LIKE` into a range scan when the index collation
  matches the operator's case sensitivity, and case-insensitive `LIKE` against a
  `BINARY` index falls back to reading every row. `EXPLAIN QUERY PLAN` says
  `SCAN` for it and `SEARCH` for the range comparison that replaces it.

  Recall on an ordinary query drops from ~5 ms to under a millisecond on a
  49,000-symbol index (`rq where` was 12 ms), and a query with no exact match
  from ~25 ms to ~18 ms. The gain scales with the index, so it's larger on a
  monorepo than these numbers suggest.

  A range is also more exact than what it replaces: `_` is a `LIKE` wildcard, so
  `connection_pool` had to be escaped to be matched literally.

### Internal
- Lowercasing during scoring borrows instead of allocating when there's nothing
  to change, which is most queries and most snake_case names. Worth ~0.5 ms on a
  query that reaches thousands of candidates.

## 0.46.1 — 2026-08-21

### Internal
- **A typo query no longer re-scores every candidate twice.** The near-miss
  retry looks only at candidates that could actually *be* a near miss (a length
  and first-letter check) rather than paying the whole name-match chain a second
  time for ten thousand rows to serve a few hundred. Two per-candidate
  allocations went with it: the separator-insensitive exact match built two
  squashed `String`s, and the namespace-depth signal built a `Vec<String>` of
  scope names when it only wanted the count.

  **Correction to what this entry first said.** It claimed scoring had got ~5x
  slower per candidate in 0.44.0/0.45.0. That was wrong — it compared a debug
  build against a release measurement and read the build profile as a
  regression. Measured like for like, 0.42.1 scores a 10,348-candidate query in
  5-6 ms and 0.46.1 in 7 ms, so everything added since costs ~1-2 ms and no
  user ever saw a slowdown. The changes above are worth keeping on their own
  terms; the regression they were credited with fixing did not exist.

## 0.46.0 — 2026-08-21

### Changed
- **One name declared in several files is now one result.** Ruby reopens a
  module across files and Rust spreads `impl` blocks the same way, so `rq
  Middleware` spent its whole first page on four declarations of
  `ActiveRecord::Middleware`, one of them a six-line autoload stub. The
  best-ranked declaration survives and the rest are recorded on it —
  `declarations` counts them and `also_in` says where they are, so the fold
  loses nothing. Only *qualified* names fold: two unqualified `Widget`s are one
  reopened class in Ruby but two unrelated types in Rust, and one row too many
  is the cheaper mistake.

### Added
- **A typo finds the definition instead of nothing.** Subsequence matching
  forgives typing too *little* and nothing else, so the two commonest typos were
  hard misses — `cnnection_pool` worked while `connectoin_pool` (swapped
  letters) and `connection_poool` (doubled letter) returned nothing at all.
  Queries that already match are untouched: the near-miss pass is a retry that
  runs only when the first pass turned up nothing worth showing, and it scores
  below every genuine fuzzy match, so it can only ever fill a gap.

  It costs an extra pass over the candidates on that path — a query that used to
  answer "no matches" instantly now takes a few hundred milliseconds on a large
  repo to answer correctly. Queries that match are unaffected.

  `--explain` shows it as `typo`.

## 0.45.0 — 2026-08-21

An independent review drove rq around the Rails source and reported what broke.
Everything below came out of that.

### Fixed
- **`--limit 1` made every result read `confidence: 1.0`.** Confidence is a
  comparison against the runner-up, and it was measured over the *returned*
  window — so asking for one answer always got a certain one. `rq X -j -l 1` is
  the natural way for an agent to ask, and it also disabled the `--show` safety
  gate: `rq initialize --show -l 1` printed one arbitrary body out of 1138
  matches as though it were the answer. Confidence is now measured before the
  limit truncates.
- **An unknown `--kind` or `--lang` reported a definitive miss.** `rq foo -k
  mehtod` filtered every result away and exited 1 — the one code a script is
  meant to trust as "this symbol does not exist". Both now reject the value and
  say what's valid.
- **An empty query returned results.** It's an error now.

### Added
- **`total` in structured output** — how many matches the window was drawn
  from, so a caller can tell it saw ten of a thousand. The `--show` refusal
  message says so too.
- **`--explain` reaches `--json`/`--ndjson`.** It was silently ignored there, so
  the "ranking is explainable" promise held only for humans. Results now carry
  an `explain` object of feature → weight when it's passed. `features` keeps its
  existing shape.
- **`Foo::Bar`, `Foo#bar`, and wildcards are documented in `--help`.** Qualified
  lookup is the surest way past an ambiguous name and nothing advertised it.

### Changed
- **Definitions with a real body outrank stubs** (`extent`). `rq where` returned
  a 3-line ActionCable method over the 9-line `ActiveRecord::QueryMethods#where`;
  `rq cache_key` returned an `alias_method` line; `rq delegate` returned a
  compiled JavaScript bundle. Log-scaled and capped, from `end_line`, which was
  already stored.
- **Leaving separators out still counts as an exact match** (`separators`).
  `parsefile` for `parse_file` is an abbreviation of an exact match, not a fuzzy
  one, and it was losing to whichever similar name had more lines. Spelling the
  name out in full still ranks higher.
- **`depth` no longer charges for ordinary namespacing.** Introduced in 0.44.0,
  it penalized every level of scope — which made it a penalty on *languages*:
  Ruby and Rust namespace library code two deep where JavaScript leaves it at
  the top level, so `ActionController::Metal#dispatch` ranked 9th of 10 behind
  eight compiled `.esm.js` bundles. Only nesting past two levels is charged now.
- **The test/spec penalty is heavier.** At its old weight it couldn't cross the
  gap between an exact match and a prefix one, so `rq conn` still answered with
  a three-line private helper in a test file.

## 0.44.0 — 2026-08-21

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
