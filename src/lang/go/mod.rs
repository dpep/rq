//! Go plugin. Extracts `func` (free → function, with a receiver → method),
//! `type … struct` → struct, `type … interface` → trait (Go's interface is
//! the same "named contract" concept), and any other named type or alias
//! (`type HandlerFunc func(*Context)`, `type ID = string`) → type. Methods are qualified by their receiver
//! type (`Handle · Server`); interface method signatures by the interface, and
//! a struct's fields (embedded ones named by their type) → field of the struct.
//! A package-level `const` (single or grouped, iota included) → constant. A
//! package-level `var` is not: it's mutable state, and calling it a constant
//! would mislabel it — even the `var ErrFoo = errors.New(…)` sentinels that are
//! constant in all but name.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with};

const LANGUAGE: &str = "go";

pub(crate) struct Go;

impl LanguagePlugin for Go {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn aliases(&self) -> &[&str] {
        &["golang"]
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
                            fields(ctx, child, &name, out);
                        }
                        Some("interface_type") => {
                            push(ctx, out, &name, Kind::Trait, child, parent);
                            // interface method signatures are methods of it
                            walk(ctx, child, Some(&name), out);
                        }
                        _ => push(ctx, out, &name, Kind::Type, child, parent),
                    }
                }
            }
            "type_alias" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    push(ctx, out, &name, Kind::Type, child, parent);
                }
            }
            // only at package level — a func body's consts are locals
            "const_declaration" if node.kind() == "source_file" => {
                constants(ctx, child, out);
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

/// Emit each name a `const` declaration binds, at its own spec's line — a
/// grouped `const ( … )` is one declaration of many specs. `_` binds nothing.
fn constants(ctx: &Ctx, decl: Node, out: &mut Vec<Symbol>) {
    let mut specs = decl.walk();
    for spec in decl.named_children(&mut specs) {
        if spec.kind() != "const_spec" {
            continue;
        }
        let mut names = spec.walk();
        for ident in spec.children_by_field_name("name", &mut names) {
            if ident.kind() == "identifier"
                && let Some(name) = ctx.node_text(ident)
                && name != "_"
            {
                push(ctx, out, &name, Kind::Constant, spec, None);
            }
        }
    }
}

/// Emit each field a struct type declares, as a child of the struct. An
/// embedded field (`*Base`, `pkg.Config`) is named by its type, as Go names it.
/// A nested anonymous struct's fields stay out: they're reached through a field
/// that is already here.
fn fields(ctx: &Ctx, spec: Node, owner: &str, out: &mut Vec<Symbol>) {
    let Some(list) = spec.child_by_field_name("type").and_then(|t| {
        let mut cursor = t.walk();
        t.named_children(&mut cursor)
            .find(|c| c.kind() == "field_declaration_list")
    }) else {
        return;
    };
    let mut cursor = list.walk();
    for decl in list.named_children(&mut cursor) {
        if decl.kind() != "field_declaration" {
            continue;
        }
        let mut names = decl.walk();
        let named: Vec<String> = decl
            .children_by_field_name("name", &mut names)
            .filter_map(|n| ctx.node_text(n))
            .collect();
        let named = if named.is_empty() {
            decl.child_by_field_name("type")
                .and_then(|t| embedded_name(ctx, t))
                .into_iter()
                .collect()
        } else {
            named
        };
        for name in named.iter().filter(|n| *n != "_") {
            push(ctx, out, name, Kind::Field, decl, Some(owner));
        }
    }
}

/// The name an embedded field goes by: its type's own name, without a pointer,
/// package qualifier or type arguments (`*pkg.List[T]` → `List`).
fn embedded_name(ctx: &Ctx, ty: Node) -> Option<String> {
    match ty.kind() {
        "type_identifier" => ctx.node_text(ty),
        "qualified_type" => ctx.field_text(ty, "name"),
        "generic_type" => ty
            .child_by_field_name("type")
            .and_then(|t| embedded_name(ctx, t)),
        _ => {
            let mut cursor = ty.walk();
            ty.named_children(&mut cursor)
                .find_map(|c| embedded_name(ctx, c))
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
    use crate::lang::testing::find;

    fn extract(source: &str) -> Vec<Symbol> {
        Go.extract("test.go", source)
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
    fn struct_fields_are_fields_embedded_ones_named_by_their_type() {
        let src = r#"
package widget

type Frame struct {
	*Base
	pkg.Config
	List[int]
	X, Y  int
	title string `json:"title"`
	Inner struct{ Deep int }
	_     int
}
"#;
        let syms = extract(src);

        for name in ["Base", "Config", "List", "X", "Y", "Inner"] {
            let f = find(&syms, name);
            assert_eq!(
                (f.kind, f.parent.as_deref()),
                (Kind::Field, Some("Frame")),
                "{name}"
            );
            assert_eq!(f.visibility, Some("public"));
        }
        assert_eq!(find(&syms, "title").visibility, Some("private"));
        // a nested anonymous struct's fields are reached through `Inner`
        for absent in ["Deep", "_"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
    }

    #[test]
    fn named_types_and_aliases_are_types() {
        let src = r#"
package widget

type HandlerFunc func(*Widget)
type HandlersChain []HandlerFunc
type state int
type Info = other.Info
type (
	Size  int
	Label = string
)
type Stack[T any] []T
"#;
        let syms = extract(src);
        for name in [
            "HandlerFunc",
            "HandlersChain",
            "Info",
            "Size",
            "Label",
            "Stack",
        ] {
            let s = find(&syms, name);
            assert_eq!(s.kind, Kind::Type, "{name}");
            assert_eq!(s.visibility, Some("public"), "{name}");
        }
        assert_eq!(find(&syms, "state").visibility, Some("private"));
    }

    #[test]
    fn package_level_consts_are_constants() {
        let src = r#"
package widget

const MaxRetries = 3

const (
	StateIdle State = iota
	stateBusy
	_
)

const Low, High = 1, 9

var DefaultWidget = Widget{}

func Build() {
	const localLimit = 5
}
"#;
        let syms = extract(src);

        let max = find(&syms, "MaxRetries");
        assert_eq!(max.kind, Kind::Constant);
        assert_eq!(max.parent, None);
        assert_eq!(max.visibility, Some("public"));

        // each name in a grouped block sits on its own line
        let idle = find(&syms, "StateIdle");
        assert_eq!(idle.kind, Kind::Constant);
        let busy = find(&syms, "stateBusy");
        assert_eq!(busy.line, idle.line + 1);
        assert_eq!(busy.visibility, Some("private"));

        // one spec may bind several names
        assert_eq!(find(&syms, "Low").kind, Kind::Constant);
        assert_eq!(find(&syms, "High").kind, Kind::Constant);

        // not constants: `_`, a mutable var, a function-local const
        for absent in ["_", "DefaultWidget", "localLimit"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
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
