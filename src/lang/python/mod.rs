//! Python plugin. Extracts `class` → class and `def` (free → function, inside a
//! class → method), qualified with `.` (`method · Account`, nested class
//! `Inner · Outer`). Decorators are transparent — the wrapped def is what counts.
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
            |ctx, root, out| walk(ctx, root, None, false, out),
        )
    }
}

/// `parent` is the enclosing qualified name; `in_class` is true inside a
/// class body, where a `def` is a method.
fn walk(ctx: &Ctx, node: Node, parent: Option<&str>, in_class: bool, out: &mut Vec<Symbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_definition" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let mut s = ctx.symbol(&name, Kind::Class, child, parent);
                    s.visibility = Some(name_visibility(&name));
                    out.push(s);
                    let qualified = qualify(parent, &name, ".");
                    walk(ctx, child, Some(&qualified), true, out);
                }
            }
            "function_definition" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let kind = if in_class {
                        Kind::Method
                    } else {
                        Kind::Function
                    };
                    let mut s = ctx.symbol(&name, kind, child, parent);
                    s.visibility = Some(name_visibility(&name));
                    out.push(s);
                }
                // don't descend into a def body (nested defs rarely navigated)
            }
            // reached only at module/class level — a def body is never walked
            "assignment" => constants(ctx, child, parent, out),
            // a decorated class/function: descend so the wrapped def is seen
            // in the same context
            _ => walk(ctx, child, parent, in_class, out),
        }
    }
}

/// Emit the constant names an assignment binds: `X = …`, `X: int = …`, each
/// name of `A, B = …`, and every target of a chained `A = B = …`.
fn constants(ctx: &Ctx, assign: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    let mut emit = |target: Node| {
        if let Some(name) = ctx.node_text(target)
            && is_constant_name(&name)
        {
            let mut s = ctx.symbol(&name, Kind::Constant, assign, parent);
            s.visibility = Some(name_visibility(&name));
            out.push(s);
        }
    };
    if let Some(left) = assign.child_by_field_name("left") {
        match left.kind() {
            "identifier" => emit(left),
            "pattern_list" | "tuple_pattern" => {
                let mut cursor = left.walk();
                left.named_children(&mut cursor)
                    .filter(|n| n.kind() == "identifier")
                    .for_each(&mut emit);
            }
            _ => {} // `obj.attr = …`, `x[i] = …`: not a new name
        }
    }
    if let Some(right) = assign.child_by_field_name("right")
        && right.kind() == "assignment"
    {
        constants(ctx, right, parent, out);
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
