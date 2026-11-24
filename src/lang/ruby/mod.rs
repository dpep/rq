//! Ruby plugin — the first language.
//!
//! Extracts classes, modules, methods (instance and singleton), and constants
//! via prism, Ruby's own parser. `parent` carries the enclosing qualified name
//! so a method renders as `Foo::Bar#baz` and a nested class as `Foo::Bar`.

#[cfg(test)]
mod parity;
mod prism;
/// The extraction prism replaced, kept as the oracle prism's must match on
/// every file both parse (D70).
#[cfg(test)]
mod tree_sitter;

use crate::core::{Kind, Symbol, Visibility};
use crate::lang::{LanguagePlugin, qualify};

const LANGUAGE: &str = "ruby";

pub(crate) struct Ruby;

impl LanguagePlugin for Ruby {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["rb"]
    }

    fn constructor(&self) -> Option<&'static str> {
        Some("initialize")
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        prism::extract(file, source)
    }
}

/// Ruby gives an alias its original's visibility at alias time, whatever
/// section it sits in. An original not defined above it in this file
/// (inherited, `Kernel#send`, another file) is taken as public, as most are.
pub(super) fn alias_visibility(
    out: &[Symbol],
    parent: Option<&str>,
    singleton: bool,
    original: &str,
) -> Visibility {
    out.iter()
        .rev()
        .find(|s| {
            s.kind == Kind::Method
                && s.singleton == singleton
                && s.parent.as_deref() == parent
                && s.name == original
        })
        .and_then(|s| s.visibility)
        .unwrap_or(Visibility::Public)
}

/// How many of a DSL macro's arguments name methods it defines.
pub(super) enum DslArgs {
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
pub(super) fn dsl_args(method: &str) -> Option<DslArgs> {
    match method {
        // `attr`'s optional boolean tail (`attr :x, true`) is skipped by the
        // literal-name filter, so All is safe for it too
        "attr" | "attr_accessor" | "attr_reader" | "attr_writer" | "delegate" => Some(DslArgs::All),
        "define_method" | "alias_method" | "scope" | "has_many" | "has_one" | "belongs_to"
        | "field" => Some(DslArgs::First),
        _ => None,
    }
}

/// Resolve a definition name that may be compact-qualified (`A::B::C`) or
/// rooted (`::Foo`) to its leaf plus the parent it belongs under. A compact
/// name folds its prefix into the parent — same shape as the nested form, so
/// the definition is found by its leaf name either way; a rooted name lives at
/// the top level, ignoring lexical nesting.
pub(super) fn leaf_and_parent<'a>(
    name: &'a str,
    parent: Option<&str>,
) -> (&'a str, Option<String>) {
    let (name, rooted) = match name.strip_prefix("::") {
        Some(rest) => (rest, true),
        None => (name, false),
    };
    let (leaf, prefix) = split_qualified(name);
    let effective_parent = if rooted {
        prefix.map(str::to_string)
    } else {
        match prefix {
            Some(p) => Some(qualify(parent, p, "::")),
            None => parent.map(str::to_string),
        }
    };
    (leaf, effective_parent)
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
    use crate::lang::testing::find;

    fn extract(source: &str) -> Vec<Symbol> {
        Ruby.extract("test.rb", source)
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
        assert_eq!(build.visibility, Some(Visibility::Public));
        // unlike `def self.x`, visibility applies inside `class << self`
        assert_eq!(find(&syms, "hidden").visibility, Some(Visibility::Private));
    }

    #[test]
    fn constant_assignments_are_indexed() {
        let src = r#"
module Config
  LIMIT = 10
  Names::DEFAULT = "x"
  ::ROOT = 1
  A, B = 1, 2
  RETRIES ||= 3

  private

  SECRET = 4
end
Version = Struct.new(:major)
"#;
        let syms = extract(src);
        let limit = find(&syms, "LIMIT");
        assert_eq!(limit.kind, Kind::Constant);
        assert_eq!(limit.parent.as_deref(), Some("Config"));
        // qualified and rooted lefts resolve like compact class definitions
        assert_eq!(
            find(&syms, "DEFAULT").parent.as_deref(),
            Some("Config::Names")
        );
        assert_eq!(find(&syms, "ROOT").parent, None);
        for name in ["A", "B", "RETRIES"] {
            assert_eq!(find(&syms, name).kind, Kind::Constant, "{name}");
        }
        // method visibility sections don't apply to constants
        assert_eq!(find(&syms, "SECRET").visibility, Some(Visibility::Public));
        // an anonymous class is at least findable by the constant naming it
        assert_eq!(find(&syms, "Version").kind, Kind::Constant);
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
        assert_eq!(hidden.visibility, Some(Visibility::Private));
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
        assert_eq!(find(&syms, "open_api").visibility, Some(Visibility::Public));
        assert_eq!(
            find(&syms, "internal").visibility,
            Some(Visibility::Private)
        );
        // a macro under `private` defines private methods too
        assert_eq!(find(&syms, "secret").visibility, Some(Visibility::Private));
        assert_eq!(find(&syms, "reopened").visibility, Some(Visibility::Public));
    }

    /// Each symbol as `Parent#name`, in source order.
    fn names(src: &str) -> Vec<String> {
        extract(src)
            .into_iter()
            .map(|s| qualify(s.parent.as_deref(), &s.name, "#"))
            .collect()
    }

    #[test]
    fn a_half_typed_definition_names_nothing_and_hides_nothing() {
        let cases: [(&str, &[&str]); 3] = [
            // a class keyword with no name yet
            (
                "class Widget\n  def a; end\nend\n\nclass\n\ndef helper(x)\n  x\nend\n",
                &["Widget", "Widget#a", "helper"],
            ),
            // a compact path cut at its `::`, whose recovery takes the next
            // line's `def` for the missing name
            (
                "module Outer\n  class Inner::\n  def m; end\nend\n",
                &["Outer"],
            ),
            // an invalid receiver, whose recovery offers a label (`target:`)
            // for the name and leaves no def of `hello`
            ("def(target:primary).hello = :hi\n", &[]),
        ];
        for (src, want) in cases {
            assert_eq!(names(src), want, "{src}");
        }
    }

    #[test]
    fn source_is_read_as_utf8_whatever_its_magic_comment_says() {
        // rq hands the parser UTF-8; a declared encoding would split its
        // characters
        for magic in [
            "encoding: shift_jis",
            "coding: euc-jp",
            "-*- coding: binary -*-",
        ] {
            let src = format!(
                "# {magic}\nclass K\n  def māÜ; end\n  attr_reader :nāÜ\n  Cā = 1\n  def after; end\nend\n"
            );
            assert_eq!(
                names(&src),
                ["K", "K#māÜ", "K#nāÜ", "K#Cā", "K#after"],
                "{magic}"
            );
        }
    }

    #[test]
    fn a_class_under_a_runtime_scope_is_named_by_its_leaf() {
        let src = "class Outer\n  class self.class::Inner\n    def m; end\n  end\nend\n";
        assert_eq!(names(src), ["Outer", "Outer#Inner", "Outer::Inner#m"]);
    }

    #[test]
    fn a_def_whose_name_is_not_typed_yet_defines_nothing_and_swallows_nothing() {
        let cases: [(&str, &[&str]); 3] = [
            // the next line's word is taken for the name
            ("class A\n  def\nend\n", &["A"]),
            // ...here `def`, taking `b` as its parameter
            ("class A\n  private def # soon\n  def b; end\nend\n", &["A"]),
            // ...here `require`, whose body is then the rest of the file
            (
                "private def\nrequire \"x\"\n\nclass Upload\n  LIMIT = 1\n  def name; end\nend\n",
                &["Upload", "Upload#LIMIT", "Upload#name"],
            ),
        ];
        for (src, want) in cases {
            assert_eq!(names(src), want, "{src}");
        }
    }

    #[test]
    fn an_error_that_leaves_the_structure_whole_walks_no_def_body() {
        let cases = [
            // an escape invalid in the declared encoding
            "# coding: US-ASCII\nclass T\n  R = /[\\x0-\\xff]/\n  def t\n    def inner; end\n  end\nend\n",
            // a semantic error
            "class T\n  def t\n    a[1, k: 2] = 3\n    def inner; end\n  end\nend\n",
        ];
        for src in cases {
            let want: &[&str] = if src.contains("R =") {
                &["T", "T#R", "T#t"]
            } else {
                &["T", "T#t"]
            };
            assert_eq!(names(src), want, "{src}");
        }
    }

    #[test]
    fn a_def_left_open_mid_edit_hides_none_of_the_defs_after_it() {
        // (an unterminated string still swallows the rest of the file: it
        // leaves no defs to find)
        let cases: [(&str, &[&str]); 2] = [
            // an unclosed parameter list
            (
                "class Store\n  def open(\n  end\n\n  def close; end\n  def flush; end\nend\n",
                &["Store", "Store#open", "Store#close", "Store#flush"],
            ),
            // a block opened and never closed
            (
                "class Store\n  def open\n    items.each do |i|\n  end\n\n  def close; end\nend\n",
                &["Store", "Store#open", "Store#close"],
            ),
        ];
        for (src, want) in cases {
            assert_eq!(names(src), want, "{src}");
        }
    }

    #[test]
    fn a_file_cut_off_mid_edit_keeps_what_is_above_the_cut() {
        let src = include_str!("../../tests/fixtures/ruby/truncated.rb");
        let got: Vec<_> = extract(src)
            .into_iter()
            .map(|s| {
                let vis = s.visibility.map_or("", |v| v.as_str());
                (s.name, s.parent, s.singleton, vis)
            })
            .collect();
        let sprocket = || Some("Gizmos::Sprocket".to_string());
        let expect = [
            ("Gizmos", None, false, "public"),
            ("Sprocket", Some("Gizmos".to_string()), false, "public"),
            ("teeth", sprocket(), false, "public"),
            ("build", sprocket(), true, "public"),
            ("spin", sprocket(), false, "public"),
            ("mesh", sprocket(), false, "private"),
        ]
        .map(|(n, p, s, v)| (n.to_string(), p, s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn names_are_read_as_written_and_text_that_isnt_code_is_skipped() {
        let syms = extract(include_str!("../../tests/fixtures/ruby/forms.rb"));
        let vector: Vec<_> = syms
            .iter()
            .filter(|s| s.parent.as_deref() == Some("Forms::Vector") && !s.singleton)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            vector,
            [
                "x",
                "y",
                "initialize",
                "length",
                "[]",
                "[]=",
                "<=>",
                "==",
                "~",
                "-@",
                "+@",
                "!",
                "call!",
                "ready?",
                "x=",
                "`",
                "describe"
            ]
        );
        // heredoc bodies, `=begin` blocks and the data after `__END__` aren't code
        for name in [
            "NotAClass",
            "not_a_method",
            "inside_a_heredoc",
            "Commented",
            "AfterEnd",
        ] {
            assert!(!syms.iter().any(|s| s.name == name), "{name}: {syms:?}");
        }
        assert_eq!(
            find(&syms, "origin").parent.as_deref(),
            Some("Forms::Vector")
        );
        assert_eq!(find(&syms, "cache").visibility, Some(Visibility::Private));
        assert_eq!(find(&syms, "norm").visibility, Some(Visibility::Private));
        assert_eq!(find(&syms, "maß").parent.as_deref(), Some("Forms::Größe"));
        let dynamic: Vec<_> = syms
            .iter()
            .filter(|s| s.parent.as_deref() == Some("Forms::Dynamic"))
            .map(|s| s.name.as_str())
            .collect();
        // a class under a runtime scope, filed under the enclosing name
        assert_eq!(dynamic, ["spin", "twirl", "Scoped"]);
    }

    #[test]
    fn a_string_names_a_method_by_its_content_and_an_interpolated_one_is_skipped() {
        let src = r#"
class Widget
  attr_reader %q(plain), %Q(double), "esc\u0061ped"
  define_method("dyn_#{kind}") { }
  attr_accessor "a#{b}", "ok"
  def hidden; end
  def secret; end
  private %w[hidden], %q(secret)
end
"#;
        let got = methods(src);
        let expect = [
            ("plain", false, "public"),
            ("double", false, "public"),
            ("escaped", false, "public"),
            ("ok", false, "public"),
            ("hidden", false, "private"),
            ("secret", false, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn a_quoted_symbol_names_a_method_and_an_interpolated_one_is_skipped() {
        let src = r#"
class Widget
  attr_reader :"ready?", :'single', %s(percent), :☠
  define_method(:"dyn_#{kind}") { }
  alias_method :"copy?", :"ready?"
  alias :"quoted alias" :single
  alias :"twin?" :single
  alias :"dyn_#{x}" :single
  def a; end
  def b; end
  private %I[a#{x} b], :"a"
end
"#;
        let got = methods(src);
        let expect = [
            ("ready?", false, "public"),
            ("single", false, "public"),
            ("percent", false, "public"),
            ("☠", false, "public"),
            ("copy?", false, "public"),
            // a name with a space in it is none a query spells
            ("twin?", false, "public"),
            ("a", false, "private"),
            ("b", false, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn a_constant_may_start_with_any_uppercase_letter() {
        // Ruby's rule, which the Tree-sitter grammar read as ASCII only
        let syms = extract("class Größe\n  ÜBER = 1\nend\n");
        assert_eq!(find(&syms, "ÜBER").kind, Kind::Constant);
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

    /// (name, singleton, visibility) of each method, in source order.
    fn methods(src: &str) -> Vec<(String, bool, &'static str)> {
        extract(src)
            .into_iter()
            .filter(|s| s.kind == Kind::Method)
            .map(|s| (s.name, s.singleton, s.visibility.unwrap().as_str()))
            .collect()
    }

    #[test]
    fn class_methods_are_singletons_in_every_spelling() {
        let src = r#"
class Widget
  private

  class << self
    alias_method :make, :build
    def build; end

    class Part
      def fit; end
    end
  end

  def self.reset; end
  def size; end
end
"#;
        let got = methods(src);
        let expect = [
            ("make", true, "public"),
            // the class body's `private` doesn't reach into `class << self`
            ("build", true, "public"),
            // a class nested in it has instance methods of its own
            ("fit", false, "public"),
            ("reset", true, "public"),
            // nor does `class << self` reach out of it
            ("size", false, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn module_functions_are_singletons_until_an_access_marker() {
        let src = r#"
module Util
  def mixin; end

  module_function

  def helper; end
  attr_reader :limit

  public

  def instance_api; end
  def retro; end
  module_function :retro, :missing

  module_function def wrapped; end
end
"#;
        let got = methods(src);
        let expect = [
            ("mixin", false, "public"),
            ("helper", true, "public"),
            ("limit", true, "public"),
            ("instance_api", false, "public"),
            ("retro", true, "public"),
            ("wrapped", true, "public"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn retroactive_access_calls_change_methods_already_defined() {
        let src = r#"
class Widget
  def self.a; end
  def self.b; end
  def self.c; end
  def self.d; end
  def self.e; end
  def self.f; end
  private_class_method :a, "b"
  private_class_method [:c]
  private_class_method(*%i[d e])
  private_class_method :f
  public_class_method :f
  private_class_method def self.g; end
  def self.h; end

  def i; end
  def j; end
  def k; end
  private :i, "j"
  private def l; end
  protected :k
  private attr_reader :m

  class << self
    def n; end
    def o; end
    private :n
    private
    def p; end
    public :p
    private_class_method :o
  end

  def q; end
end

class Widget
  private_class_method :h
  private :q
end
"#;
        let got = methods(src);
        let expect = [
            ("a", true, "private"),
            ("b", true, "private"),
            ("c", true, "private"),
            ("d", true, "private"),
            ("e", true, "private"),
            ("f", true, "public"),
            ("g", true, "private"),
            // made private by the reopening below
            ("h", true, "private"),
            ("i", false, "private"),
            ("j", false, "private"),
            ("k", false, "protected"),
            ("l", false, "private"),
            ("m", false, "private"),
            ("n", true, "private"),
            // `private_class_method` inside `class << self` is about the
            // singleton class's own singleton, not these
            ("o", true, "public"),
            ("p", true, "public"),
            ("q", false, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn a_retroactive_call_reaches_only_its_own_side_of_the_class() {
        let src = r#"
class Widget
  def self.x; end
  def x; end
  private :x
end

module Util
  module_function
  def helper; end
  private :helper
end
"#;
        let got = methods(src);
        let expect = [
            ("x", true, "public"),
            ("x", false, "private"),
            // `private :helper` hides the mixin copy; the module's stays public
            ("helper", true, "public"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn a_retroactive_call_does_not_reach_into_a_block() {
        // a block's defs land wherever it's evaluated (`Helper`, the includer)
        let src = r#"
class Thing
  def go; end
  Helper = Struct.new(:a) do
    def go; end
    def stop; end
    attr_reader :size
    private :stop
  end
  attr_reader :size
  private :go, :size
end
"#;
        let got = methods(src);
        let expect = [
            ("go", false, "private"),
            ("go", false, "public"),
            ("stop", false, "private"),
            ("size", false, "public"),
            ("size", false, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }

    #[test]
    fn an_alias_takes_its_originals_visibility_not_the_sections() {
        let src = r#"
class Widget
  def open; end
  def shut; end
  private :shut

  private

  alias a1 open
  alias_method :a2, :open
  alias a3 inherited_elsewhere
  def hidden; end

  public

  alias a4 hidden
  alias_method :a5, :shut
  def later; end
  alias a6 later
  private :later

  class << self
    def build; end
    private :build
    alias a7 build
    alias_method :a8, :build
  end
end
"#;
        let got = methods(src);
        let expect = [
            ("open", false, "public"),
            ("shut", false, "private"),
            ("a1", false, "public"),
            ("a2", false, "public"),
            // an original defined elsewhere is assumed public, as most are
            ("a3", false, "public"),
            ("hidden", false, "private"),
            ("a4", false, "private"),
            ("a5", false, "private"),
            // `private :later` after the alias doesn't reach the copy
            ("later", false, "private"),
            ("a6", false, "public"),
            ("build", true, "private"),
            ("a7", true, "private"),
            ("a8", true, "private"),
        ]
        .map(|(n, s, v)| (n.to_string(), s, v));
        assert_eq!(got, expect);
    }
}
