//! Python plugin. Extracts `class` → class and `def` (free → function, inside a
//! class → method), qualified with `.` (`method · Account`, nested class
//! `Inner · Outer`). Decorators are transparent — the wrapped def is what counts.
//! A `def` nested in another (a closure, a decorator's wrapper) → function,
//! qualified by its enclosing def, and `local`: nothing outside can reach it.
//! Classes and assignments inside a def are locals and stay out. A class whose
//! base visibly is an enum (`Enum`, `IntFlag`, `models.TextChoices`) → enum,
//! and each name its body assigns → variant.
//! An `UPPER_SNAKE` assignment at module or class level → constant: Python has
//! no `const`, so the naming convention is the only declaration of intent there
//! is. A lowercase module variable is ordinary state and stays out.

use tree_sitter::Node;

use crate::core::{Kind, Symbol};
use crate::lang::{Ctx, LanguagePlugin, extract_with, qualify};

const LANGUAGE: &str = "python";

pub(crate) struct Python;

impl LanguagePlugin for Python {
    fn language(&self) -> &'static str {
        LANGUAGE
    }

    fn extensions(&self) -> &[&str] {
        &["py"]
    }

    fn constructor(&self) -> Option<&'static str> {
        Some("__init__")
    }

    fn extract(&self, file: &str, source: &str) -> Vec<Symbol> {
        extract_with(
            LANGUAGE,
            tree_sitter_python::LANGUAGE.into(),
            file,
            source,
            |ctx, root, out| walk(ctx, root, None, Scope::Module, out),
        )
    }
}

/// What a node sits directly inside, which decides what a `def` there is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Module,
    Class,
    /// The body of a class that visibly subclasses an enum: its bindings are
    /// members.
    Enum,
    /// A def body: only nested defs are definitions here.
    Function,
}

/// `parent` is the enclosing qualified name.
fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, scope: Scope, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_definition" if scope != Scope::Function => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let (kind, body) = if is_enum(ctx, child) {
                        (Kind::Enum, Scope::Enum)
                    } else {
                        (Kind::Class, Scope::Class)
                    };
                    let mut s = ctx.symbol(&name, kind, child, parent);
                    s.visibility = Some(name_visibility(&name));
                    out.push(s);
                    let qualified = qualify(parent, &name, ".");
                    walk(ctx, child, Some(&qualified), body, out);
                }
            }
            "function_definition" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let (kind, vis) = match scope {
                        Scope::Class | Scope::Enum => (Kind::Method, name_visibility(&name)),
                        Scope::Module => (Kind::Function, name_visibility(&name)),
                        Scope::Function => (Kind::Function, "local"),
                    };
                    let mut s = ctx.symbol(&name, kind, child, parent);
                    s.visibility = Some(vis);
                    out.push(s);
                    let qualified = qualify(parent, &name, ".");
                    walk(ctx, child, Some(&qualified), Scope::Function, out);
                }
            }
            // a local class holds nothing reachable from outside the def
            "class_definition" => {}
            "assignment" if scope == Scope::Enum => members(ctx, child, parent, out),
            "assignment" if scope != Scope::Function => constants(ctx, child, parent, out),
            // a decorated class/function: descend so the wrapped def is seen
            // in the same context
            _ => walk(ctx, child, parent, scope, out),
        }
    }
}

/// Whether a class visibly subclasses an enum: a base whose last dotted
/// segment ends in `Enum`, `Flag` or `Choices` (`enum.Enum`, `IntFlag`,
/// Django's `models.TextChoices`). Read off the source, so a subclass of an
/// enum that doesn't say so in its name stays a class.
fn is_enum(ctx: &Ctx, class: Node) -> bool {
    let Some(bases) = class.child_by_field_name("superclasses") else {
        return false;
    };
    let mut cursor = bases.walk();
    bases
        .named_children(&mut cursor)
        .filter(|b| matches!(b.kind(), "identifier" | "attribute"))
        .filter_map(|b| ctx.node_text(b))
        .any(|b| {
            let last = b.rsplit('.').next().unwrap_or(&b);
            ["Enum", "Flag", "Choices"]
                .iter()
                .any(|s| last.ends_with(s))
        })
}

/// Emit each member an enum body's assignment binds, as a variant. Names with
/// a leading underscore are Enum's own (`_ignore_`) or private, not members,
/// and a bare annotation (`size: int`) assigns nothing.
fn members(ctx: &Ctx, assign: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    if assign.child_by_field_name("right").is_none() {
        return;
    }
    bindings(assign, &mut |target| {
        if let Some(name) = ctx.node_text(target)
            && !name.starts_with('_')
        {
            let mut s = ctx.symbol(&name, Kind::Variant, assign, parent);
            s.visibility = Some("public");
            out.push(s);
        }
    });
}

/// Emit the constant names an assignment binds.
fn constants(ctx: &Ctx, assign: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    bindings(assign, &mut |target| {
        if let Some(name) = ctx.node_text(target)
            && is_constant_name(&name)
        {
            let mut s = ctx.symbol(&name, Kind::Constant, assign, parent);
            s.visibility = Some(name_visibility(&name));
            out.push(s);
        }
    });
}

/// Hand `emit` each name an assignment binds: `X = …`, `X: int = …`, each
/// name of `A, B = …`, and every target of a chained `A = B = …`.
fn bindings(assign: Node, emit: &mut impl FnMut(Node)) {
    if let Some(left) = assign.child_by_field_name("left") {
        match left.kind() {
            "identifier" => emit(left),
            "pattern_list" | "tuple_pattern" => {
                let mut cursor = left.walk();
                left.named_children(&mut cursor)
                    .filter(|n| n.kind() == "identifier")
                    .for_each(&mut *emit);
            }
            _ => {} // `obj.attr = …`, `x[i] = …`: not a new name
        }
    }
    if let Some(right) = assign.child_by_field_name("right")
        && right.kind() == "assignment"
    {
        bindings(right, emit);
    }
}

/// `UPPER_SNAKE`, with at least two letters — a lone capital is a `TypeVar`
/// (`T = TypeVar("T")`), not a constant.
fn is_constant_name(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().filter(char::is_ascii_uppercase).count() >= 2
}

/// Python's naming convention: a leading underscore marks internal —
/// except dunders (`__init__`), which are the public protocol surface.
fn name_visibility(name: &str) -> &'static str {
    let dunder = name.starts_with("__") && name.ends_with("__");
    if name.starts_with('_') && !dunder {
        "private"
    } else {
        "public"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::testing::find;

    fn extract(source: &str) -> Vec<Symbol> {
        Python.extract("test.py", source)
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
    fn nested_defs_are_private_functions_of_their_enclosing_def() {
        let src = r#"
def _multi_decorate(decorators, method):
    def _wrapper(self, *args):
        def inner():
            pass
        return inner

    class Local:
        def hidden(self):
            pass

    LIMIT = 3
    return _wrapper

class Account:
    def deposit(self):
        @cached
        def helper():
            pass
"#;
        let syms = extract(src);

        let wrapper = find(&syms, "_wrapper");
        assert_eq!(wrapper.kind, Kind::Function);
        assert_eq!(wrapper.parent.as_deref(), Some("_multi_decorate"));
        assert_eq!(wrapper.visibility, Some("local"));

        let inner = find(&syms, "inner");
        assert_eq!(inner.parent.as_deref(), Some("_multi_decorate._wrapper"));
        assert_eq!(inner.visibility, Some("local"));

        // a closure in a method is a function, not a method, of that method
        let helper = find(&syms, "helper");
        assert_eq!(helper.kind, Kind::Function);
        assert_eq!(helper.parent.as_deref(), Some("Account.deposit"));

        // locals of a def: a class (and its methods) and an assignment
        for absent in ["Local", "hidden", "LIMIT"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
    }

    #[test]
    fn visible_enum_subclasses_are_enums_of_variants() {
        let src = r#"
import enum
from django.db import models

class Color(enum.Enum):
    RED = 1
    green = auto()
    _ignore_ = ["tmp"]
    size: int

    def describe(self):
        return self.name

class Year(models.TextChoices):
    FRESHMAN = "FR", _("Freshman")

class Perm(IntFlag, metaclass=Meta):
    READ = 4

class Plain(Base):
    LIMIT = 3
"#;
        let syms = extract(src);
        for (name, parent) in [("Color", None), ("Year", None), ("Perm", None)] {
            let s = find(&syms, name);
            assert_eq!(
                (s.kind, s.parent.as_deref()),
                (Kind::Enum, parent),
                "{name}"
            );
        }
        for (name, parent) in [
            ("RED", "Color"),
            ("green", "Color"),
            ("FRESHMAN", "Year"),
            ("READ", "Perm"),
        ] {
            let s = find(&syms, name);
            assert_eq!(
                (s.kind, s.parent.as_deref()),
                (Kind::Variant, Some(parent)),
                "{name}"
            );
        }
        assert_eq!(find(&syms, "describe").kind, Kind::Method);

        // not members: Enum's own names and a bare annotation
        for absent in ["_ignore_", "size"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
        // a base that doesn't say enum leaves a class of constants
        assert_eq!(find(&syms, "Plain").kind, Kind::Class);
        assert_eq!(find(&syms, "LIMIT").kind, Kind::Constant);
    }

    #[test]
    fn upper_snake_assignments_are_constants() {
        let src = r#"
MAX_RETRIES = 3
TIMEOUT: float = 1.5
LOW, HIGH = 1, 9
FIRST = SECOND = 0
_INTERNAL_LIMIT = 2
T = TypeVar("T")
default_widget = None

try:
    FAST_PATH = True
except ImportError:
    pass

class Account:
    DEFAULT_BALANCE = 0
    kind = "basic"

    def deposit(self, amount):
        LOCAL_CAP = 10
        self.LIMIT = amount
"#;
        let syms = extract(src);

        let max = find(&syms, "MAX_RETRIES");
        assert_eq!(max.kind, Kind::Constant);
        assert_eq!(max.parent, None);
        assert_eq!(max.visibility, Some("public"));

        for name in ["TIMEOUT", "LOW", "HIGH", "FIRST", "SECOND", "FAST_PATH"] {
            assert_eq!(find(&syms, name).kind, Kind::Constant, "{name}");
        }
        assert_eq!(find(&syms, "_INTERNAL_LIMIT").visibility, Some("private"));

        // a class-level constant belongs to its class
        let default = find(&syms, "DEFAULT_BALANCE");
        assert_eq!(default.kind, Kind::Constant);
        assert_eq!(default.parent.as_deref(), Some("Account"));

        // not constants: a TypeVar, lowercase state, a def's locals and attrs
        for absent in ["T", "default_widget", "kind", "LOCAL_CAP", "LIMIT"] {
            assert!(!syms.iter().any(|s| s.name == absent), "{absent}: {syms:?}");
        }
    }

    #[test]
    fn empty_and_unparseable_yield_no_symbols() {
        assert!(extract("").is_empty());
        assert!(extract("# just a comment\n").is_empty());
    }

    #[test]
    fn underscore_names_read_as_private_except_dunders() {
        let src = "class Account:\n    def _internal(self):\n        pass\n    def __init__(self):\n        pass\n\ndef fetch():\n    pass\n";
        let syms = extract(src);
        assert_eq!(find(&syms, "_internal").visibility, Some("private"));
        assert_eq!(find(&syms, "__init__").visibility, Some("public"));
        assert_eq!(find(&syms, "fetch").visibility, Some("public"));
    }
}
