# Dogfood misses

Queries where rq, used for real work, missed the definition or ranked it below
#1. Each entry: the query, the repo, what was meant, where it ranked. Fixed
entries move into the recall set (`script/recall.py`) and out of this list.

| query | repo | meant | rank | notes |
|---|---|---|---|---|
| `clock` | rq | the `clock` module (`src/core/clock.rs`) | absent; `tests · clock` #1 | Dogfood set. Module files aren't symbols. `tests` now ranks below as test code (D33). A module symbol was tried and not adopted (D34) |
| `block_on` | tokio | `Runtime::block_on` / `Handle::block_on` | #9 | Rust tester. `pub(crate)` impls and `future/block_on.rs` rank above. D35 measured a `pub(crate)` penalty and left it out; D45 found no signal short of re-export and stability data. A regress case |
| `process` | @types/node | `declare var process` (`globals.d.ts`) | #3, behind `interface Process` and `declare module "process"` | Extraction-gaps lane. Found since D38. The `stub` penalty (−150) holds both declared vars back, not kind (15 of a 189-point gap); left, with a results-relative `stub` as the lead (D46) |
| `grep_searcher::Searcher` | ripgrep | `Searcher` in `crates/searcher/` | `scope_not_found`, `found_in` names it | D27's limit: a crate named other than its directory. Crate names from manifests weighed and declined (D39) |
