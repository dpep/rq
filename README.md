rq — Reference Query
====================

> **Status: early, working.** Phases 1–3 are functional — `rq <query>` returns
> ranked Ruby results (with `--explain`), the index warms opportunistically on
> first use, stale results self-heal, and ranking learns from the results you
> pick. Editor integration (phase 5) is next. See
> [docs/ROADMAP.md](docs/ROADMAP.md).

**A code navigation engine, not a code search engine.** `rq` helps you reach
the file, symbol, or definition you are *most likely* looking for as fast as
possible — not to enumerate every technically-correct match.

A good `rq` result feels like *"that's exactly the file I wanted"* even when
hundreds of other matches exist.

```sh
rq refund        # → RefundProcessor   app/services/refund_processor.rb:7
rq perform       # → the perform you actually meant, ranked first
rq usr           # → User              app/models/user.rb:1
rq refundproc    # → RefundProcessor   (fuzzy, abbreviation-aware)
```

Search is the default command — `rq <query>`, not `rq search <query>`. The
tool aims to feel as immediate as `rg`, `fd`, and `fzf`.

## Design goals

- **Relevance over completeness** — fewer, better results beat many mediocre ones.
- **Navigation over discovery** — get to the answer, don't survey the space.
- **Speed over exhaustiveness** — initial results in < 50 ms whenever possible,
  then progressively improved.
- **Learned over static** — ranking improves from what you actually open and select.

Users should never think about indexes, scan state, or storage. The system
degrades gracefully: it is useful when 0%, 5%, or 100% of a repository has been
indexed.

## How it works (overview)

Search is a staged, streaming pipeline that stops early once confidence is high:

1. exact / prefix symbol match (indexed) — fastest, highest confidence
2. fuzzy symbol match (abbreviation-aware)
3. path / filename match
4. live repository scan (streamed when the index is thin)
5. opportunistic symbol extraction from newly-scanned files (persisted)

Symbols come from Tree-sitter. **Ruby is the first language**, but the core is
language-agnostic — a plugin emits a common symbol model and the core never
contains language-specific assumptions. Go, TypeScript, Python, and Java are
addable without redesigning the core.

Every interaction (search / open / select) is recorded so ranking can learn
which results you actually choose. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and
[docs/ROADMAP.md](docs/ROADMAP.md) for what ships when.

## CLI

```sh
rq <query>              # search definitions; ranked
rq <query> --explain    # show the score breakdown for each result
rq --index [PATH]       # index a repository (incremental; safe to re-run)
rq --status             # coverage per known repository
rq -C <dir> <query>     # run as if from <dir> (like `git -C`)
```

Operations are flags, not subcommands, so no word is reserved — `rq index`,
`rq status`, and `rq record` search for those symbols like any other query.

Each result is a navigable `path:line` location, intended to open directly in
an editor:

```sh
$ rq corpus --explain
lib/iriq/corpus.rb:14  class Corpus · Iriq
    score 1015 = exact 1000 + kind 15
lib/iriq/corpus.rb:431  method corpus_token · Iriq::Corpus
    score 694 = prefix 694
```

The index lives at `$RQ_DB`, or `~/.local/share/rq/rq.db` by default. You rarely
need `rq index` explicitly: the first query inside a git repository indexes it
opportunistically, and results self-heal — the files behind the top hits are
revalidated on each search, so edited files are re-read and deleted ones drop
out. Outside a git repository, `rq` falls back to a live scan so it still
answers at zero coverage.

## Performance

Search is index-backed and runs well inside the latency budget. On iriq's Ruby
library (412 symbols), the in-process search pipeline measures p50 ~160 µs and
max < 0.25 ms — roughly 200× under the 50 ms target. Re-run with:

```sh
make bench REPO=/path/to/repo
```

## Learning from what you pick

Ranking improves as you use it. `rq` logs each search, and a thin hook reports
which result you opened so a `learned` boost lifts that result next time you run
the same query:

```sh
rq --record --file app/services/refund_processor.rb --line 7 refund
```

Editors and shell wrappers call `rq --record` after you jump to a result — it's
the editor-independent ingestion point. A pick for a shorter query (`ref`) also
informs longer ones (`refund`). And repeating a search without opening anything
is read as a miss: that query's learned boost decays, so a stale favorite stops
dominating and alternatives resurface.

Aggregation isn't a background daemon: each `rq` invocation does a small,
bounded chunk of deferred work (rolling up events, warming the index) after
printing results, so the cost amortizes across normal use.

## Install

`rq` is a single static Rust binary. Install instructions land once the first
release ships.

## Repository identity

Every symbol belongs to a repository. Identity is normalized from git remotes:

- `github.com/org/repo` — derived from the upstream remote (preferred)
- `local:/absolute/path` — fallback when there is no remote
- or an explicit user-defined name

`rq` is built for **many** repositories and millions of symbols from day one;
it never assumes all indexed symbols belong to a single project.

## Non-goals (MVP)

`rq` indexes **definitions only** — classes, modules, methods, functions. It
does **not** build call graphs, type inference, reference tracking, inheritance
analysis, or LSP features. The MVP is useful with definitions alone.

## License

TBD.
