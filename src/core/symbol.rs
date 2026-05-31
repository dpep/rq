//! The common symbol model every language plugin emits.

use std::fmt;

/// The kind of definition a [`Symbol`] represents.
///
/// Intentionally small: the MVP indexes *definitions only*. Call graphs,
/// references, and inheritance are explicit non-goals (see `docs/ROADMAP.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Class,
    Module,
    Method,
    Function,
}

impl Kind {
    /// Stable lowercase tag used in storage and output.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Class => "class",
            Kind::Module => "module",
            Kind::Method => "method",
            Kind::Function => "function",
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
    }
}
