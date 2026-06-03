//! Language plugins — the only seam languages plug into.
//!
//! A plugin maps source text to the common [`Symbol`](crate::core::Symbol)
//! model. The core stays language-agnostic; adding a language is a new plugin,
//! not a core change.

use crate::core::Symbol;

pub mod go;
pub mod python;
pub mod ruby;
pub mod rust;

/// Extracts definitions from a single source file.
pub trait LanguagePlugin {
    /// The language tag emitted on every [`Symbol`] (e.g. `"ruby"`). Also the
    /// canonical name `--lang` matches against.
    fn language(&self) -> &'static str;

    /// File extensions this plugin handles, without the dot (e.g. `["rb"]`).
    fn extensions(&self) -> &[&str];

    /// Extract definitions from `source`. `file` is the repo-relative path,
    /// recorded on each emitted [`Symbol`].
    fn extract(&self, file: &str, source: &str) -> Vec<Symbol>;
}

/// The tags of all registered languages — the set `--lang` matches against, so
/// it can't drift from the registry.
pub fn languages() -> Vec<&'static str> {
    registry().iter().map(|p| p.language()).collect()
}

/// The registered language plugins. Adding a language is one line here.
pub fn registry() -> Vec<Box<dyn LanguagePlugin>> {
    vec![
        Box::new(ruby::Ruby),
        Box::new(rust::Rust),
        Box::new(go::Go),
        Box::new(python::Python),
    ]
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
    fn languages_are_registered_by_extension() {
        for ext in ["rb", "rs", "go", "py"] {
            assert!(plugin_for_extension(ext).is_some(), "{ext} should resolve");
        }
        assert!(plugin_for_extension("java").is_none());
    }
}
