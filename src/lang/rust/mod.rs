//! Rust plugin — the second language, and what rq dogfoods on its own source.
//!
//! Extracts the definitions you navigate to: `fn` (free → function, inside an
//! `impl`/`trait` → method), `struct`, `enum`, `trait`, and `mod`. `parent`
//! carries the enclosing qualified name (`::`-joined) so a method renders as
//! `bar · Foo` and a nested type as `outer · mod`. `impl` blocks aren't symbols
//! themselves; they just supply the parent for the methods inside them.

use tree_sitter::{Node, Parser};

use crate::core::{Kind, Symbol};
use crate::lang::LanguagePlugin;

const LANGUAGE: &str = "rust";

pub struct Rust;

impl LanguagePlugin for Rust {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_err()
        {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let ctx = Ctx {
            src: source.as_bytes(),
            file,
        };
        ctx.walk(tree.root_node(), None, false, &mut out);
        out
    }
}

struct Ctx<'a> {
    src: &'a [u8],
    file: &'a str,
}

impl Ctx<'_> {
    /// Recursively collect definitions. `parent` is the enclosing qualified name;
    /// `in_impl` is true inside an `impl`/`trait` body, where an `fn` is a method.
    fn walk(&self, node: Node, parent: Option<&str>, in_impl: bool, out: &mut Vec<Symbol>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                // `function_item` has a body; `function_signature_item` is a
                // bodyless signature (a trait method declaration)
                "function_item" | "function_signature_item" => {
                    if let Some(name) = self.field_text(child, "name") {
                        let kind = if in_impl {
                            Kind::Method
                        } else {
                            Kind::Function
                        };
                        out.push(self.symbol(&name, kind, child, parent));
                    }
                    // bodies rarely hold further definitions worth surfacing
                }
                "struct_item" | "enum_item" | "union_item" => {
                    if let Some(name) = self.field_text(child, "name") {
                        let kind = match child.kind() {
                            "enum_item" => Kind::Enum,
                            _ => Kind::Struct,
                        };
                        out.push(self.symbol(&name, kind, child, parent));
                    }
                }
                "trait_item" => {
                    if let Some(name) = self.field_text(child, "name") {
                        out.push(self.symbol(&name, Kind::Trait, child, parent));
                        // trait method signatures are methods of the trait
                        let qualified = qualify(parent, &name);
                        self.walk(child, Some(&qualified), true, out);
                    }
                }
                "mod_item" => {
                    // Only a module *with a body* is a definition worth surfacing.
                    // A bare `mod x;` is just a re-export pointer to another file —
                    // indexing it competes with (and can outrank) the real
                    // definitions it forwards to.
                    if child.child_by_field_name("body").is_some()
                        && let Some(name) = self.field_text(child, "name")
                    {
                        out.push(self.symbol(&name, Kind::Module, child, parent));
                        let qualified = qualify(parent, &name);
                        self.walk(child, Some(&qualified), false, out);
                    }
                }
                "impl_item" => {
                    // an impl isn't a symbol; its `type` becomes the parent of the
                    // methods inside it
                    let ty = self.field_text(child, "type").map(|t| base_type(&t));
                    let qualified = match &ty {
                        Some(t) => qualify(parent, t),
                        None => parent.map(str::to_string).unwrap_or_default(),
                    };
                    let p = if qualified.is_empty() {
                        None
                    } else {
                        Some(qualified.as_str())
                    };
                    self.walk(child, p, true, out);
                }
                _ => self.walk(child, parent, in_impl, out),
            }
        }
    }

    fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .and_then(|n| n.utf8_text(self.src).ok())
            .map(str::to_string)
    }

    fn symbol(&self, name: &str, kind: Kind, node: Node, parent: Option<&str>) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            language: LANGUAGE.to_string(),
            file: self.file.to_string(),
            line: node.start_position().row as u32 + 1,
            parent: parent.map(str::to_string),
        }
    }
}

fn qualify(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(p) => format!("{p}::{name}"),
        None => name.to_string(),
    }
}

/// The base type name from an impl's `type` field, dropping any generic
/// arguments and path qualifier: `Foo<T>` → `Foo`, `a::b::Foo` → `Foo`.
fn base_type(ty: &str) -> String {
    let head = ty.split('<').next().unwrap_or(ty).trim();
    head.rsplit("::").next().unwrap_or(head).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<Symbol> {
        Rust.extract("test.rs", source)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }

    #[test]
    fn extracts_types_functions_and_impl_methods() {
        let src = r#"
pub struct Widget {
    size: u32,
}

pub enum Color {
    Red,
    Green,
}

pub trait Render {
    fn render(&self) -> String;
}

impl Widget {
    pub fn new() -> Self {
        Widget { size: 0 }
    }
}

pub fn build() -> Widget {
    Widget::new()
}
"#;
        let syms = extract(src);

        let widget = find(&syms, "Widget");
        assert_eq!(widget.kind, Kind::Struct);
        assert_eq!(widget.parent, None);

        assert_eq!(find(&syms, "Color").kind, Kind::Enum);
        assert_eq!(find(&syms, "Render").kind, Kind::Trait);

        // a free fn is a function; an fn inside `impl` is a method of the type
        let build = find(&syms, "build");
        assert_eq!(build.kind, Kind::Function);
        assert_eq!(build.parent, None);

        let new = find(&syms, "new");
        assert_eq!(new.kind, Kind::Method);
        assert_eq!(new.parent.as_deref(), Some("Widget"));

        // a trait method signature is a method of the trait
        let render = find(&syms, "render");
        assert_eq!(render.kind, Kind::Method);
        assert_eq!(render.parent.as_deref(), Some("Render"));

        assert_eq!(widget.language, "rust");
    }

    #[test]
    fn qualifies_through_modules_and_generic_impls() {
        let src = r#"
mod outer {
    pub struct Store<T> {
        inner: T,
    }

    impl<T> Store<T> {
        pub fn get(&self) -> &T {
            &self.inner
        }
    }
}
"#;
        let syms = extract(src);

        assert_eq!(find(&syms, "outer").kind, Kind::Module);
        assert_eq!(find(&syms, "Store").parent.as_deref(), Some("outer"));
        // generic args and the module path resolve to the bare type name
        assert_eq!(find(&syms, "get").parent.as_deref(), Some("outer::Store"));
    }

    #[test]
    fn bare_module_declarations_are_not_indexed() {
        // `mod foo;` is a re-export pointer, not a definition; only a module with
        // a body is surfaced.
        let syms = extract("mod search;\nmod handler { pub fn run() {} }\n");
        assert!(
            !syms.iter().any(|s| s.name == "search"),
            "bare `mod search;` should be skipped: {syms:?}"
        );
        assert_eq!(find(&syms, "handler").kind, Kind::Module);
        assert_eq!(find(&syms, "run").kind, Kind::Function);
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("// just a comment\n").is_empty());
    }
}
