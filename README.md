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
> Ruby; the index warms on first use and self-heals. Shipped editor hooks + a
> shell wrapper; a packaged extension and more languages are next. See
> [docs/ROADMAP.md](docs/ROADMAP.md).

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
rq <query>              # search definitions; ranked
rq <query> --explain    # show the score behind each result
rq --index [PATH]       # index a repository (incremental; safe to re-run)
rq --status             # indexing coverage per known repository
```

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

Symbols come from Tree-sitter (Ruby first; the core is language-agnostic). A
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
indexes it opportunistically, and results self-heal: the files behind the top
hits are revalidated each search, so edited files are re-read and deleted ones
drop out. Outside a git repository rq falls back to a live scan, so it answers
even at zero coverage. The index is a SQLite file at `$RQ_DB` (default
`~/.local/share/rq/rq.db`).

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
