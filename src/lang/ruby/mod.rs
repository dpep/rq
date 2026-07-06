//! Ruby plugin — the first language.
//!
//! Extracts classes, modules, and methods (instance and singleton) via
//! Tree-sitter. `parent` carries the enclosing qualified name so a method
//! renders as `Foo::Bar#baz` and a nested class as `Foo::Bar`.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with, qualify};

const LANGUAGE: &str = "ruby";

pub struct Ruby;

impl LanguagePlugin for Ruby {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["rb"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        extract_with(
            LANGUAGE,
            tree_sitter_ruby::LANGUAGE.into(),
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
            "class" | "module" => {
                let kind = if child.kind() == "class" {
                    Kind::Class
                } else {
                    Kind::Module
                };
                if let Some(name) = ctx.field_text(child, "name") {
                    // a compact definition (`class A::B::C`) names the leaf `C`
                    // with `A::B` folded into the parent — same shape as the
                    // nested `module A; module B; class C` form, so the class
                    // is found by its leaf name either way
                    let (leaf, prefix) = split_qualified(&name);
                    let effective_parent = match prefix {
                        Some(p) => Some(qualify(parent, p, "::")),
                        None => parent.map(str::to_string),
                    };
                    out.push(ctx.symbol(leaf, kind, child, effective_parent.as_deref()));
                    let qualified = qualify(effective_parent.as_deref(), leaf, "::");
                    walk(ctx, child, Some(&qualified), out);
                } else {
                    walk(ctx, child, parent, out);
                }
            }
            "method" | "singleton_method" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    out.push(ctx.symbol(&name, Kind::Method, child, parent));
                }
                // method bodies rarely hold further definitions; don't recurse.
            }
            _ => walk(ctx, child, parent, out),
        }
    }
}

/// Split a possibly compact-qualified definition name (`A::B::C`) into its leaf
/// (`C`) and namespace prefix (`A::B`). A plain name has no prefix; a leading
/// `::` (absolute `::Foo`) yields no prefix either.
fn split_qualified(name: &str) -> (&str, Option<&str>) {
    match name.rfind("::") {
        Some(i) => {
            let prefix = &name[..i];
            (&name[i + 2..], (!prefix.is_empty()).then_some(prefix))
        }
        None => (name, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<Symbol> {
        Ruby.extract("test.rb", source)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }

    #[test]
    fn extracts_class_module_and_methods_with_nesting() {
        let src = r#"
module Billing
  class RefundProcessor
    def perform
    end

    def self.build
    end
  end
end
"#;
        let syms = extract(src);

        let module = find(&syms, "Billing");
        assert_eq!(module.kind, Kind::Module);
        assert_eq!(module.parent, None);
        assert_eq!(module.line, 2);
        // end_line spans the whole body to the matching `end`
        assert_eq!(module.end_line, 10);

        let class = find(&syms, "RefundProcessor");
        assert_eq!(class.kind, Kind::Class);
        assert_eq!(class.parent.as_deref(), Some("Billing"));

        let perform = find(&syms, "perform");
        assert_eq!(perform.kind, Kind::Method);
        assert_eq!(perform.parent.as_deref(), Some("Billing::RefundProcessor"));
        // the method body is lines 4..=5 (`def perform` through its `end`)
        assert_eq!((perform.line, perform.end_line), (4, 5));

        // singleton method (def self.build) is captured too
        let build = find(&syms, "build");
        assert_eq!(build.kind, Kind::Method);
        assert_eq!(build.parent.as_deref(), Some("Billing::RefundProcessor"));
    }

    #[test]
    fn compact_namespace_is_split_into_leaf_and_parent() {
        // `class A::B::C` names the leaf `C`, with `A::B` folded into the parent —
        // so it's found by its leaf name just like the nested form, and a method
        // inside it still qualifies fully
        let src = "class My::Module::EmployeesController\n  def index\n  end\nend\n";
        let syms = extract(src);

        let class = find(&syms, "EmployeesController");
        assert_eq!(class.kind, Kind::Class);
        assert_eq!(class.parent.as_deref(), Some("My::Module"));

        let index = find(&syms, "index");
        assert_eq!(
            index.parent.as_deref(),
            Some("My::Module::EmployeesController")
        );
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("# just a comment\n").is_empty());
    }

    #[test]
    fn language_tag_is_set() {
        let syms = extract("class Foo\nend\n");
        assert_eq!(syms[0].language, "ruby");
    }
}
