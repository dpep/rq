# rq development conventions

`rq` (Reference Query) is a **code navigation engine** — it gets you to the
definition you most likely want, fast. Read [README.md](README.md) for the
product vision, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design, and
[docs/ROADMAP.md](docs/ROADMAP.md) for what ships when.

> **Design-stage repo.** The architecture is settled; code has not started.
> When implementing, the design docs are the contract — change them in the same
> PR if the design changes.

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
    lang/        ← Tree-sitter plugins (ruby, rust, go, python)
      ruby/      ← first plugin
      rust/      ← rq dogfoods on its own source
    events/      ← interaction capture + async rollup
    adapters/    ← editor event ingestion (thin)
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

**Dogfooding.** Rust is the dogfood language: `make dogfood Q=<query>` fully
indexes this repo into a throwaway DB and runs a query, so you can feel the
ranking on real Rust. Use it to catch quality regressions a unit test wouldn't.

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

A bump is three edits, landed together:

1. `Cargo.toml` `version`
2. `Cargo.lock` — run `cargo build` so the `rq` entry updates
3. the Homebrew formula `version` in
   `~/code/lib/homebrew-tools/Formula/rq.rb` (push the tap too)

The formula tracks `branch: "main"` with a pinned `version`, so bumping it is
what makes `brew upgrade` rebuild from the latest `main` — skip it and installs
serve a stale cached build.
