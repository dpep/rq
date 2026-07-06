//! Go plugin. Extracts `func` (free → function, with a receiver → method),
//! `type … struct` → struct, and `type … interface` → trait (Go's interface is
//! the same "named contract" concept). Methods are qualified by their receiver
//! type (`Handle · Server`); interface method signatures by the interface.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with};

const LANGUAGE: &str = "go";

pub struct Go;

impl LanguagePlugin for Go {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        extract_with(
            LANGUAGE,
            tree_sitter_go::LANGUAGE.into(),
            file,
            source,
            |ctx, root, out| walk(ctx, root, None, out),
        )
    }
}

fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Function, child, parent);
                }
            }
            "method_declaration" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    // qualify by the receiver type: `func (s *Server) Handle()`
                    let recv = child
                        .child_by_field_name("receiver")
                        .and_then(|r| type_identifier(ctx, r));
                    push(ctx, out, &name, Kind::Method, child, recv.as_deref());
                }
            }
            "type_spec" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    match child.child_by_field_name("type").map(|t| t.kind()) {
                        Some("struct_type") => {
                            push(ctx, out, &name, Kind::Struct, child, parent);
                        }
                        Some("interface_type") => {
                            push(ctx, out, &name, Kind::Trait, child, parent);
                            // interface method signatures are methods of it
                            walk(ctx, child, Some(&name), out);
                        }
                        _ => {}
                    }
                }
            }
            // interface method signatures (node name varies by grammar version)
            "method_spec" | "method_elem" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Method, child, parent);
                }
            }
            _ => walk(ctx, child, parent, out),
        }
    }
}

/// Emit a symbol carrying Go's capitalization-is-visibility convention:
/// an exported (uppercase) name is public, an unexported one private.
fn push(ctx: &Ctx, out: &mut Vec<Symbol>, name: &str, kind: Kind, node: Node, p: Option<&str>) {
    let mut s = ctx.symbol(name, kind, node, p);
    s.visibility = Some(if name.chars().next().is_some_and(char::is_uppercase) {
        "public"
    } else {
        "private"
    });
    out.push(s);
}

/// The first `type_identifier` within `node` — used to pull the bare type
/// name out of a receiver like `(s *Server)` or `(s *Stack[T])`.
fn type_identifier(ctx: &Ctx, node: Node) -> Option<String> {
    if node.kind() == "type_identifier" {
        return ctx.node_text(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(name) = type_identifier(ctx, child) {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<Symbol> {
        Go.extract("test.go", source)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }

    #[test]
    fn extracts_funcs_types_and_methods() {
        let src = r#"
package widget

type Widget struct {
	Size int
}

type Renderer interface {
	Render() string
}

func (w *Widget) Resize(n int) {
	w.Size = n
}

func Build() *Widget {
	return &Widget{}
}
"#;
        let syms = extract(src);

        assert_eq!(find(&syms, "Widget").kind, Kind::Struct);
        assert_eq!(find(&syms, "Renderer").kind, Kind::Trait);

        // a free func vs a method qualified by its receiver type
        let build = find(&syms, "Build");
        assert_eq!(build.kind, Kind::Function);
        assert_eq!(build.parent, None);

        let resize = find(&syms, "Resize");
        assert_eq!(resize.kind, Kind::Method);
        assert_eq!(resize.parent.as_deref(), Some("Widget"));

        // an interface method signature is a method of the interface
        let render = find(&syms, "Render");
        assert_eq!(render.kind, Kind::Method);
        assert_eq!(render.parent.as_deref(), Some("Renderer"));

        assert_eq!(build.language, "go");
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("package x\n").is_empty());
    }

    #[test]
    fn capitalization_is_visibility() {
        let src = "package x\n\nfunc Exported() {}\nfunc internal() {}\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "Exported").visibility, Some("public"));
        assert_eq!(find(&syms, "internal").visibility, Some("private"));
    }
}
