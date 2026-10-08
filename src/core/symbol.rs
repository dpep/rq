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
pub(crate) enum Kind {
    Class,
    Module,
    Method,
    Function,
    Struct,
    Enum,
    Trait,
    Constant,
    /// A macro definition (Rust `macro_rules!`).
    Macro,
    /// A named type that isn't a struct, enum or trait: a type alias (Rust and
    /// TypeScript `type X = …`) or a Go named type (`type Celsius float64`).
    Type,
    /// One case of an enum: a Rust variant, a TypeScript or Python enum member.
    Variant,
    /// A named slot a type declares, parented by the type: a Rust or Go struct
    /// field, a TypeScript property, a Python class attribute.
    Field,
}

impl Kind {
    /// Every kind, for whatever lists or parses the vocabulary.
    pub(crate) const ALL: [Kind; 12] = [
        Kind::Class,
        Kind::Module,
        Kind::Method,
        Kind::Function,
        Kind::Struct,
        Kind::Enum,
        Kind::Trait,
        Kind::Constant,
        Kind::Type,
        Kind::Macro,
        Kind::Variant,
        Kind::Field,
    ];

    /// The kind a stored tag names; `None` for one this build doesn't know.
    pub(crate) fn from_tag(tag: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == tag)
    }

    /// Stable lowercase tag used in storage and output.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Class => "class",
            Kind::Module => "module",
            Kind::Method => "method",
            Kind::Function => "function",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::Constant => "constant",
            Kind::Macro => "macro",
            Kind::Type => "type",
            Kind::Variant => "variant",
            Kind::Field => "field",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How far a definition its language marks private can be called from: the
/// file that defines it (an ES module, a Rust module's own file), or every file
/// in its directory (a Go package, and the default where a language's privacy
/// isn't a place at all: a Ruby private method, Python's `_x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivateScope {
    File,
    Directory,
}

/// A definition extracted from source.
///
/// Every language plugin emits this same shape; the core never sees a
/// language-specific concept. `parent` records *lexical* nesting only
/// (e.g. `Foo::Bar#baz`) — it is not reference tracking or inheritance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    /// The defined name, e.g. `RefundProcessor`, `perform`.
    pub name: String,
    pub kind: Kind,
    /// Language tag, e.g. `ruby`.
    pub language: String,
    /// Repository-relative path.
    pub file: String,
    /// 1-based line of the definition.
    pub line: u32,
    /// 1-based last line of the definition's body — with `line`, the span to
    /// read to see the whole definition. Equals `line` for a one-line symbol.
    pub end_line: u32,
    /// Enclosing symbol name, if any (lexical nesting only).
    pub parent: Option<String>,
    /// Access level when the language expresses one: `public`, `crate`,
    /// `private`, `protected`, or `local` (only its enclosing definition's body
    /// can reach it: a closure). `None` when unknown. A ranking hint (private
    /// helpers sit below public API), never a filter.
    pub visibility: Option<&'static str>,
    /// Declares a definition whose body lives elsewhere: a TypeScript ambient
    /// `declare` or `.d.ts` entry, an overload signature. A ranking hint (the
    /// implementation outranks it), never a filter.
    pub stub: bool,
    /// A member of a type that belongs to the type itself rather than to its
    /// instances: a Ruby class method, a Python `@classmethod`/`@staticmethod`,
    /// a TypeScript `static` member, a Rust associated fn without `self`. Only
    /// members that could be either carry it — a constant or nested type never.
    pub singleton: bool,
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
        assert_eq!(Kind::Constant.as_str(), "constant");
        assert_eq!(Kind::Macro.as_str(), "macro");
        assert_eq!(Kind::Type.as_str(), "type");
        assert_eq!(Kind::Variant.as_str(), "variant");
        assert_eq!(Kind::Field.as_str(), "field");
    }
}
