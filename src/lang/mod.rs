//! Language plugins — the only seam languages plug into.
//!
//! A plugin maps source text to the common [`Symbol`](crate::core::Symbol)
//! model. The core stays language-agnostic; adding a language is a new plugin,
//! not a core change.

use crate::core::Symbol;

pub mod ruby;

/// Extracts definitions from a single source file.
pub trait LanguagePlugin {
    /// File extensions this plugin handles, without the dot (e.g. `["rb"]`).
    fn extensions(&self) -> &[&str];

    /// Extract definitions from `source`. `file` is the repo-relative path,
    /// recorded on each emitted [`Symbol`].
    fn extract(&self, file: &str, source: &str) -> Vec<Symbol>;
}
