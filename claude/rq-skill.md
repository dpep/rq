---
name: rq
description: Find where a symbol is defined with the `rq` code-navigation CLI (Ruby, Rust, Go, Python, TypeScript, JavaScript). Use when locating a definition by name — "where is the X class / Y method / Z function defined", "find the definition of …", "jump to …", or any "where is <name>" about code; when getting oriented in an unfamiliar codebase, where you know roughly what a thing is called but not where it lives; and `rq --symbols <file>` to outline a file's definitions before reading it. Prefer over grep/rg for symbol navigation — it ranks the definition you meant first instead of listing every textual hit. Not for free-text/content search, and not for finding code by what it does (use semantic search or rg).
---

# rq — find the definition

`rq` is a code *navigation* engine: given a name, it returns the most likely
definition first, instead of every textual match. Reach for it whenever the
question is **"where is this defined?"** — a class, module, method, function,
struct, enum, trait or constant, in Ruby, Rust, Go, Python, TypeScript or
JavaScript.

rq finds **names**, not behaviour. If you know what the code does but not what
it's called ("where do we retry failed jobs"), use semantic search (`contour`)
if it's installed, or `rg` for text.

The names below (`Widget`, `WidgetProcessor`, `Account`) are placeholders;
substitute your own.

## Use it like this

Always ask for JSON:

```sh
rq <name> --json
```

Reading the definition rather than just locating it? Use `--show`. When the top
match's confidence is at least 0.85 (before a `warming` result scales it down
for the share read) it adds a `body` field with the full source; otherwise it
returns the ranked list. It also tells rq which definition you
used, which is how ranking improves for this repo — prefer it over a search
followed by a separate file read:

```sh
rq WidgetProcessor --show --json
```

When you're reading a file and look up a name used in it, pass where you are
with `--anchor` (see below). It's the single biggest ranking win you can give
rq.

## Reading a result

```json
{ "name": "WidgetProcessor", "kind": "class", "language": "ruby",
  "file": "app/services/widget_processor.rb", "root": "/home/me/code/app",
  "line": 2, "end_line": 6, "parent": "Billing", "visibility": "public",
  "repo": "github.com/org/app", "confidence": 0.9,
  "features": ["prefix","recency","path","extent","kind"],
  "signature": "class WidgetProcessor", "total": 1 }
```

- `file` is relative to `root`, the absolute checkout it came from. Join the two
  to read it, even under `-a/--all-repos`, where results span repos.
- `line`/`end_line` bound the whole definition — read exactly that span, not the
  whole file.
- `signature` is the definition's first line, so you often don't need to open
  the file to confirm a match.
- `confidence` (0–1) is how sure rq is this is the one you meant. Near 1.0, take
  it; about 0.5 means it's level with the next result, a coin flip. A low value or
  several close results, disambiguate (add a kind, scope, or path).
- `total` is how many matches the results were drawn from, before `-l`.
- `source` is `live` when rq answered from a bounded scan of files on disk
  because nothing is indexed there yet — a directory outside a git repo, or a
  repo asked with `--no-wait` before its first index finished. `index`
  otherwise. The hit is real; its ranking is provisional. In a repo it carries
  `warming` and a confidence scaled like an indexed hit's. Asking outside a
  repo often? `rq --index <dir>` once.
- `warming` (`{read, of, interrupted, phase, phase_secs, hint}`) means the
  repo's index is still being built: `read` of `of` files are in. `phase` is
  `reading`, or `finishing` while `read` stands still at the end of a pass (not
  hung); `phase_secs` is how long it has been in that phase. Another definition may not be
  indexed yet, and `confidence` is scaled down to match (to 0 when `of` is
  missing: outside git nothing counts the tree). Use the answer; if it
  matters, ask again once indexing finishes (`hint` says how).
- Fields that don't apply (`parent`, `visibility`, `end_line`, `singleton`, …) are omitted,
  never `null`. `declarations` and `also_in` appear when one name is declared in
  several places (a reopened module) and rq folded them into one result.

## Misses and errors

On a miss, JSON is one `{"status": …, "query": …}` object, not results:

- `no_match` (exit 1) — definitive. Fall back to `rg`. Under `-a`, an
  `incomplete` list names checkouts not fully indexed, which it can't speak
  for; searching in one indexes it.
- `scope_not_found` (exit 1) — nothing inside the scope you named; `found_in`
  says where the name does live. Re-ask with that scope.
- `warming` (exit 2) — index incomplete; retry. Mostly after `--no-wait`;
  otherwise rq indexes a cold repo before answering. Its `warming` field says
  how far indexing got. When it found something a
  file not read yet could beat, the matches so far are in `provisional`: a
  lead, not an answer. (Under `-a`, a match from another checkout nothing is
  indexing answers instead, with `warming.interrupted` and the `rq --index`
  that finishes it: asking again wouldn't change it. One another process is
  still indexing holds back every checkout's match, and the search waits on it.)
- `interrupted` (exit 2) — indexing was stopped; run again.

An error is JSON too, on stdout: `{"error": "…", "kind": "usage", "code": 64}`.
Check for an `error` key before reading results. `code` is the exit code, and
each means one thing:

- `64` `usage` — fix the command (an unknown `-k`, a bad `--wait`, `--json` on
  piped queries); don't retry it unchanged.
- `66` `not_found` — the file you named (`--symbols`) doesn't exist.
- `69` `no_remote`/`launch` — nothing to hand off to (`-w` with no git host).
- `70` `internal` — a bug in rq.
- `74` `database`/`index` — the index can't be opened, read or written.

## Scope when you know more

- **Position.** Reading `app/models/widget.rb` and want the `valid?` called on
  line 3? `rq 'valid?' --anchor app/models/widget.rb:3 --json`. rq then prefers
  the definition in the enclosing class/module, then the same file and nearby
  directories; a trailing `:COL` is fine. On real call sites in rails and
  discourse it put the resolved definition first 79% of the time, against 48%
  without. It only reorders, so pass it whenever you know the file. It can't see
  inherited methods, and a call on another object (`other.save`) is better asked
  as `rq Other#save`.
- **Fuzzy.** Abbreviations and prefixes work: `rq widgetproc`, `rq wp`.
- **Scope.** `rq Billing::WidgetProcessor`, or `rq WidgetProcessor#perform` (or
  `.perform`) for a method inside a class — rq keeps only definitions in that
  scope, so use it when the surrounding code tells you the enclosing
  class. A scope that isn't a class matches the path instead: a Go package
  (`hugolib.HugoSites`), a Python module (`django.db.models.QuerySet`), a Rust
  module (`mpsc::Sender`). `rq Account.new` finds the constructor (`initialize`, `__init__`,
  `constructor`), or the class itself when the constructor is inherited.
- **Kind.** `rq save -k method`, or the shorthand `rq method save`. Kinds are
  `class`/`module`/`method`/`function`/`struct`/`enum`/`trait`/`constant`/
  `type`/`variant`/`macro`/`field` (shortcuts `c`/`mod`/`m`/`f`/`s`/`e`/`t`/`const`/`v`,
  comma-separable: `-k m,f`; `interface`, `alias`, `member` and `property` work too).
  The kinds are shared across languages: a Go or TypeScript interface is a
  `trait`, a named type or alias (`type HandlerFunc func(…)`, `type Size = …`)
  a `type`, an enum member (Rust, TypeScript, a Python `Enum`) a `variant`, and
  a component wrapped in `memo(…)` a `function`. `-k type` also covers
  `struct`. A Python def nested in another is a `function` with `visibility`
  `local`, ranked below any same-named definition outside a function.
  A `field` is a named slot a type declares, parented by it: a Rust or Go
  struct field (an embedded Go field goes by its type's name), a TypeScript
  class or interface property, a Python class attribute (a dataclass or model
  field; not `self.x = …`). Fields rank below any same-named method, function
  or type, so ask with the owner to land on one: `rq Hit.also_in`,
  `rq User::email`, `rq -k field name`.
- **Directory.** `rq save app/models` (rg-style trailing path, repeatable) or
  `--path`.
- **Count.** `-l 1` for just the best hit, larger to survey, `-l 0` for every
  match.
- **Repo.** Results come from the current repo by default; `-a`/`--all-repos`
  searches every repo you've indexed (so `no_match` means absent *here*).
- **Wildcards.** `*` (any run) and `?` (one char) — **quote them** so the shell
  doesn't glob: `rq 'Widget*proc'`. A lone trailing `?` is part of the name, so
  `rq 'valid?'` finds a Ruby predicate. `::`, `#` and `.` need no quoting.

```sh
rq save -k method app/models --json
rq Widget -l 1 --json          # just the top hit
```

## Many lookups at once

Looking up a list of names? Pipe them on stdin, one per line, with `-J`. rq
resolves the repo and opens the index once instead of per query — roughly 2x
faster on a mid-size repo and 4x on a large monorepo, where it matters most.

```sh
printf 'WidgetProcessor\nAccount\nNoSuchThing\n' | rq -J
rq -J < names.txt
```

Every row carries the `query` it answers, and a miss still reports
`{"query": …, "status": "no_match"}` rather than vanishing. The run exits 0 if
any query matched, non-zero only if all missed — so read each row's `status`,
not the exit code. Needs `-J` (`--json` can't frame several result sets);
`--show`/`--open` don't apply. `--anchor` does.

## Outline a file

`rq --symbols <file>` lists a file's definitions in line order — a structural map
(with `line`/`end_line`) to read *before* opening the file, so you jump to the
right span instead of scanning the whole thing.

```sh
rq --symbols app/models/widget.rb --json
rq --symbols app/models/widget.rb -k method --json   # just the methods
```

A class method carries `"singleton": true` — `def self.x`, anything inside
`class << self` (its `private` section too), a `module_function`, a Python
`@classmethod`/`@staticmethod`, a TS/JS `static` member, a Rust associated fn
without `self`. It is omitted on instance methods, so a private method with
`singleton` is a private class method, not a private helper.

## Installing / updating the binary

If `rq` isn't on PATH, install it, then retry the search:

```sh
brew install dpep/tools/rq      # macOS/Homebrew (builds from source; needs Rust at build time)
```

No Homebrew? Build from source (needs the Rust toolchain):

```sh
cargo install --git https://github.com/dpep/rq
```

To update: `brew upgrade dpep/tools/rq` (or re-run the `cargo install --git …`
line). `--anchor` and the `root` field need rq 0.53 or newer. Source and issues:
<https://github.com/dpep/rq>.

## Notes

- rq indexes the current git repo on first use; no setup needed.
- Run from inside the target repo (or set the subprocess working directory) —
  rq resolves the repo from its cwd.
