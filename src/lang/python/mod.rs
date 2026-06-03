//! Python plugin. Extracts `class` → class and `def` (free → function, inside a
//! class → method), qualified with `.` (`method · Account`, nested class
//! `Inner · Outer`). Decorators are transparent — the wrapped def is what counts.

use tree_sitter::{Node, Parser};

use crate::core::{Kind, Symbol};
use crate::lang::LanguagePlugin;

const LANGUAGE: &str = "python";

pub struct Python;

impl LanguagePlugin for Python {
    fn extensions(&self) -> &[&str] {
        &["py"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
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
    /// `parent` is the enclosing qualified name; `in_class` is true inside a
    /// class body, where a `def` is a method.
    fn walk(&self, node: Node, parent: Option<&str>, in_class: bool, out: &mut Vec<Symbol>) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_definition" => {
                    if let Some(name) = self.field_text(child, "name") {
                        out.push(self.symbol(&name, Kind::Class, child, parent));
                        let qualified = qualify(parent, &name);
                        self.walk(child, Some(&qualified), true, out);
                    }
                }
                "function_definition" => {
                    if let Some(name) = self.field_text(child, "name") {
                        let kind = if in_class {
                            Kind::Method
                        } else {
                            Kind::Function
                        };
                        out.push(self.symbol(&name, kind, child, parent));
                    }
                    // don't descend into a def body (nested defs rarely navigated)
                }
                // a decorated class/function: descend so the wrapped def is seen
                // in the same context
                _ => self.walk(child, parent, in_class, out),
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
        Some(p) => format!("{p}.{name}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<Symbol> {
        Python.extract("test.py", source)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }

    #[test]
    fn extracts_classes_methods_and_functions() {
        let src = r#"
class Account:
    def deposit(self, amount):
        pass

    @property
    def balance(self):
        return 0

def build():
    return Account()
"#;
        let syms = extract(src);

        let account = find(&syms, "Account");
        assert_eq!(account.kind, Kind::Class);
        assert_eq!(account.parent, None);

        let deposit = find(&syms, "deposit");
        assert_eq!(deposit.kind, Kind::Method);
        assert_eq!(deposit.parent.as_deref(), Some("Account"));

        // a decorated method is still found, still a method
        assert_eq!(find(&syms, "balance").kind, Kind::Method);

        // a module-level def is a function
        let build = find(&syms, "build");
        assert_eq!(build.kind, Kind::Function);
        assert_eq!(build.parent, None);

        assert_eq!(account.language, "python");
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("# just a comment\n").is_empty());
    }
}
