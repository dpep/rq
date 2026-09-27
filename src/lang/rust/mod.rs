//! Rust plugin — the second language, and what rq dogfoods on its own source.
//!
//! Extracts the definitions you navigate to: `fn` (free → function, inside an
//! `impl`/`trait` → method), `struct`, `enum` and its variants, `trait`, `mod`,
//! `type` aliases, `macro_rules!`, and `const`/`static` (→ constant). `parent`
//! carries the enclosing qualified name (`::`-joined) so a method renders as
//! `bar · Foo` and a nested type as `outer · mod`. `impl` blocks aren't symbols
//! themselves; they just supply the parent for the methods inside them.
//!
//! Items inside a braced macro call at item level (`cfg_rt! { pub struct … }`,
//! the way tokio gates half its API) are extracted too: the grammar sees the
//! body as opaque tokens, so it is re-parsed as source, and kept only when it
//! parses cleanly as items.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with, qualify};

const LANGUAGE: &str = "rust";

pub(crate) struct Rust;

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
            "struct_item" | "union_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Struct, child, parent);
                }
            }
            "enum_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Enum, child, parent);
                    variants(ctx, child, &qualify(parent, &name, "::"), out);
                }
            }
            // An associated type in an impl or trait (`type Output = …;`) is a
            // trait's required slot, not a definition anyone navigates to, and
            // every `Iterator` impl would add another `Item`.
            "type_item" if !in_impl_or_trait(child) => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Type, child, parent);
                }
            }
            "macro_definition" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let mut s = ctx.symbol(&name, Kind::Macro, child, parent);
                    // `macro_rules!` has no `pub`: `#[macro_export]` makes it
                    // public API, otherwise it's in scope within the crate
                    s.visibility = Some(if exported(ctx, child) {
                        "public"
                    } else {
                        "crate"
                    });
                    out.push(s);
                }
            }
            "macro_invocation" => {
                if let Some(body) = braced_body(child) {
                    ctx.walk_fragment(&tree_sitter_rust::LANGUAGE.into(), body, |ctx, root| {
                        walk(ctx, root, parent, out)
                    });
                }
            }
            "const_item" | "static_item" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Constant, child, parent);
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

/// Each variant of an enum, as a child of the enum and with its visibility.
fn variants(ctx: &Ctx, node: Node, qualified: &str, out: &mut Vec<Symbol>) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let vis = visibility(ctx, node);
    let mut cursor = body.walk();
    for v in body.named_children(&mut cursor) {
        if v.kind() == "enum_variant"
            && let Some(name) = ctx.field_text(v, "name")
        {
            let mut s = ctx.symbol(&name, Kind::Variant, v, Some(qualified));
            s.visibility = Some(vis);
            out.push(s);
        }
    }
}

/// Whether an item sits directly in an `impl` or `trait` body.
fn in_impl_or_trait(node: Node) -> bool {
    node.parent()
        .and_then(|list| list.parent())
        .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"))
}

/// Whether a `macro_rules!` carries `#[macro_export]` among the attributes
/// directly above it.
fn exported(ctx: &Ctx, node: Node) -> bool {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev.filter(|p| matches!(p.kind(), "attribute_item" | "line_comment")) {
        if ctx.node_text(p).is_some_and(|t| t.contains("macro_export")) {
            return true;
        }
        prev = p.prev_sibling();
    }
    false
}

/// The inside of a `{ … }` macro call's body, as (start byte, end byte, row).
/// Parenthesized and bracketed calls are expressions, not item blocks.
fn braced_body(node: Node) -> Option<(usize, usize, usize)> {
    let mut cursor = node.walk();
    let tt = node
        .children(&mut cursor)
        .find(|c| c.kind() == "token_tree")?;
    let open = tt.child(0)?;
    (open.kind() == "{").then(|| (open.end_byte(), tt.end_byte() - 1, tt.start_position().row))
}

/// Emit a symbol carrying the item's declared visibility.
fn push(ctx: &Ctx, out: &mut Vec<Symbol>, name: &str, kind: Kind, node: Node, p: Option<&str>) {
    let mut s = ctx.symbol(name, kind, node, p);
    s.visibility = item_visibility(ctx, node);
    out.push(s);
}

/// An item's visibility. A trait's items carry none of their own: they are as
/// visible as the trait. A trait impl's are as visible as a trait this file
/// may not declare, so they're unknown rather than private.
fn item_visibility(ctx: &Ctx, node: Node) -> Option<&'static str> {
    let owner = node.parent().and_then(|list| list.parent());
    match owner {
        Some(t) if t.kind() == "trait_item" => Some(visibility(ctx, t)),
        Some(i) if i.kind() == "impl_item" && i.child_by_field_name("trait").is_some() => None,
        _ => Some(visibility(ctx, node)),
    }
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
    use crate::lang::testing::find;

    fn extract(source: &str) -> Vec<Symbol> {
        Rust.extract("test.rs", source)
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

        // a trait method signature is a method of the trait, and as public
        let render = find(&syms, "render");
        assert_eq!(render.kind, Kind::Method);
        assert_eq!(render.parent.as_deref(), Some("Render"));
        assert_eq!(render.visibility, Some("public"));

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
    fn consts_and_statics_are_constants() {
        let src = r#"
pub const MAX: u32 = 10;
static NAME: &str = "x";

pub struct Widget;

impl Widget {
    pub const DEFAULT: u32 = 1;
}
"#;
        let syms = extract(src);
        let max = find(&syms, "MAX");
        assert_eq!(max.kind, Kind::Constant);
        assert_eq!(max.visibility, Some("public"));
        assert_eq!(find(&syms, "NAME").visibility, Some("private"));
        // an associated const belongs to its impl's type
        assert_eq!(find(&syms, "DEFAULT").parent.as_deref(), Some("Widget"));
    }

    #[test]
    fn variants_aliases_and_macros_are_definitions() {
        let src = r#"
pub enum Shape {
    Circle,
    Square { side: u32 },
}

pub type Result<T> = std::result::Result<T, Error>;

#[macro_export]
macro_rules! shout {
    () => {};
}

macro_rules! helper {
    () => {};
}

impl Iterator for Walker {
    type Item = u32;
    fn next(&mut self) -> Option<u32> { None }
}
"#;
        let syms = extract(src);
        // a trait impl's method is as visible as the trait, which isn't here
        assert_eq!(find(&syms, "next").visibility, None);
        let square = find(&syms, "Square");
        assert_eq!(square.kind, Kind::Variant);
        assert_eq!(square.parent.as_deref(), Some("Shape"));
        assert_eq!(square.visibility, Some("public"));
        assert_eq!(find(&syms, "Result").kind, Kind::Type);
        let shout = find(&syms, "shout");
        assert_eq!(shout.kind, Kind::Macro);
        assert_eq!(shout.visibility, Some("public"));
        assert_eq!(find(&syms, "helper").visibility, Some("crate"));
        // an associated type is the trait's slot, not a definition
        assert!(!syms.iter().any(|s| s.name == "Item"), "{syms:?}");
    }

    #[test]
    fn items_inside_a_braced_macro_call_keep_their_lines() {
        let src = r#"
cfg_rt! {
    /// docs
    pub struct JoinHandle<T> {
        raw: T,
    }

    cfg_net! {
        pub fn spawn() {}
    }
}

impl Runtime {
    cfg_rt! {
        pub fn block_on(&self) {}
    }
}

not_items! { a => b, c }
"#;
        let syms = extract(src);
        let handle = find(&syms, "JoinHandle");
        assert_eq!(
            (handle.kind, handle.line, handle.end_line),
            (Kind::Struct, 4, 6)
        );
        assert_eq!(handle.visibility, Some("public"));
        // nested calls, and calls inside an impl, which keep the impl's type
        assert_eq!(find(&syms, "spawn").line, 9);
        let block_on = find(&syms, "block_on");
        assert_eq!(block_on.parent.as_deref(), Some("Runtime"));
        assert_eq!(block_on.line, 15);
        // a body that isn't items yields nothing rather than noise
        assert_eq!(syms.len(), 3, "{syms:?}");
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
