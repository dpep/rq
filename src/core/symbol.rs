//! The common symbol model every language plugin emits.

use std::fmt;

/// The kind of definition a [`Symbol`] represents.
///
/// A small, *language-agnostic* vocabulary of definition kinds — the shared
/// model every plugin maps onto, deliberately generalized rather than per
/// language (Rust's `struct`/`enum`/`trait` sit beside Ruby's `class`/`module`).
/// It covers *definitions only*: call graphs, references, and inheritance are
/// explicit non-goals (see `docs/ROADMAP.md`). Add a variant when a language
/// needs a kind the model can't yet express, not a language-specific one-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Class,
    Module,
    Method,
    Function,
    Struct,
    Enum,
    Trait,
}

impl Kind {
    /// Stable lowercase tag used in storage and output.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Class => "class",
            Kind::Module => "module",
            Kind::Method => "method",
            Kind::Function => "function",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A definition extracted from source.
///
/// Every language plugin emits this same shape; the core never sees a
/// language-specific concept. `parent` records *lexical* nesting only
/// (e.g. `Foo::Bar#baz`) — it is not reference tracking or inheritance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The defined name, e.g. `RefundProcessor`, `perform`.
    pub name: String,
    pub kind: Kind,
    /// Language tag, e.g. `ruby`.
    pub language: String,
    /// Repository-relative path.
    pub file: String,
    /// 1-based line of the definition.
    pub line: u32,
    /// Enclosing symbol name, if any (lexical nesting only).
    pub parent: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tag_is_stable_and_lowercase() {
        assert_eq!(Kind::Class.as_str(), "class");
        assert_eq!(Kind::Module.to_string(), "module");
        assert_eq!(Kind::Method.as_str(), "method");
        assert_eq!(Kind::Function.as_str(), "function");
        assert_eq!(Kind::Struct.as_str(), "struct");
        assert_eq!(Kind::Enum.as_str(), "enum");
        assert_eq!(Kind::Trait.as_str(), "trait");
    }
}
