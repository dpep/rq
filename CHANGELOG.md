# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

## Unreleased

A tree's existing index may hold files a background warm read that an ignore
rule excludes (in next.js, 721 files of `packages/next/src/compiled/**`). They
are dropped at the next full sweep, which a commit, checkout or pull starts;
run `rq --index` once to drop them now.

The first run after upgrading re-reads every TypeScript and JavaScript file
once, in the background, for the visibility below; results answer from the
old rows meanwhile. Nothing to run by hand.

### Changed
- **Asked from inside a test or example tree, `--anchor` holds back a private
  TypeScript or JavaScript definition in a sibling file**, which the anchor
  can't call: such privacy stops at the file. A Go unexported name or a
  private Rust item beside the anchor is still lifted, a Go package and a
  Rust module's children sharing its directory (DECISIONS D61).

### Fixed
- **A background warm and `rq --index` read the same files.** A warm listed
  files with git and read tracked ones that `.gitignore`, `.ignore` or
  `.git/info/exclude` excludes, which `rq --index` skips, so the set flipped
  between passes; and it missed a submodule's files, which `rq --index` reads.
  Both now read what the index's walk reaches, submodules included. An edited
  file that an ignore rule excludes no longer leaves a search reporting
  "still indexing" forever (DECISIONS D62).
- **A declaration a CommonJS module exports reads public**, not private:
  `function View() {}` with `module.exports = View`, `const keep = …` with
  `exports.keep = keep`, and each shorthand or name-valued key of
  `module.exports = { a, b: c }` no longer take the private-visibility
  penalty, as an ESM `export { … }` list already didn't. An export made inside
  a function or an `if` doesn't count.

## 0.60.6 — 2026-10-07

The first run after upgrading re-reads every TypeScript and JavaScript file
once, in the background, for the definitions below; results answer from the
old rows meanwhile. Nothing to run by hand.

### Added
- **CommonJS definitions are indexed.** A top-level `exports.x = function`,
  `module.exports.x = …`, `X.prototype.y = function`, `obj.y = () => …` or
  `obj.Y = class …` defines `x` (a function) or `y` (a method of `X` or
  `obj`), and each function-valued key of `module.exports = { … }` is a
  function. Each target of a chain (`res.set = res.header = function`) or a
  sequence counts, and `X.prototype = { m() {} }` gives `X` its methods. In
  express `rq json` found nothing and `rq listen` an example app; they now
  answer `res.json` and `app.listen` (#30, DECISIONS D57). Patches to host
  globals (`window.onload =`, `globalThis.fetch =`, `self.onmessage =`), an
  anonymous `exports.default`, and a call's result (`exports.methods =
  METHODS.map(fn)`) aren't definitions.

### Changed
- **`--anchor` prefers definitions the anchor file's language can refer to.**
  Asked from a `.tsx` file in a repo that also holds Rust, a TypeScript or
  JavaScript definition now ranks ahead of a Rust one of the same name; asked from
  Ruby, a Ruby one does. A new `reachable` feature in `--explain`, present only
  when some candidate the search considered is in a language the anchor can't
  refer to (counted before `--lang` or the relevance gate narrow what's
  shown), so scores and confidence in a one-language repo are as without
  `--anchor`; nothing is filtered out, and queries without `--anchor` are
  unchanged.
- **Asked from inside a test or example tree, `--anchor` treats that tree's
  definitions as the context**, not secondary: the test/example penalty is
  lifted for what the anchor can call in its own app or suite (the innermost
  test or example directory and one level below, `examples/blog/`), as it
  already was in the anchor's own file. A private definition there is lifted
  only in the anchor's own directory, where a Go package's unexported names
  live.

### Fixed
- **A declaration exported by a later `export { … }` list or `export default
  name` reads public**, not private: `const Main = styled.main…` followed by
  `export { Main }` no longer takes the private-visibility penalty. An enum
  exported that way takes its variants with it.
- **`export let` and `export var` are indexed**, as constants, like `export
  const`: react's `rq renderStartTime` found nothing (#31). A module's own
  unexported `let`/`var` stays out (DECISIONS D56).

## 0.60.5 — 2026-10-05

### Changed
- **`--drop --json` omits `root` rather than sending `null`** when it dropped a
  whole repo by identity or found nothing at a path that doesn't exist, as the
  README has always said fields that don't apply are. `--index --json` likewise
  omits `files`/`symbols` in the rare case the checkout can't be counted. A
  reader that took `null` as "no root" should treat a missing key the same.
- **A batch whose lines fail with different error codes exits with the
  highest**, where it took the first.

### Fixed
- **A FIFO or device at a source path no longer hangs rq.** A FIFO where a
  tracked `.rb` used to be blocked a search, and the background warm it left
  behind, until something wrote to the FIFO. rq now reads regular files only,
  and treats a file over 64 MiB as it does a binary one: held, not parsed, so
  its definitions are skipped and a search won't find them. `--symbols` on
  such a file says it's over the size cap (exit 64, `kind: "usage"`) rather
  than "no symbols".
- **A file or checkout rq can't stat is no longer forgotten.** A permission
  error on the way to a file (or to a whole checkout's root) read as "deleted",
  so an index pass — a background warm or `rq --index` alike — dropped the
  file, or every file under a directory it couldn't list, and `--status`, `-a`
  or any pass dropped the checkout. Only a path known to be absent is
  forgotten now.
- **A symlinked source file is indexed by no pass, rather than by some.** A
  background warm read a tracked symlink while `rq --index` and the live walk
  skipped it, so its definitions came and went with whichever ran last. Its
  target, when it's in the tree, is still found. Run `rq --index` once to drop
  symlinked files an earlier version indexed. Likewise a file whose name
  isn't valid UTF-8 (Linux only; macOS refuses such names) is skipped
  everywhere: a lossy key couldn't lead back to the file.
- **A branch file with a non-ASCII name gets the branch boost.** git quoted
  `café.rb` as `"caf\303\251.rb"`, which matched no indexed file.

## 0.60.4 — 2026-10-05

Results and `--symbols` rows may carry a new field, `singleton`. The first run
after upgrading re-reads every Ruby, Rust, Python, TypeScript and JavaScript
file in the background (Go is untouched); results answer from the old rows
meanwhile, without the field.

### Added
- **A class method says it is one: `"singleton": true`.** `--symbols` showed a
  method inside `class << self` exactly like an instance method, so a private
  class method read as a private helper (#29). It is now set for Ruby's `def
  self.x`, `def Const.x`, everything inside `class << self` and module
  functions; Python's `@classmethod`/`@staticmethod`; a TypeScript/JavaScript
  `static` method, accessor or field; and a Rust associated fn without `self`.
  Omitted when false, on search results, `--show`, batch rows and `--symbols`
  alike; text prints `singleton method` (DECISIONS D55).
- **`--lang` takes any extension a language reads.** `-x mjs`, `cjs`, `mts`
  and `cts` now select JavaScript and TypeScript, as `js` and `ts` did.

### Fixed
- **A class method and an instance method of the same name are two results.**
  `def self.call` and `def call` in one class folded into a single result with
  `declarations: 2`.
- **Visibility set after the def is read.** `private :x`, `protected :x`,
  `public :x`, `private_class_method :x` and `public_class_method :x` — with
  strings, arrays, `*%i[…]` or a wrapped `def self.x` — now change the
  methods they name, in the same body or an earlier reopening in the same
  file — not a same-named def inside a block (`Struct.new do … end`).
  An `alias`/`alias_method` takes its original's visibility, as Ruby does,
  rather than its section's (DECISIONS D55 addendum).
- **A Rust fn with a typed receiver is a method.** `self: Pin<&mut Self>`,
  `self: Box<Self>` and the like — every `Future::poll` — read as a `function`.
- **`class << self` starts public.** A `private` earlier in the class body made
  every method in a later `class << self` read as private.
- **A background warm run by another user is left alone.** A second warmer
  read another user's live warm as dead and took over its lock, so two warmed
  the same checkout at once.
- **A damaged name index no longer crashes a search.** A truncated row in the
  index fuzzy recall reads made the search panic; the repo is now read from
  its symbols for that search and its name index rebuilt on the next.
- **A miss in an indexed directory outside git is a miss (exit 1).** With no
  git to ask whether the tree had changed, rq assumed it had: every miss in an
  `rq --index`ed non-git directory answered `warming` (exit 2), forever, with
  nothing left to index. rq now compares the tree with the index (files added,
  removed, or modified since), and says `warming` only when something did
  change. The same goes for a git repo with nothing committed or staged,
  where an edit used to go unnoticed. A file rq can't open is left out of the
  comparison until it can, so one no longer keeps every miss at `warming`.
- **A source file that isn't UTF-8 is indexed.** A Latin-1 comment made rq
  skip the whole file; its definitions are now found, the stray bytes read as
  `�`. A binary file with a source extension (a NUL in its first 8 KB, such
  as an MPEG-TS video named `.ts`) is still skipped: it holds no definitions.
- **A live scan spends its budget reading files.** A `--no-wait` search of a
  checkout still being indexed asked git for the repo's identity and file list
  twice, inside the scan's 250 ms; on a busy machine that could leave no time
  to find the answer. Both are now resolved once, before the scan starts.
- **A batch with nothing found says "retry" when a retry could help.** Piped
  queries that all came up empty exited with the last query's code, so a
  batch ending in a miss (1) hid an earlier query that only found the index
  still warming (2). It now exits 2 when any query could still hit, 1 only
  when every query is a definite miss, and an error's own code before either.
- **A batch notices an edit.** Piped queries against a fully indexed repo
  never asked whether the worktree had changed, so a method added since the
  last index was a definite miss (exit 1) on every run — what `gqls
  --resolve` saw. A batch now asks once, before its first query: it reads the
  edit in before answering, or with `--no-wait` answers a miss `warming`
  (exit 2) and leaves the reading to the background.
- **A batch no longer indexes a directory rq doesn't track.** Piped queries
  outside git indexed the directory they ran in, where a single search only
  reads it live; only `rq --index` opts a non-git directory in.
- **A file deleted without `git rm` no longer keeps misses `warming`.** Until
  the delete was staged or committed, the index held on to the file, and
  every miss in the repo answered `warming` (exit 2).
- **`--no-wait` outside git, in a directory rq doesn't track, answers a miss
  as a miss (exit 1).** It said "indexing stopped part-way (0 files read)"
  (exit 2) on every run, though nothing was indexing and the plain search
  answered 1.
- **An empty `--lang` is a usage error (exit 64), as an empty `--kind` is.**
  `-x ""` and a trailing comma (`-x ruby,`) selected every language.
- **`--symbols FILE` outlines a file in a directory rq doesn't track.**
  Outside git and never `--index`ed, it printed "no symbols" (exit 1) for any
  file; it now reads the file, as it does in a repo.
- **`--usage` with nothing recorded says so on stderr**, as an empty search
  and an empty `--symbols` do, so stdout carries only data.
- **Two modes at once are a usage error (exit 64).** `--usage --drop`,
  `--symbols FILE --usage`, `--warm --usage` and the like ran whichever mode
  rq checked first and ignored the other. `--show`, `--open`, `--web` and
  `--anchor` are refused beside any mode, `--usage` and `--completions`
  included.

## 0.60.3 — 2026-10-01

A result from an index still being built, a `warming` miss and a `--status` row
for a checkout being indexed may carry two new fields, `phase` and
`phase_secs`; nothing else in their shape changes.

### Added
- **`warming.phase` says what indexing is doing, so a stalled count reads as
  progress.** At the end of each pass (every 50,000 files) the files-read count
  stands still while rq rebuilds its name index and reads commit times; a script
  polling `read` could take that for a hang. `phase` is `reading` or
  `finishing`, and `phase_secs` how long it has been in it, on results,
  `provisional` answers, `warming` misses and `--status` rows (DECISIONS D54).
  Text output adds "finishing a pass (N s)".

### Fixed
- **A live-scan result from a repo being indexed is on the same confidence
  scale as an indexed one.** Asked with `--no-wait` before a repo's first index
  finished (or after `--drop`), results from the bounded live scan kept their
  unscaled confidence and carried no `warming`: a first query could answer
  exit 0 at 0.67 with nothing indexed. They now carry the checkout's `warming`
  and are scaled by `read / of` like every other result in the answer — 0
  until the index holds something. Outside a repo, where nothing is indexing
  and the scan is the answer, nothing changes. A `warming` miss in that window
  now carries `warming` too (`read: 0`), where it had none.

## 0.60.2 — 2026-10-01

Scripts that read exit `2` as "nothing found yet" keep working; the `warming`
object may now carry the matches so far, under `provisional`. A result from an
index still being built now says so, in a new `warming` field.

### Fixed
- **A search no longer answers from a half-built index as if it were whole.**
  While a repo is being indexed (a first run, after `--drop`, or the rebuild an
  upgrade starts), rq used to return the first exact or prefix match it had,
  with normal confidence: `User` could answer `UserFieldsController`. Now it
  waits for a match no unread file can beat on its name: an exact match in the
  capitals you typed, or any exact or prefix match once every file containing
  the name has been read. If the wait ends first (`--no-wait`, `--wait`, the
  1-minute default for scripts), it reports `warming` (exit 2) with what it
  found as `provisional` (DECISIONS D52).
- **`--status` no longer reads a checkout being indexed as `unindexed 0
  files`** while the first pass is still reading for a search's name; it says
  `warming`, with how many files the tree spans (`of`).
- **`rq --index` no longer fails with "database is locked" (exit 74)** while
  another rq process is building the same index; it waits its turn, for up to
  30 seconds, and at a terminal says what it's waiting for.
- **An untracked file `rq --index` read is no longer forgotten** when
  background indexing later completes the checkout (or reindexes it after an
  edit): it stays found while it's on disk.
- **The wait follows whoever is indexing.** A search no longer stops waiting
  when its own indexing ends while another rq process is still building the
  index. At a terminal it shows which checkout it's waiting on, and Ctrl-C
  prints what it has; it stops waiting on a process that has written nothing
  for 5 seconds.
- **Background indexing of a large repo runs until the index is complete**
  (for up to 5 minutes), rather than stopping after 20 seconds and leaving the
  rest for the next search in that checkout.

### Added
- **`warming` on results from an index still being built**: `{read, of,
  interrupted, hint}`, files indexed of the files the tree spans. Their
  `confidence` is scaled by `read / of`, since another definition may not be
  indexed yet. Text output adds one line on stderr. A `warming` miss carries the
  same field, and `--status` rows for a partial checkout the same `of`.

## 0.60.1 — 2026-09-29

Nothing to do: rq now recovers on its own from an index it can't use.

### Fixed
- **A damaged index, or an upgrade that fails, is rebuilt instead of failing
  every command.** rq moves the old file aside (`rq.db.broken-<time>`, only the
  newest kept), says so in one line on stderr, and indexes again as on a first
  run; `--json` output is the usual `warming`/`not_indexed`. `rq --status`
  lists the kept copy, which is safe to delete (DECISIONS D51).
- **An older rq no longer breaks on a newer rq's index**, from the next schema
  change on. It leaves that file alone and keeps its own beside it
  (`rq.v23.db`), so a brew install and a dev build can take turns without
  rebuilding each other's index. rq 0.60.0 and older can't do this, so they
  still fail against a later schema.

## 0.60.0 — 2026-09-29

Upgrading keeps each repo's index with one of its checkouts still on disk, the
one you last searched in where rq can tell. Other worktrees and clones of the
same remote re-index on their next search, parsing only the files that differ.
The upgrade takes seconds on a large index; the first rq says so on stderr,
and others started meanwhile wait for it rather than failing "database is
locked". An older rq can't read the upgraded database (a search fails with
`no such column`); `rm` the database, or point `RQ_DB` elsewhere, to go back.

### Fixed
- **Worktrees, clones and detached checkouts of one repo no longer overwrite
  each other.** They share a remote identity, and the index kept one row per
  path, so the last checkout indexed won: once B indexed, a method added on
  branch A came back `warming` from A until a warm re-indexed A over B, and then
  B's answers flipped the same way. Each checkout now answers from its own
  files, and has its own coverage, warm state and branch boost (DECISIONS D50).
- **The first search after saving an edit answers from it** when warming runs
  in-process (`RQ_WARM_DETACH=0`): it reindexed the edit and then reported
  the miss it had before, `no_match`, so only the second search found it.
- **A file a branch switch deleted is no longer a hit** on the first search
  after the switch; the index drops it with the warm that follows.
- **`--drop --json` names a repo by its identity even when there was nothing
  to drop**, with the path it was asked about in `root`; it gave the path as
  `repo`.

### Changed
- **A file version is stored once per repo.** Identical files across checkouts
  share one parse and one set of rows, so a new worktree indexes in the time it
  takes to read and hash its files.
- **`--all-repos` spans every checkout.** A definition several checkouts hold
  at the same path is one result, whatever line each has it on, taken from the
  checkout you're in, else the newest. Checkouts deleted from disk are
  forgotten.
- **`--status` lists each checkout**, with its `root` in JSON beside `repo`;
  `--index` reports the `root` it indexed. A checkout rq knows but holds
  nothing for (a repo's other worktrees, after the upgrade) reads `unindexed`
  until a search there indexes it.
- **An `-a` miss names the checkouts it can't speak for**: `incomplete` in
  JSON lists the roots not fully indexed.
- **`--drop` inside a checkout drops only that checkout.** `--drop <repo>`
  still drops the repo, now with every checkout of it, and reports `files` and
  `symbols` summed over them as `--status` showed them.
- **Searches in a linked worktree answer about 10× sooner.** rq read git's
  state from disk only when `.git` was a directory, so in a `git worktree` it
  forked git for HEAD and recomputed the branch's changed files on every query
  (~25 ms on rails). It now follows `.git`'s pointer to the worktree's own git
  dir, as git does.

## 0.59.0 — 2026-09-28

Upgrading re-reads every indexed Rust, Go, Python, TypeScript and JavaScript
file once, in the background, so fields appear without a `--drop`.

### Added
- **Fields are indexed, as a new `field` kind** parented by their type: Rust and
  Go struct fields (an embedded Go field named by its type), TypeScript/JavaScript
  class properties and interface and object-type-alias properties, and Python
  class attributes (dataclass, pydantic, TypedDict and model fields; not
  `self.x = …`). `rq Hit.also_in`, `rq -k field email` or `rq field email` find
  one. A field ranks below any same-named method, function or type, so
  `rq name` still answers with the definition first (DECISIONS D48). `-k field`
  also takes `property` and `prop`; `kind` in JSON may now be `field`. The VS
  Code extension shows fields with the Field icon.

## 0.58.2 — 2026-09-28

### Changed
- **A bare type name answers the top-level definition first.** `rq Account` ranks
  `class Account < ApplicationRecord` above a `Billing::Providers::Account`, which
  used to tie with it and win on body length. It applies only when the same name is
  also nested somewhere; a qualified query (`Admin::Account`) or an `--anchor`
  inside the namespace still gets the nested one (DECISIONS D43).

## 0.58.1 — 2026-09-27

### Changed
- **A capital typed into a near miss, prefix or fuzzy query counts.** `COUNTESR`
  ranks `COUNTERS` over `counters`, and `Plaec` `Place` over `place`, graded by
  how many of the query's capitals the name shares. An all-lowercase query
  stays case-blind (DECISIONS D40).
- **A consonant skeleton may drop a word's first vowel.** `prsnch` finds
  `parse_anchor` and `mxncls` `MAX_ENCLOSING` (D41). The name index is rebuilt
  once per repo, on the first search after upgrading.
- **A query may skip whole words.** `braboost` finds `BRANCH_DIR_BOOST` and
  `maxbonus` `MAX_BODY_BONUS`; a name that enters every word still ranks first
  (D42). Short queries match more names, so fuzzy searches take slightly longer
  (first answer 3.3 → 3.9 ms median on rails and discourse).

### Fixed
- **rq ignores an inherited `GIT_DIR`.** It picks a repo by walking up to its
  `.git`; under `git rebase --exec` (which exports `GIT_DIR`) its git calls
  went to the rebasing repo instead, and could index or report the wrong one.

## 0.58.0 — 2026-09-27

Upgrading re-reads every indexed Python, TypeScript and JavaScript file once,
in the background, so the definitions below appear without a `--drop`.

### Added
- **TypeScript: ambient declarations are indexed.** `declare function`,
  `declare const`/`let`/`var`, `declare class`, `declare namespace`,
  `declare module "name" { … }` (a module named without its quotes, holding its
  members) and `declare global { … }` (added to the top level), with or without
  `export`, and everything in a `.d.ts`. A declaration-only API such as a
  vendored library's `.d.ts` is now found. Where a declaration and its
  implementation share a name, the implementation ranks first (`stub` in
  `--explain`).
- **Python: a class defined inside a def is indexed**, as a `class` whose
  parent is the def and whose visibility is `local`, like a nested def:
  django's `RelatedManager` (built inside
  `create_reverse_many_to_one_manager`) used to be unfindable. Its body stays
  out.

### Changed
- **`rq --help` is simpler.** Flags are grouped (narrow the search, output, waiting on
  the index, the index, debugging), each with a one-line summary under `-h`, and
  the examples match the README.
- **Overload signatures fold into their implementation.** A TypeScript
  function with overloads is one result at the implementation, with
  `declarations` counting the signatures and `also_in` listing them. More
  generally, a top-level name declared twice in one file (Rust `#[cfg]`
  alternatives) now folds the way a reopened module does.

## 0.57.0 — 2026-09-27

Upgrading re-reads every indexed Go, Python, TypeScript and JavaScript file
once, in the background, so the symbols below appear without a `--drop`.

### Added
- **Go: named types that aren't a struct or interface are indexed**, as
  `type`: `type HandlerFunc func(*Context)`, `type HandlersChain []HandlerFunc`,
  `type Celsius float64` and aliases (`type GitInfo = gitmap.GitInfo`). They
  used to be dropped, so `rq HandlerFunc` found nothing in gin.
- **Python: a `def` nested in another is indexed**, as a `function` whose
  parent is the enclosing def (`_wrapper · _multi_decorate`). Its
  visibility is the new value `local`, and it ranks below every same-named
  definition outside a function body (`local` in `--explain`).
- **Python: enum classes are `enum`s of `variant`s.** A class whose base
  visibly is an enum (`enum.Enum`, `IntFlag`, `models.TextChoices`) is kind
  `enum`, and each name its body assigns is a `variant`: `rq Color.RED`,
  `-k variant`. Lowercase members, never constants, are now found too.
- **TypeScript: enum members are indexed**, as `variant`s of their enum:
  `rq EVENT.MOUSE_MOVE` and `rq MOUSE_MOVE -k variant` find them.

### Changed
- **Python enum classes print as `enum`, and their members as `variant`**,
  where they were `class` and `constant`. `-k class` no longer lists them;
  `-k enum` and `-k variant` do.
- **TypeScript type aliases are kind `type`, not `struct`.** `type Size = …`
  now prints and serializes as `type`, like Rust's and Go's. `-k type` finds
  them as before; `-k struct` no longer does.
- **TypeScript/JavaScript: a component wrapped in a call is a `function`.**
  `const Badge = memo((props) => …)` and `forwardRef(…)` were `constant`, so
  `-k function` missed memoized components. A function literal passed first to
  any call now makes the binding a function; `memo(BadgeBase)`, which wraps a
  name, stays a constant.

## 0.56.0 — 2026-09-27

Upgrading re-reads every indexed file once, in the background: the first
search in each repo starts it, and answers from the old index meanwhile.

### Fixed
- **Generated code ranks below hand-written code.** In hugo, `rq String`
  returned stringer's `*_string.go` files ten deep. A file whose header says
  it's generated (`// Code generated … DO NOT EDIT.`, `@generated`, protoc's
  `DO NOT EDIT!`) now takes the test-path penalty (`generated` in
  `--explain`). It is still the answer when nothing hand-written shares its
  name.
- **Example, demo and docs apps rank below the library.** In excalidraw,
  `rq Excalidraw` put a Next.js example's wrapper and a docs site's scaffold
  above the exported component in `packages/excalidraw/`. Definitions under
  `examples/`, `example/`, `_examples/`, `demo/`, `demos/`, `docs/` and
  `dev-docs/` take the test-path penalty too (`example_path`).
- **A package or module scope finds its definitions.** `hugolib.HugoSites`,
  `gin.Context`, `django.db.models.QuerySet` and `mpsc::Sender` failed with
  `scope_not_found`, because only a recorded parent could answer a scope, and
  no language records its packages or modules as one. A scope the parent
  doesn't hold is now read off the file's path (`path_scope` in `--explain`):
  the repo's name, its directories and the file's stem. A parent still wins
  where one matches, and a scope's own directory over its subdirectories.
  `found_in` now names only a definition of that exact name.
- **Go's godoc receiver syntax reads as its type.** `rq '(*HugoSites).Build'`
  searched for a glob (the `*`) and found unrelated files; it now means
  `HugoSites.Build`. A scope is compared by its name, so punctuation around it
  is dropped.
- **An anchor inside a test file ranks that file's definitions.** With
  `--anchor tests/helpers/api.ts:120`, the method defined on that very line
  ranked third: the test-path penalty (−400) outweighed the anchor. The
  anchor's own file no longer takes the test, generated or example penalty.
- **An empty query says what to do instead.** `rq '' -k interface` reads as a
  request to list every interface, which rq doesn't do: it navigates to a
  name. The usage error now points at `rq --symbols FILE`, the listing it has.

### Added
- **Rust: enum variants, `type` aliases and `macro_rules!` are indexed**, as
  the new kinds `variant`, `type` and `macro`. `rq TryRecvError::Empty` and
  `rq select` now find them, and `-k variant`, `-k type` and `-k macro`
  filter to them. `-k member` is the same as `-k variant`, and `-k alias`
  is the same as `-k type`.
- **Rust: items inside a braced macro call are indexed.** tokio declares much
  of its API inside `cfg_rt! { … }` blocks, so `rq JoinHandle` used to return
  a private test mock. The call's body is parsed as items when it parses as
  items cleanly.

### Changed
- **`-k type` now means any named type:** the new `type` kind plus `struct`.
  It used to mean `struct` alone. `-k struct` (`-k s`) is unchanged, and
  `-k alias` selects aliases only.
- **Rust trait methods take the trait's visibility.** A `pub trait` method
  used to count as private and rank below public API. A trait impl's methods
  now carry no visibility at all.
- **A literal match no longer hides the other case convention.**
  `rq abort_handle` used to list only the `abort_handle` methods. It now also
  shows the `AbortHandle` struct, and `rq ThreadId` also shows `thread_id`. The
  spelling you typed still wins when the evidence is otherwise even.
- **Tests beside the code rank as tests.** A definition inside a `mod tests`
  (or `test`, or `*_tests`) now takes the same penalty as one under a `tests/`
  directory, so Rust unit tests stop outranking the code they test. `--explain`
  shows it as `test_scope`.

## 0.55.1 — 2026-09-27

### Fixed
- **`--status` no longer says `never` for a repo that's being indexed.** A
  first index (or the rebuild after `--drop`) read `never` until its pass
  finished, however many files it had already indexed. It now reads `warming`,
  like any partial index; `never` is no longer reported.
- **A repo no longer sticks at `warming` after a concurrent `--index`.** A
  background warm that ran out of time recorded `warming` over the `complete`
  an `rq --index` had written while it ran, so with nothing left indexing, a
  miss exited 2 (warming) instead of 1. A pass that didn't finish now leaves a
  `complete` recorded during it alone.
- **An index killed on its first pass no longer slows every `-a` search.** A
  cold `--index` or warm stopped before its end left the repo's name index
  suspended, so fuzzy `--all-repos` searches from outside the repo read its
  rows directly (about 250 ms on a large repo, against ~2) until a pass inside
  it finished. A search now rebuilds an index whose pass is gone.
- **Fuzzy matching folds non-ASCII case.** A query was lowercased in full but
  a name's letters only in ASCII, so `ΣΑΣprs` and `σασprs` couldn't find
  `ΣΑΣParser`. Letters whose lowercase is ASCII (`İ`,
  the Kelvin sign) still match only as typed.
- **A background warm no longer indexes hidden files.** `rq --index` skips
  dot-files and dot-directories (`.devcontainer/`, `.prettierrc.cjs`), but a
  warm, which lists files with `git ls-files`, indexed the tracked ones, so what
  was indexed depended on which pass ran last. Neither indexes them now; the
  next full pass forgets any a warm added.
- **`RQ_DB` and `HOME` are checked before anything is opened.** An empty
  `RQ_DB=` now means the default path instead of failing. An `RQ_DB` that names
  a directory (`/some/dir/`) is refused with exit 64 rather than creating a file
  named after it. The hint for a relative `RQ_DB` suggests `$HOME/…` for a
  `~/…` path, where it used to suggest `$PWD/~/…`. With `RQ_DB` unset, an unset
  `HOME` now says so, and a relative `HOME` is refused; both exit 64 (an unset
  `HOME` used to exit 74 with a message that didn't name it).

### Upgrade note (correction to 0.55.0)
- **An older rq can't index a migrated database at all.** 0.55.0's note said an
  older rq fails fuzzy queries and a repo's first index. It fails every
  `--index`, incremental ones too, with `cannot start a transaction within a
  transaction` (exit 74), and a search it runs fails whenever it has no exact or
  prefix match. Its failed passes write nothing; the next pass of a current rq
  picks up the files they missed. If an older rq ran `--drop` or you'd rather
  start clean, run `rq --drop` in the repo and then `rq --index`, both with the
  current rq.

### Corrected
- **0.55.0's `tets_br` example overstated it.** The typo reading is offered
  beside the in-order names, but `test_br` ranks second for `tets_br`, below
  `test_sub_regions`, which holds the query's letters in order. 0.54.1 ranked
  it first, so for this query 0.55.0 is a step back, not a recovery.

### Documented
- **Exit codes of the non-search commands.** `--status`, `--index` and `--drop`
  exit 0 whenever they ran, even with nothing to show or drop; `--usage` exits 1
  when nothing is recorded yet. `--help` and the README now say so.
- **What `--no-wait` and `source: live` mean.** On a repo with no finished
  index — never indexed, or just dropped — `--no-wait` answers from a live scan
  (`"source": "live"`) rather than `warming`, so on a small or indexed repo it
  looks the same as a normal query. The README had `live` meaning only "a
  directory rq doesn't track".

## 0.55.0 — 2026-09-27

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
