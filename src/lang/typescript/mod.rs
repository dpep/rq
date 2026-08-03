//! TypeScript / JavaScript plugin. One grammar family, two language tags:
//! JavaScript is TypeScript with the types taken out, so both share a walk and
//! `-x ts` / `-x js` still mean what you'd expect.
//!
//! Extracts `class` → class, `interface` → trait (a named contract, like Go's),
//! `type` → struct (a named shape), `enum` → enum, `namespace` → module,
//! `function` → function, and class/interface members → method. A
//! `const f = () => …` is a function too — in modern JS that *is* how functions
//! are declared. `parent` is `.`-joined, so a method renders as `deposit ·
//! Account`.
//!
//! Visibility: a class member takes its `private`/`protected` modifier (or `#`
//! prefix); anything module-level reads public when `export`ed and private when
//! not. That last convention is ESM's — a CommonJS file (`module.exports = …`)
//! exports nothing the grammar can see, so its definitions all read private.
//! Visibility is only ever a small ranking nudge, so the mislabel costs little.

use tree_sitter::{Language, Node};

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with_key, qualify};

const TYPESCRIPT: &str = "typescript";
const JAVASCRIPT: &str = "javascript";

/// A grammar paired with the parser-cache key naming it. The key identifies the
/// *grammar*, not the language tag, so the two are never named apart.
type Grammar = (&'static str, Language);

pub struct TypeScript;
pub struct JavaScript;

impl LanguagePlugin for TypeScript {
    fn language(&self) -> &'static str {
        TYPESCRIPT
    }

    fn extensions(&self) -> &[&str] {
        &["ts", "mts", "cts", "tsx"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        // The two grammars disagree on `<T>`: TSX reads it as a JSX tag, TS as a
        // type parameter. Give each file the one it means.
        let grammar = if is_tsx(file) { tsx() } else { ts() };
        run(TYPESCRIPT, grammar, file, source)
    }
}

impl LanguagePlugin for JavaScript {
    fn language(&self) -> &'static str {
        JAVASCRIPT
    }

    fn extensions(&self) -> &[&str] {
        &["js", "mjs", "cjs", "jsx"]
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        // TSX is the JSX-aware superset — it parses plain JS, and `.js` holding
        // JSX is routine in React projects.
        run(JAVASCRIPT, tsx(), file, source)
    }
}

fn ts() -> Grammar {
    ("ts", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
}

fn tsx() -> Grammar {
    ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into())
}

fn run(language: &'static str, (key, grammar): Grammar, file: &str, source: &str) -> Vec<Symbol> {
    extract_with_key(key, language, grammar, file, source, |ctx, root, out| {
        walk(ctx, root, None, false, out)
    })
}

/// Whether `file` is a `.tsx` — the JSX-bearing dialect of TypeScript.
fn is_tsx(file: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsx"))
}

/// Recursively collect definitions. `parent` is the enclosing qualified name;
/// `exported` is set while walking under an `export`.
fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, exported: bool, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // `export …` isn't a definition; it marks the one that follows public
            "export_statement" => walk(ctx, child, parent, true, out),

            // types that hold members: emit, then descend so the members are
            // qualified by them
            "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "internal_module" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = match child.kind() {
                        "interface_declaration" => Kind::Trait,
                        "internal_module" => Kind::Module,
                        _ => Kind::Class,
                    };
                    let vis = module_visibility(exported);
                    push(ctx, out, &name, kind, child, parent, vis);
                    let qualified = qualify(parent, &name, ".");
                    // members carry their own visibility; a namespace body
                    // re-declares `export` for what it re-exports
                    walk(ctx, child, Some(&qualified), false, out);
                }
            }

            // a type alias names a shape; an enum names a closed set
            "type_alias_declaration" | "enum_declaration" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = match child.kind() {
                        "enum_declaration" => Kind::Enum,
                        _ => Kind::Struct,
                    };
                    let vis = module_visibility(exported);
                    push(ctx, out, &name, kind, child, parent, vis);
                }
            }

            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let vis = module_visibility(exported);
                    push(ctx, out, &name, Kind::Function, child, parent, vis);
                }
                // bodies hold locals and callbacks, not navigation targets
            }

            // `const handler = () => …` — the modern function declaration
            "lexical_declaration" | "variable_declaration" => {
                declared_functions(ctx, child, parent, module_visibility(exported), out);
            }

            // class and interface members
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                push_member(ctx, out, child, parent);
            }

            // `handleClick = () => …` in a class body: a method but for syntax
            "public_field_definition" | "field_definition" => {
                if is_function(child.child_by_field_name("value")) {
                    push_member(ctx, out, child, parent);
                }
            }

            // never descend into a function body reached some other way (a
            // callback argument, an IIFE) — its locals aren't definitions
            "arrow_function" | "function_expression" | "function" => {}

            _ => walk(ctx, child, parent, exported, out),
        }
    }
}

/// Emit a member of a type as a method. An ES private name (`#tally`) is
/// indexed without its `#`, so it's found by the name you'd think to search.
fn push_member(ctx: &Ctx, out: &mut Vec<Symbol>, node: Node, parent: Option<&str>) {
    if let Some(raw) = ctx.field_text(node, "name") {
        let vis = member_visibility(ctx, node, &raw);
        let name = raw.trim_start_matches('#');
        push(ctx, out, name, Kind::Method, node, parent, vis);
    }
}

/// Emit the function-valued declarators of a `const`/`let`/`var` statement.
fn declared_functions(
    ctx: &Ctx,
    node: Node,
    parent: Option<&str>,
    visibility: &'static str,
    out: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for d in node.children(&mut cursor) {
        if d.kind() != "variable_declarator" || !is_function(d.child_by_field_name("value")) {
            continue;
        }
        if let Some(name) = ctx.field_text(d, "name") {
            // span the whole statement, so `end_line` covers the closing brace
            push(ctx, out, &name, Kind::Function, node, parent, visibility);
        }
    }
}

/// Whether a declarator's value is a function in some spelling.
fn is_function(value: Option<Node>) -> bool {
    matches!(
        value.map(|v| v.kind()),
        Some("arrow_function" | "function_expression" | "function")
    )
}

fn push(
    ctx: &Ctx,
    out: &mut Vec<Symbol>,
    name: &str,
    kind: Kind,
    node: Node,
    parent: Option<&str>,
    visibility: &'static str,
) {
    let mut s = ctx.symbol(name, kind, node, parent);
    s.visibility = Some(visibility);
    out.push(s);
}

/// ESM's convention: what a module exports is its public API.
fn module_visibility(exported: bool) -> &'static str {
    if exported { "public" } else { "private" }
}

/// A member's declared access: the TypeScript modifier if it has one, else the
/// `#` prefix of an ES private name, else public (both languages' default).
fn member_visibility(ctx: &Ctx, node: Node, name: &str) -> &'static str {
    if name.starts_with('#') {
        return "private";
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            return match ctx.node_text(child).as_deref() {
                Some("private") => "private",
                Some("protected") => "protected",
                _ => "public",
            };
        }
    }
    "public"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<Symbol> {
        TypeScript.extract("test.ts", source)
    }

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol named {name} in {syms:?}"))
    }

    #[test]
    fn extracts_types_functions_and_members() {
        let src = r#"
export interface Renderer {
  render(): string;
}

export type Size = { width: number };

export enum Color {
  Red,
}

export class Widget implements Renderer {
  render(): string {
    return "";
  }

  private resize(n: number) {}
}

export function buildWidget(): Widget {
  return new Widget();
}

export const makeWidget = () => new Widget();
"#;
        let syms = extract(src);

        assert_eq!(find(&syms, "Renderer").kind, Kind::Trait);
        assert_eq!(find(&syms, "Size").kind, Kind::Struct);
        assert_eq!(find(&syms, "Color").kind, Kind::Enum);

        let widget = find(&syms, "Widget");
        assert_eq!(widget.kind, Kind::Class);
        assert_eq!(widget.parent, None);
        assert_eq!(widget.language, "typescript");

        // members are methods qualified by the type that holds them
        let render = find(&syms, "render");
        assert_eq!(render.kind, Kind::Method);
        // both the class method and the interface signature are recorded
        let renders: Vec<_> = syms.iter().filter(|s| s.name == "render").collect();
        assert_eq!(renders.len(), 2, "{syms:?}");
        assert!(
            renders
                .iter()
                .any(|s| s.parent.as_deref() == Some("Widget"))
        );
        assert!(
            renders
                .iter()
                .any(|s| s.parent.as_deref() == Some("Renderer"))
        );

        assert_eq!(find(&syms, "buildWidget").kind, Kind::Function);
        // an arrow assigned to a const is a function, not a mystery
        assert_eq!(find(&syms, "makeWidget").kind, Kind::Function);
    }

    #[test]
    fn qualifies_through_namespaces() {
        let src = "namespace Outer {\n  export class Store {\n    get() {}\n  }\n}\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "Outer").kind, Kind::Module);
        assert_eq!(find(&syms, "Store").parent.as_deref(), Some("Outer"));
        assert_eq!(find(&syms, "get").parent.as_deref(), Some("Outer.Store"));
    }

    #[test]
    fn callback_locals_are_not_definitions() {
        // a helper defined inside a test callback isn't a navigation target
        let src = "describe('widget', () => {\n  const helper = () => 1;\n});\n";
        assert!(extract(src).is_empty(), "{:?}", extract(src));
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("// just a comment\n").is_empty());
    }

    #[test]
    fn visibility_reflects_exports_and_member_modifiers() {
        let src = r#"
export function open() {}
function helper() {}

export class Account {
  deposit() {}
  private audit() {}
  protected hook() {}
  #secret() {}
}
"#;
        let syms = extract(src);
        assert_eq!(find(&syms, "open").visibility, Some("public"));
        assert_eq!(find(&syms, "helper").visibility, Some("private"));
        assert_eq!(find(&syms, "deposit").visibility, Some("public"));
        assert_eq!(find(&syms, "audit").visibility, Some("private"));
        assert_eq!(find(&syms, "hook").visibility, Some("protected"));
        // an ES private name is private, and navigable without the `#`
        assert_eq!(find(&syms, "secret").visibility, Some("private"));
    }

    #[test]
    fn tsx_and_jsx_parse_as_their_own_languages() {
        let component = "export const Widget = () => <div>hi</div>;\n";

        let tsx = TypeScript.extract("Widget.tsx", component);
        assert_eq!(find(&tsx, "Widget").kind, Kind::Function);
        assert_eq!(find(&tsx, "Widget").language, "typescript");

        let jsx = JavaScript.extract("Widget.jsx", component);
        assert_eq!(find(&jsx, "Widget").language, "javascript");

        // a `.ts` file reads `<T>` as a type parameter, not a JSX tag
        let generic = TypeScript.extract("id.ts", "export const id = <T>(x: T): T => x;\n");
        assert_eq!(find(&generic, "id").kind, Kind::Function);
    }

    #[test]
    fn class_properties_holding_arrows_are_methods() {
        let src = "class Widget {\n  handleClick = () => {};\n  size = 3;\n}\n";
        let syms = extract(src);
        let click = find(&syms, "handleClick");
        assert_eq!(click.kind, Kind::Method);
        assert_eq!(click.parent.as_deref(), Some("Widget"));
        // a plain data field isn't a definition worth navigating to
        assert!(!syms.iter().any(|s| s.name == "size"), "{syms:?}");
    }
}
