//! Language plugins — the only seam languages plug into.
//!
//! A plugin maps source text to the common [`Symbol`]
//! model. The core stays language-agnostic; adding a language is a new plugin,
//! not a core change.

use tree_sitter::{Language, Node, Parser};

use crate::core::{ImportTarget, Kind, PrivateScope, Symbol};

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
    /// Rows above `src` in the real file: nonzero only for a re-parsed fragment.
    row_offset: usize,
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

    /// The 1-based line in the real file of a parse-tree row.
    pub(crate) fn line(&self, row: usize) -> u32 {
        (self.row_offset + row) as u32 + 1
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
            line: self.line(node.start_position().row),
            end_line: self.line(node.end_position().row),
            parent: parent.map(str::to_string),
            visibility: None, // plugins that know it set it on the result
            stub: false,
            singleton: false,
        }
    }
}

impl Ctx<'_> {
    /// Parse the source between `start` and `end` (bytes, starting on `row`) as
    /// a standalone fragment and walk it: for code the grammar sees only as
    /// opaque tokens, such as a Rust macro body. Symbols keep their real lines.
    /// A fragment that doesn't parse cleanly is not walked, and returns false.
    pub(crate) fn walk_fragment(
        &self,
        grammar: &Language,
        (start, end, row): (usize, usize, usize),
        walk: impl FnOnce(&Ctx, Node),
    ) -> bool {
        let src = &self.src[start..end];
        // A parser of its own: the file's parser stays borrowed for the walk.
        // Released before walking, so a fragment inside a fragment can parse.
        let tree = FRAGMENT_PARSERS.with(|cell| {
            let mut parsers = cell.borrow_mut();
            let parser = match parsers.entry(self.language) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    let mut p = Parser::new();
                    p.set_language(grammar).ok()?;
                    v.insert(p)
                }
            };
            parser.parse(src, None)
        });
        let Some(tree) = tree.filter(|t| !t.root_node().has_error()) else {
            return false;
        };
        let ctx = Ctx {
            src,
            file: self.file,
            language: self.language,
            row_offset: self.row_offset + row,
        };
        walk(&ctx, tree.root_node());
        true
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
    static FRAGMENT_PARSERS: std::cell::RefCell<std::collections::HashMap<&'static str, Parser>> =
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
            row_offset: 0,
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
    /// Each also names the language to `--lang`.
    fn extensions(&self) -> &[&str];

    /// Other names `--lang` accepts for the language, beyond a prefix of its
    /// tag and its extensions.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// The family of languages that can refer to one another's definitions, as
    /// a tag every member returns: code in one can reach a definition in any.
    /// TypeScript and JavaScript import each other, so they share one. A
    /// language that reaches only itself keeps the default, its own tag.
    fn family(&self) -> &'static str {
        self.language()
    }

    /// Where a definition this language marks private can be called from.
    /// The default, its directory, is the widest any language's privacy
    /// reaches by place; a language whose private names stop at their file
    /// says so.
    fn private_scope(&self) -> PrivateScope {
        PrivateScope::Directory
    }

    /// Extract definitions from `source`. `file` is the repo-relative path,
    /// recorded on each emitted [`Symbol`].
    fn extract(&self, file: &str, source: &str) -> Vec<Symbol>;

    /// The method a `Foo.new` call runs, when the language spells it otherwise
    /// (Ruby's `initialize`). `None` where `new` is already the literal name.
    fn constructor(&self) -> Option<&'static str> {
        None
    }

    /// Where `name`, as `file` (relative to the checkout at `root`) uses it at
    /// `line`, is defined by way of the file's imports, as the language
    /// resolves its modules within the checkout: the imported file first, then
    /// each it re-exports from. Empty when the file doesn't import the name,
    /// and by default: a language that resolves no imports earns no
    /// `imported` boost.
    fn resolve_import(
        &self,
        _root: &std::path::Path,
        _file: &str,
        _line: usize,
        _name: &str,
    ) -> Vec<ImportTarget> {
        Vec::new()
    }
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

/// The language tags whose definitions code in `file` can refer to: its
/// plugin's family. Empty when no plugin handles the file.
pub(crate) fn reachable_from(file: &str) -> Vec<&'static str> {
    let Some(from) = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(plugin_for_extension)
    else {
        return Vec::new();
    };
    REGISTRY
        .iter()
        .filter(|p| p.family() == from.family())
        .map(|p| p.language())
        .collect()
}

/// Where a private definition in `language` can be called from, as its plugin
/// declares it; a language no plugin registers gets the default.
pub(crate) fn private_scope(language: &str) -> PrivateScope {
    REGISTRY
        .iter()
        .find(|p| p.language() == language)
        .map_or(PrivateScope::Directory, |p| p.private_scope())
}

/// Where `name`, used at `line` of `file`, is defined through that file's
/// imports, by its plugin; empty when no plugin handles the file.
pub(crate) fn resolve_import(
    root: &std::path::Path,
    file: &str,
    line: usize,
    name: &str,
) -> Vec<ImportTarget> {
    std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(plugin_for_extension)
        .map_or_else(Vec::new, |p| p.resolve_import(root, file, line, name))
}

/// Is `name` the constructor a `Foo.new` query means, in `language`?
pub(crate) fn is_constructor(language: &str, name: &str) -> bool {
    REGISTRY
        .iter()
        .any(|p| p.language() == language && p.constructor() == Some(name))
}

/// Every registered constructor name, for recalling them on a `Foo.new` query.
pub(crate) fn constructors() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().filter_map(|p| p.constructor())
}

/// Assertions every plugin's tests share. Each plugin keeps its own `extract`
/// — that's the language-specific half — and borrows the rest from here.
#[cfg(test)]
pub(crate) mod testing {
    use super::Symbol;

    /// The extracted symbol with this name, or a panic naming what was found
    /// instead — the diagnostic is the reason this isn't a bare `find`.
    pub(crate) fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_scope_is_the_plugins_and_defaults_to_the_directory() {
        for (language, scope) in [
            ("typescript", PrivateScope::File),
            ("javascript", PrivateScope::File),
            ("rust", PrivateScope::Directory),
            ("go", PrivateScope::Directory),
            ("ruby", PrivateScope::Directory),
            ("python", PrivateScope::Directory),
            ("cobol", PrivateScope::Directory),
        ] {
            assert_eq!(private_scope(language), scope, "{language}");
        }
    }

    #[test]
    fn languages_are_registered_by_extension() {
        for ext in ["rb", "rs", "go", "py", "ts", "tsx", "js", "jsx"] {
            assert!(plugin_for_extension(ext).is_some(), "{ext} should resolve");
        }
        assert!(plugin_for_extension("java").is_none());
    }

    #[test]
    fn a_file_reaches_its_language_family() {
        let ecma = reachable_from("app/page.tsx");
        assert_eq!(ecma, ["typescript", "javascript"]);
        assert_eq!(
            reachable_from("lib/util.cjs"),
            ecma,
            "the family is symmetric"
        );
        assert_eq!(reachable_from("index.d.ts"), ecma);
        assert_eq!(reachable_from("app/models/user.rb"), ["ruby"]);
        assert!(
            reachable_from("README.md").is_empty(),
            "no plugin, no family"
        );
        assert!(reachable_from("Makefile").is_empty());
    }
}
