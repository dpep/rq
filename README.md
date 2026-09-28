rq — Reference Query
====================

**rq finds the code you're looking for.** Name a class, method, function, struct or constant, and rq shows where it's defined.

In a Rails checkout:

```sh
rq HashWithIndifferentAccess  # → activesupport/lib/active_support/hash_with_indifferent_access.rb:55
rq hwia                       # → the same class: fuzzy and abbreviation-aware
rq 'Hash*Access'              # → wildcards: `*` any run, `?` one char (quote them)
rq ActiveRecord::Base         # → the Base defined inside ActiveRecord
rq Persistence#save           # → the save defined inside Persistence
rq Migration.new              # → Migration's constructor (initialize, __init__, constructor)
rq class Base                 # → a leading kind keyword is shorthand for -k class
rq perform activejob          # → the perform under activejob/, ranked
```

rq finds **names**, not behaviour. If you know what the code does but not what
it's called, reach for text search (`rg`) instead.

## Why not grep / ctags / an LSP?

- **grep / rg** give every textual mention; rq gives the one place a symbol is *defined*, ranked.
- **ctags** is static and relevance-blind; rq ranks by match quality, your current repo, recency, and the branch you're on.
- **an LSP** is heavy — per-language, per-project, slow to warm. rq is one binary across all your repos: sub-millisecond search, warms itself on first use, and self-heals on edits.

Definitions come from [Tree-sitter](https://tree-sitter.github.io/) for Ruby, Rust, Go, Python,
TypeScript, and JavaScript.

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
rq <query> -k/--kind KIND   # restrict to kind: class|module|method|function|struct|enum|trait|constant|type|macro|variant
rq KIND <query>             # a leading kind keyword is shorthand for -k (rq class Widget)
rq Scope::name              # scope-aware: only the name defined inside Scope (or Scope#method)
rq <query> -x/--lang LANG   # restrict to language: ruby|rust|go|python|typescript|javascript
                            #   (prefix-matched; r=ruby+rust; aliases rb/rs/ts/js)
rq <query> -l/--limit N     # cap the number of results (default 10; 0 = every match)
rq <query> -a/--all-repos   # search every indexed repo (default: just the current one)
rq <query> --anchor F:LINE  # rank as if asked from that line (F:LINE[:COL])
rq <query> --show           # print the definition's source (confident match only)
rq <query> -o/--open        # open the best match in your editor
rq <query> -w/--web         # open the best match on GitHub, pinned to a pushed sha
rq --symbols FILE           # outline a file's definitions, in line order
rq --index [PATH]           # index a repository (incremental; safe to re-run)
rq --index --path DIR       # seed the index with a subtree first (big monorepos)
rq --drop [PATH|IDENTITY]   # remove a repo's index (opposite of --index)
rq --status                 # indexing coverage per known repository
rq --usage                  # searches per day, by caller and flags
```

## Opening results

`rq -o <query>` jumps to the best match in your editor. On a terminal with
several matches it asks you to choose; otherwise it takes the top hit. The
launcher is, in order:

1. `RQ_OPEN`, a command template: `{file}` is the absolute path, `{line}` the
   line, `{}` both as `path:line`. A template with none of them gets `path:line`
   appended as its last argument.
2. VS Code (`code`).
3. `$VISUAL`, then `$EDITOR`.
4. Nothing found: rq prints `path:line`.

```sh
rq -o Widget                               # open the top match
RQ_OPEN='vim +{line} {file}' rq -o Widget  # runs `vim +12 /abs/path/widget.rb`
RQ_OPEN=subl rq -o Widget                  # runs `subl /abs/path/widget.rb:12`
```

`rq -w <query>` does the same in the browser. It opens the match on the repo's
git host (`https://<remote>/blob/<sha>/<file>#L<line>`), pinned to a commit so
the link survives the branch moving: HEAD, or if HEAD isn't pushed yet, the
newest pushed commit in its history. A result from another repo (`-a`) links to
that host's default branch, since rq doesn't know what's checked out there. The
launcher is `$BROWSER`, then `open`/`xdg-open`, else rq prints the URL.

For an interactive fzf picker, `script/rq-open` is a small wrapper around `rq`.
[docs/EDITORS.md](docs/EDITORS.md) covers it, the VS Code extension
(Cmd-click Go to Definition) and Neovim.

## Ranking from where you are

`--anchor FILE:LINE[:COL]` tells rq where the question comes from — an editor's
cursor, or the file an agent is reading. Definitions in the classes and modules
enclosing that line rank first, then the same file and nearby directories.

```sh
$ rq 'valid?' -l 2
activemodel/lib/active_model/validations.rb:342  method valid? · ActiveModel::Validations
activerecord/lib/active_record/validations.rb:69  method valid? · ActiveRecord::Validations

$ rq 'valid?' --anchor activerecord/lib/active_record/validations.rb:48 -l 1
activerecord/lib/active_record/validations.rb:69  method valid? · ActiveRecord::Validations
```

It reorders and never filters, so it's safe whenever you know the file. `FILE`
is relative to the current directory; `COL` is accepted and ignored. rq records
no inheritance, so an inherited method gets no credit from the enclosing class.
The VS Code extension passes `--anchor` on every click.

## For agents / scripts

`-j/--json` (array) and `-J/--ndjson` (one object per line) are the structured
surface for editors, scripts, and AI agents. Every command honors them, not just
search.

### Result fields

A search result, a `--show` result and a `--symbols` row share one shape. A field
that doesn't apply is **omitted**, never `null`.

| Field | Present | Meaning |
| --- | --- | --- |
| `name` | always | The symbol's name. |
| `kind` | always | `class`, `module`, `method`, `function`, `struct`, `enum`, `trait` or `constant`. |
| `language` | always | `ruby`, `rust`, `go`, `python`, `typescript` or `javascript`. |
| `file` | always | Path relative to `root`. |
| `root` | when rq knows a checkout for the repo (always for `--symbols`) | Absolute checkout root. Per result, because `-a` spans repos: join `root` and `file` to read it. |
| `line` | always | 1-based first line of the definition. |
| `end_line` | when known | Last line: `line..=end_line` is the whole definition. |
| `parent` | when nested | The enclosing scope, e.g. `ActiveRecord::Migration`. |
| `visibility` | when the language expresses one | `public`, `crate`, `private`, `protected` or `local` (a function nested in another). |
| `repo` | always | Repo identity: `github.com/org/repo`, or `local:/abs/path`. |
| `source` | search | `index`, or `live` when the result came from a live scan of files on disk because no index pass has finished for this directory yet: one rq doesn't track, or a repo asked with `--no-wait` before its first index (or after `--drop`). The hit is real; only its ranking is provisional. See [Staying current](#staying-current). |
| `confidence` | search | 0–1: match quality × how far it leads the runner-up. Near 1 means take it; about 0.5 means it's level with the next result, a coin flip. |
| `features` | search | The scoring signals that fired, strongest first. |
| `signature` | when the line is non-empty | The definition's first source line, trimmed. |
| `body` | `--show`, confident match | The full `line..=end_line` source. |
| `declarations` | when more than one | How many places declare this name (a reopened module, `impl` blocks across files), folded into one result. |
| `also_in` | with `declarations` | `file:line` of the other declarations. |
| `total` | search | Matches the window was drawn from, before `--limit`. |
| `explain` | `--explain` | Feature name → score contribution, in whole points. |
| `query` | batch mode | The stdin line this row answers. |

### Misses and exit codes

A miss is one `{"status": …, "query": …}` object instead of results:

| `status` | Exit | Meaning |
| --- | --- | --- |
| `no_match` | 1 | Definitive: nothing by that name. |
| `scope_not_found` | 1 | Nothing in the scope you named; `found_in` says where the name does live. |
| `warming` | 2 | The index is incomplete; retry. Mostly with `--no-wait`, since otherwise a cold repo blocks until it can answer. |
| `interrupted` | 2 | Indexing was stopped (Ctrl-C) before it could answer; run again. |

A match exits `0`. Every miss is non-zero, so `rq … && …` reads as "found
something".

### Errors

When a run fails under `--json`/`--ndjson` — a bad flag or value, an empty
query, an index that can't be opened, a `--symbols` file that doesn't exist —
stdout carries one object instead of results:

```json
{ "error": "rq: unknown --kind \"widget\" (class, module, method, function, struct, enum, trait, constant, type, macro, variant)", "kind": "usage", "code": 64 }
```

`kind` is stable, and `code` is the exit code. The message also goes to stderr.
Errors take codes from `sysexits(3)`, so none is ever mistaken for a miss or a
retry:

| `kind` | Exit | Meaning |
| --- | --- | --- |
| `usage` | 64 | The command line is wrong: an unknown or conflicting flag, a bad value, an empty query — including flags before `--json`. Fix the command; it won't succeed on retry. |
| `not_found` | 66 | A file the command names doesn't exist (`--symbols`). |
| `no_remote`, `launch` | 69 | Nothing to hand off to: `-w` on a repo with no git host, or an editor or browser that won't start. |
| `internal` | 70 | rq couldn't render its own output — a bug. |
| `database`, `index` | 74 | The index can't be opened, read or written. |

Every code means one thing, so a script can branch on the number: `1` is
absent, `2` is ask again, and anything else is an error, which `kind` names.
`rq --help` lists the same table.

### Batch mode

Pipe queries on stdin, one per line, with `-J`. rq resolves the repo and opens
the index once instead of per query, which on a large repo is most of what a
lookup costs:

```sh
printf 'HashWithIndifferentAccess\nNoSuchThing\n' | rq -J -l 1
```

```json
{"query":"HashWithIndifferentAccess","name":"HashWithIndifferentAccess","kind":"class",…}
{"query":"NoSuchThing","status":"no_match"}
```

Each row carries its `query`; a miss row carries its own `status`. The run exits
`0` if any query matched, non-zero only if every one missed. `--json` can't frame
several result sets, so batch needs `-J`, and `--show`/`--open`/`--web` don't
apply. A cold repo is indexed up front, within the `--wait` budget, before the
first answer.

### Reading the source

`--show` locates and reads in one call. When the top match's confidence is at
least 0.85 it prints the full `line..=end_line` span (`body` in JSON); otherwise
it prints the ranked list, so it never dumps a definition it isn't sure about.

```sh
rq ActiveSupport::HashWithIndifferentAccess --show   # confident: prints the class
rq perform --show                                    # 122 candidates: prints the list
```

### Other commands

`rq --status --json` emits coverage rows (`repo`, `status`, `files`, `symbols`).
`status` is `complete`, or `warming` while the index is partial — a first index
still running, or a pass cut short that the next query continues. `files` and
`symbols` count what's indexed so far. A dropped repo is gone from `--status`
until a query or `--index` starts rebuilding it, and then reads `warming`.
`rq --index --json` emits this run's counts (`files_added`, `symbols_added`)
plus the repo's totals. `rq --drop --json` reports what it removed (`repo`,
`files`, `symbols`, `dropped`). Single-result commands emit one object.

These commands exit `0` whenever they ran, including when there's nothing to
report: an empty `--status`, an `--index` that found nothing new, or a `--drop`
of a repo that isn't indexed (`"dropped": false` tells you). They exit non-zero
only with an error code from the table above. `--usage` is the exception: with
nothing recorded yet it exits `1`.

### Waiting on the index

`--no-wait` answers from whatever's already indexed instead of waiting on a
warming repo — say, right after a branch switch on a huge repo. A miss reports
`warming` (exit 2) so you can retry; warming continues in a detached background
process. On a repo with no index yet it answers from a quick live scan
(`"source": "live"`), and misses only what that scan can't reach. On a small or
fully indexed repo it answers the same as without the flag: the difference
shows only while a large index is being built.

`--wait <dur>` caps how long a query may wait: `50ms`, `2s`, `1m`, or bare
seconds (`--wait 0` is `--no-wait`). It overrides `RQ_WAIT_BUDGET_MS` (default 1
minute) for that call.

## File outline

`rq --symbols <file>` lists every definition in a file, in line order — a
structural outline, not a ranked search. Honors `-k/--kind` and `-x/--lang`, and
emits the same fields as a search result, minus the scoring ones.

A file with no definitions — none at all, none of the kinds asked for, or in a
language rq doesn't parse — is a miss like a search's: `{"status": "no_match"}`,
exit 1, so `rq --symbols f && …` reads as "it defines something". A file that
doesn't exist is an error (`not_found`, 66).

```sh
rq --symbols src/search/score.rs
rq --symbols src/store/mod.rs -k struct,enum --json
```

## Ranking

A query is matched and scored by an additive sum of named signals, and
`--explain` shows the sum for each result:

```sh
$ rq Store --explain -l 2
src/store/mod.rs:84  struct Store
    pub(crate) struct Store {
    confidence 0.90 · score 1296 = exact 1000 + case 150 + extent 11 + kind 15 + recency 120
src/cli/mod.rs:1873  method store · BranchRefresh
    fn store(self, store: &Store) {
    confidence 0.04 · score 1123 = exact 1000 + private -15 + extent 18 + recency 120
```

- **match quality** — exact > prefix > camel/underscore abbreviation > subsequence
- **visibility** — public API edges out private/protected helpers (Rust `pub`,
  Ruby `private` sections, Python `_underscore`, Go capitalization, TypeScript
  member modifiers and ESM `export`), and a function nested in another ranks
  below every same-named definition that isn't (`local`)
- **qualifier** — a scoped query (`Foo::Bar`, `Foo#bar`, `Foo.bar`) keeps only the definitions inside that scope; `Foo.new` finds the constructor, or the class itself when it inherits one. A package or module scope is read off the file's path, so `hugolib.HugoSites`, `models.QuerySet` and `mpsc::Sender` work too
- **path** — the query also matches the file's name
- **current repo** — results are scoped to the repo you're in by default
  (`-a`/`--all-repos` to search every indexed repo)
- **recency** — symbols in recently edited or committed files
- **branch** — on a feature branch, files you're changing vs the trunk (and
  their directory neighbors) — where you're most likely working
- **anchor** — with `--anchor`, definitions enclosing that line (`enclosing`),
  then those in the same file and nearby directories (`proximity`)

Fewer, better, ranked results are the goal — not completeness.

## Staying current

You rarely run `rq --index` by hand. The first query in a git repo warms the
index, files you're changing on this branch first, and once your answer prints
a detached, low-priority process finishes the sweep in the background. A
**cold** repo is the exception: the first query indexes until it can answer
rather than report a false miss. On a terminal, a progress line appears if that
takes longer than half a second, and Ctrl-C stops it. It's a one-time cost — the
index persists and self-heals as you search, re-reading edited files and
reconciling added and removed ones.

A non-git directory isn't warmed on a stray query, but `rq --index <dir>` tracks
it like any repo under a `local:<path>` identity; otherwise rq live-scans it, so
it still answers at zero coverage. A `--no-wait` query live-scans a git repo
whose first index hasn't finished the same way. A live answer is marked
`"source": "live"` in JSON (text output doesn't mark it), `-v` notes the files it scanned and how long it took against its budget
(`RQ_FALLBACK_BUDGET_MS`, default 250 ms), and `--usage` counts these answers
apart. The index is a SQLite file at `$RQ_DB`
(default `~/.local/share/rq/rq.db`; an empty `RQ_DB` means the default).
`RQ_DB` must be an absolute path to a file: a relative one would resolve
against each command's working directory and split the index, so rq refuses it
with a usage error (exit 64), as it does a directory, or a default path under an
unset or relative `HOME`.

## Shell completions

```sh
rq --completions <shell>        # bash, zsh, fish, elvish, powershell
```

Homebrew installs bash/zsh completions automatically.

## Using with Claude Code

`rq` ships with a Claude Code skill (`claude/rq-skill.md`) so Claude reaches for it when locating a definition instead of grepping the tree. Install the marketplace plugin, which updates itself and brings the sibling skills:

```
/plugin marketplace add dpep/claude
/plugin install code@dpep
```

Or copy the one file:

```sh
mkdir -p ~/.claude/skills/rq
cp claude/rq-skill.md ~/.claude/skills/rq/SKILL.md
```

The plugin is the better default; [`claude/INSTALL.md`](claude/INSTALL.md) covers when it isn't.

## Performance

The in-process search pipeline measures p50 ~160 µs, max < 0.25 ms on a mid-size
library (a few hundred symbols) — microseconds against a 50 ms first-answer
budget. Benchmark your own tree: `make bench REPO=/path/to/repo`.

## Scope

rq indexes **definitions** — classes, modules, methods, functions. It does
**not** do call graphs, type inference, reference tracking, inheritance, or LSP
features. It's built for many repositories and millions of symbols, and never
assumes everything belongs to one project. Repository identity is normalized
from the git remote (`github.com/org/repo`), falling back to
`local:/absolute/path`.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design.

## License

[MIT](LICENSE.txt) © Daniel Pepper.
