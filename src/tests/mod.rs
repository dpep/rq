//! Tests that exercise the crate through its internals rather than its CLI.
//!
//! These live inside the lib, not in `tests/`, because an integration test is a
//! separate crate and can only reach `pub` items — which is what kept `store`,
//! `index`, `search` and `core` in the published API long after nothing outside
//! the crate had any use for them. Tests of the CLI contract stay in `tests/`,
//! where `CARGO_BIN_EXE_rq` is available and the binary is the thing under test.

mod bench;
mod budgeted_index;
mod candidate_recall;
mod git_metadata;
mod index_integration;
mod lang_fixtures;
mod learning;
mod live_search;
mod ranking_aspirations;
mod ranking_dogfood;
mod rust_fixture;
mod staleness;
mod support;
