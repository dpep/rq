rq — Reference Query
====================

**rq finds where a symbol is *defined* and ranks the one you meant to the top.** Ask for a name and you get the single most-likely definition first — a class, method, function, struct — not every line that mentions it. Navigation, not enumeration.

```sh
rq refund        # → RefundProcessor   app/services/refund_processor.rb:7
rq perform       # → the perform you actually meant, ranked first
rq usr           # → User              app/models/user.rb:1  (fuzzy, abbreviation-aware)
rq refund*proc   # → explicit gaps: `*` any run, `?`/`.` one char
rq Account::save # → the save defined inside Account (scope-aware; also Account::Refund)
rq class Widget  # → a leading kind keyword is shorthand for -k class
```

Search is the default action — `rq <query>`, no subcommand. Every *operation* is a flag (`--index`, `--status`, `--symbols`), so no word is reserved: `rq index` searches for a symbol named "index" like any other query. The feel is `rg`/`fd`: type a name, get an answer.

## Why not grep / ctags / an LSP?

- **grep / rg** give every textual mention; rq gives the one place a symbol is *defined*, ranked.
- **ctags** is static and relevance-blind; rq ranks by match quality, your current repo, recency, and what you've opened before.
- **an LSP** is heavy — per-language, per-project, slow to warm. rq is one fast binary across all your repos: in-process search at `rg` speed (sub-millisecond), warms itself on first use, self-heals on edits, and learns from the results you actually open.

Definitions come from [Tree-sitter](https://tree-sitter.github.io/) for Ruby, Rust, Go, and Python.

## Install

```sh
brew install dpep/tools/rq      # builds from source; no runtime deps
```

Or build it yourself — rq needs Rust only at build time:

```sh
cargo install --path .          # or: make install
```

## Usage

```sh
rq <query>                  # search definitions; ranked
rq <query> -e/--explain     # show the score behind each result
rq <query> -j/--json        # JSON array (-J/--ndjson for one object per line)
rq <query> [DIR...]         # restrict to directories (rg-style; or -p/--path)
rq <query> -k/--kind KIND   # restrict to kind: class|module|method|function|struct|enum|trait
rq KIND <query>             # a leading kind keyword is shorthand for -k (rq class Widget)
rq Scope::name              # scope-aware: prefer the name defined inside Scope (or Scope::Type#method)
rq <query> -x/--lang LANG   # restrict to language: ruby|rust|go|python (prefix-matched; r=ruby+rust)
rq <query> -l/--limit N     # cap the number of results (default 10)
rq <query> -o/--open        # open the best match in your editor + record the pick
rq --symbols FILE           # outline a file's definitions, in line order
rq --index [PATH]           # index a repository (incremental; safe to re-run)
rq --index --path DIR       # index only a subtree (partial — for big monorepos)
rq --drop [PATH|IDENTITY]   # remove a repo's index (opposite of --index)
rq --status                 # indexing coverage per known repository
```

## Opening results

`rq -o <query>` jumps straight to the best match in your editor and records the
pick, so ranking learns which result you actually wanted. On a terminal with
several matches it prompts you to choose; otherwise it takes the top hit. The
launcher is resolved in order: `RQ_OPEN` (a command template with `{file}`,
`{line}`, or `{}` = `path:line`), then VS Code (`code`), then `$VISUAL`/`$EDITOR`,
and failing all that it just prints the resolved `path:line`.

```sh
rq -o refund                          # open the top match, record it
RQ_OPEN='vim +{line} {file}' rq -o x  # force a specific launcher
```

For an interactive fzf picker (or to wire a custom flow), `script/rq-open` is a
small reference wrapper around `rq` + `rq --record`.

## For agents / scripts

`-j/--json` (array) and `-J/--ndjson` (one object per line) are the structured
surface for editors, scripts, and AI agents. Each result is an object with
`name`, `kind`, `language`, `file`, `line`, `parent`, `repo`, `score`, the
scoring `features`, and `signature` (the definition's source line, so you can
judge a result without opening the file). Exit codes: `0` matched, `1` no match,
`2` no match *yet* — the index is still warming, so re-run (or `rq --index`)
rather than concluding the symbol is absent. All non-zero, so `rq … && …` is
unchanged.

On a cold repo a query **blocks and indexes until it can answer**, rather than
returning a premature empty result — correctness over the first query's latency,
and once warm the repo answers fast. Set `RQ_WAIT_BUDGET_MS=0` for a strictly
non-blocking query (answers from whatever's already indexed).

`--json`/`--ndjson` work for every command, not just search: `rq --status --json`
emits the coverage rows (`repo`, `status`, `files`, `symbols`), `rq --index --json`
emits this run's counts plus the index totals, and `rq --drop --json` reports what
it removed (`repo`, `files`, `symbols`, `dropped`). Single-result commands emit
one object; `--ndjson` is the compact one-line form.

Reach for `rq` over `grep`/`rg` when you want **where a symbol is defined** —
it returns the most likely definition first instead of every textual mention.
Narrow with `--path` when you know the area:

```sh
rq RefundProcessor --json                 # jump to the definition
rq perform app/services --json            # ...scoped to a subtree (rg-style)
```

Pass `--no-record` for speculative/agent searches so they don't perturb the
learned ranking (which is meant to reflect deliberate, human picks).

Text results show each definition's source line and highlight the characters
your query matched — in the name, the filename, and that line (handy for fuzzy
queries, where it shows exactly what `rq` latched onto). Color is on only when
output is a terminal, honors `NO_COLOR`, and takes its style from `GREP_COLORS`
(`mt`/`ms`) if set.

Run `rq` with no arguments for help. Operations are flags, not subcommands, so
no word is reserved — `rq index`, `rq status`, and `rq record` search for those
symbols like any other query. rq works on the current repository; to target
another, run it from there.

## File outline

`rq --symbols <file>` lists every definition in a file, in line order — a
structural outline, not a ranked search. Honors `-k/--kind` and `-x/--lang`, and
emits `--json`/`--ndjson` like everything else.

```sh
rq --symbols src/search/score.rs
rq --symbols src/store/schema.rs -k struct,enum --json
```

Each result is a navigable `path:line`. `--explain` shows the additive score:

```sh
$ rq Store --explain
src/store/mod.rs:56  struct Store
    pub struct Store {
    score 1290 = exact 1000 + kind 15 + current_repo 200 + recency 75
src/search/mod.rs:316  function store_with · tests
    fn store_with(symbols: &[Symbol]) -> Store {
    score 954 = prefix 695 + current_repo 200 + recency 59
```

## Ranking

Symbols come from Tree-sitter (Ruby, Rust, Go, Python; the core is
language-agnostic). A
query is matched and scored by an additive, explainable sum of signals:

- **match quality** — exact > prefix > camel/underscore abbreviation > subsequence
- **qualifier** — a scoped query (`Foo::Bar`) prefers the definition inside that scope
- **path** — the query also matches the file's name
- **current repo** — the project you're in outranks others
- **recency** — symbols in recently-edited or recently-committed files
- **branch** — on a feature branch, files you're changing vs the trunk (and
  their directory neighbors) — where you're most likely working
- **learned** — results you've opened before for this query (see below)

Returning fewer, better, ranked results is the goal — not completeness.

## Staying current

You rarely run `rq --index` by hand. The first query in a git repo warms the
index opportunistically — files you're changing on this branch first — then tops
up a little per query until coverage is complete, so a warm repo answers in
milliseconds. A **cold** repo is the exception: the first query blocks and
indexes until it can answer (progress shown, Ctrl-C to stop) rather than lie with
a false miss. It's a one-time cost — the index persists and self-heals as you
search, re-reading edited files and reconciling added/removed ones on the warm
sweep.

A non-git directory isn't warmed on a stray query, but `rq --index <dir>` tracks
it like any repo under a `local:<path>` identity; otherwise rq live-scans it, so
it still answers at zero coverage. The index is a SQLite file at `$RQ_DB`
(default `~/.local/share/rq/rq.db`).

## Learning from what you pick

Ranking improves as you use it. rq logs each search; a thin hook reports which
result you opened, so a `learned` boost lifts it next time:

```sh
rq --record --file app/services/refund_processor.rb --line 7 refund
```

The `--event` kind is `select` (chosen from results) or `open` (jumped to in an
editor) — both feed the `learned` boost; `select` is the default.

A pick for a shorter query (`ref`) also informs longer ones (`refund`), and
repeating a search without opening anything is read as a miss — that query's
learned boost decays so a stale favorite stops dominating.

This isn't a daemon: each invocation does a small, bounded chunk of deferred
work (rolling up events, warming the index) *after* printing results, so the
cost amortizes across normal use.

The wrapper [`script/rq-open`](script/rq-open) does search → pick → open →
record in one step. See [docs/EDITORS.md](docs/EDITORS.md) for VS Code and
Neovim — it's just `rq` plus `rq --record`, no socket.

## Shell completions

```sh
rq --completions <shell>        # bash, zsh, fish, elvish, powershell
```

Homebrew installs bash/zsh completions automatically.

## Performance

The in-process search pipeline measures p50 ~160 µs, max < 0.25 ms on a mid-size
library (a few hundred symbols) — microseconds against a 50 ms first-answer
budget. Benchmark your own tree: `make bench REPO=/path/to/repo`.

## Scope

rq indexes **definitions** — classes, modules, methods, functions. It does
**not** do call graphs, type inference, reference tracking, inheritance, or LSP
features; it's useful with definitions alone. It's built for many repositories
and millions of symbols, and never assumes everything belongs to one project.
Repository identity is normalized from the git remote (`github.com/org/repo`),
falling back to `local:/absolute/path`.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design.

## License

[MIT](LICENSE.txt) © Daniel Pepper.
