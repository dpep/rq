//! The Ruby walk over prism's syntax tree.
//!
//! State flows down one frame per node — the access section, the side of the
//! class, `module_function` — and a bare marker (`private` alone) flips the
//! frame it sits in, for the siblings after it. That is the shape the
//! Tree-sitter walk (kept in tests as the parity oracle) gave Ruby, kept so the
//! two extract alike from any file both can parse.

use ruby_prism::{CallNode, Location, Node, Visit};

use super::{DslArgs, LANGUAGE, alias_visibility, dsl_args, leaf_and_parent};
use crate::core::{Kind, Symbol, Visibility};
use crate::lang::qualify;

/// Every definition in `source`. Prism recovers from a syntax error locally,
/// so a file mid-edit keeps the definitions around the broken spot.
pub(super) fn extract(file: &str, source: &str) -> Vec<Symbol> {
    let result = ruby_prism::parse(source.as_bytes());
    let mut walk = Walk {
        file,
        lines: result.line_offsets(),
        out: Vec::new(),
        scopes: Vec::new(),
        parent: None,
        frame: Frame::fresh(false),
        stack: Vec::new(),
        flip: None,
    };
    walk.visit(&result.node());
    walk.out
}

/// Whether prism parses `source` without a syntax error: the files the
/// Tree-sitter oracle is expected to agree on.
#[cfg(test)]
pub(super) fn parses_cleanly(source: &str) -> bool {
    ruby_prism::parse(source.as_bytes())
        .errors()
        .next()
        .is_none()
}

/// What a node's definitions inherit from where it sits.
#[derive(Clone, Copy)]
struct Frame {
    /// The access section in effect.
    vis: Visibility,
    /// Definitions here are the class's own (`class << self`, or under a
    /// `module_function` marker further up).
    singleton: bool,
    /// A bare `module_function` came before: what follows is the module's own,
    /// from the next node down, until an access marker ends it.
    module_function: bool,
    /// The side of the class this frame is on before `module_function`, which
    /// makes the module's copy but leaves `private :x` aimed at the mixin's.
    class_side: bool,
    /// The block (by start offset) a def here lands in, short of a class
    /// body: it lands wherever the block runs, out of an access call's reach
    /// from outside the block.
    block: Option<u32>,
}

impl Frame {
    /// A body's fresh, public frame: a class or module's, or `class << self`.
    fn fresh(singleton: bool) -> Frame {
        Frame {
            vis: Visibility::Public,
            singleton,
            module_function: false,
            class_side: singleton,
            block: None,
        }
    }

    fn flip(&mut self, marker: Marker) {
        match marker {
            Marker::Access(vis) => (self.vis, self.module_function) = (vis, false),
            // the module's copy is public; its instance copy is private
            Marker::ModuleFunction => (self.vis, self.module_function) = (Visibility::Public, true),
        }
    }
}

#[derive(Clone, Copy)]
enum Marker {
    Access(Visibility),
    ModuleFunction,
}

fn marker(name: &[u8]) -> Option<Marker> {
    match name {
        b"private" => Some(Marker::Access(Visibility::Private)),
        b"protected" => Some(Marker::Access(Visibility::Protected)),
        b"public" => Some(Marker::Access(Visibility::Public)),
        b"module_function" => Some(Marker::ModuleFunction),
        _ => None,
    }
}

struct Walk<'a> {
    file: &'a str,
    /// The byte offset each line starts at.
    lines: &'a [u32],
    out: Vec<Symbol>,
    /// The block each of `out` was defined in ([`Frame::block`]).
    scopes: Vec<Option<u32>>,
    /// The enclosing qualified name.
    parent: Option<String>,
    frame: Frame,
    /// The frames of the nodes enclosing this one.
    stack: Vec<Frame>,
    /// A bare marker just visited, for its enclosing frame once it's left.
    flip: Option<Marker>,
}

impl Walk<'_> {
    /// The 1-based line holding byte `offset`.
    fn line(&self, offset: u32) -> u32 {
        self.lines.partition_point(|&start| start <= offset) as u32
    }

    fn push(
        &mut self,
        name: &str,
        kind: Kind,
        at: &Location,
        parent: Option<String>,
        visibility: Visibility,
        singleton: bool,
    ) {
        self.out.push(Symbol {
            name: name.to_string(),
            kind,
            language: LANGUAGE.to_string(),
            file: self.file.to_string(),
            line: self.line(at.start()),
            end_line: self.line(at.end()),
            parent,
            visibility: Some(visibility),
            stub: false,
            singleton,
        });
        self.scopes.push(self.frame.block);
    }

    /// A method defined in the frame here, under the enclosing name.
    fn method(&mut self, name: &str, at: &Location, visibility: Visibility, singleton: bool) {
        let parent = self.parent.clone();
        self.push(name, Kind::Method, at, parent, visibility, singleton);
    }

    /// A class or module named `path` (`Foo`, `A::B`, `::Top`), and its body
    /// visited in a fresh frame under its qualified name.
    fn namespace(&mut self, kind: Kind, path: &Node, at: &Location, body: impl FnOnce(&mut Self)) {
        let (leaf, parent) = leaf_and_parent(text(&path.location()), self.parent.as_deref());
        if leaf.is_empty() {
            // a name lost to a syntax error: the body is still walked, in place
            return body(self);
        }
        let qualified = qualify(parent.as_deref(), leaf, "::");
        self.push(leaf, kind, at, parent, Visibility::Public, false);
        let outer = self.parent.replace(qualified);
        self.frame = Frame::fresh(false);
        body(self);
        self.parent = outer;
    }

    /// A constant assigned by `name` (`LIMIT`, `Foo::BAR`, `::ROOT`). Method
    /// sections never apply to it, and `private_constant` is out of static
    /// reach, so it's public.
    fn constant(&mut self, name: &str, at: &Location) {
        let (leaf, parent) = leaf_and_parent(name, self.parent.as_deref());
        // a lowercase leaf is a setter call (`Foo::bar = 1`), not a constant
        if leaf.starts_with(char::is_uppercase) {
            self.push(leaf, Kind::Constant, at, parent, Visibility::Public, false);
        }
    }

    /// Each constant a multiple assignment's targets name.
    fn constant_targets<'pr>(&mut self, targets: impl Iterator<Item = Node<'pr>>, at: &Location) {
        for target in targets {
            if target.as_constant_target_node().is_some()
                || target.as_constant_path_target_node().is_some()
            {
                self.constant(text(&target.location()), at);
            }
        }
    }

    /// `attr_accessor :x`, `has_many :users`, … define methods no `def`
    /// spells: emit their literal names, at the macro's line. A computed name
    /// (`define_method(name)`) can't be resolved statically, so it's skipped.
    fn dsl_symbols(&mut self, call: &CallNode, method: &[u8]) {
        let Some(args) = std::str::from_utf8(method).ok().and_then(dsl_args) else {
            return;
        };
        let Some(arguments) = call.arguments() else {
            return;
        };
        let arguments = arguments.arguments();
        let (vis, singleton) = (self.frame.vis, self.frame.singleton);
        let vis = if method == b"alias_method" {
            let original = arguments.iter().nth(1);
            let original = original.as_ref().and_then(literal_name).unwrap_or_default();
            alias_visibility(&self.out, self.parent.as_deref(), singleton, original)
        } else {
            vis
        };
        // later args of a one-name macro are options (`scope :active, -> {…}`)
        let names = match args {
            DslArgs::All => usize::MAX,
            DslArgs::First => 1,
        };
        for arg in arguments.iter().take(names) {
            if let Some(name) = literal_name(&arg).filter(|n| !n.is_empty()) {
                self.method(name, &call.location(), vis, singleton);
            }
        }
    }

    /// `module_function :a, :b` names methods already defined above it: they
    /// become the module's own (and public there).
    fn module_functions(&mut self, call: &CallNode, method: &[u8]) {
        if method != b"module_function" {
            return;
        }
        let Some(arguments) = call.arguments() else {
            return;
        };
        for name in arguments
            .arguments()
            .iter()
            .filter_map(|a| literal_name(&a))
        {
            for s in self
                .out
                .iter_mut()
                .filter(|s| s.kind == Kind::Method && s.name == name && s.parent == self.parent)
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
    fn access_call(&mut self, call: &CallNode, method: &[u8], wrapped: usize) {
        // the frame the call sits in, not its own
        let class_side = self.stack.last().is_some_and(|f| f.class_side);
        let (vis, singleton) = match method {
            b"private" => (Visibility::Private, class_side),
            b"protected" => (Visibility::Protected, class_side),
            b"public" => (Visibility::Public, class_side),
            // inside `class << self` these address the singleton class's own
            // singleton, which rq doesn't model (and Ruby raises for a plain def)
            b"private_class_method" if !class_side => (Visibility::Private, true),
            b"public_class_method" if !class_side => (Visibility::Public, true),
            _ => return,
        };
        let Some(arguments) = call.arguments() else {
            return;
        };
        let mut names = Vec::new();
        for arg in &arguments.arguments() {
            literal_names(&arg, &mut names);
        }
        let scope = self.frame.block;
        for (i, s) in self.out.iter_mut().enumerate() {
            let ours =
                s.kind == Kind::Method && s.singleton == singleton && s.parent == self.parent;
            if ours
                && (i >= wrapped || (self.scopes[i] == scope && names.contains(&s.name.as_str())))
            {
                s.visibility = Some(vis);
            }
        }
    }
}

/// The source text at `loc`. Prism's locations fall on character boundaries
/// of the `&str` it parsed, so the slice is UTF-8.
fn text<'pr>(loc: &Location<'pr>) -> &'pr str {
    std::str::from_utf8(loc.as_slice()).unwrap_or_default()
}

/// The name a literal `:symbol` or quoted string argument carries, as its
/// source spells it. Not a `:"quoted"` symbol, a heredoc, a `?c` character,
/// or adjacent strings run together.
fn literal_name<'pr>(node: &Node<'pr>) -> Option<&'pr str> {
    if let Some(symbol) = node.as_symbol_node() {
        let t = text(&symbol.location());
        return (t.starts_with(':') && !t.starts_with(":\"") && !t.starts_with(":'"))
            .then(|| t.trim_start_matches(':'));
    }
    let opening = match node.as_string_node() {
        Some(s) => s.opening_loc(),
        None => node.as_interpolated_string_node()?.opening_loc(),
    };
    let opening = text(&opening?);
    if opening.starts_with("<<") || opening == "?" {
        return None;
    }
    Some(text(&node.location()).trim_matches(|c| c == '"' || c == '\''))
}

/// Every literal name an access call's argument holds, through array literals
/// (`[:a, :b]`, `%i[a b]`, `%w[a b]`) and splats of them (`*%i[a b]`).
fn literal_names<'pr>(node: &Node<'pr>, names: &mut Vec<&'pr str>) {
    if let Some(array) = node.as_array_node() {
        let words = array
            .opening_loc()
            .is_some_and(|o| o.as_slice().starts_with(b"%"));
        for element in &array.elements() {
            if words {
                names.push(text(&element.location()));
            } else {
                literal_names(&element, names);
            }
        }
    } else if let Some(splat) = node.as_splat_node() {
        if let Some(inner) = splat.expression() {
            literal_names(&inner, names);
        }
    } else {
        names.extend(literal_name(node));
    }
}

impl<'pr> Visit<'pr> for Walk<'_> {
    fn visit_branch_node_enter(&mut self, _node: Node<'pr>) {
        self.stack.push(self.frame);
        let singleton = self.frame.singleton || self.frame.module_function;
        self.frame = Frame {
            singleton,
            module_function: false,
            class_side: singleton,
            ..self.frame
        };
    }

    fn visit_branch_node_leave(&mut self) {
        self.frame = self.stack.pop().expect("a leave follows its enter");
        if let Some(marker) = self.flip.take() {
            self.frame.flip(marker);
        }
    }

    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        let (superclass, body) = (node.superclass(), node.body());
        self.namespace(Kind::Class, &node.constant_path(), &node.location(), |w| {
            if let Some(s) = superclass {
                w.visit(&s);
            }
            if let Some(b) = body {
                w.visit(&b);
            }
        });
    }

    fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
        let body = node.body();
        self.namespace(Kind::Module, &node.constant_path(), &node.location(), |w| {
            if let Some(b) = body {
                w.visit(&b);
            }
        });
    }

    /// `class << self`: a fresh access section, on the class itself.
    fn visit_singleton_class_node(&mut self, node: &ruby_prism::SingletonClassNode<'pr>) {
        self.frame = Frame::fresh(true);
        ruby_prism::visit_singleton_class_node(self, node);
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        // the name as written: prism's own drops the `@` of `def ~@`
        let name = text(&node.name_loc());
        // `private` sections don't apply to `def self.x`
        let own = node.receiver().is_some();
        let vis = if own {
            Visibility::Public
        } else {
            self.frame.vis
        };
        let singleton = own || self.frame.singleton;
        self.method(name, &node.location(), vis, singleton);
        // method bodies rarely hold further definitions; don't descend
    }

    /// `alias new old`, the keyword form of `alias_method`. A global-variable
    /// alias is another node, and defines no method.
    fn visit_alias_method_node(&mut self, node: &ruby_prism::AliasMethodNode<'pr>) {
        let name = text(&node.new_name().location()).trim_start_matches(':');
        let original = text(&node.old_name().location()).trim_start_matches(':');
        let singleton = self.frame.singleton;
        let vis = alias_visibility(&self.out, self.parent.as_deref(), singleton, original);
        self.method(name, &node.location(), vis, singleton);
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let method = node.name().as_slice();
        if node.is_variable_call() {
            // a bare marker flips the section for the siblings after it
            self.flip = marker(method);
            return;
        }
        let bare = node.receiver().is_none();
        if bare {
            self.dsl_symbols(node, method);
            self.module_functions(node, method);
        }
        // `private def foo`: the marker's name scopes what the call wraps
        if let Some(marker) = marker(method) {
            self.frame.flip(marker);
        }
        let wrapped = self.out.len();
        ruby_prism::visit_call_node(self, node);
        if bare {
            self.access_call(node, method, wrapped);
        }
    }

    fn visit_block_node(&mut self, node: &ruby_prism::BlockNode<'pr>) {
        self.frame.block = Some(node.location().start());
        ruby_prism::visit_block_node(self, node);
    }

    fn visit_lambda_node(&mut self, node: &ruby_prism::LambdaNode<'pr>) {
        self.frame.block = Some(node.location().start());
        ruby_prism::visit_lambda_node(self, node);
    }

    // `CONST = …`, `Foo::BAR = …`, `A, B = …`, `RETRIES ||= …` define
    // constants; the right side can hold definitions too (`Foo = Class.new do`)

    fn visit_constant_write_node(&mut self, node: &ruby_prism::ConstantWriteNode<'pr>) {
        self.constant(text(&node.name_loc()), &node.location());
        ruby_prism::visit_constant_write_node(self, node);
    }

    fn visit_constant_or_write_node(&mut self, node: &ruby_prism::ConstantOrWriteNode<'pr>) {
        self.constant(text(&node.name_loc()), &node.location());
        ruby_prism::visit_constant_or_write_node(self, node);
    }

    fn visit_constant_and_write_node(&mut self, node: &ruby_prism::ConstantAndWriteNode<'pr>) {
        self.constant(text(&node.name_loc()), &node.location());
        ruby_prism::visit_constant_and_write_node(self, node);
    }

    fn visit_constant_operator_write_node(
        &mut self,
        node: &ruby_prism::ConstantOperatorWriteNode<'pr>,
    ) {
        self.constant(text(&node.name_loc()), &node.location());
        ruby_prism::visit_constant_operator_write_node(self, node);
    }

    fn visit_constant_path_write_node(&mut self, node: &ruby_prism::ConstantPathWriteNode<'pr>) {
        self.constant(text(&node.target().location()), &node.location());
        ruby_prism::visit_constant_path_write_node(self, node);
    }

    fn visit_constant_path_or_write_node(
        &mut self,
        node: &ruby_prism::ConstantPathOrWriteNode<'pr>,
    ) {
        self.constant(text(&node.target().location()), &node.location());
        ruby_prism::visit_constant_path_or_write_node(self, node);
    }

    fn visit_constant_path_and_write_node(
        &mut self,
        node: &ruby_prism::ConstantPathAndWriteNode<'pr>,
    ) {
        self.constant(text(&node.target().location()), &node.location());
        ruby_prism::visit_constant_path_and_write_node(self, node);
    }

    fn visit_constant_path_operator_write_node(
        &mut self,
        node: &ruby_prism::ConstantPathOperatorWriteNode<'pr>,
    ) {
        self.constant(text(&node.target().location()), &node.location());
        ruby_prism::visit_constant_path_operator_write_node(self, node);
    }

    fn visit_multi_write_node(&mut self, node: &ruby_prism::MultiWriteNode<'pr>) {
        let at = node.location();
        self.constant_targets(node.lefts().iter().chain(node.rights().iter()), &at);
        ruby_prism::visit_multi_write_node(self, node);
    }
}
