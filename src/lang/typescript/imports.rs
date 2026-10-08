//! Where an imported name is defined, by the module resolution TypeScript and
//! Node apply, within the checkout (D63).
//!
//! The importing file's bindings are read from its top-level statements:
//! `import { a, b as c } from`, `import a from`, `import * as ns from`,
//! `import a = require(…)`, `export { a } from`, CommonJS's
//! `const { a, b: c } = require(…)`, `const ns = require(…)` and
//! `const a = require(…).b`. A namespace binding counts only where the anchor's
//! line spells `ns.name`. The named file is then followed through its own
//! re-exports (`export { a } from`, `export * from`, `module.exports =
//! require(…)`, an import it passes on), a bounded number of hops.
//!
//! A relative specifier is probed as TypeScript does: `./x.js` naming `./x.ts`,
//! then the extensions, then `/index`. A bare one resolves only to a package
//! the checkout's own workspace declares (`package.json` `workspaces`,
//! `pnpm-workspace.yaml`), by its `exports`, `types`, `module` or `main`; a
//! built path missing from the checkout (`dist/x.js`) is read as its source
//! (`src/x.ts`). Third-party packages, `node_modules` and `tsconfig` path
//! aliases aren't resolved.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tree_sitter::{Node, Parser};

use crate::core::ImportTarget;

/// Re-export hops followed past the imported file: a package's entry to its
/// barrel to the defining file is two.
const MAX_HOPS: usize = 3;
/// Files a resolution may name, bounding an `export *` fan-out.
const MAX_TARGETS: usize = 32;
/// Larger files are bundles, not modules anyone imports by hand.
const MAX_BYTES: u64 = 1 << 20;
/// Larger sources parse only their module header and later module statements
/// (see [`module_statements`]): a whole 200 KB module costs tens of
/// milliseconds per query.
const PARSE_WHOLE: usize = 16 * 1024;
/// Workspace directories a discovery may list.
const MAX_PACKAGES: usize = 5_000;
const EXTS: [&str; 9] = [
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
];
/// Output directories a package's manifest points into, absent from a checkout
/// that hasn't been built; their source is conventionally `src/`.
const BUILD_DIRS: [&str; 7] = ["dist", "build", "lib", "esm", "cjs", "out", "es"];
/// `exports` conditions in the order they're tried; the rest follow.
const CONDITIONS: [&str; 7] = [
    "types", "source", "import", "module", "default", "require", "node",
];

/// The definitions `name`, as `file` (relative to `root`) uses it at `line`,
/// resolves to: the imported file first, then each it re-exports from.
pub(super) fn resolve(root: &Path, file: &str, line: usize, name: &str) -> Vec<ImportTarget> {
    let mut r = Resolver {
        root,
        packages: None,
    };
    let mut out: Vec<ImportTarget> = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(file.to_string(), name.to_string(), 0)]);
    while let Some((from, wanted, depth)) = queue.pop_front() {
        let Some(source) = read(&root.join(&from)) else {
            continue;
        };
        // a cheap test before a parse: what can't name it can't pass it on
        if !source.contains(wanted.as_str())
            && (depth == 0 || !(source.contains('*') || source.contains("module.exports")))
        {
            continue;
        }
        let used = (depth == 0).then(|| {
            let row = line.saturating_sub(1);
            (row, source.lines().nth(row).unwrap_or(""))
        });
        let bindings = bindings(&from, &module_statements(&source));
        for (spec, target) in edges(&bindings, &wanted, used) {
            for found in r.locate(&from, &spec) {
                if found == from || !seen.insert((found.clone(), target.clone())) {
                    continue;
                }
                out.push(ImportTarget {
                    file: found.clone(),
                    name: target.clone(),
                });
                if out.len() >= MAX_TARGETS {
                    return out;
                }
                if depth < MAX_HOPS {
                    queue.push_back((found, target.clone(), depth + 1));
                }
            }
        }
    }
    out
}

/// A name a module's top-level statements bind from another module.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// `import { imported as local }`, `export { imported as local } from`,
    /// `const { imported: local } = require(…)`.
    Named {
        local: String,
        imported: String,
        spec: String,
        /// The statement's lines, 0-based: where the imported name is spelled.
        rows: (usize, usize),
    },
    /// `import local from`.
    Default { local: String, spec: String },
    /// `import * as local`, `const local = require(…)`, `import local = require(…)`.
    Namespace { local: String, spec: String },
    /// `export * from`, `module.exports = require(…)`.
    Star { spec: String },
}

/// Where `wanted` leads from these bindings: each specifier and the name the
/// definition has there. `used` is the anchor's row and line in the importing
/// file; `None` past the first hop, where a star re-export also counts, unless
/// an explicit one names the name.
///
/// A module binds, and re-exports, the local name: `import { a as b }` and
/// `export { a as b } from` are `b`. The original `a` counts only on the
/// anchor's own import statement, as a namespace counts only where the
/// anchor's line spells `ns.name`.
fn edges(bindings: &[Binding], wanted: &str, used: Option<(usize, &str)>) -> Vec<(String, String)> {
    let mut explicit = Vec::new();
    let mut stars = Vec::new();
    for b in bindings {
        match b {
            Binding::Named {
                local,
                imported,
                spec,
                rows,
            } if local == wanted
                || (imported == wanted
                    && used.is_some_and(|(row, _)| (rows.0..=rows.1).contains(&row))) =>
            {
                // a default has no name of its own to look up; the local one is the best guess
                let target = if imported == "default" {
                    local
                } else {
                    imported
                };
                explicit.push((spec.clone(), target.clone()));
            }
            Binding::Default { local, spec } if local == wanted => {
                explicit.push((spec.clone(), wanted.to_string()));
            }
            Binding::Namespace { local, spec }
                if local == wanted || used.is_some_and(|(_, l)| spells(l, local, wanted)) =>
            {
                explicit.push((spec.clone(), wanted.to_string()));
            }
            Binding::Star { spec } if used.is_none() => {
                stars.push((spec.clone(), wanted.to_string()));
            }
            _ => {}
        }
    }
    if explicit.is_empty() { stars } else { explicit }
}

/// `line` spells `ns.name` as a member access of its own: not `myns.name` or
/// `ns.names`.
fn spells(line: &str, ns: &str, name: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let needle = format!("{ns}.{name}");
    line.match_indices(&needle).any(|(i, _)| {
        !line[..i]
            .chars()
            .next_back()
            .is_some_and(|c| ident(c) || c == '.')
            && !line[i + needle.len()..].chars().next().is_some_and(ident)
    })
}

/// What of a large `source` can bind a module: the header up to the first line
/// of code at column 0 (a function, class, type or exported declaration; a
/// `const` may be a `require` and doesn't end it), then any later top-level
/// `import`, `export … from` or `module.exports = require(…)` statement. A
/// small source is parsed whole. Skipped lines are left blank, so a statement
/// keeps its line.
fn module_statements(source: &str) -> std::borrow::Cow<'_, str> {
    if source.len() <= PARSE_WHOLE {
        return source.into();
    }
    let mut lines: Vec<&str> = source.lines().collect();
    let Some(cut) = lines.iter().position(|l| is_code(l)) else {
        return source.into();
    };
    let mut i = cut;
    while i < lines.len() {
        let line = lines[i];
        let module = line.starts_with("import ")
            || (line.starts_with("export ") && !is_code(line))
            || line.starts_with("module.exports = require(");
        if !module {
            lines[i] = "";
            i += 1;
            continue;
        }
        // the statement runs to its specifier, or to the end of a local list
        let end = (i..lines.len().min(i + 64))
            .find(|&j| {
                let l = lines[j];
                l.contains("from") || l.contains("require(") || l.trim_end().ends_with(';')
            })
            .unwrap_or(i);
        i = end + 1;
    }
    lines.join("\n").into()
}

/// A top-level line that starts code rather than a module statement.
fn is_code(line: &str) -> bool {
    const CODE: [&str; 26] = [
        "function ",
        "function*",
        "async ",
        "class ",
        "abstract ",
        "interface ",
        "enum ",
        "declare ",
        "namespace ",
        "if ",
        "if(",
        "for ",
        "while ",
        "try ",
        "switch ",
        "export function",
        "export async",
        "export class",
        "export abstract",
        "export default",
        "export const ",
        "export let ",
        "export var ",
        "export enum",
        "export interface",
        "export declare",
    ];
    CODE.iter().any(|c| line.starts_with(c))
        || ((line.starts_with("type ") || line.starts_with("export type "))
            && !line.contains(" from")
            && !line.contains('{'))
}

/// The module bindings of `file`'s top-level statements.
fn bindings(file: &str, source: &str) -> Vec<Binding> {
    thread_local! {
        static PARSERS: std::cell::RefCell<HashMap<&'static str, Parser>> =
            std::cell::RefCell::new(HashMap::new());
    }
    // the plugin's own choice: TSX reads JS too, but TS's `<T>` is a type argument
    let (key, grammar) =
        if file.ends_with(".ts") || file.ends_with(".mts") || file.ends_with(".cts") {
            super::ts()
        } else {
            super::tsx()
        };
    let tree = PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let parser = match parsers.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut p = Parser::new();
                p.set_language(&grammar).ok()?;
                v.insert(p)
            }
        };
        parser.parse(source, None)
    });
    let Some(tree) = tree else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut out = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        match stmt.kind() {
            "import_statement" => import(stmt, src, &mut out),
            "export_statement" => reexport(stmt, src, &mut out),
            "lexical_declaration" | "variable_declaration" => required(stmt, src, &mut out),
            "expression_statement" => module_exports(stmt, src, &mut out),
            _ => {}
        }
    }
    out
}

fn text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or_default().to_string()
}

/// A string literal's contents.
fn string(node: Node, src: &[u8]) -> Option<String> {
    let t = node.utf8_text(src).ok()?;
    (node.kind() == "string" && t.len() >= 2).then(|| t[1..t.len() - 1].to_string())
}

fn children(node: Node) -> Vec<Node> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}

fn import(stmt: Node, src: &[u8], out: &mut Vec<Binding>) {
    for child in children(stmt) {
        if child.kind() == "import_require_clause" {
            let local = children(child)
                .into_iter()
                .find(|n| n.kind() == "identifier");
            if let (Some(local), Some(spec)) = (
                local,
                child
                    .child_by_field_name("source")
                    .and_then(|s| string(s, src)),
            ) {
                out.push(Binding::Namespace {
                    local: text(local, src),
                    spec,
                });
            }
        }
    }
    let Some(spec) = stmt
        .child_by_field_name("source")
        .and_then(|s| string(s, src))
    else {
        return;
    };
    let Some(clause) = children(stmt)
        .into_iter()
        .find(|n| n.kind() == "import_clause")
    else {
        return;
    };
    for part in children(clause) {
        match part.kind() {
            "identifier" => out.push(Binding::Default {
                local: text(part, src),
                spec: spec.clone(),
            }),
            "namespace_import" => {
                if let Some(id) = children(part)
                    .into_iter()
                    .find(|n| n.kind() == "identifier")
                {
                    out.push(Binding::Namespace {
                        local: text(id, src),
                        spec: spec.clone(),
                    });
                }
            }
            "named_imports" => {
                for s in children(part)
                    .into_iter()
                    .filter(|n| n.kind() == "import_specifier")
                {
                    out.extend(specifier(s, src, &spec, rows(stmt)));
                }
            }
            _ => {}
        }
    }
}

/// A statement's first and last rows.
fn rows(node: Node) -> (usize, usize) {
    (node.start_position().row, node.end_position().row)
}

/// An `import_specifier` / `export_specifier`: `name` or `name as alias`.
fn specifier(node: Node, src: &[u8], spec: &str, rows: (usize, usize)) -> Option<Binding> {
    let name = node.child_by_field_name("name")?;
    let imported = string(name, src).unwrap_or_else(|| text(name, src));
    let local = node
        .child_by_field_name("alias")
        .map_or_else(|| imported.clone(), |a| text(a, src));
    Some(Binding::Named {
        local,
        imported,
        spec: spec.to_string(),
        rows,
    })
}

fn reexport(stmt: Node, src: &[u8], out: &mut Vec<Binding>) {
    let Some(spec) = stmt
        .child_by_field_name("source")
        .and_then(|s| string(s, src))
    else {
        return;
    };
    let mut named = false;
    for part in children(stmt) {
        match part.kind() {
            "export_clause" => {
                named = true;
                for s in children(part)
                    .into_iter()
                    .filter(|n| n.kind() == "export_specifier")
                {
                    out.extend(specifier(s, src, &spec, rows(stmt)));
                }
            }
            "namespace_export" => {
                named = true;
                if let Some(id) = children(part).into_iter().find(|n| n.kind() != "string") {
                    out.push(Binding::Namespace {
                        local: text(id, src),
                        spec: spec.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    if !named {
        out.push(Binding::Star { spec });
    }
}

/// `require('x')`'s specifier.
fn require_of(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "call_expression"
        || node
            .child_by_field_name("function")
            .map(|f| text(f, src))
            .as_deref()
            != Some("require")
    {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    children(args)
        .into_iter()
        .next()
        .and_then(|a| string(a, src))
}

fn required(stmt: Node, src: &[u8], out: &mut Vec<Binding>) {
    for decl in children(stmt)
        .into_iter()
        .filter(|n| n.kind() == "variable_declarator")
    {
        let (Some(name), Some(value)) = (
            decl.child_by_field_name("name"),
            decl.child_by_field_name("value"),
        ) else {
            continue;
        };
        // `const a = require('x').b`
        if value.kind() == "member_expression"
            && name.kind() == "identifier"
            && let Some(spec) = value
                .child_by_field_name("object")
                .and_then(|o| require_of(o, src))
            && let Some(prop) = value.child_by_field_name("property")
        {
            out.push(Binding::Named {
                local: text(name, src),
                imported: text(prop, src),
                spec,
                rows: rows(stmt),
            });
            continue;
        }
        let Some(spec) = require_of(value, src) else {
            continue;
        };
        match name.kind() {
            "identifier" => out.push(Binding::Namespace {
                local: text(name, src),
                spec,
            }),
            "object_pattern" => {
                for p in children(name) {
                    let (imported, local) = match p.kind() {
                        "shorthand_property_identifier_pattern" => (text(p, src), text(p, src)),
                        "pair_pattern" => {
                            match (p.child_by_field_name("key"), p.child_by_field_name("value")) {
                                (Some(k), Some(v)) if v.kind() == "identifier" => {
                                    (text(k, src), text(v, src))
                                }
                                _ => continue,
                            }
                        }
                        _ => continue,
                    };
                    out.push(Binding::Named {
                        local,
                        imported,
                        spec: spec.clone(),
                        rows: rows(stmt),
                    });
                }
            }
            _ => {}
        }
    }
}

/// `module.exports = require('x')`: everything `x` exports.
fn module_exports(stmt: Node, src: &[u8], out: &mut Vec<Binding>) {
    let Some(assign) = children(stmt)
        .into_iter()
        .find(|n| n.kind() == "assignment_expression")
    else {
        return;
    };
    if assign
        .child_by_field_name("left")
        .map(|l| text(l, src))
        .as_deref()
        == Some("module.exports")
        && let Some(spec) = assign
            .child_by_field_name("right")
            .and_then(|r| require_of(r, src))
    {
        out.push(Binding::Star { spec });
    }
}

/// A source or manifest, read as the index reads source (regular files only,
/// binary skipped), and no bigger than a module anyone navigates through.
fn read(path: &Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    crate::index::read_source(path)
        .ok()
        .filter(|s| !s.is_empty())
}

/// A repo-relative path with `.` and `..` folded; `None` if it leaves the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Declaration extensions, each with the implementations it declares.
const DECLARATIONS: [(&str, &[&str]); 3] = [
    (".d.ts", &[".js", ".jsx"]),
    (".d.mts", &[".mjs"]),
    (".d.cts", &[".cjs"]),
];

/// `path` without a module extension (`.d.ts` whole).
fn stem(path: &str) -> &str {
    DECLARATIONS
        .iter()
        .map(|(d, _)| d)
        .chain(EXTS.iter())
        .find_map(|e| path.strip_suffix(e))
        .unwrap_or(path)
}

/// A declaration file: types only, its implementation elsewhere.
fn is_declaration(path: &str) -> bool {
    DECLARATIONS.iter().any(|(d, _)| path.ends_with(d))
}

/// A workspace's packages: name → directory relative to the checkout root.
type Packages = Arc<HashMap<String, String>>;

struct Resolver<'a> {
    root: &'a Path,
    packages: Option<Packages>,
}

impl Resolver<'_> {
    fn is_file(&self, rel: &str) -> bool {
        !rel.is_empty() && self.root.join(rel).is_file()
    }

    /// The files `spec`, imported from `from`, names: the module TypeScript
    /// resolves, and when that's a declaration (`.d.ts`, a manifest's `types`),
    /// the implementation it declares too, which the stub penalty ranks first.
    fn locate(&mut self, from: &str, spec: &str) -> Vec<String> {
        if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
            let dir = from.rfind('/').map_or("", |i| &from[..i]);
            let Some(found) = normalize(&join(dir, spec)).and_then(|base| self.probe(&base)) else {
                return Vec::new();
            };
            let beside = self.beside(&found);
            return std::iter::once(found).chain(beside).collect();
        }
        if spec.starts_with('/') || spec.contains(':') {
            return Vec::new(); // absolute, or a scheme (`node:fs`)
        }
        self.package(spec)
    }

    /// The implementation beside a declaration: `x.js` for `x.d.ts`.
    fn beside(&self, found: &str) -> Option<String> {
        let (decl, impls) = DECLARATIONS.iter().find(|(d, _)| found.ends_with(d))?;
        let stem = &found[..found.len() - decl.len()];
        impls
            .iter()
            .map(|e| format!("{stem}{e}"))
            .find(|p| self.is_file(p))
    }

    /// TypeScript's probing for a module path, then the source of a built one.
    fn probe(&self, base: &str) -> Option<String> {
        self.probe_exact(base).or_else(|| {
            let segs: Vec<&str> = base.split('/').collect();
            let i = segs.iter().rposition(|s| BUILD_DIRS.contains(s))?;
            // `dist/esm/x.js` is `src/x.ts`: any build dirs after it go too
            let rest: Vec<&str> = segs[i + 1..]
                .iter()
                .copied()
                .skip_while(|s| BUILD_DIRS.contains(s))
                .collect();
            let source = [&segs[..i], &["src"], &rest[..]].concat().join("/");
            self.probe_exact(stem(&source))
        })
    }

    fn probe_exact(&self, base: &str) -> Option<String> {
        // `./x.js` in TypeScript's ESM style names the `.ts` it compiles from
        let sources: &[&str] = match base.rsplit_once('.').map(|(_, e)| e) {
            Some("js") => &[".ts", ".tsx"],
            Some("jsx") => &[".tsx"],
            Some("mjs") => &[".mts"],
            Some("cjs") => &[".cts"],
            _ => &[],
        };
        let base_stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
        sources
            .iter()
            .map(|e| format!("{base_stem}{e}"))
            .chain(std::iter::once(base.to_string()))
            .chain(EXTS.iter().map(|e| format!("{base}{e}")))
            .chain(EXTS.iter().map(|e| format!("{base}/index{e}")))
            .find(|p| self.is_file(p))
    }

    /// A bare specifier naming one of the workspace's own packages: its
    /// preferred entry, then if that's a declaration, the first entry that
    /// isn't (`main` beside `types`), or else the implementation beside it.
    fn package(&mut self, spec: &str) -> Vec<String> {
        let parts: Vec<&str> = spec.splitn(3, '/').collect();
        let (name, sub) = if spec.starts_with('@') && parts.len() >= 2 {
            (
                format!("{}/{}", parts[0], parts[1]),
                parts.get(2).copied().unwrap_or(""),
            )
        } else {
            (
                parts[0].to_string(),
                spec.split_once('/').map_or("", |(_, s)| s),
            )
        };
        let root = self.root;
        let packages = self.packages.get_or_insert_with(|| workspace(root));
        let Some(dir) = packages.get(&name).cloned() else {
            return Vec::new();
        };
        let manifest: Value = read(&self.root.join(join(&dir, "package.json")))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let mut found = entries(&manifest, sub)
            .into_iter()
            .filter_map(|e| normalize(&join(&dir, &e)))
            .filter_map(|p| self.probe(&p));
        let Some(first) = found.next() else {
            return Vec::new();
        };
        if !is_declaration(&first) {
            return vec![first];
        }
        let implementation = found
            .find(|p| !is_declaration(p))
            .or_else(|| self.beside(&first));
        std::iter::once(first).chain(implementation).collect()
    }
}

/// The paths (relative to the package) a manifest maps subpath `sub` to, most
/// preferred first; the subpath itself or the package root comes last.
fn entries(manifest: &Value, sub: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(exports) = manifest.get("exports") {
        let key = if sub.is_empty() {
            ".".to_string()
        } else {
            format!("./{sub}")
        };
        // a bare string, array or condition map is the `.` entry
        let sugar = !exports
            .as_object()
            .is_some_and(|m| m.keys().any(|k| k.starts_with('.')));
        if sugar {
            if sub.is_empty() {
                conditions(exports, None, &mut out);
            }
        } else if let Some(map) = exports.as_object() {
            if let Some(v) = map.get(&key) {
                conditions(v, None, &mut out);
            } else if let Some((v, star)) = map.iter().find_map(|(k, v)| {
                let (pre, post) = k.split_once('*')?;
                let mid = key.strip_prefix(pre)?.strip_suffix(post)?;
                Some((v, mid))
            }) {
                conditions(v, Some(star), &mut out);
            }
        }
    }
    if sub.is_empty() {
        for field in ["types", "typings", "module", "main"] {
            if let Some(s) = manifest.get(field).and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
        out.push(".".to_string());
    } else {
        out.push(sub.to_string());
    }
    out
}

/// An `exports` value's targets, preferred conditions first; `star` fills a
/// pattern's `*`.
fn conditions(v: &Value, star: Option<&str>, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(star.map_or_else(|| s.clone(), |m| s.replace('*', m))),
        Value::Array(items) => items.iter().for_each(|i| conditions(i, star, out)),
        Value::Object(map) => {
            let preferred = CONDITIONS.iter().filter_map(|c| map.get(*c));
            let rest = map
                .iter()
                .filter(|(k, _)| !CONDITIONS.contains(&k.as_str()))
                .map(|(_, v)| v);
            for v in preferred.chain(rest) {
                conditions(v, star, out);
            }
        }
        _ => {}
    }
}

/// The workspace's packages, by name → directory (relative to `root`, `""` for
/// the root's own), read once per process.
fn workspace(root: &Path) -> Packages {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Packages>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().ok().and_then(|c| c.get(root).cloned()) {
        return found;
    }
    let found = Arc::new(discover(root));
    if let Ok(mut c) = cache.lock() {
        c.insert(root.to_path_buf(), Arc::clone(&found));
    }
    found
}

fn discover(root: &Path) -> HashMap<String, String> {
    let manifest: Value = read(&root.join("package.json"))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut patterns: Vec<String> = manifest
        .get("workspaces")
        .and_then(|w| w.as_array().or_else(|| w.get("packages")?.as_array()))
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    patterns.extend(read(&root.join("pnpm-workspace.yaml")).map_or_else(Vec::new, |y| pnpm(&y)));
    if patterns.is_empty() {
        patterns.push("packages/*".to_string());
    }
    let mut out = HashMap::new();
    if let Some(name) = manifest.get("name").and_then(Value::as_str) {
        out.insert(name.to_string(), String::new());
    }
    let mut dirs = Vec::new();
    for p in patterns.iter().filter(|p| !p.starts_with('!')) {
        expand(root, p.trim_end_matches('/'), &mut dirs);
    }
    for dir in dirs {
        let name = read(&root.join(join(&dir, "package.json")))
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|m| m.get("name")?.as_str().map(str::to_string));
        if let Some(name) = name {
            out.entry(name).or_insert(dir);
        }
    }
    out
}

/// The `packages:` list of a `pnpm-workspace.yaml`, read line by line.
fn pnpm(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in yaml.lines() {
        if !line.starts_with([' ', '\t', '-']) {
            inside = line.trim_end() == "packages:";
            continue;
        }
        if inside && let Some(item) = line.trim().strip_prefix('-') {
            out.push(item.trim().trim_matches(['\'', '"']).to_string());
        }
    }
    out
}

/// The directories a workspace glob names: one `*` (or `**`, read as one
/// level) per segment, never into `node_modules` or a hidden directory.
fn expand(root: &Path, pattern: &str, out: &mut Vec<String>) {
    let mut dirs = vec![String::new()];
    for seg in pattern.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let mut next = Vec::new();
        for d in &dirs {
            if !seg.contains('*') {
                next.push(join(d, seg));
                continue;
            }
            let (pre, post) = seg.split_once('*').unwrap_or((seg, ""));
            let post = post.trim_start_matches('*');
            let Ok(entries) = std::fs::read_dir(root.join(d)) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.')
                    || name == "node_modules"
                    || !e.file_type().is_ok_and(|t| t.is_dir())
                    || !(name.starts_with(pre) && name.ends_with(post))
                {
                    continue;
                }
                next.push(join(d, &name));
            }
        }
        next.sort();
        next.truncate(MAX_PACKAGES);
        dirs = next;
    }
    out.extend(dirs.into_iter().filter(|d| !d.is_empty()));
    out.truncate(MAX_PACKAGES);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A checkout holding `files` (path, contents), under a fresh temp dir.
    fn checkout(label: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rq-imports-{}-{label}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        for (path, body) in files {
            let p = dir.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn targets(root: &Path, file: &str, line: usize, name: &str) -> Vec<(String, String)> {
        resolve(root, file, line, name)
            .into_iter()
            .map(|t| (t.file, t.name))
            .collect()
    }

    fn one(file: &str, name: &str) -> Vec<(String, String)> {
        vec![(file.to_string(), name.to_string())]
    }

    #[test]
    fn relative_imports_resolve_by_typescripts_probing() {
        let root = checkout(
            "relative",
            &[
                (
                    "app/page.tsx",
                    "import { Widget } from './widget'\n\
                     import Card, { Panel as P } from '../lib/card.js'\n\
                     import { Grid } from '../lib/grid'\n\
                     import type { Shape } from './shape.mjs'\n",
                ),
                ("app/widget.tsx", "export function Widget() {}\n"),
                (
                    "lib/card.ts",
                    "export default function Card() {}\nexport const Panel = 1\n",
                ),
                ("lib/grid/index.ts", "export class Grid {}\n"),
                ("app/shape.mts", "export type Shape = {}\n"),
            ],
        );
        let cases = [
            ("Widget", 1, one("app/widget.tsx", "Widget")),
            ("Card", 1, one("lib/card.ts", "Card")),
            ("P", 1, one("lib/card.ts", "Panel")),
            // the original name only on the import that spells it
            ("Panel", 2, one("lib/card.ts", "Panel")),
            ("Panel", 1, vec![]),
            ("Grid", 1, one("lib/grid/index.ts", "Grid")),
            ("Shape", 1, one("app/shape.mts", "Shape")),
            ("Missing", 1, vec![]),
        ];
        for (name, line, want) in cases {
            assert_eq!(targets(&root, "app/page.tsx", line, name), want, "{name}");
        }
    }

    #[test]
    fn a_rename_binds_its_local_name() {
        let root = checkout(
            "rename",
            &[
                (
                    "app.ts",
                    "import { Button } from './ui'
                     import { Card as Base } from './base'
                     import { Card } from './card'
",
                ),
                (
                    "ui.ts",
                    "export { Button as LegacyButton } from './legacy'
                     export * from './current'
",
                ),
                (
                    "legacy.ts",
                    "export function Button() {}
",
                ),
                (
                    "current.ts",
                    "export function Button() {}
",
                ),
                (
                    "base.ts",
                    "export function Card() {}
",
                ),
                (
                    "card.ts",
                    "export function Card() {}
",
                ),
            ],
        );
        assert_eq!(
            targets(&root, "app.ts", 1, "Button"),
            vec![
                ("ui.ts".into(), "Button".into()),
                ("current.ts".into(), "Button".into())
            ],
            "a renamed re-export doesn't shadow the star that exports the name"
        );
        assert_eq!(targets(&root, "app.ts", 4, "Card"), one("card.ts", "Card"));
        assert_eq!(
            targets(&root, "app.ts", 2, "Card"),
            vec![
                ("base.ts".into(), "Card".into()),
                ("card.ts".into(), "Card".into())
            ],
            "on the aliasing import, the original name is the one spelled"
        );
    }

    #[test]
    fn re_exports_are_followed_a_bounded_number_of_hops() {
        let root = checkout(
            "reexport",
            &[
                ("a.ts", "import { Foo, Bar } from './barrel'\n"),
                (
                    "barrel/index.ts",
                    "export { Foo } from './foo'\nexport * from './star'\nexport * from './other'\n",
                ),
                ("barrel/foo.ts", "export * from './deep/foo'\n"),
                ("barrel/deep/foo.ts", "export class Foo {}\n"),
                ("barrel/star.ts", "export const Bar = 1\n"),
                ("barrel/other.ts", "export const Baz = 1\n"),
            ],
        );
        let foo = targets(&root, "a.ts", 1, "Foo");
        assert_eq!(foo.len(), 3, "{foo:?}");
        assert_eq!(foo[2], ("barrel/deep/foo.ts".into(), "Foo".into()));
        let bar = targets(&root, "a.ts", 1, "Bar");
        assert!(
            bar.contains(&("barrel/star.ts".into(), "Bar".into())),
            "{bar:?}"
        );

        // a cycle of re-exports ends
        let root = checkout(
            "cycle",
            &[
                ("a.ts", "import { X } from './b'\n"),
                ("b.ts", "export * from './c'\n"),
                ("c.ts", "export * from './b'\n"),
            ],
        );
        assert_eq!(targets(&root, "a.ts", 1, "X").len(), 2);
    }

    #[test]
    fn commonjs_requires_and_namespaces_bind_names() {
        let root = checkout(
            "commonjs",
            &[
                (
                    "main.js",
                    "const { parse, emit: write } = require('./util');\n\
                     const helpers = require('./helpers');\n\
                     const run = require('./run').start;\n\
                     import * as ns from './ns';\n\
                     helpers.format(x); ns.load();\n",
                ),
                (
                    "util.js",
                    "exports.parse = function () {};\nexports.emit = 1;\n",
                ),
                ("helpers.js", "module.exports = require('./impl');\n"),
                (
                    "impl.js",
                    "function format() {}\nmodule.exports = { format };\n",
                ),
                ("run.js", "exports.start = () => {};\n"),
                ("ns.ts", "export function load() {}\n"),
            ],
        );
        assert_eq!(
            targets(&root, "main.js", 1, "parse"),
            one("util.js", "parse")
        );
        assert_eq!(
            targets(&root, "main.js", 1, "write"),
            one("util.js", "emit")
        );
        assert_eq!(targets(&root, "main.js", 3, "run"), one("run.js", "start"));
        assert_eq!(
            targets(&root, "main.js", 5, "format"),
            vec![
                ("helpers.js".into(), "format".into()),
                ("impl.js".into(), "format".into())
            ],
            "through the namespace, then the CommonJS re-export"
        );
        assert_eq!(targets(&root, "main.js", 5, "load"), one("ns.ts", "load"));
        assert!(
            targets(&root, "main.js", 1, "format").is_empty(),
            "a namespace counts only where the line spells it"
        );
    }

    #[test]
    fn a_namespace_is_spelled_as_a_whole_member_access() {
        for (line, want) in [
            ("ns.load()", true),
            ("x = ns.load", true),
            ("(ns.load)", true),
            ("myns.load()", false),
            ("a.ns.load()", false),
            ("ns.loadAll()", false),
            ("$ns.load()", false),
            ("ns.load$()", false),
        ] {
            assert_eq!(spells(line, "ns", "load"), want, "{line}");
        }
    }

    #[test]
    fn workspace_packages_resolve_to_their_source() {
        let root = checkout(
            "workspace",
            &[
                (
                    "package.json",
                    r#"{"name": "root", "workspaces": ["packages/*"]}"#,
                ),
                (
                    "pnpm-workspace.yaml",
                    "packages:\n  - 'tools/*'\nother: 1\n",
                ),
                (
                    "packages/core/package.json",
                    r#"{"name": "@acme/core", "main": "./build/index.js",
                        "exports": {".": {"types": "./build/index.d.ts", "default": "./build/index.js"},
                                    "./sub/*": "./build/sub/*.js"}}"#,
                ),
                (
                    "packages/core/src/index.ts",
                    "export type { Widget } from './types'\n",
                ),
                ("packages/core/src/types.ts", "export interface Widget {}\n"),
                ("packages/core/src/sub/thing.ts", "export const Thing = 1\n"),
                ("packages/web/package.json", r#"{"name": "web"}"#),
                (
                    "packages/web/document.js",
                    "module.exports = require('./dist/pages/doc')\n",
                ),
                (
                    "packages/web/src/pages/doc.tsx",
                    "export function Main() {}\n",
                ),
                (
                    "tools/cli/package.json",
                    r#"{"name": "cli", "main": "lib/main.js"}"#,
                ),
                ("tools/cli/src/main.ts", "export function run() {}\n"),
                (
                    "examples/app/page.tsx",
                    "import { Widget } from '@acme/core'\n\
                     import { Thing } from '@acme/core/sub/thing'\n\
                     import { Main } from 'web/document'\n\
                     import { run } from 'cli'\n\
                     import { useState } from 'react'\n",
                ),
            ],
        );
        let page = "examples/app/page.tsx";
        assert_eq!(
            targets(&root, page, 1, "Widget"),
            vec![
                ("packages/core/src/index.ts".into(), "Widget".into()),
                ("packages/core/src/types.ts".into(), "Widget".into())
            ]
        );
        assert_eq!(
            targets(&root, page, 1, "Thing"),
            one("packages/core/src/sub/thing.ts", "Thing")
        );
        assert_eq!(
            targets(&root, page, 1, "Main"),
            vec![
                ("packages/web/document.js".into(), "Main".into()),
                ("packages/web/src/pages/doc.tsx".into(), "Main".into())
            ]
        );
        assert_eq!(
            targets(&root, page, 1, "run"),
            one("tools/cli/src/main.ts", "run")
        );
        assert!(
            targets(&root, page, 1, "useState").is_empty(),
            "a third-party package isn't resolved"
        );
    }

    #[test]
    fn a_large_module_is_read_for_its_module_statements_only() {
        let body = "function filler() {\n  return 1;\n}\n".repeat(1_000);
        let late = format!(
            "import {{ Early }} from './early'\nconst {{ req }} = require('./req')\n\
             {body}\nexport {{\n  Late,\n}} from './late'\nexport {{ local }}\n\
             import {{ After }} from './after'\n"
        );
        assert!(late.len() > PARSE_WHOLE);
        let kept = module_statements(&late);
        assert!(!kept.contains("filler"), "code is skipped");
        let names: Vec<String> = bindings("big.ts", &kept)
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["Early", "req", "Late", "After"]);
    }

    #[test]
    fn a_declaration_brings_its_implementation() {
        let root = checkout(
            "declaration",
            &[
                ("package.json", r#"{"workspaces": ["packages/*"]}"#),
                (
                    "packages/fmt/package.json",
                    r#"{"name": "fmt", "main": "lib/index.js", "types": "types/index.d.ts"}"#,
                ),
                ("packages/fmt/lib/index.js", "exports.format = 1\n"),
                (
                    "packages/fmt/types/index.d.ts",
                    "export declare const format: number\n",
                ),
                ("lib/loader.js", "exports.parse = 1\n"),
                ("lib/loader.d.ts", "export declare const parse: number\n"),
                (
                    "main.js",
                    "const { format } = require('fmt')\n\
                     const { parse } = require('./lib/loader')\n",
                ),
            ],
        );
        assert_eq!(
            targets(&root, "main.js", 1, "format"),
            vec![
                ("packages/fmt/types/index.d.ts".into(), "format".into()),
                ("packages/fmt/lib/index.js".into(), "format".into())
            ]
        );
        assert_eq!(
            targets(&root, "main.js", 2, "parse"),
            vec![
                ("lib/loader.d.ts".into(), "parse".into()),
                ("lib/loader.js".into(), "parse".into())
            ]
        );
    }

    #[test]
    fn a_specifier_never_leaves_the_checkout() {
        let root = checkout("escape", &[("a.ts", "import { X } from '../../x'\n")]);
        assert!(targets(&root, "a.ts", 1, "X").is_empty());
        assert_eq!(normalize("a/../../b"), None);
        assert_eq!(normalize("a/./b/../c").as_deref(), Some("a/c"));
    }

    #[test]
    fn pnpm_workspace_lists_its_packages() {
        assert_eq!(
            pnpm("packages:\n  - 'apps/*'\n  - \"crates/*/js\"\nallowBuilds:\n  - x\n"),
            ["apps/*", "crates/*/js"]
        );
    }
}
