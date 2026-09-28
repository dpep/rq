//! TypeScript / JavaScript plugin. One grammar family, two language tags:
//! JavaScript is TypeScript with the types taken out, so both share a walk and
//! `-x ts` / `-x js` still mean what you'd expect.
//!
//! Extracts `class` → class, `interface` → trait (a named contract, like Go's),
//! `type` → type, `enum` → enum and its members → variant, `namespace` →
//! module, `function` → function, and the members a class, interface, or object
//! type declares → method. A
//! `const f = () => …` is a function too — in modern JS that *is* how functions
//! are declared — and so is `const C = memo((props) => …)`, a function literal
//! handed to a wrapping call. Any other module- or namespace-level `const` → constant: the
//! keyword is the declaration of intent, whatever the casing, and a camelCase
//! `const router = createRouter()` is as much a jump target as `MAX_RETRIES`. A
//! `require(…)` binding is an import, not a definition; `let`/`var` are mutable.
//! A class's `static readonly` field → constant of the class. `parent` is
//! `.`-joined, so a method renders as `deposit · Account`.
//!
//! Ambient declarations (`declare …`, and everything in a `.d.ts`) are
//! extracted like the definitions they describe, as public stubs: a `declare
//! module "fs"` is a module named `fs`, `declare global` adds to the top level,
//! and a declared `let`/`var` is a constant, the definition of a global. A
//! declared interface or type alias is no stub: types have nothing elsewhere
//! to be a declaration of. An
//! overload signature is a stub of the same name, and folds into the
//! implementation after it at search time.
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

pub(crate) struct TypeScript;
pub(crate) struct JavaScript;

impl LanguagePlugin for TypeScript {
    fn language(&self) -> &'static str {
        TYPESCRIPT
    }

    fn extensions(&self) -> &[&str] {
        &["ts", "mts", "cts", "tsx"]
    }

    fn constructor(&self) -> Option<&'static str> {
        Some("constructor")
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

    fn constructor(&self) -> Option<&'static str> {
        Some("constructor")
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
    let scope = Scope {
        parent: None,
        exported: false,
        ambient: is_declaration_file(file),
    };
    extract_with_key(key, language, grammar, file, source, |ctx, root, out| {
        walk(ctx, root, scope, out)
    })
}

/// Whether `file` is a declaration file (`.d.ts`, `.d.mts`, `.d.cts`), where
/// everything is ambient whether or not it says `declare`.
fn is_declaration_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|ext| name.len() > ext.len() && name.to_ascii_lowercase().ends_with(ext))
}

/// Where a walk is: the enclosing qualified name, whether under an `export`,
/// and whether under a `declare` (or in a `.d.ts`), whose definitions are
/// public stubs of what is implemented elsewhere.
#[derive(Clone, Copy)]
struct Scope<'a> {
    parent: Option<&'a str>,
    exported: bool,
    ambient: bool,
}

impl<'a> Scope<'a> {
    fn within(self, parent: Option<&'a str>) -> Self {
        Scope {
            parent,
            exported: false,
            ..self
        }
    }

    /// ESM's convention, where the module's own visibility applies: what a
    /// module exports is its public API. A declaration describes API that
    /// exists, so an ambient one is public.
    fn visibility(self) -> &'static str {
        if self.exported || self.ambient {
            "public"
        } else {
            "private"
        }
    }
}

/// Whether `file` is a `.tsx` — the JSX-bearing dialect of TypeScript.
fn is_tsx(file: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsx"))
}

/// Recursively collect definitions.
fn walk(ctx: &Ctx, node: Node, scope: Scope, out: &mut Vec<Symbol>) {
    let parent = scope.parent;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // `export …` isn't a definition; it marks the one that follows public
            "export_statement" => walk(
                ctx,
                child,
                Scope {
                    exported: true,
                    ..scope
                },
                out,
            ),

            // `declare …`: what follows is implemented elsewhere. `declare
            // global { … }` adds to the top level, so it's no scope.
            "ambient_declaration" => {
                let ambient = Scope {
                    ambient: true,
                    ..scope
                };
                let global = has_token(child, "global");
                walk(
                    ctx,
                    child,
                    if global {
                        ambient.within(None)
                    } else {
                        ambient
                    },
                    out,
                );
            }

            // `declare module "fs" { … }`: a module named by its string. A
            // wildcard (`"*.svg"`) or bodiless one declares nothing to visit.
            "module" => {
                let name = ctx.field_text(child, "name");
                let name = name
                    .as_deref()
                    .map(|n| n.trim_matches(|c| c == '"' || c == '\''));
                if let Some(name) = name.filter(|n| !n.contains('*'))
                    && child.child_by_field_name("body").is_some()
                {
                    let vis = scope.visibility();
                    push(
                        ctx,
                        out,
                        name,
                        Kind::Module,
                        child,
                        parent,
                        vis,
                        scope.ambient,
                    );
                    let qualified = qualify(parent, name, ".");
                    walk(ctx, child, scope.within(Some(&qualified)), out);
                }
            }

            // a named type (or namespace): emit it, then descend so whatever
            // members it declares are qualified by it. `type Foo = { run(): … }`
            // holds methods exactly like `interface Foo` does, so it's the same
            // arm, and an enum's body holds its members.
            "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "internal_module" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = match child.kind() {
                        "interface_declaration" => Kind::Trait,
                        "type_alias_declaration" => Kind::Type,
                        "enum_declaration" => Kind::Enum,
                        "internal_module" => Kind::Module,
                        _ => Kind::Class,
                    };
                    let vis = scope.visibility();
                    // a namespace merged into the function or class before it
                    // (`function f` + `namespace f { … }`) adds members to that
                    // definition, and isn't another one
                    // a type has no implementation to be a stub of: declared
                    // (`declare global { interface Window … }`), it is the
                    // definition, and so are its members
                    let is_type = matches!(kind, Kind::Trait | Kind::Type);
                    let stub = scope.ambient && !is_type;
                    if !(kind == Kind::Module && merges_into(out, &name, parent)) {
                        push(ctx, out, &name, kind, child, parent, vis, stub);
                    }
                    let qualified = qualify(parent, &name, ".");
                    if kind == Kind::Enum {
                        members(ctx, child, &qualified, vis, scope.ambient, out);
                        continue;
                    }
                    // members carry their own visibility; a namespace body
                    // re-declares `export` for what it re-exports
                    let inner = Scope {
                        ambient: stub,
                        ..scope.within(Some(&qualified))
                    };
                    walk(ctx, child, inner, out);
                }
            }

            // an overload signature is a stub of the implementation after it,
            // and folds into it; a `declare function` is a stub of one elsewhere
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let stub = scope.ambient || child.kind() == "function_signature";
                    let vis = scope.visibility();
                    push(ctx, out, &name, Kind::Function, child, parent, vis, stub);
                }
                // bodies hold locals and callbacks, not navigation targets
            }

            // `const handler = () => …` — the modern function declaration —
            // and, at module level, `const LIMIT = …`
            "lexical_declaration" | "variable_declaration" => {
                declarations(ctx, child, scope, out);
            }

            // class and interface members. In a class, a bodiless signature is
            // an overload of the method that follows it.
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                let overload = child.kind() == "method_signature" && node.kind() == "class_body";
                push_member(ctx, out, child, Kind::Method, scope, overload);
            }

            // `handleClick = () => …` in a class body: a method but for syntax;
            // `static readonly LIMIT = …`: the class's constant
            "public_field_definition" | "field_definition" => {
                if is_function(child.child_by_field_name("value")) {
                    push_member(ctx, out, child, Kind::Method, scope, false);
                } else if has_token(child, "static") && has_token(child, "readonly") {
                    push_member(ctx, out, child, Kind::Constant, scope, false);
                }
            }

            // never descend into a function body reached some other way (a
            // callback argument, an IIFE) — its locals aren't definitions
            "arrow_function" | "function_expression" | "function" => {}

            // `global { … }` inside a module, which the grammar doesn't know:
            // it adds to the top level
            "statement_block" if is_global_block(ctx, child) => {
                walk(ctx, child, scope.within(None), out)
            }

            _ => walk(ctx, child, scope, out),
        }
    }
}

/// Whether a definition a namespace named `name` would merge into — a
/// function, class or enum of that name in the same scope — is already out.
fn merges_into(out: &[Symbol], name: &str, parent: Option<&str>) -> bool {
    out.iter().rev().any(|s| {
        s.name == name
            && s.parent.as_deref() == parent
            && matches!(s.kind, Kind::Function | Kind::Class | Kind::Enum)
    })
}

/// Whether `block` is the body of a `global { … }` nested in a module, which
/// the grammar recovers as `global` (an ERROR, or a statement missing its
/// `;`) followed by a bare block.
fn is_global_block(ctx: &Ctx, block: Node) -> bool {
    block
        .prev_sibling()
        .filter(|p| matches!(p.kind(), "ERROR" | "expression_statement"))
        .and_then(|p| ctx.node_text(p))
        .is_some_and(|t| t.trim() == "global")
}

/// Each member of an enum, as a variant of it with the enum's visibility.
/// A quoted name (`"kebab-case" = 1`) is indexed without its quotes.
fn members(
    ctx: &Ctx,
    node: Node,
    qualified: &str,
    vis: &'static str,
    stub: bool,
    out: &mut Vec<Symbol>,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for m in body.named_children(&mut cursor) {
        let name = match m.kind() {
            "enum_assignment" => m.child_by_field_name("name"),
            "property_identifier" | "string" => Some(m),
            _ => None, // a comment, or a computed name
        };
        if let Some(name) = name.and_then(|n| ctx.node_text(n)) {
            let name = name.trim_matches(|c| c == '"' || c == '\'');
            push(ctx, out, name, Kind::Variant, m, Some(qualified), vis, stub);
        }
    }
}

/// Emit a member of a type. An ES private name (`#tally`) is indexed without
/// its `#`, so it's found by the name you'd think to search.
fn push_member(
    ctx: &Ctx,
    out: &mut Vec<Symbol>,
    node: Node,
    kind: Kind,
    scope: Scope,
    overload: bool,
) {
    if let Some(raw) = ctx.field_text(node, "name") {
        let vis = member_visibility(ctx, node, &raw);
        let name = raw.trim_start_matches('#');
        let stub = scope.ambient || overload;
        push(ctx, out, name, kind, node, scope.parent, vis, stub);
    }
}

/// Emit the definitions a `const`/`let`/`var` statement makes: each
/// function-valued declarator as a function, and — for a module-level `const`
/// only — every other simply-named, non-`require` one as a constant. A
/// declared (ambient) binding is a global's definition, whatever its keyword.
fn declarations(ctx: &Ctx, node: Node, scope: Scope, out: &mut Vec<Symbol>) {
    let binding = scope.ambient || node.child(0).is_some_and(|k| k.kind() == "const");
    let constants = binding && at_module_level(ctx, node);
    let mut cursor = node.walk();
    for d in node.children(&mut cursor) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let value = d.child_by_field_name("value");
        let kind = if is_function(value) {
            Kind::Function
        } else if constants && !is_require(ctx, value) {
            Kind::Constant
        } else {
            continue;
        };
        // a destructuring pattern binds names, but defines nothing to jump to
        if let Some(name) = d.child_by_field_name("name")
            && name.kind() == "identifier"
            && let Some(name) = ctx.node_text(name)
        {
            // span the whole statement, so `end_line` covers the closing brace
            let vis = scope.visibility();
            push(
                ctx,
                out,
                &name,
                kind,
                node,
                scope.parent,
                vis,
                scope.ambient,
            );
        }
    }
}

/// Whether a declaration statement sits directly in a module or a namespace
/// body (through an `export` or `declare`), not in a block, callback, or
/// static block.
fn at_module_level(ctx: &Ctx, stmt: Node) -> bool {
    let mut up = stmt.parent();
    while let Some(n) =
        up.filter(|n| matches!(n.kind(), "export_statement" | "ambient_declaration"))
    {
        up = n.parent();
    }
    up.is_some_and(|n| match n.kind() {
        "program" => true,
        // a namespace's, an ambient module's, or `global`'s body
        "statement_block" => {
            is_global_block(ctx, n)
                || n.parent().is_some_and(|m| {
                    matches!(
                        m.kind(),
                        "internal_module" | "module" | "ambient_declaration"
                    )
                })
        }
        _ => false,
    })
}

/// Whether a declarator's value is a CommonJS import: `require(…)`, or a
/// member read off one (`require("x").Widget`).
fn is_require(ctx: &Ctx, mut value: Option<Node>) -> bool {
    while let Some(v) = value {
        match v.kind() {
            "member_expression" => value = v.child_by_field_name("object"),
            "call_expression" => {
                return v
                    .child_by_field_name("function")
                    .and_then(|f| ctx.node_text(f))
                    .is_some_and(|f| f == "require");
            }
            _ => return false,
        }
    }
    false
}

/// Whether `node` carries the keyword token `kw` (`static`, `readonly`).
fn has_token(node: Node, kw: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kw)
}

/// Whether a declarator's value is a function in some spelling: a function
/// literal, or one passed first to a wrapping call, as React's
/// `memo((props) => …)` and `forwardRef(…)` do. The wrapper's name isn't read.
fn is_function(value: Option<Node>) -> bool {
    match value.map(|v| (v, v.kind())) {
        Some((_, "arrow_function" | "function_expression" | "function")) => true,
        Some((call, "call_expression")) => is_function(
            call.child_by_field_name("arguments")
                .and_then(|args| args.named_child(0)),
        ),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)] // one call shape shared by every arm
fn push(
    ctx: &Ctx,
    out: &mut Vec<Symbol>,
    name: &str,
    kind: Kind,
    node: Node,
    parent: Option<&str>,
    visibility: &'static str,
    stub: bool,
) {
    let mut s = ctx.symbol(name, kind, node, parent);
    s.visibility = Some(visibility);
    s.stub = stub;
    out.push(s);
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
    use crate::lang::testing::find;

    fn extract(source: &str) -> Vec<Symbol> {
        TypeScript.extract("test.ts", source)
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
        assert_eq!(find(&syms, "Size").kind, Kind::Type);
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
    fn an_object_type_declares_methods_like_an_interface() {
        // the two spellings are interchangeable in TypeScript, so a method is
        // just as navigable through either
        let src = "type Renderer = {\n  render(): string;\n};\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "Renderer").kind, Kind::Type);
        let render = find(&syms, "render");
        assert_eq!(render.kind, Kind::Method);
        assert_eq!(render.parent.as_deref(), Some("Renderer"));
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
    fn module_level_consts_are_constants() {
        let src = r#"
export const MAX_RETRIES = 3;
const router = createRouter();
export const handler = () => 1;
const fs = require("fs");
const { Widget } = require("./widget");
const Gadget = require("./gadget").Gadget;
const { width, height } = defaults;
let counter = 0;
var legacy = 1;

namespace Limits {
  export const CEILING = 9;
}

function build() {
  const localLimit = 5;
}

if (ready) {
  const inBlock = 1;
}

describe("widget", () => {
  const inCallback = 1;
});
"#;
        let syms = extract(src);

        let max = find(&syms, "MAX_RETRIES");
        assert_eq!(max.kind, Kind::Constant);
        assert_eq!(max.parent, None);
        assert_eq!(max.visibility, Some("public"));

        // the keyword decides, not the casing
        let router = find(&syms, "router");
        assert_eq!(router.kind, Kind::Constant);
        assert_eq!(router.visibility, Some("private"));

        let ceiling = find(&syms, "CEILING");
        assert_eq!(ceiling.kind, Kind::Constant);
        assert_eq!(ceiling.parent.as_deref(), Some("Limits"));

        // an arrow const is a function, once — not a constant too
        let handlers: Vec<_> = syms.iter().filter(|s| s.name == "handler").collect();
        assert_eq!(handlers.len(), 1, "{syms:?}");
        assert_eq!(handlers[0].kind, Kind::Function);

        // not definitions: imports, destructuring, mutable bindings, and
        // anything below module level
        for absent in [
            "fs",
            "Widget",
            "Gadget",
            "width",
            "counter",
            "legacy",
            "localLimit",
            "inBlock",
            "inCallback",
        ] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
    }

    #[test]
    fn static_readonly_fields_are_class_constants() {
        let src = r#"
enum Color {
  Red,
}

class Widget {
  static readonly DEFAULT_SIZE = 3;
  private static readonly SECRET = "x";
  static count = 0;
  readonly id = 1;
}
"#;
        let syms = extract(src);

        let size = find(&syms, "DEFAULT_SIZE");
        assert_eq!(size.kind, Kind::Constant);
        assert_eq!(size.parent.as_deref(), Some("Widget"));
        assert_eq!(size.visibility, Some("public"));
        assert_eq!(find(&syms, "SECRET").visibility, Some("private"));

        // mutable statics and instance fields stay out
        for absent in ["count", "id"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
    }

    #[test]
    fn enum_members_are_variants_of_their_enum() {
        let src = r#"
export enum EVENT {
  MOUSE_MOVE = "mousemove",
  // a comment between members
  "key-down" = "keydown",
  Wheel,
}

const enum Hidden {
  Inner = 1,
}
"#;
        let syms = extract(src);
        for name in ["MOUSE_MOVE", "key-down", "Wheel"] {
            let m = find(&syms, name);
            assert_eq!(m.kind, Kind::Variant, "{name}");
            assert_eq!(m.parent.as_deref(), Some("EVENT"), "{name}");
            assert_eq!(m.visibility, Some("public"), "{name}");
        }
        assert_eq!(find(&syms, "Inner").visibility, Some("private"));
        assert_eq!(syms.len(), 6, "{syms:?}");
    }

    #[test]
    fn a_function_passed_to_a_wrapping_call_is_a_function() {
        let src = r#"
export const Badge = memo((props: Props) => <div />);
export const Field = React.forwardRef<Ref, Props>(function Field(props, ref) {
  return <input ref={ref} />;
});
export const Nested = memo(forwardRef((props, ref) => null));
export const Aliased = memo(BadgeBase, areEqual);
export const store = createStore({ size: 1 });
"#;
        let syms = TypeScript.extract("badge.tsx", src);
        for name in ["Badge", "Field", "Nested"] {
            assert_eq!(find(&syms, name).kind, Kind::Function, "{name}");
        }
        // a wrapped identifier or a call without a function stays a constant
        assert_eq!(find(&syms, "Aliased").kind, Kind::Constant);
        assert_eq!(find(&syms, "store").kind, Kind::Constant);
    }

    #[test]
    fn ambient_declarations_are_public_stubs() {
        let src = r#"
declare function setup(name: string): void;
export declare function teardown(): void;
declare const VERSION: string;
declare let counter: number;
declare var process: Process;
declare class Store {
  get(key: string): string;
}
declare namespace Widgets {
  function build(): Widget;
  const LIMIT: number;
}
declare module "widget-store" {
  export function open(path: string): Store;
  export namespace open {
    function sync(): void;
  }
  global {
    function reset(): void;
    var registry: Store;
  }
}
declare module "*.svg";
declare module "side-effect";
declare global {
  interface Window {
    app: App;
    focusApp(): void;
  }
  function track(event: string): void;
  var DEBUG: boolean;
}
"#;
        let syms = extract(src);
        let at = |name: &str| {
            let s = find(&syms, name);
            (s.kind, s.parent.as_deref(), s.visibility, s.stub)
        };
        let top = |kind| (kind, None, Some("public"), true);
        assert_eq!(at("setup"), top(Kind::Function));
        assert_eq!(at("teardown"), top(Kind::Function));
        // a declared binding is a global's definition, whatever its keyword
        for name in ["VERSION", "counter", "process", "DEBUG"] {
            assert_eq!(at(name), top(Kind::Constant), "{name}");
        }
        assert_eq!(at("Store"), top(Kind::Class));
        assert_eq!(
            at("get"),
            (Kind::Method, Some("Store"), Some("public"), true)
        );
        assert_eq!(at("Widgets"), top(Kind::Module));
        assert_eq!(
            at("build"),
            (Kind::Function, Some("Widgets"), Some("public"), true)
        );
        assert_eq!(
            at("LIMIT"),
            (Kind::Constant, Some("Widgets"), Some("public"), true)
        );
        // a module named by a string is named without its quotes
        assert_eq!(at("widget-store"), top(Kind::Module));
        // a namespace merged into a function adds to it, and isn't another
        let open: Vec<_> = syms.iter().filter(|s| s.name == "open").collect();
        assert_eq!(open.len(), 1, "{open:?}");
        assert_eq!(open[0].kind, Kind::Function);
        assert_eq!(at("sync").1, Some("widget-store.open"));
        // `global` in a module is the top level too
        assert_eq!(at("reset"), top(Kind::Function));
        assert_eq!(at("registry"), top(Kind::Constant));
        // `global` is the top level, not a scope; a declared type is the
        // definition, with nothing elsewhere to be a stub of
        assert_eq!(at("Window"), (Kind::Trait, None, Some("public"), false));
        assert_eq!(
            at("focusApp"),
            (Kind::Method, Some("Window"), Some("public"), false)
        );
        assert_eq!(at("track"), top(Kind::Function));
        // a wildcard or bodiless module declares no definition
        assert!(!syms.iter().any(|s| s.name.contains("svg")), "{syms:?}");
        assert!(!syms.iter().any(|s| s.name == "side-effect"), "{syms:?}");
    }

    #[test]
    fn a_declaration_file_is_ambient_without_declare() {
        let src = "export default function isReady(): boolean;\nexport class Pool {\n  size(): number;\n}\n";
        let syms = TypeScript.extract("types/index.d.ts", src);
        for name in ["isReady", "Pool", "size"] {
            let s = find(&syms, name);
            assert!(s.stub, "{name}");
            assert_eq!(s.visibility, Some("public"), "{name}");
        }
        // the same class in a source file is its own definition
        let src = "export class Pool {\n  size(): number { return 1; }\n}\n";
        assert!(!find(&extract(src), "Pool").stub);
    }

    #[test]
    fn overload_signatures_are_stubs_of_the_implementation() {
        let src = r#"
/** Parses a widget. */
export function parse(input: string): Widget;
export function parse(input: Buffer): Widget;
export function parse(input: string | Buffer): Widget {
  return build(input);
}

class Codec {
  encode(value: string): string;
  encode(value: number): string;
  encode(value: unknown): string {
    return String(value);
  }
}
"#;
        let syms = extract(src);
        for name in ["parse", "encode"] {
            let all: Vec<_> = syms.iter().filter(|s| s.name == name).collect();
            let stubs: Vec<_> = all.iter().map(|s| s.stub).collect();
            assert_eq!(stubs, [true, true, false], "{name}: {all:?}");
            // one kind and scope, so they fold into the implementation
            assert!(
                all.iter()
                    .all(|s| (s.kind, &s.parent) == (all[2].kind, &all[2].parent))
            );
        }
        assert_eq!(find(&syms, "parse").visibility, Some("public"));
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
