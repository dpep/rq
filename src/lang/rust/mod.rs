//! Rust plugin — the second language, and what rq dogfoods on its own source.
//!
//! Extracts the definitions you navigate to: `fn` (free → function, inside an
//! `impl`/`trait` → method), `struct`, `enum`, `trait`, and `mod`. `parent`
//! carries the enclosing qualified name (`::`-joined) so a method renders as
//! `bar · Foo` and a nested type as `outer · mod`. `impl` blocks aren't symbols
//! themselves; they just supply the parent for the methods inside them.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with, qualify};

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
        extract_with(
            LANGUAGE,
            tree_sitter_rust::LANGUAGE.into(),
            file,
            source,
            |ctx, root, out| walk(ctx, root, None, out),
        )
    }
}

/// Recursively collect definitions. `parent` is the enclosing qualified name.
fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // `function_item` has a body; `function_signature_item` is a
            // bodyless signature (a trait method declaration). A `self`
            // receiver makes it a method; without one it's a function —
            // including an associated fn like `Widget::new()`.
            "function_item" | "function_signature_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = if has_self(child) {
                        Kind::Method
                    } else {
                        Kind::Function
                    };
                    push(ctx, out, &name, kind, child, parent);
                }
                // bodies rarely hold further definitions worth surfacing
            }
            "struct_item" | "enum_item" | "union_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = match child.kind() {
                        "enum_item" => Kind::Enum,
                        _ => Kind::Struct,
                    };
                    push(ctx, out, &name, kind, child, parent);
                }
            }
            "trait_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Trait, child, parent);
                    // trait method signatures are methods of the trait
                    let qualified = qualify(parent, &name, "::");
                    walk(ctx, child, Some(&qualified), out);
                }
            }
            "mod_item" => {
                // Only a module *with a body* is a definition worth surfacing.
                // A bare `mod x;` is just a re-export pointer to another file —
                // indexing it competes with (and can outrank) the real
                // definitions it forwards to.
                if child.child_by_field_name("body").is_some()
                    && let Some(name) = ctx.field_text(child, "name")
                {
                    push(ctx, out, &name, Kind::Module, child, parent);
                    let qualified = qualify(parent, &name, "::");
                    walk(ctx, child, Some(&qualified), out);
                }
            }
            "impl_item" => {
                // an impl isn't a symbol; its `type` becomes the parent of the
                // methods inside it
                let ty = ctx.field_text(child, "type").map(|t| base_type(&t));
                let qualified = match &ty {
                    Some(t) => qualify(parent, t, "::"),
                    None => parent.map(str::to_string).unwrap_or_default(),
                };
                let p = if qualified.is_empty() {
                    None
                } else {
                    Some(qualified.as_str())
                };
                walk(ctx, child, p, out);
            }
            _ => walk(ctx, child, parent, out),
        }
    }
}

/// Emit a symbol carrying the item's declared visibility.
fn push(ctx: &Ctx, out: &mut Vec<Symbol>, name: &str, kind: Kind, node: Node, p: Option<&str>) {
    let mut s = ctx.symbol(name, kind, node, p);
    s.visibility = Some(visibility(ctx, node));
    out.push(s);
}

/// The item's declared visibility: `pub` → public, any scoped `pub(...)` →
/// crate, none → private (Rust's default).
fn visibility(ctx: &Ctx, node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = ctx.node_text(child).unwrap_or_default();
            return if text.contains('(') {
                "crate"
            } else {
                "public"
            };
        }
    }
    "private"
}

/// Whether an fn declares a `self` receiver (an instance method).
fn has_self(node: Node) -> bool {
    node.child_by_field_name("parameters")
        .is_some_and(|params| {
            let mut cursor = params.walk();
            params
                .children(&mut cursor)
                .any(|p| p.kind() == "self_parameter")
        })
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

        // a free fn is a function; an fn with a self receiver is a method
        let build = find(&syms, "build");
        assert_eq!(build.kind, Kind::Function);
        assert_eq!(build.parent, None);

        // an associated fn (no self) is a *function* of the type, not a method
        let new = find(&syms, "new");
        assert_eq!(new.kind, Kind::Function);
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

    #[test]
    fn visibility_reflects_the_pub_modifier() {
        let src = "pub fn open() {}\npub(crate) fn shared() {}\nfn helper() {}\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "open").visibility, Some("public"));
        assert_eq!(find(&syms, "shared").visibility, Some("crate"));
        assert_eq!(find(&syms, "helper").visibility, Some("private"));
    }
}
