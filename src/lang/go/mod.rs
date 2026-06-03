//! Go plugin. Extracts `func` (free → function, with a receiver → method),
//! `type … struct` → struct, and `type … interface` → trait (Go's interface is
//! the same "named contract" concept). Methods are qualified by their receiver
//! type (`Handle · Server`); interface method signatures by the interface.

use tree_sitter::{Node, Parser};

use crate::core::{Kind, Symbol};
use crate::lang::LanguagePlugin;

const LANGUAGE: &str = "go";

pub struct Go;

impl LanguagePlugin for Go {
    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
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
        ctx.walk(tree.root_node(), None, &mut out);
        out
    }
}

struct Ctx<'a> {
    src: &'a [u8],
    file: &'a str,
}

impl Ctx<'_> {
    fn walk(&self, node: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "function_declaration" => {
                    if let Some(name) = self.field_text(child, "name") {
                        out.push(self.symbol(&name, Kind::Function, child, parent));
                    }
                }
                "method_declaration" => {
                    if let Some(name) = self.field_text(child, "name") {
                        // qualify by the receiver type: `func (s *Server) Handle()`
                        let recv = child
                            .child_by_field_name("receiver")
                            .and_then(|r| self.type_identifier(r));
                        out.push(self.symbol(&name, Kind::Method, child, recv.as_deref()));
                    }
                }
                "type_spec" => {
                    if let Some(name) = self.field_text(child, "name") {
                        match child.child_by_field_name("type").map(|t| t.kind()) {
                            Some("struct_type") => {
                                out.push(self.symbol(&name, Kind::Struct, child, parent));
                            }
                            Some("interface_type") => {
                                out.push(self.symbol(&name, Kind::Trait, child, parent));
                                // interface method signatures are methods of it
                                self.walk(child, Some(&name), out);
                            }
                            _ => {}
                        }
                    }
                }
                // interface method signatures (node name varies by grammar version)
                "method_spec" | "method_elem" => {
                    if let Some(name) = self.field_text(child, "name") {
                        out.push(self.symbol(&name, Kind::Method, child, parent));
                    }
                }
                _ => self.walk(child, parent, out),
            }
        }
    }

    fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .and_then(|n| n.utf8_text(self.src).ok())
            .map(str::to_string)
    }

    /// The first `type_identifier` within `node` — used to pull the bare type
    /// name out of a receiver like `(s *Server)` or `(s *Stack[T])`.
    fn type_identifier(&self, node: Node) -> Option<String> {
        if node.kind() == "type_identifier" {
            return node.utf8_text(self.src).ok().map(str::to_string);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(name) = self.type_identifier(child) {
                return Some(name);
            }
        }
        None
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
}
