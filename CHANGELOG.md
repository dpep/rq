# Changelog

Notable changes to `rq`. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning follows [SemVer](https://semver.org/).

Releases before 0.40.0 predate this file; `git log v0.39.1..v0.40.0` and the
tags around it are the record for those.

## Unreleased

### Changed

- **The published library API is a third of its former size** — 146 public
  items down to 53. `rq` ships a `reference-query` library alongside the `rq`
  binary, and `lib.rs` re-exported all eight modules, so most of that surface
  was public by default rather than by decision. `lang`, `profile` and `trace`
  are now crate-private, along with 45 items inside the five modules that
  consumers genuinely use.

  **No effect on the `rq` binary or its behaviour.** This matters only if you
  depend on `reference-query` as a library — `index::Stats`,
  `store::CoverageRow`, `store::Store`, `search::Hit` and the search entry
  points remain public; most other paths do not.

### Fixed

- `script/check.sh` could report "all green" while `cargo clippy` was failing,
  so a red tree looked shippable. A shell function on the left of a pipe runs
  with `set -e` suppressed: a failing step didn't stop the run, and the
  pipeline reported the *last* step's status instead of the first failure's.
  The gate now stops at the first failing step and exits non-zero.

### Internal

- `unreachable_pub` is on. A `pub` item inside a private module is reachable
  from nowhere, and `pub` is exactly what makes the `dead_code` lint skip an
  item — so the two together were hiding unused code. Nothing was found dead in
  `rq` once they were visible, which is the point: the lint can now answer.
