//! Ruby plugin — the first language.
//!
//! Extracts classes, modules, and methods (instance and singleton) via
//! Tree-sitter. `parent` carries the enclosing qualified name so a method
//! renders as `Foo::Bar#baz` and a nested class as `Foo::Bar`.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with, qualify};

const LANGUAGE: &str = "ruby";

pub(crate) struct Ruby;

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
            |ctx, root, out| walk(ctx, root, None, "public", out),
        )
    }
}

/// Recursively collect definitions. `parent` is the enclosing qualified name;
/// `vis` is the access section in effect (a bare `private`/`protected`/`public`
/// marker flips it for everything after, including through wrapping nodes like
/// `private def foo`).
fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, vis: &'static str, out: &mut Vec<Symbol>) {
    let mut vis = vis;
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
                    let (name, rooted) = match name.strip_prefix("::") {
                        Some(rest) => (rest, true),
                        None => (name.as_str(), false),
                    };
                    let (leaf, prefix) = split_qualified(name);
                    let effective_parent = if rooted {
                        // `class ::Bar` defines at the top level, ignoring nesting
                        prefix.map(str::to_string)
                    } else {
                        match prefix {
                            Some(p) => Some(qualify(parent, p, "::")),
                            None => parent.map(str::to_string),
                        }
                    };
                    let mut s = ctx.symbol(leaf, kind, child, effective_parent.as_deref());
                    s.visibility = Some("public");
                    out.push(s);
                    let qualified = qualify(effective_parent.as_deref(), leaf, "::");
                    // a fresh body starts a fresh (public) access section
                    walk(ctx, child, Some(&qualified), "public", out);
                } else {
                    walk(ctx, child, parent, vis, out);
                }
            }
            "method" | "singleton_method" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let mut s = ctx.symbol(&name, Kind::Method, child, parent);
                    // `private` sections don't apply to `def self.x`
                    s.visibility = Some(if child.kind() == "singleton_method" {
                        "public"
                    } else {
                        vis
                    });
                    out.push(s);
                }
                // method bodies rarely hold further definitions; don't recurse.
            }
            "alias" => {
                // `alias new old` — the keyword form of alias_method; a
                // global-variable alias (`alias $a $b`) defines no method
                if let Some(name) = ctx.field_text(child, "name")
                    && !name.starts_with('$')
                {
                    let mut s =
                        ctx.symbol(name.trim_start_matches(':'), Kind::Method, child, parent);
                    s.visibility = Some(vis);
                    out.push(s);
                }
            }
            // a bare access marker flips the section for what follows
            "identifier" => match ctx.node_text(child).as_deref() {
                Some("private") => vis = "private",
                Some("protected") => vis = "protected",
                Some("public") => vis = "public",
                _ => {}
            },
            "call" => {
                // metaprogramming: `attr_accessor :x`, `has_many :users`, … are
                // calls that *define* methods Tree-sitter can't see as defs.
                // Emit the literal names, pointing at the macro's line.
                dsl_symbols(ctx, child, parent, vis, out);
                // still recurse: a call can wrap real definitions
                // (`private def foo` — its `private` identifier flips `vis`
                // on the way down — or `Class.new do … end`)
                walk(ctx, child, parent, vis, out);
            }
            _ => walk(ctx, child, parent, vis, out),
        }
    }
}

/// How many of a DSL macro's arguments name methods it defines.
enum DslArgs {
    /// Every literal argument (`attr_accessor :a, :b`, `delegate :x, :y, to:`).
    All,
    /// Only the first (`define_method(:x)`, `scope :active`, `has_many :users`).
    First,
}

/// The method-defining macro vocabulary: Ruby core plus the everyday Rails
/// surface. Deliberately small — literal, high-confidence definitions only.
///
/// `field` earns its place the same way `has_many` does: it's the declaration
/// that defines the member, across several schema DSLs (graphql-ruby, Mongoid,
/// dry-types). Without it, a field declared `field :email, String` has no
/// definition to navigate to at all — the receiver form (`f.field :email`, a
/// form builder) is already excluded, which is where the name would otherwise
/// be ambiguous.
fn dsl_args(method: &str) -> Option<DslArgs> {
    match method {
        // `attr`'s optional boolean tail (`attr :x, true`) is skipped by the
        // literal-name filter, so All is safe for it too
        "attr" | "attr_accessor" | "attr_reader" | "attr_writer" | "delegate" => Some(DslArgs::All),
        "define_method" | "alias_method" | "scope" | "has_many" | "has_one" | "belongs_to"
        | "field" => Some(DslArgs::First),
        _ => None,
    }
}

/// Emit method symbols for a metaprogramming call: `attr_accessor :balance`
/// defines `balance` even though no `def` exists. Only *literal* symbol/string
/// arguments count — a computed name (`define_method(name)`) is unresolvable
/// statically, so it's skipped rather than guessed. Keyword arguments
/// (`delegate …, to: :owner`) are `pair` nodes and naturally excluded.
fn dsl_symbols(
    ctx: &Ctx,
    call: Node,
    parent: Option<&str>,
    vis: &'static str,
    out: &mut Vec<Symbol>,
) {
    if call.child_by_field_name("receiver").is_some() {
        return; // `Foo.attr_accessor` isn't the macro form we index
    }
    let Some(method) = ctx.field_text(call, "method") else {
        return;
    };
    let Some(args) = dsl_args(&method) else {
        return;
    };
    let Some(arg_list) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = arg_list.walk();
    for arg in arg_list.children(&mut cursor) {
        if !arg.is_named() {
            continue; // parens and commas
        }
        if let Some(name) = literal_name(ctx, arg)
            && !name.is_empty()
        {
            let mut s = ctx.symbol(&name, Kind::Method, call, parent);
            s.visibility = Some(vis);
            out.push(s);
        }
        if matches!(args, DslArgs::First) {
            break; // later args are options (`scope :active, -> {…}`), not names
        }
    }
}

/// The name a literal `:symbol` or `"string"` argument carries, if any.
fn literal_name(ctx: &Ctx, node: Node) -> Option<String> {
    match node.kind() {
        "simple_symbol" => ctx
            .node_text(node)
            .map(|t| t.trim_start_matches(':').to_string()),
        "string" => ctx
            .node_text(node)
            .map(|t| t.trim_matches(|c| c == '"' || c == '\'').to_string()),
        _ => None,
    }
}

/// Split a possibly compact-qualified definition name (`A::B::C`) into its leaf
/// (`C`) and namespace prefix (`A::B`). A plain name has no prefix. Callers
/// strip a rooted `::` before splitting.
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
    fn metaprogramming_macros_define_methods() {
        let src = r#"
class Account
  attr_accessor :balance, :currency
  attr_reader "label"
  has_many :transactions, dependent: :destroy
  scope :active, -> { where(active: true) }
  delegate :name, :email, to: :owner, prefix: true
  define_method(:refresh!) { reload }
  alias_method :bal, :balance
end
"#;
        let syms = extract(src);

        for name in [
            "balance",
            "currency",
            "label",
            "transactions",
            "active",
            "name",
            "email",
            "refresh!",
            "bal",
        ] {
            let s = find(&syms, name);
            assert_eq!(s.kind, Kind::Method, "{name} is a method");
            assert_eq!(s.parent.as_deref(), Some("Account"), "{name} in Account");
        }

        // option arguments never become symbols
        for non_name in ["destroy", "owner", "dependent", "to", "prefix", "where"] {
            assert!(
                !syms.iter().any(|s| s.name == non_name),
                "{non_name} is an option, not a defined method: {syms:?}"
            );
        }
    }

    #[test]
    fn schema_dsl_field_declarations_define_methods() {
        let src = r#"
module Types
  class UserType < Types::BaseObject
    field :id, ID, null: false
    field :email, String, null: true
    field :posts, [Types::PostType], null: false do
      argument :first, Integer, required: false
    end

    def posts(first: nil)
      object.posts.limit(first)
    end
  end
end
"#;
        let syms = extract(src);

        for name in ["id", "email", "posts"] {
            let s = find(&syms, name);
            assert_eq!(s.kind, Kind::Method, "{name} is a method");
            assert_eq!(
                s.parent.as_deref(),
                Some("Types::UserType"),
                "{name} in UserType"
            );
        }
        // the block form declares `posts` once as a field and once as a real
        // `def`; both are definitions of the same member, and both are indexed
        assert_eq!(
            syms.iter().filter(|s| s.name == "posts").count(),
            2,
            "{syms:?}"
        );
        // type arguments and options are not members
        for non_name in ["ID", "String", "null", "required", "first"] {
            assert!(
                !syms.iter().any(|s| s.name == non_name),
                "{non_name} is not a defined method: {syms:?}"
            );
        }
    }

    #[test]
    fn alias_keyword_defines_the_new_name() {
        let src = r#"
class Foo
  def bar
  end

  alias baz bar
  alias :qux :bar
  alias $copy $orig
end
"#;
        let syms = extract(src);
        for name in ["baz", "qux"] {
            let s = find(&syms, name);
            assert_eq!(s.kind, Kind::Method, "{name} is a method");
            assert_eq!(s.parent.as_deref(), Some("Foo"));
        }
        // a global-variable alias defines no method
        assert!(!syms.iter().any(|s| s.name.contains("copy")), "{syms:?}");
    }

    #[test]
    fn bare_attr_defines_readers() {
        let src = "class Foo\n  attr :size, :color\n  attr :flag, true\nend\n";
        let syms = extract(src);
        for name in ["size", "color", "flag"] {
            assert_eq!(find(&syms, name).kind, Kind::Method, "{name}");
        }
        // the boolean writer switch is an option, not a name
        assert_eq!(syms.len(), 4, "{syms:?}");
    }

    #[test]
    fn rooted_definition_resets_to_top_level() {
        // `class ::Bar` inside a module defines top-level `Bar`, not `Foo::Bar`
        let src = "module Foo\n  class ::Bar\n  end\n  class ::Baz::Qux\n  end\nend\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "Bar").parent, None);
        assert_eq!(find(&syms, "Qux").parent.as_deref(), Some("Baz"));
    }

    #[test]
    fn singleton_class_methods_belong_to_the_class() {
        let src = r#"
class Foo
  class << self
    def build
    end

    private

    def hidden
    end
  end
end
"#;
        let syms = extract(src);
        let build = find(&syms, "build");
        assert_eq!(build.parent.as_deref(), Some("Foo"));
        assert_eq!(build.visibility, Some("public"));
        // unlike `def self.x`, visibility applies inside `class << self`
        assert_eq!(find(&syms, "hidden").visibility, Some("private"));
    }

    #[test]
    fn computed_and_received_macro_names_are_skipped() {
        let src = r#"
class Widget
  define_method(dynamic_name) { }
  Other.attr_accessor :not_ours
  form.field :not_ours_either
end
"#;
        let syms = extract(src);
        // only the class itself — no guessed names, no receiver-form macros
        assert_eq!(syms.len(), 1, "{syms:?}");
        assert_eq!(syms[0].name, "Widget");
    }

    #[test]
    fn a_def_wrapped_in_a_visibility_call_is_still_found() {
        let src = "class Widget\n  private def hidden\n  end\nend\n";
        let syms = extract(src);
        let hidden = find(&syms, "hidden");
        assert_eq!(hidden.kind, Kind::Method);
        assert_eq!(hidden.parent.as_deref(), Some("Widget"));
        assert_eq!(hidden.visibility, Some("private"));
    }

    #[test]
    fn access_sections_set_visibility() {
        let src = r#"
class Widget
  def open_api
  end

  private

  def internal
  end
  attr_reader :secret

  public

  def reopened
  end
end
"#;
        let syms = extract(src);
        assert_eq!(find(&syms, "open_api").visibility, Some("public"));
        assert_eq!(find(&syms, "internal").visibility, Some("private"));
        // a macro under `private` defines private methods too
        assert_eq!(find(&syms, "secret").visibility, Some("private"));
        assert_eq!(find(&syms, "reopened").visibility, Some("public"));
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
