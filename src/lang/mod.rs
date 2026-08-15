//! Language plugins — the only seam languages plug into.
//!
//! A plugin maps source text to the common [`Symbol`](crate::core::Symbol)
//! model. The core stays language-agnostic; adding a language is a new plugin,
//! not a core change.

use tree_sitter::{Language, Node, Parser};

use crate::core::{Kind, Symbol};

pub(crate) mod go;
pub(crate) mod python;
pub(crate) mod ruby;
pub(crate) mod rust;
pub(crate) mod typescript;

/// Per-file extraction context shared by every plugin: the source bytes, the
/// repo-relative path, and the language tag stamped on each emitted symbol.
pub(crate) struct Ctx<'a> {
    src: &'a [u8],
    file: &'a str,
    language: &'static str,
}

impl Ctx<'_> {
    /// The text of `node`'s named field, if present.
    pub(crate) fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .and_then(|n| n.utf8_text(self.src).ok())
            .map(str::to_string)
    }

    /// The text of `node` itself.
    pub(crate) fn node_text(&self, node: Node) -> Option<String> {
        node.utf8_text(self.src).ok().map(str::to_string)
    }

    /// Build a [`Symbol`] for `node` (1-based line span).
    pub(crate) fn symbol(
        &self,
        name: &str,
        kind: Kind,
        node: Node,
        parent: Option<&str>,
    ) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            language: self.language.to_string(),
            file: self.file.to_string(),
            line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            parent: parent.map(str::to_string),
            visibility: None, // plugins that know it set it on the result
        }
    }
}

/// Join a name onto its enclosing qualified name with the language's separator.
pub(crate) fn qualify(parent: Option<&str>, name: &str, sep: &str) -> String {
    match parent {
        Some(p) => format!("{p}{sep}{name}"),
        None => name.to_string(),
    }
}

thread_local! {
    /// One parser per language per thread. `set_language` (grammar table
    /// loading) is the expensive step of parser setup, and the indexer calls
    /// `extract` once per file — reuse makes that a one-time cost per worker.
    static PARSERS: std::cell::RefCell<std::collections::HashMap<&'static str, Parser>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Parse `source` with `grammar` and hand the tree's root (plus a [`Ctx`]) to
/// the plugin's `walk`. All the per-file plumbing lives here; a plugin is just
/// its walk. (The parser cache is borrowed across the walk, so a walk must
/// never recurse into another `extract` — none does.)
pub(crate) fn extract_with(
    language: &'static str,
    grammar: Language,
    file: &str,
    source: &str,
    walk: impl FnOnce(&Ctx, Node, &mut Vec<Symbol>),
) -> Vec<Symbol> {
    extract_with_key(language, language, grammar, file, source, walk)
}

/// [`extract_with`] with the parser-cache key named separately from the language
/// tag — for a plugin that spans more than one grammar (TypeScript's `.ts` vs
/// `.tsx`) or a grammar shared by two tags. The key identifies the *grammar*, so
/// it must be distinct per grammar and identical wherever that grammar is used.
pub(crate) fn extract_with_key(
    key: &'static str,
    language: &'static str,
    grammar: Language,
    file: &str,
    source: &str,
    walk: impl FnOnce(&Ctx, Node, &mut Vec<Symbol>),
) -> Vec<Symbol> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let parser = match parsers.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut p = Parser::new();
                if p.set_language(&grammar).is_err() {
                    return Vec::new();
                }
                v.insert(p)
            }
        };
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let ctx = Ctx {
            src: source.as_bytes(),
            file,
            language,
        };
        walk(&ctx, tree.root_node(), &mut out);
        out
    })
}

/// Extracts definitions from a single source file.
pub(crate) trait LanguagePlugin {
    /// The language tag emitted on every [`Symbol`] (e.g. `"ruby"`). Also the
    /// canonical name `--lang` matches against.
    fn language(&self) -> &'static str;

    /// File extensions this plugin handles, without the dot (e.g. `["rb"]`).
    fn extensions(&self) -> &[&str];

    /// Extract definitions from `source`. `file` is the repo-relative path,
    /// recorded on each emitted [`Symbol`].
    fn extract(&self, file: &str, source: &str) -> Vec<Symbol>;
}

/// The registered language plugins. Adding a language is one line here.
static REGISTRY: [&(dyn LanguagePlugin + Sync); 6] = [
    &ruby::Ruby,
    &rust::Rust,
    &go::Go,
    &python::Python,
    &typescript::TypeScript,
    &typescript::JavaScript,
];

/// The tags of all registered languages — the set `--lang` matches against, so
/// it can't drift from the registry.
pub(crate) fn languages() -> Vec<&'static str> {
    registry().iter().map(|p| p.language()).collect()
}

/// The registered language plugins.
pub(crate) fn registry() -> &'static [&'static (dyn LanguagePlugin + Sync)] {
    &REGISTRY
}

/// The plugin handling files with the given extension (without the dot), if any.
pub(crate) fn plugin_for_extension(ext: &str) -> Option<&'static (dyn LanguagePlugin + Sync)> {
    REGISTRY
        .iter()
        .copied()
        .find(|p| p.extensions().contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_are_registered_by_extension() {
        for ext in ["rb", "rs", "go", "py", "ts", "tsx", "js", "jsx"] {
            assert!(plugin_for_extension(ext).is_some(), "{ext} should resolve");
        }
        assert!(plugin_for_extension("java").is_none());
    }
}
