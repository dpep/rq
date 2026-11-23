//! The Ruby walk over Tree-sitter's parse tree.

use tree_sitter::Node;

use super::{DslArgs, alias_visibility, dsl_args, leaf_and_parent};
use crate::core::{Kind, Symbol, Visibility};
use crate::lang::{Ctx, extract_with, qualify};

/// Every definition in `source`, from Tree-sitter's parse.
pub(super) fn extract(file: &str, source: &str) -> Vec<Symbol> {
    extract_with(
        super::LANGUAGE,
        tree_sitter_ruby::LANGUAGE.into(),
        file,
        source,
        |ctx, root, out| walk(ctx, root, None, Visibility::Public, false, out),
    )
}

/// The 1-based line of the grammar's first error or missing node in
/// `source`, if its parse has one.
pub(super) fn first_error_line(source: &str) -> Option<u32> {
    let grammar = tree_sitter_ruby::LANGUAGE.into();
    let tree = crate::lang::parse(super::LANGUAGE, &grammar, source)?;
    let mut node = tree.root_node();
    if !node.has_error() {
        return None;
    }
    while !(node.is_error() || node.is_missing()) {
        let mut cursor = node.walk();
        let Some(next) = node.children(&mut cursor).find(Node::has_error) else {
            break;
        };
        node = next;
    }
    Some(node.start_position().row as u32 + 1)
}

/// Recursively collect definitions. `parent` is the enclosing qualified name;
/// `vis` is the access section in effect (a bare `private`/`protected`/`public`
/// marker flips it for everything after, including through wrapping nodes like
/// `private def foo`). `singleton` is set inside `class << self`, whose
/// definitions are the class's own.
fn walk(
    ctx: &Ctx,
    node: Node,
    parent: Option<&str>,
    vis: Visibility,
    singleton: bool,
    out: &mut Vec<Symbol>,
) {
    let mut vis = vis;
    // the side of the class this body is on, before `module_function` (which
    // makes a module's own copy but leaves `private :x` aimed at the mixin's)
    let class_side = singleton;
    // a bare `module_function` makes what follows the module's own, until an
    // access marker ends it
    let mut module_function = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let singleton = singleton || module_function;
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
                    let (leaf, effective_parent) = leaf_and_parent(&name, parent);
                    let mut s = ctx.symbol(leaf, kind, child, effective_parent.as_deref());
                    s.visibility = Some(Visibility::Public);
                    out.push(s);
                    let qualified = qualify(effective_parent.as_deref(), leaf, "::");
                    // a fresh body starts a fresh (public) access section
                    walk(ctx, child, Some(&qualified), Visibility::Public, false, out);
                } else {
                    walk(ctx, child, parent, vis, singleton, out);
                }
            }
            // `class << self`: a fresh access section, on the class itself
            "singleton_class" => walk(ctx, child, parent, Visibility::Public, true, out),
            "method" | "singleton_method" => {
                if let Some(name) = ctx.field_text(child, "name") {
                    let mut s = ctx.symbol(&name, Kind::Method, child, parent);
                    // `private` sections don't apply to `def self.x`
                    let own = child.kind() == "singleton_method";
                    s.visibility = Some(if own { Visibility::Public } else { vis });
                    s.singleton = own || singleton;
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
                    let original = ctx.field_text(child, "alias").unwrap_or_default();
                    let original = original.trim_start_matches(':');
                    let mut s =
                        ctx.symbol(name.trim_start_matches(':'), Kind::Method, child, parent);
                    s.visibility = Some(alias_visibility(out, parent, singleton, original));
                    s.singleton = singleton;
                    out.push(s);
                }
            }
            "assignment" | "operator_assignment" => {
                // `CONST = …`, `Foo::BAR = …`, `A, B = …`, `RETRIES ||= …`
                // define constants; the right side can hold definitions too
                // (`Foo = Class.new do … end`)
                if let Some(left) = child.child_by_field_name("left") {
                    constants(ctx, left, child, parent, out);
                }
                walk(ctx, child, parent, vis, singleton, out);
            }
            // a bare access marker flips the section for what follows
            "identifier" => match ctx.node_text(child).as_deref() {
                Some(marker @ ("private" | "protected" | "public")) => {
                    vis = match marker {
                        "private" => Visibility::Private,
                        "protected" => Visibility::Protected,
                        _ => Visibility::Public,
                    };
                    module_function = false;
                }
                // the module's copy is public; its instance copy is private
                Some("module_function") => {
                    vis = Visibility::Public;
                    module_function = true;
                }
                _ => {}
            },
            "call" => {
                // metaprogramming: `attr_accessor :x`, `has_many :users`, … are
                // calls that *define* methods Tree-sitter can't see as defs.
                // Emit the literal names, pointing at the macro's line.
                dsl_symbols(ctx, child, parent, vis, singleton, out);
                module_functions(ctx, child, parent, out);
                // still recurse: a call can wrap real definitions
                // (`private def foo` — its `private` identifier flips `vis`
                // on the way down — or `Class.new do … end`)
                let wrapped = out.len();
                walk(ctx, child, parent, vis, singleton, out);
                access_call(ctx, child, parent, class_side, wrapped, out);
            }
            _ => walk(ctx, child, parent, vis, singleton, out),
        }
    }
}

/// `module_function :a, :b` names methods already defined above it: they
/// become the module's own (and public there).
fn module_functions(ctx: &Ctx, call: Node, parent: Option<&str>, out: &mut [Symbol]) {
    if call.child_by_field_name("receiver").is_some()
        || ctx.field_text(call, "method").as_deref() != Some("module_function")
    {
        return;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for name in args
        .named_children(&mut cursor)
        .filter_map(|a| literal_name(ctx, a))
    {
        for s in out
            .iter_mut()
            .filter(|s| s.kind == Kind::Method && s.name == name && s.parent.as_deref() == parent)
        {
            s.singleton = true;
            s.visibility = Some(Visibility::Public);
        }
    }
}

/// An access call with arguments — `private :a, "b"`, `protected [:c]`,
/// `private_class_method :x`, `public_class_method def self.y` — sets the
/// visibility of the methods it names, already defined in this body or an
/// earlier opening of the class in this file, and of the ones it wraps
/// (`out[wrapped..]`). It leaves the section after it alone. Another file's
/// reopening is out of reach: extraction sees one file.
fn access_call(
    ctx: &Ctx,
    call: Node,
    parent: Option<&str>,
    class_side: bool,
    wrapped: usize,
    out: &mut [Symbol],
) {
    if call.child_by_field_name("receiver").is_some() {
        return;
    }
    let Some(method) = ctx.field_text(call, "method") else {
        return;
    };
    let (vis, singleton) = match method.as_str() {
        "private" => (Visibility::Private, class_side),
        "protected" => (Visibility::Protected, class_side),
        "public" => (Visibility::Public, class_side),
        // inside `class << self` these address the singleton class's own
        // singleton, which rq doesn't model (and Ruby raises for a plain def)
        "private_class_method" if !class_side => (Visibility::Private, true),
        "public_class_method" if !class_side => (Visibility::Public, true),
        _ => return,
    };
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut names = Vec::new();
    literal_names(ctx, args, &mut names);
    let ours = |s: &Symbol| {
        s.kind == Kind::Method && s.singleton == singleton && s.parent.as_deref() == parent
    };
    let root = std::iter::successors(Some(call), Node::parent)
        .last()
        .unwrap_or(call);
    let scope = block_scope(call);
    let (before, inside) = out.split_at_mut(wrapped);
    for s in before
        .iter_mut()
        .filter(|s| ours(s) && names.contains(&s.name))
        .filter(|s| block_scope_at(ctx, root, s.line) == scope)
        .chain(inside.iter_mut().filter(|s| ours(s)))
    {
        s.visibility = Some(vis);
    }
}

/// The block a node sits in, short of a class body: a def there lands
/// wherever the block is evaluated (`Struct.new do`, `included do`), so an
/// access call reaches it only from inside the same block.
fn block_scope(node: Node) -> Option<usize> {
    std::iter::successors(node.parent(), Node::parent)
        .find_map(|n| match n.kind() {
            "block" | "do_block" => Some(Some(n.id())),
            "class" | "module" | "singleton_class" => Some(None),
            _ => None,
        })
        .flatten()
}

/// [`block_scope`] of the definition at `line` (a def, or a DSL call like
/// `attr_reader`), found by descending from `root`.
fn block_scope_at(ctx: &Ctx, root: Node, line: u32) -> Option<usize> {
    let mut node = root;
    // not into a def's body, whose blocks hold no defs of the class
    while !matches!(node.kind(), "method" | "singleton_method") {
        let mut cursor = node.walk();
        let next = node.children(&mut cursor).find(|c| {
            (ctx.line(c.start_position().row)..=ctx.line(c.end_position().row)).contains(&line)
        });
        let Some(next) = next else { break };
        node = next;
    }
    block_scope(node)
}

/// Every literal name among `node`'s arguments, through array literals
/// (`[:a, :b]`, `%i[a b]`, `%w[a b]`) and splats of them (`*%i[a b]`).
fn literal_names(ctx: &Ctx, node: Node, names: &mut Vec<String>) {
    let mut cursor = node.walk();
    for arg in node.named_children(&mut cursor) {
        match arg.kind() {
            "array" | "symbol_array" | "string_array" | "splat_argument" => {
                literal_names(ctx, arg, names);
            }
            "bare_symbol" | "bare_string" => names.extend(ctx.node_text(arg)),
            _ => names.extend(literal_name(ctx, arg)),
        }
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
    vis: Visibility,
    singleton: bool,
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
    let vis = if method == "alias_method" {
        let mut cursor = arg_list.walk();
        let original = arg_list
            .named_children(&mut cursor)
            .nth(1)
            .and_then(|a| literal_name(ctx, a))
            .unwrap_or_default();
        alias_visibility(out, parent, singleton, &original)
    } else {
        vis
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
            s.singleton = singleton;
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

/// Emit constant symbols for an assignment's left side: a bare `CONST`, a
/// qualified `Foo::BAR`, or each constant in a multi-assignment list. Always
/// public — method visibility sections never apply to constants, and the
/// retroactive `private_constant` is out of static reach.
fn constants(ctx: &Ctx, left: Node, def: Node, parent: Option<&str>, out: &mut Vec<Symbol>) {
    match left.kind() {
        "constant" | "scope_resolution" => {
            if let Some(name) = ctx.node_text(left) {
                let (leaf, effective_parent) = leaf_and_parent(&name, parent);
                // a scope_resolution leaf can be lowercase (`Foo::bar = 1` is a
                // setter call, not a constant)
                if leaf.starts_with(char::is_uppercase) {
                    let mut s = ctx.symbol(leaf, Kind::Constant, def, effective_parent.as_deref());
                    s.visibility = Some(Visibility::Public);
                    out.push(s);
                }
            }
        }
        "left_assignment_list" => {
            let mut cursor = left.walk();
            for item in left.children(&mut cursor) {
                constants(ctx, item, def, parent, out);
            }
        }
        _ => {}
    }
}
