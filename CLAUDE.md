# rq development conventions

`rq` (Reference Query) is a **code navigation engine** — it gets you to the
definition you most likely want, fast. Read [README.md](README.md) for the
product vision, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design,
[docs/ROADMAP.md](docs/ROADMAP.md) for what ships when, and
[docs/DECISIONS.md](docs/DECISIONS.md) for what was considered and turned down —
check it before proposing an optimization or a ranking signal, because the
rejections carry the numbers that settled them.

> **Shipping, maintained tool.** The design docs are the contract — keep them
> in sync with the code, changing them in the same commit when the design
> changes.

## First principles (do not drift from these)

- **Navigation, not search.** Fewer, better, ranked results beat exhaustive
  ones. When a change trades relevance for completeness, it's probably wrong.
- **The core is language-agnostic.** No Ruby-specific (or any-language)
  assumption leaks out of `src/lang/` into `index`/`search`/scoring. Languages
  plug in via `LanguagePlugin`. The shared *model* may grow to fit a language —
  e.g. Rust added `struct`/`enum`/`trait` to `core::Kind` — but that's
  generalizing the vocabulary all languages share, not a one-off. Prefer
  generalizing over a special case; change `core/` when it genuinely earns it.
- **Results stream.** The API is incremental from the start — sub-50 ms first
  answer, then progressive improvement. Don't add synchronous "collect
  everything" paths.
- **Ranking is explainable.** Scoring is an additive sum of named features;
  `--explain` must always be able to show why a result ranked where it did.
- **Partial is normal.** Never assume a complete index. Code must work at 0%,
  5%, and 100% coverage.
- **Every command is agent/script-friendly.** Any command that prints output
  honors `--json`/`--ndjson`, not just search — `--status`, `--index`, `--drop`,
  and anything new. `--json` is a pretty object (single-result commands) or array
  (multi-row); `--ndjson` is one compact object per line. Keep field names stable
  and consistent across commands (a repo identity is always `repo`). Route
  single-object commands through the `emit_json` helper. Exit codes stay
  meaningful (0 = something happened/matched, non-zero = nothing). When you add a
  command, add its structured output and an e2e assertion in the same change.

## Language and toolchain

Rust, single static binary. Tree-sitter for symbol extraction, `rusqlite` for
storage (SQLite, WAL mode).

This machine's Rust came via Homebrew's keg-only `rustup`, so `cargo` may not be
on `PATH`. Either add it once —

```sh
echo 'export PATH="/opt/homebrew/opt/rustup/bin:$PATH"' >> ~/.bash_profile
```

— or invoke directly: `/opt/homebrew/opt/rustup/bin/cargo`.

## Repo layout

Single binary crate; modules mirror the architecture. Language specifics are
quarantined under `src/lang/`.

```text
rq/
  Cargo.toml
  src/
    main.rs      ← CLI entry
    cli/         ← `rq <query>` default command, arg parsing, output
    core/        ← symbol model, repo identity, scoring — NO language specifics
    store/       ← SQLite schema, migrations, queries (WAL)
    index/       ← walker, incremental indexer, coverage
    search/      ← staged pipeline, scorer, --explain
    lang/        ← Tree-sitter plugins (ruby, rust, go, python, typescript)
      ruby/      ← first plugin
      rust/      ← rq dogfoods on its own source
  docs/          ← ARCHITECTURE.md, ROADMAP.md
  tests/         ← integration tests + fixtures
```

Keep it a single crate until there's a concrete reason to split into a
workspace (e.g. a reusable library extracted for editor adapters). Simpler
wins.

## Building, testing, linting

```sh
cargo build                 # dev build → target/debug/rq
cargo build --release       # optimized → target/release/rq
cargo run -- refund         # run the CLI from source
cargo test                  # unit + integration tests
cargo clippy --all-targets  # lint — keep it clean
cargo fmt                    # format — run before committing
```

Before committing: `cargo fmt && cargo clippy --all-targets && cargo test`.

## Testing conventions

- Write tests for new code, but keep them focused on quality, not quantity —
  edge cases and error handling over restating the happy path.
- Ranking is the heart of the tool: test it with **fixture repos** under
  `tests/fixtures/` and assert on *ordering* (the right result ranks first),
  not just membership.
- A new language plugin ships with a fixture file of source + expected symbols.
- **Use generic, non-identifying test data** — neutral placeholders (`Widget`,
  `Foo`, `HandlerA`, `Account`) over real class names, company/product terms, or
  anything tied to a specific employer or codebase. This is a public repo; keep
  fixtures and assertions domain-neutral.
- Spec descriptions stay simple and resilient ("ranks the exact match first",
  not a brittle exact-string assertion).
- **Verify through `cargo test`, not by hand-running the binary.** CLI behavior
  is covered by `tests/cli_e2e.rs`, which drives the built binary
  (`CARGO_BIN_EXE_rq`) with an isolated `RQ_DB` and a temp repo — reproducible,
  CI-checked, and no permission prompts. Extend that test rather than running
  ad-hoc `rq …` invocations to confirm a change. Logic that would otherwise
  need a manual run (e.g. git-log parsing) is factored into a pure function with
  its own unit test.

## Adding a language plugin

1. Add the Tree-sitter grammar dependency.
2. Implement `LanguagePlugin` in `src/lang/<lang>/`: `extensions()` +
   `extract(source) -> Vec<Symbol>`.
3. Register it in the extension→plugin registry.
4. Add a fixture (source + expected `Symbol`s) under `tests/fixtures/<lang>/`,
   and assert on *ordering* end-to-end (see `tests/rust_fixture.rs`).

`index/` and `search/` should not need to change — if they do, a language
specific leaked and the design doc needs revisiting. The exception is the shared
`core::Kind` vocabulary: a language may add a kind it genuinely needs (Rust
added `struct`/`enum`/`trait`), which also touches the kind-keyed spots in
`search/score.rs` (weight + the path-only "primary definition" gate) and the
`--kind` canonicalizer in `cli/`. That's generalizing the model, not a leak —
prefer it over a one-off, and keep the new kind language-neutral.

**Dogfooding.** `make dogfood Q=<query>` fully indexes a repo into a throwaway
DB and runs a query from inside it, so you can feel the ranking on real code.
Use it to catch quality regressions a unit test wouldn't.

`REPO=` picks the target; it defaults to this repo, which makes Rust the
default dogfood language. Reach for someone else's code whenever the question
is about *ranking* rather than extraction: rq's own source is ~600 symbols, too
few for same-name collisions and ambiguity to appear at all, and it can only
ever exercise the Rust plugin. `~/code/lib/ruby/rails` is a good large Ruby
corpus (~3k files, indexes in seconds):

```sh
make dogfood REPO=~/code/lib/ruby/rails Q=Middleware
```

## Schema changes

`store/` owns the schema and migrations. A schema change is a migration plus an
update to the schema block in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — keep
them in sync in the same PR.

## Landing changes

No pull requests for this repo — commit or merge directly to `main` and push.
It's a solo project; the PR ceremony is overhead we skip here.

Keep changes small, focused, and logically connected; change behavior or
structure, not both at once. Make sure CI is green
(`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`)
before pushing.

## Versioning / releasing

Bump the version when a change reaches users — i.e. it alters the **built
binary** (behavior, a flag, ranking, even `--help`/output wording). Stay below
1.0 for now — **only minor or patch bumps**, never a major:

- **patch** (`0.1.x`) — fixes, output/`--help` wording, internal cleanups
- **minor** (`0.x.0`) — new user-facing capability (a flag, a ranking signal, a
  language plugin)

Repo-only docs (README, CLAUDE.md, `docs/`) **don't** bump — they don't change
what `brew` builds, so a bump would only force an identical rebuild.

Releasing is one command — **don't do these steps by hand**:

```sh
release 0.41.0 --steps         # the checklist: what to do, in order, with ✓ on what's done
release minor --summary "…"    # or run the whole thing
release 0.41.0 --dry-run       # what an automated run would do and skip
release --audit                # is anything out of sync, across every tool in the tap?
```

`--steps` is often the right one. The script's value is knowing the ordering,
the derived values, and the steps that get forgotten — not executing them. Use
it to drive a release yourself and keep judgment at each stop; it works on a
dirty tree and part-way through.

`release` lives at `~/.claude/bin/release` and is shared by every tool in the
`dpep/tools` tap. It bumps `Cargo.toml`/`Cargo.lock`, runs `script/check.sh`,
pushes `main`, **waits for CI**, tags, publishes `reference-query` to crates.io,
hashes the tag tarball into `~/code/lib/homebrew-tools/Formula/rq.rb`, builds +
tests + audits the formula, pushes the tap, opens the GitHub release from the
changelog section, and syncs the skill with a plugin-version bump. Every step is
idempotent, so a run that dies partway is just re-run.

Ordering that the script enforces and a hand-run forgets: CI has to be green
before the tag exists, and the tag has to be on GitHub before its tarball can be
hashed. Skip the formula bump and installs serve a stale cached build.

rq ships through **four** channels — tag, crates.io, tap, plugin skill — each
forgettable on its own. `release --audit` compares all four across every tool in
the tap and is the way to catch one that was missed weeks ago.

`CHANGELOG.md` keeps a rolling `## Unreleased` heading: log a change under it in
the commit that earns it, and `release` retitles that heading as the version and
date. Four commits once shipped with no entries at all, and reconstructing them a
week later is worse than writing them cold.

## The skill has three copies; keep them one

`claude/rq-skill.md` is the source. It is copied verbatim — no edits, no
stripping — to:

- `~/code/lib/claude/plugins/code/skills/rq/SKILL.md`, the public marketplace
- whatever a user installed, which updates only when the **code plugin's
  version** moves in `plugins/code/.claude-plugin/plugin.json`

`release` does the copy and the plugin-version bump for you, and `release
--audit` reports a skill copy that has drifted. If you change the skill *without*
releasing, do both by hand in the same change — or it reaches nobody: `claude
plugin update` compares versions, not content, and reports a plugin current
while serving the old file. That has already happened once, to gqls — four
skill-touching commits under one plugin version.

Install guidance for humans lives in `claude/INSTALL.md`, deliberately outside
the skill so the copy stays a copy.
