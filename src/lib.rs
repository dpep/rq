//! rq — a code navigation engine.
//!
//! The goal is to reach the file, symbol, or definition a developer is most
//! likely looking for as fast as possible — not to enumerate every match.
//! See `docs/ARCHITECTURE.md` for the design these modules implement.

#[macro_use]
pub(crate) mod profile;
pub(crate) mod trace;

pub mod cli;
pub(crate) mod core;
pub(crate) mod index;
pub(crate) mod lang;
pub(crate) mod search;
pub(crate) mod store;

#[cfg(test)]
mod tests;
