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

/// The registered language plugins. Adding a language is one line here.
pub fn registry() -> Vec<Box<dyn LanguagePlugin>> {
    vec![Box::new(ruby::Ruby)]
}

/// The plugin handling files with the given extension (without the dot), if any.
pub fn plugin_for_extension(ext: &str) -> Option<Box<dyn LanguagePlugin>> {
    registry()
        .into_iter()
        .find(|p| p.extensions().contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruby_is_registered_for_rb() {
        assert!(plugin_for_extension("rb").is_some());
        assert!(plugin_for_extension("py").is_none());
    }
}
