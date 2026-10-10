//! What of a large JS/TS source can bind a module (D63).

/// Larger sources parse only their module header and later module statements
/// (see [`module_statements`]): a whole 200 KB module costs tens of
/// milliseconds per query.
pub(super) const PARSE_WHOLE: usize = 16 * 1024;

/// What of a large `source` can bind a module: a header of at most
/// [`PARSE_WHOLE`] bytes, up to the first line of code at column 0 (a function,
/// class, type or exported declaration; a `const` may be a `require` and
/// doesn't end it), and on to the end of a statement the cap cut in two, then
/// any later line that starts a top-level `import`, `export … from`,
/// `module.exports = require(…)` or one-line `require` declaration. Each
/// statement is no bigger than the header. Bundles have no cut, or a cut a
/// megabyte in; the cap keeps them from being parsed whole. A small source is
/// parsed whole. Skipped lines are left blank, so a statement keeps its line.
pub(super) fn module_statements(source: &str) -> std::borrow::Cow<'_, str> {
    if source.len() <= PARSE_WHOLE {
        return source.into();
    }
    let mut lines: Vec<&str> = source.lines().collect();
    let mut bytes = 0;
    let cut = lines
        .iter()
        .position(|l| {
            bytes += l.len() + 1;
            bytes > PARSE_WHOLE || is_code(l)
        })
        .unwrap_or(lines.len());
    let ends = Ends::new(&lines);
    let mut i = cut;
    let capped = (0..cut)
        .rev()
        .find(|&j| opens_statement(lines[j]))
        .filter(|_| bytes > PARSE_WHOLE);
    if let Some(start) = capped {
        match ends.of(start) {
            Some(end) if end >= cut => i = end + 1,
            Some(_) => {}
            // a fragment would parse as garbage swallowing what follows it
            None => lines[start..cut].fill(""),
        }
    }
    // inside a template literal, an `import` at column 0 is text
    let quoted = in_template(&lines);
    while i < lines.len() {
        // the statement runs to its specifier, or to the end of a local list
        match ends
            .of(i)
            .filter(|_| !quoted[i] && opens_statement(lines[i]))
        {
            Some(end) => i = end + 1,
            None => {
                lines[i] = "";
                i += 1;
            }
        }
    }
    lines.join("\n").into()
}

/// Which lines start inside a template literal, by backtick parity counted
/// from the top. A count that ends unbalanced misread a backtick (a regex, JSX
/// text) somewhere, so it marks no line rather than every line after that one.
fn in_template(lines: &[&str]) -> Vec<bool> {
    let mut quoted = false;
    let mut starts: Vec<bool> = lines
        .iter()
        .map(|line| {
            let start = quoted;
            quoted = template_after(line, quoted);
            start
        })
        .collect();
    if quoted {
        starts.fill(false);
    }
    starts
}

/// Whether a template literal is open after `line`, given whether one was
/// before it. Outside one, a string's and a comment's backticks are text.
fn template_after(line: &str, mut quoted: bool) -> bool {
    if !quoted && ["/*", "*"].iter().any(|c| line.trim_start().starts_with(c)) {
        return false;
    }
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '`' => quoted = !quoted,
            _ if quoted => {}
            '\'' | '"' => {
                while let Some(d) = chars.next() {
                    match d {
                        '\\' => {
                            chars.next();
                        }
                        _ if d == c => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.as_str().starts_with('/') => break,
            _ => {}
        }
    }
    quoted
}

/// A top-level line starting a statement that can bind a module.
fn opens_statement(line: &str) -> bool {
    line.starts_with("import ")
        || (line.starts_with("export ") && !is_code(line))
        || line.starts_with("module.exports = require(")
        || (["const ", "let ", "var "]
            .iter()
            .any(|k| line.starts_with(k))
            && line.contains("require("))
}

/// Where each line's statement ends, found for every line in one pass.
struct Ends {
    /// The first line at or after each that holds a specifier, ends with `;`
    /// or closes a list.
    next: Vec<Option<usize>>,
    /// Bytes before each line.
    offset: Vec<usize>,
}

impl Ends {
    fn new(lines: &[&str]) -> Self {
        let mut next = vec![None; lines.len()];
        let mut after = None;
        for (j, l) in lines.iter().enumerate().rev() {
            // a local list closes at column 0, as a formatter writes it
            if names_module(l)
                || l.contains("require(")
                || l.trim_end().ends_with(';')
                || l.starts_with('}')
            {
                after = Some(j);
            }
            next[j] = after;
        }
        let offset = std::iter::once(0)
            .chain(lines.iter().scan(0, |sum, l| {
                *sum += l.len() + 1;
                Some(*sum)
            }))
            .collect();
        Ends { next, offset }
    }

    /// The last line of the statement opening at `start`, no bigger than the
    /// header.
    fn of(&self, start: usize) -> Option<usize> {
        self.next[start].filter(|&e| self.offset[e + 1] - self.offset[start] <= PARSE_WHOLE)
    }
}

/// `line` holds a `from '…'` specifier: not a name like `fromThing`.
fn names_module(line: &str) -> bool {
    line.match_indices("from")
        .any(|(i, _)| line[i + 4..].trim_start().starts_with(['\'', '"']))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::typescript::imports::{Binding, bindings};

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
    fn a_late_statement_runs_to_its_specifier() {
        let body = "function filler() {\n  return 1;\n}\n".repeat(1_000);
        // names holding "from" don't end the statement
        let late = format!(
            "{body}import {{\n  fromThing,\n  transformFrom,\n  from,\n  Target,\n}} from './fr'\n"
        );
        assert!(late.len() > PARSE_WHOLE);
        let names: Vec<String> = bindings("big.ts", &module_statements(&late))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["fromThing", "transformFrom", "from", "Target"]);

        // a long type alias isn't kept as a fragment that swallows the next
        let fields = "  a: number\n".repeat(70);
        let late = format!(
            "{body}export type Shape = {{\n{fields}}}\n\nexport {{\n  After,\n}} from './after'\n"
        );
        let names: Vec<String> = bindings("big.ts", &module_statements(&late))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["After"]);
    }

    #[test]
    fn the_header_runs_to_the_end_of_a_statement_it_started() {
        let singles: String = (0..480)
            .map(|i| format!("import {{ One{i:03} }} from './one'\n"))
            .collect();
        let list: String = (0..200).map(|i| format!("  Two{i:03},\n")).collect();
        let source = format!("{singles}import {{\n{list}}} from './two'\n\nTwo199()\n");
        assert!(singles.len() < PARSE_WHOLE && singles.len() + list.len() > PARSE_WHOLE);
        let names: Vec<String> = bindings("big.ts", &module_statements(&source))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        assert_eq!(names.len(), 680);
        assert_eq!(names.last().map(String::as_str), Some("Two199"));
    }

    #[test]
    fn a_statement_in_a_template_literal_is_text() {
        let body = "function filler() {\n  return 1;\n}\n".repeat(1_000);
        let source = format!(
            "import {{ Real }} from './real'\n{body}export function gen() {{\n  return `\n\
             import {{ Fake }} from './fake'\n`\n}}\nimport {{ Late }} from './late'\n"
        );
        assert_eq!(named_locals(&source), ["Real", "Late"]);
    }

    fn named_locals(source: &str) -> Vec<String> {
        bindings("big.ts", &module_statements(source))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_stray_backtick_never_hides_a_later_import() {
        let body = "function filler() {\n  return 1;\n}\n".repeat(500);
        let template = "export function gen() {\n  return `\nimport { Fake } from './fake'\n`\n}\n";
        // a string's or comment's backtick is skipped; one the count can't
        // place (a regex, JSX text) leaves it unbalanced, and parity unused
        let cases: [(&str, &[&str]); 4] = [
            ("const FENCE = '```'", &["Early", "Late"]),
            ("const q = 1 // don't use ` here", &["Early", "Late"]),
            ("const RE = /`/g", &["Early", "Fake", "Late"]),
            (
                "export const C = () => <p>press ` to open</p>",
                &["Early", "Fake", "Late"],
            ),
        ];
        for (stray, want) in cases {
            let source = format!(
                "import {{ Early }} from './early'\n{body}{stray}\n{body}{template}import {{ Late }} from './late'\n"
            );
            assert_eq!(named_locals(&source), want, "after {stray}");
        }
    }

    #[test]
    fn a_header_cut_inside_a_template_keeps_its_parity() {
        let schema = "  type Widget { id: ID }\n".repeat(1_000);
        let body = "function filler() {\n  return 1;\n}\n".repeat(100);
        let source = format!(
            "import {{ Early }} from './early'\nconst typeDefs = gql`\n{schema}`\n{body}import {{ Late }} from './late'\n"
        );
        assert_eq!(named_locals(&source), ["Early", "Late"]);
    }

    #[test]
    fn a_bundle_is_never_parsed_whole() {
        // one minified line, and a bundle whose code starts with `var`
        let minified = format!("!function(e){{{}}}\n", "var t=e*2;".repeat(4_000));
        let unminified = format!(
            "\"use strict\";\n{}var dep = require('./dep');\nfunction f() {{}}\n",
            "var x = Object.create;\n".repeat(2_000)
        );
        for source in [&minified, &unminified] {
            assert!(source.len() > PARSE_WHOLE);
            let kept = module_statements(source);
            assert_eq!(
                kept.split('\n').count(),
                source.lines().count(),
                "lines kept"
            );
            let text: usize = kept.lines().map(str::len).sum();
            assert!(text <= PARSE_WHOLE, "{text}");
        }
        let names: Vec<String> = bindings("bundle.js", &module_statements(&unminified))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Namespace { local, .. } => Some(local),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["dep"], "a require past the header still binds");
    }
}
