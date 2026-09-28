# Dogfood misses

Queries where rq, used for real work, missed the definition or ranked it below
#1. Each entry: the query, the repo, what was meant, where it ranked. Fixed
entries move into the recall set (`script/recall.py`) and out of this list.

| query | repo | meant | rank | notes |
|---|---|---|---|---|
| `braboost` | rq | `BRANCH_DIR_BOOST` | not found; `branch_boost_adds_to_the_score` (a test) #1 | Dogfood set. The test now takes the test-scope penalty (D33). The source still isn't found: a first+last query across SCREAMING words, which is core-search's |
| `clock` | rq | the `clock` module (`src/core/clock.rs`) | absent; `tests · clock` #1 | Dogfood set. Module files aren't symbols. `tests` now ranks below as test code (D33). A module symbol was tried and not adopted (D34) |
| `block_on` | tokio | `Runtime::block_on` / `Handle::block_on` | #9 | Rust tester. `pub(crate)` impls and `future/block_on.rs` rank above. D35 measured a `pub(crate)` penalty and left it out. A regress case |
| `prsnch` | rq | `parse_anchor` | not found | Dogfood set. A consonant skeleton that skips a word-initial vowel (`a`nchor) crosses the mid-word gap limit. Generic fuzzy behaviour, not Rust |
