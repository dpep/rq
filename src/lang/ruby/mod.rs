//! Ruby plugin — the first language.
//!
//! TODO(phase-1): extract classes, modules, and methods via Tree-sitter
//! (`tree-sitter` + `tree-sitter-ruby`). For now this is a registered no-op so
//! the plugin seam compiles and the registry has a member.

use crate::core::Symbol;
use crate::lang::LanguagePlugin;

pub struct Ruby;

impl LanguagePlugin for Ruby {
    fn extensions(&self) -> &[&str] {
        &["rb"]
    }

    fn extract(&self, _file: &str, _source: &str) -> Vec<Symbol> {
        // TODO(phase-1): real Tree-sitter extraction.
        Vec::new()
    }
}
