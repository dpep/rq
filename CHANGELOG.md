# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

## Unreleased

### Added
- **Fuzzy recall reads a name index.** Each repo's distinct symbol names and
  file names get a small signature of what a query could match, so fuzzy recall
  screens every name and hands the scorer exactly the names it accepts, instead
  of a capped net that missed some. With the ranking changes below, on the
  recall harness the source ranks first for 56.4% of queries (was 49.0%), in the
  top 10 for 75.6% (69.2%), and is found at all for 86.4% (81.5%); the query
  phase's median drops from ~7 ms to ~2 ms (DECISIONS D23, D24). The database
  migrates itself: the name index replaces the trigram table the nets read,
  so it stays about the size it was (a fresh rails + discourse index is 27.8
  MB, against 28.0 before), and each repo's index is built by the first search
  or index pass that needs it (a fraction of a second, once) (D26).

### Upgrade note
- **Upgrade every rq that shares the database.** The migration drops the
  trigram table older releases search, so an older rq on a migrated database
  fails any query without an exact or prefix match, and the first index of any
  repo, and its writes leave the name index behind. That includes a copy pinned elsewhere on
  `PATH`, or another machine's rq on a shared `RQ_DB`.

### Changed
- **A relative `RQ_DB` is refused (exit 64).** It resolved against each
  command's working directory, quietly creating a separate, empty index
  wherever rq ran. If you set `RQ_DB` to a relative path, make it absolute.
- **Fuzzy matches rank by how well the name reads.** A definition's kind, size,
  file name, nesting and visibility now count in proportion to how surely its
  name matched, so a big class that merely holds the query's letters no longer
  outranks a method that reads as the query (DECISIONS D24).
- **A strong fuzzy match in a test outranks a weak one elsewhere.** A test
  definition matched approximately gives up a share of its match instead of a
  flat 400 points, so it still ranks below an equally good match outside tests,
  but no longer below any match outside tests at all.
- **A typo reading is offered beside a name that holds the letters in order.**
  `tets_br` finds `test_br` again, ranked by how well each reads, where before
  the typo was dropped whenever an in-order name read better.
- **A leading `_` in a fuzzy query favours the underscored name.** `_dshrz`
  ranks `_dasherize` above the public `dasherize`.

### Fixed
- **A search or index pass no longer hangs forever at 0% CPU.** On repos
  with a couple of thousand source files or more, the file walk could be left
  waiting on parse threads that had already stopped: when a batch write failed
  (a busy database, say), or, rarely, when a live scan's time budget ran out
  at the wrong moment. `rq --no-wait` on a cold repo, `rq --index`, a cold
  search, and the background warm could all hang. Stuck `rq --warm` processes
  from earlier versions are safe to kill.
- **Two index passes at once wait for each other instead of failing.** A
  batch write that lost a race with another writer failed immediately with
  "database is locked", ignoring the busy timeout; now it waits its turn.
- **A burst of first searches against a new database all succeed.** Several
  rq processes opening a fresh (or older) database at once each tried to lay
  down the schema, and some exited 74 with "table already exists" or
  "database is locked". Now one does it and the rest wait for it.
- **One background warm per repo, even when searches race.** Searches in
  quick succession could each start a background `rq --warm` for the same
  repo, which then fought over the database. Claiming the warm is now atomic.

## 0.54.1 — 2026-09-27

### Fixed
- **An older rq no longer lowers a newer database's schema version.** Opening
  a database written by a newer rq used to write the older version back, so
  the newer rq re-ran migrations it had already applied — breaking any that
  add a column. Now the version is only ever raised.

## 0.54.0 — 2026-09-27

### Changed
- **Breaking for scripts: errors have their own exit codes.** A usage error — an
  unknown or conflicting flag, a bad value, an empty query — now exits `64`
  instead of `2` (clap's) or `1` (rq's own), so it can no longer be mistaken for
  "warming, ask again" or "no match". The other errors move too: a missing
  `--symbols` file exits `66`, no git host/editor/browser to hand off to `69`, an
  internal error `70`, and an index that can't be opened, read or written `74`.
  `0` hit, `1` miss and `2` warming are unchanged. If a script treated `2` as
  "retry" it now stops retrying typos; if it read `1` as "absent" it now sees an
  index failure as the error it is. Check any script that branches on exit codes
  other than `0`/`1`/`2`. The JSON error's `code` field carries the new number.
  `rq --help` lists the table. Exit codes are public API, so the next release is a
  minor one.

### Added
- **A live-scan answer says so.** Outside a git repo, rq answers from a bounded
  scan of the directory rather than the index, and that used to look identical.
  Each JSON result now carries `source`: `index` or `live`. `-v` notes the scan
  (files, time, budget), and `--usage` counts live answers in a new `live` field.
  The index database migrates itself on first use; nothing to do.

## 0.53.0 — 2026-09-26

### Added
- **JSON results say which checkout `file` is relative to.** Every search
  result, `--show` and `--symbols` row in `--json`/`--ndjson` carries `root`, the
  absolute checkout root. It is per result, because `-a/--all-repos` spans
  repos: join `root` and `file` rather than guessing the repo from the cwd. A new
  output field, so the next release is a minor one.
- **`--anchor FILE:LINE[:COL]` ranks from where you are.** Pass the position a
  lookup is asked from, such as an editor's cursor or the file an agent is
  reading. Definitions in the classes and modules enclosing that line rank
  first, then the same file and nearby directories. On real call sites in rails
  and discourse it ranked the resolved definition first 79% of the time,
  against 48% without. It reorders and never filters, and works with batch
  stdin too. Without the flag, ranking is unchanged. A new flag, so the next
  release is a minor one.

### Fixed
- **`-w` on a repo with no git remote names the match.** The error used to name
  only the repo identity. It now gives the `file:line` it picked and suggests
  `-o` to open it locally, or `git remote add` to make it linkable.
- **`RQ_OPEN` without a placeholder opens the match.** `RQ_OPEN=subl rq -o x`
  used to run `subl` with no file and exit 0. A template with no `{file}`,
  `{line}` or `{}` now gets `path:line` as its last argument.
- **`save` no longer reads `save!` as an exact match.** A trailing `!`, `?` or `=`
  is part of a name, not a separator, so `rq save` ranks `save!`, `save?` and
  `save=` as prefix matches, well below `save`. `rq 'Foo#save' --show` used to fall back to
  the list because `save!` scored within 50 of it. Only `_`, `-` and `.` may
  still be left out of an exact match.
- **Discarding an edit (`git checkout -- file`) is picked up.** The tree reads
  clean again, so rq's `git status` check used to see nothing to reindex, and
  the index kept the discarded version: a method the edit had removed stayed a
  confident "no match". rq now also rechecks the files it indexed as edits.
- **A repo's first search no longer answers from other repos.** Until its first
  index registered it, a repo searched every indexed repo, so `rq Widget` could
  return another checkout's `Widget`. It now answers only for itself, as it
  does from then on. This showed most in a repo with no commits yet. There,
  every search also reindexed the repo and reported a miss as `warming`. Such
  a repo now behaves like one with commits.
- **`-a -o` opens another repo's match where it is.** `--open` joined the match's
  path onto the current checkout, so a hit from another repo opened a file that
  didn't exist. It now uses the match's own `root`.
- **`total` counts every match under `-k`, `-x` or a path filter.** It was
  counted after the list was cut to the limit, so `rq class Base -l 1` reported
  `"total": 2`. It now counts the filtered matches before `--limit`, as it does
  without a filter.
- **`--explain` JSON reports whole points, as the text does.** Its values
  carried every float digit (`"extent": 33.67295829986474`). They are now rounded
  to the whole number the text shows. Ranking still sums the exact values.
- **A search outside any repo stays within its live-scan budget.** Outside a
  repo, rq scans the files around you directly (250 ms by default). Among
  folders with no source files it could overrun: a search from `/tmp` took over
  a second. It now stops on time, and `--profile` reports the scan as its own
  line.
- **Names with non-ASCII letters match in any case.** Queries were lowercased
  for ASCII letters only, while the index lowercases all letters, so `über`
  found nothing for a class `Über`. Queries now fold case the same way the
  index does.

### Changed
- **Errors under `--json`/`--ndjson` are JSON.** A failed structured run used to
  print nothing on stdout. Now it prints one
  `{"error": …, "kind": …, "code": …}` object: an unknown `-k`/`-x`, an empty
  query, a bad `--wait`, `--json` on piped queries, an index that can't be
  opened, a missing `--symbols` file. Errors clap raises while parsing the
  command line (a bad value, conflicting flags) are covered too, since rq
  reads `--json`/`-j`/`--ndjson`/`-J` off the command line itself. The human
  message still goes to stderr and exit codes are unchanged. `kind` is one of
  `usage`, `database`, `not_found`, `index`, `internal`. Scripts that treated
  empty stdout as failure should check for `error` instead. This changes the
  output shape, so the next release is a minor one.
- **`rq --symbols` on a file that doesn't exist says so.** It used to report
  "no symbols" (`{"status": "no_match"}` in JSON), which also describes a
  real file with no definitions. It is now a `not_found` error. Exit 1 as before.
- **The VS Code extension (0.3.0) no longer lists a symbol twice in Cmd/Ctrl-T.**
  With `rq.workspaceSymbols: dedupe` (the default), rq adds only the symbols no
  language server already returned — e.g. beside trekr for Ruby — and `off`
  leaves workspace symbols to the servers entirely. The rq binary is unchanged.
- **The index is about a fifth smaller.** The fuzzy (trigram) table no longer
  stores token positions, which recall never read: a fresh rails index is
  12.3 MB instead of 15.1 MB, and a cold index spends less time making names
  fuzzy-searchable. Results are unchanged. The first run after upgrading
  rebuilds that table from what's already indexed (a quarter second at 170k
  symbols); nothing is re-parsed and there is nothing to do. An existing
  database file keeps its size, and new writes reuse the freed space.

## 0.52.4 — 2026-09-26

### Added
- **Go constants are indexed.** Package-level `const` declarations, grouped
  blocks and `iota` enumerations included, are found as `constant` (and by
  `-k constant`). They appear automatically after upgrading: the first
  search in each repo re-indexes its Go, Python and TypeScript/JavaScript files
  in the background. Until that finishes, a search there that finds nothing
  waits on it, as on a first index, rather than reporting "no match".
- **Python constants are indexed.** An `UPPER_SNAKE` assignment at module or
  class level (`MAX_RETRIES = 3`, `TIMEOUT: float = 1.5`, `A, B = …`) is a
  `constant`, qualified by its class when it has one. Lowercase variables and
  anything inside a function stay out. They appear as Go's do (above).
- **TypeScript and JavaScript constants are indexed.** A module- or
  namespace-level `const` that isn't a function (those are already functions)
  or a `require(…)` import is a `constant`, whatever its casing — `const
  router = createRouter()` is as much a definition as `MAX_RETRIES`. So is a
  class's `static readonly` field. `let`, `var`, destructuring and enum
  members stay out. They appear as Go's do (above).

### Changed
- **`--help` and the unknown-`--kind` error list `constant`.** `-k constant`
  (or `const`) has worked since 0.50.1; it just wasn't listed.
- **The VS Code extension (0.2.0) answers for every rq language without
  duplicating a language server.** In the new default `rq.mode: fallback`, rq
  answers Go to Definition only where the other providers find nothing, so
  it's on for Ruby, Rust, Go, Python and TypeScript/JavaScript alongside Ruby
  LSP, rust-analyzer, gopls, Pylance and TypeScript. `always` restores the old
  behavior. The rq binary is unchanged.

## 0.52.3 — 2026-09-26

### Fixed
- **`Foo.new` finds the class outside a git repo too.** A directory rq scans
  live (not indexed) missed the class when its constructor was inherited or
  implicit; it now falls back to the class as an indexed repo does.

### Added
- **A VS Code extension** in `editors/vscode/`: Cmd/Ctrl-click, F12 and Peek
  Definition answered by rq, plus Cmd-T workspace symbols and an `rq: Search
  Definitions` picker. It serves Ruby by default (`rq.languages` adds more).
  Not on the Marketplace — build the `.vsix` and install it; see
  `docs/EDITORS.md`. The rq binary is unchanged.

### Changed
- **Back-to-back searches exit sooner.** After a search confirms nothing changed
  since indexing, searches over the next 10 seconds skip that check, as long as
  git's state still matches. Commits, checkouts, pulls and `git add` are noticed
  immediately. An unstaged edit is noticed by the first search after that
  window. On a large repo, a burst of searches finishes about a third faster.

## 0.52.2 — 2026-09-26

### Changed
- **Fuzzy searches are faster.** Abbreviations and typos (`conpool`, `usr`,
  `connectoin_pool`) used to hand the scorer thousands of rows it would reject;
  those are now dropped inside the database before being read. Search time
  falls by about a third on Rails and Discourse, with identical results.
- **A typo can beat a name that merely holds your letters.** `fethc_version`
  now finds `fetch_version` instead of `fetch_conversations`. A near miss used
  to be tried only when nothing else matched at all; it now competes whenever
  nothing matched literally, scored by how many of your letters it keeps.
- **Letters picked from inside a word count for less.** `testag` no longer
  prefers `ActiveStorage` (whose `t` and `e` sit mid-word in "Active") over
  names that read as the query. A match now earns credit from the first word
  start it reaches.
- **Results print before the search is counted.** On a busy machine the usage
  write could hold the first result back by 5–15 ms; it now runs after the
  output. `--usage` counts are unchanged.

### Fixed
- **Fuzzy searches on large repos find matches they used to miss.** A fuzzy
  search scored at most 8,000 of the names sharing a letter or trigram with
  the query, taken in index order, so on a big repo the answer could be cut
  off unseen: `test_floa_tlimits` found nothing on Rails. With the cheaper
  filter above, a search now reads four times as far. With the two ranking
  fixes above, the searched-for name ranks first ~49% of the time instead of
  ~40% on a 2,300-query benchmark. The most ambiguous short queries on the
  largest repos can take up to ~50% longer.

## 0.52.1 — 2026-09-26

### Fixed
- **Ruby predicates are found by their full name.** `rq only_uploads?` read the
  `?` as a one-char wildcard and missed `only_uploads?`; a lone trailing `?` is
  now part of the name. Wildcards also step over `_` in names the way they
  already ignored it in the query, so `rq 'only_up*s'` finds `only_uploads`.

### Changed
- **The first search in a repo that isn't indexed yet finds its answer sooner.**
  Files that contain the name you searched for are indexed before the rest.
  On a 14k-file repo the wait dropped from ~1.4 s typical (up to ~7 s) to
  ~0.25 s (up to ~0.6 s). On a repo too big to index in one pass, a search that
  used to report `warming` (exit 2) until later searches caught up now answers
  the first time. A search whose text appears nowhere, such as a typo or an
  abbreviation, waits for one extra read of the repo.

### Fixed
- **`--symbols` outlines its file on a large repo that isn't indexed yet.** The
  file is now indexed first however far the warm gets; a file with a short name
  (`m19.rb`) used to come back empty until the rest of the repo caught up.

## 0.52.0 — 2026-09-26

### Removed
- **Learned ranking is gone.** Results you opened or `--show`ed no longer lift
  that result for the same query next time. Six weeks of real use left the
  signal empty, so ranking is now a function of the index, recency, and your
  branch alone — the `learned` row no longer appears in `--explain`.
- **`--record` is gone** (and its `--file`, `--line`, `--event`). An editor
  hook or script that calls `rq --record` will now fail with an unknown-flag
  error — delete that call. `--open`, `--web`, and `--show` no longer record
  anything, and `script/rq-open` no longer calls it.
- **`--no-record` is gone.** Every search now counts toward `--usage`; there
  was nothing left for it to keep out of ranking. A script or benchmark that
  passes it will now fail with an unknown-flag error — drop the flag. To keep
  a benchmark out of your real counts, point it at a scratch `RQ_DB`.
- **The database drops its learning tables.** The first run of this version
  migrates the DB, deleting `selection_stats` and the raw `events` log; nothing
  to do. `--usage` keeps every count it had.

## 0.51.3 — 2026-09-26

### Fixed
- **`Foo.new` on a class that inherits its constructor finds the class.** It
  used to fall through to the typo retry and answer with a similarly named
  class's constructor (`Widget.new` → `Widgey#initialize`). The class is now
  the answer, flagged `constructor_owner` in `--explain`, at 0.75 confidence.

## 0.51.2 — 2026-09-26

### Added
- **`-w`/`--web` opens a result on GitHub.** Like `-o`, but the browser gets a
  permalink (`blob/<sha>/<file>#L<line>`) pinned to your checked-out commit, or
  the newest pushed one before it, so the link always resolves.

### Changed
- **Searches return sooner on large repos.** A hit no longer waits for a
  `git status` over the whole worktree before exiting; the background warm
  asks instead. Rails: ~15 ms → ~5 ms; a 14k-file repo: ~32 ms → ~10 ms.
- **Fewer `git` processes when indexing.** The background warm reuses the
  repo identity it already knows, the branch is read from `.git/HEAD`, and a
  non-git directory never asks git for a remote. A no-op `rq --index` on rails
  is ~56 ms (was ~66); the warm after an edit ~35 ms (was ~47). `rq --index`
  still asks git for the remote, so it remains how a checkout picks up a newly
  added one.
- **Fuzzy searches are ~15–20% faster** — the index is read memory-mapped.
- **The first search in a new repo answers about twice as fast.** The warm it
  runs now indexes names for fuzzy search in one step instead of row by row,
  as `rq --index` already did. Rails, from nothing: ~0.9 s → ~0.35 s for a
  class, ~1.4 s → ~0.7 s for a fuzzy query or a miss.
- **Indexing a new repo no longer re-indexes every other repo's names.** The
  first `rq --index` of a repo rebuilt the fuzzy-search index for the whole
  database; now it adds only what's missing. A 2k-symbol repo into a 176k-symbol
  database: ~313 ms → ~109 ms.
- **`--profile` covers the whole run.** `total` now runs to process exit, a
  `first answer` row marks when results were printed, and the work after it
  (usage rollup, the worktree check) has its own rows. Misses, `--symbols` and
  `--open`/`--web` report too; before, they printed nothing. A stored
  `total_ms` baseline from an earlier version measured less and isn't
  comparable.
- **`--symbols` is faster** — ~4 ms on a fully indexed repo, from ~15 ms,
  and ~5 ms from ~41 ms on a 5,000-line file. It re-scanned the file from the
  top for every symbol's signature line, and asked git about the whole
  worktree when only the one file matters.
- **`--symbols` sees a new untracked file, and a deleted one has no
  outline.** Before, a file git didn't track yet listed nothing until
  committed or `--index`ed, and a deleted file could list stale symbols.

### Fixed
- **Other indexed repos no longer crowd out the one you're in.** Recall capped
  each layer across *every* repo and filtered to yours afterwards, so a big
  neighbour could use up the cap, and an exact match elsewhere could stop a
  fuzzy match here from being looked for at all (`rq wdgt` missing your
  `Widget` because another repo defines `wdgt`). With eight repos indexed,
  fuzzy searches are also 15–35% faster, and short ones (`usr`) about 40%
  faster again via a new per-repo name index. The database upgrades itself on
  first run (schema v12, a second or so on a large index); nothing to do.
- **Uncommitted edits no longer make every miss say "still warming".** Any
  dirty tracked file counted as "changed since indexed", so while you had
  uncommitted work each search started a background re-index and each miss
  exited 2 (`warming`) instead of 1 — indefinitely. Now an edit counts only
  until it's indexed.
- **Two clones of one repo each read their own files.** Clones of the same
  remote share one set of index rows, but revalidation read from the
  first-recorded clone and signatures/`--show` from the newest, so a search in
  one clone could show the other's source lines. Both now read from the
  checkout you're in.

## 0.51.1 — 2026-09-25

### Changed
- **`.` is a scope separator first.** `rq Foo.bar` now means `bar` inside
  `Foo`, like `Foo#bar` — the way a Ruby class method or a Python/JS member is
  written. When no scope answers, `.` falls back to its old meaning as a
  one-character wildcard, so `find.controller` still works.
- **A typo in the scope is forgiven** the way a typo in the name already was:
  `Widgit.new` and `Widgit::Foo` land inside `Widget`, at typo-level confidence.

### Added
- **`rq Foo.new` finds the constructor** — Ruby `initialize`, Python
  `__init__`, JS/TS `constructor` — inside `Foo`. `Foo::new` works the same.

## 0.51.0 — 2026-08-24

### Added
- **`--profile` now covers indexing.** `rq --index --profile` reports where the
  run's time went — setup, candidate enumeration, the fused walk+parse+write
  phase with its overlapping parse and write components, the FTS rebuild,
  deletion reconcile, and git metadata — plus counters (files seen, parsed,
  skipped by mtime, symbols, batches committed, parse jobs) and the five slowest
  files to parse, which is how one pathological generated file gets found.
  Human-readable to stderr; under `--json`/`--ndjson` it is a single JSON object
  on stderr (`total_ms`, `phases`, `counters`, `slowest`), so stdout stays
  exactly the result. Search `--profile` JSON gains the same `counters` and
  `slowest` keys, empty for a search. Free when off, and `RQ_PROFILE=1` turns it
  on for an installed binary the same way it does for search.

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
