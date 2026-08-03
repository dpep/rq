# Changelog

Notable changes to `rq`. The CLI surface — flags, output shape, exit codes — is
the public API.

Entries are reconstructed from tags and their release notes, so they summarise
what shipped rather than every commit. Releases before 0.26.2 predate tagging
and aren't listed; see `git log` for those.

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
