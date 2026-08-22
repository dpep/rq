---
name: rq
description: Find where a symbol is defined with the `rq` code-navigation CLI (Ruby, Rust, Go, Python, TypeScript, JavaScript). Use when locating a definition by name — "where is the X class / Y method / Z function defined", "find the definition of …", "jump to …", or any "where is <name>" about code; when getting oriented in an unfamiliar codebase, where you know roughly what a thing is called but not where it lives; and `rq --symbols <file>` to outline a file's definitions before reading it. Prefer over grep/rg for symbol navigation — it ranks the definition you meant first instead of listing every textual hit. Not for free-text/content search.
---

# rq — find the definition

`rq` is a code *navigation* engine: given a name, it returns the single
most-likely definition first (ranked), instead of every textual match. Reach
for it whenever the goal is **"where is this defined?"** — a class, module,
method, function, struct, enum, or trait, across Ruby, Rust, Go, Python,
TypeScript and JavaScript. Use
`grep`/`rg` instead for content/text search.

## Use it like this

Always ask for JSON:

```sh
rq <name> --json
```

Reading the definition rather than just locating it? Use `--show`, which prints
the body when it's confident. It also tells rq which definition you actually
used, which is how ranking improves for this repo — prefer it over a search
followed by a separate file read:

```sh
rq <name> --show --json
```

Each result is an object:

```json
{ "name": "RefundProcessor", "kind": "class", "file": "app/services/refund_processor.rb",
  "line": 7, "end_line": 34, "parent": "Billing", "repo": "github.com/org/app",
  "confidence": 0.98, "features": ["exact","current_repo","recency"],
  "signature": "class RefundProcessor < Base" }
```

## Many lookups at once

Looking up a list of symbols? Pipe them on stdin, one per line, and rq answers
them all in a single run — it resolves the repo, opens the index and checks the
worktree once instead of per query.

Per-lookup cost grows with repo size and the saving grows with it, so batching
matters most exactly where the repo is largest: roughly 2x on a mid-size repo
(~28ms per separate lookup against ~14ms batched, 3k files) and around 4x on a
large monorepo (~350ms against ~85ms, 90k files). Treat the ratio as the durable
number — the absolute figures depend on the repo, the machine and how warm the
index is.

```sh
printf 'RefundProcessor\nBillingJob\nInvoice\n' | rq -J
rq -J < symbols.txt
```

Each row carries the `query` that produced it, so a single stream stays
attributable, and a name that matched nothing still reports
`{"query": …, "status": "no_match"}` rather than vanishing — a miss and a
lookup that never ran are otherwise indistinguishable. Needs `--ndjson`/`-J`
(`--json` can't frame several result sets), and `--show`/`--open` don't apply.

`signature` is the definition's source line, so you usually don't need to open
the file to confirm a match. `confidence` (0–1) is how sure rq is this is the one
you meant — near 1.0 means take it; a low value or several close results means
disambiguate (add a kind, scope, or `--path`). `line`/`end_line` bound the whole
definition — read exactly that span instead of the whole file, or let rq hand you
the source directly:

```sh
rq RefundProcessor --show --json   # adds a "body" field with the source
```

`--show` prints the definition's full source when the top match is confident
(else it returns the ranked list) — one call to locate *and* read.

On a miss, JSON is a `{"status": …}` object, not results: `no_match` (definitive
— fall back to `rg`), `warming` (index incomplete — retry), or `interrupted`.
Exit codes mirror it: `0` matched, `1` no match, `2` warming. `2`/`warming` is
rare — rq blocks and indexes a cold repo before answering — so you normally just
get results.

## Scope when you know more

- Fuzzy/abbreviation works: `rq refundproc`, `rq usr`, `rq perform`.
- Scope: `rq Billing::RefundProcessor`, or `rq RefundProcessor#perform` for a
  method inside a class — rq prefers the definition in that scope, so use it when
  you know the enclosing module/class from the surrounding code.
- Kind: `rq save -k method`, or the shorthand `rq method save`. Kinds are
  `class`/`module`/`method`/`function`/`struct`/`enum`/`trait` (shortcuts
  `c`/`mod`/`m`/`f`/`s`/`e`/`t`, comma-separable: `-k m,f`).
- Directory: `rq perform app/services` (rg-style trailing path, repeatable) or `--path`.
- Count: `-l 1` to jump straight to the best hit, larger to survey, `-l 0` for
  every match.
- Repo: results are scoped to the current repo by default; add `-a`/`--all-repos` to
  search every repo you've indexed (a `no_match` means it's absent *here*).
- Wildcards: `*` (any run) and `?` (one char) — **quote these** so the shell
  doesn't glob them: `rq 'refund*proc'`. (`::` and `#` need no quoting.)

```sh
rq perform -k method app/services --json
rq Widget -l 1 --json          # just the top hit
```

## Outline a file

`rq --symbols <file>` lists a file's definitions in line order — a structural map
(with `line`/`end_line`) to read *before* opening the file, so you jump to the
right span instead of scanning the whole thing.

```sh
rq --symbols src/store/mod.rs --json
rq --symbols src/store/mod.rs -k method --json   # just the methods
```

## Installing / updating the binary

If `rq` isn't on PATH, install it, then retry the search:

```sh
brew install dpep/tools/rq      # macOS/Homebrew (builds from source; needs Rust at build time)
```

No Homebrew? Build from source (needs the Rust toolchain):

```sh
cargo install --git https://github.com/dpep/rq
```

To update to the latest: `brew upgrade dpep/tools/rq` (or re-run the
`cargo install --git …` line). Source + issues: <https://github.com/dpep/rq>.

## Notes

- rq auto-indexes the current git repo on first use; no setup needed. Ruby,
  Rust, Go, Python, TypeScript and JavaScript are supported.
- Run from inside the target repo (or set the subprocess working directory) —
  rq resolves the repo from its cwd.
- Let rq learn from you. Ranking improves from which definition actually got
  used, and your lookups are the bulk of the traffic — `--show` reports that for
  free, and after navigating via a plain search you can report the hit you used:
  `rq --record --file <f> --line <n> <query>`. Only record the one you actually
  worked from; speculative searches cost nothing and teach nothing.
- Pass `--no-record` when the same query repeats mechanically — a benchmark, a
  test harness, a loop over a list — so one query doesn't dominate the signal.
