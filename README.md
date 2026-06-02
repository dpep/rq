rq — Reference Query
====================

**Find the code you're looking for.** rq is a code *navigation* engine, not a
search engine: it ranks aggressively to take you to the file, symbol, or
definition you most likely want, instead of enumerating every match. A good
result feels like *"that's exactly the one I wanted"* even when hundreds of
others technically match.

```sh
rq refund        # → RefundProcessor   app/services/refund_processor.rb:7
rq perform       # → the perform you actually meant, ranked first
rq usr           # → User              app/models/user.rb:1
rq refundproc    # → RefundProcessor   (fuzzy, abbreviation-aware)
```

Search is the default action (`rq <query>`, not `rq search …`), aiming to feel
as immediate as `rg`, `fd`, and `fzf`.

> **Status: early, working.** Ranking (exact/prefix, abbreviation-fuzzy, path,
> current-repo, recency, and learned-from-your-picks signals) is functional for
> Ruby and Rust (rq indexes its own source); the index warms on first use and
> self-heals. Shipped editor hooks + a shell wrapper; a packaged extension and
> more languages are next. See [docs/ROADMAP.md](docs/ROADMAP.md).

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
rq <query> -k/--kind KIND   # restrict to class|module|method|function (c/mod/m/f)
rq <query> -l/--limit N     # cap the number of results (default 10)
rq --index [PATH]           # index a repository (incremental; safe to re-run)
rq --index --path DIR       # index only a subtree (partial — for big monorepos)
rq --status                 # indexing coverage per known repository
```

## For agents / scripts

`-j/--json` (array) and `-J/--ndjson` (one object per line) are the structured
surface for editors, scripts, and AI agents. Each result is an object with
`name`, `kind`, `file`, `line`, `parent`, `repo`, `score`, the scoring
`features`, and `signature` (the definition's source line, so you can judge a
result without opening the file). Exit code is `0` when something matched,
non-zero when nothing did.

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

Each result is a navigable `path:line`. `--explain` shows the additive score:

```sh
$ rq corpus --explain
lib/iriq/corpus.rb:14  class Corpus · Iriq
    score 1015 = exact 1000 + kind 15
lib/iriq/corpus.rb:431  method corpus_token · Iriq::Corpus
    score 694 = prefix 694
```

## Ranking

Symbols come from Tree-sitter (Ruby and Rust; the core is language-agnostic). A
query is matched and scored by an additive, explainable sum of signals:

- **match quality** — exact > prefix > camel/underscore abbreviation > subsequence
- **path** — the query also matches the file's name
- **current repo** — the project you're in outranks others
- **recency** — symbols in recently-edited or recently-committed files
- **branch** — on a feature branch, files you're changing vs the trunk (and
  their directory neighbors) — where you're most likely working
- **learned** — results you've opened before for this query (see below)

Returning fewer, better, ranked results is the goal — not completeness.

## Staying current

You rarely run `rq --index` by hand. The first query inside a git repository
warms the index opportunistically — but *time-bounded*, so even a huge repo
never blocks the first answer: it indexes the files you're changing on this
branch first, answers, then keeps warming a little per query until coverage is
complete. Results also self-heal: the files behind the top hits are revalidated
each search, so edited files are re-read and deleted ones drop out, and the warm
pass picks up added/changed/removed files as it sweeps. Outside a git repository
rq falls back to a live scan, so it answers even at zero coverage. The index is
a SQLite file at `$RQ_DB` (default `~/.local/share/rq/rq.db`).

## Learning from what you pick

Ranking improves as you use it. rq logs each search; a thin hook reports which
result you opened, so a `learned` boost lifts it next time:

```sh
rq --record --file app/services/refund_processor.rb --line 7 refund
```

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

On iriq's Ruby library (412 symbols) the in-process search pipeline measures
p50 ~160 µs, max < 0.25 ms — ~200× under the 50 ms target. Re-run with
`make bench REPO=/path/to/repo`.

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
