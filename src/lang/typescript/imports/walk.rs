//! What of a large JS/TS source can bind a module: its top-level `import`,
//! `export … from`, `export { … }`, `export default name`, `require`
//! declaration and `module.exports = require(…)` statements, found by one
//! lexical pass (D63).

use std::borrow::Cow;
use std::ops::Range;

/// Larger sources parse only their module statements: a whole 200 KB module
/// costs tens of milliseconds per query. It also bounds each statement, so a
/// minified bundle's one line is never parsed whole.
pub(super) const PARSE_WHOLE: usize = 16 * 1024;

/// The module statements of a large `source`, everything else blanked to its
/// newlines so a statement keeps its line. A small source is returned whole.
///
/// One lexical pass finds the statements at the top level, past strings,
/// comments, templates, regexes and, outside TypeScript's own `.ts`, JSX
/// text. A pass that ends out of balance misread something (a regex read as
/// division); the source is then read again trusting each line that starts a
/// module statement at column 0, as the line walk before it did.
pub(super) fn module_statements<'a>(file: &str, source: &'a str) -> Cow<'a, str> {
    if source.len() <= PARSE_WHOLE {
        return source.into();
    }
    let jsx = super::jsx(file);
    let scan = |resync| Scan::new(source.as_bytes(), jsx, resync).run();
    let spans = scan(false).or_else(|| scan(true)).unwrap_or_default();
    let mut out = String::new();
    let mut at = 0;
    let newlines = |text: &str| "\n".repeat(text.bytes().filter(|&b| b == b'\n').count());
    for span in spans {
        let (Some(gap), Some(statement)) = (source.get(at..span.start), source.get(span.clone()))
        else {
            continue;
        };
        out.push_str(&newlines(gap));
        out.push_str(statement);
        if !statement.ends_with(';') {
            out.push(';');
        }
        at = span.end;
    }
    out.push_str(&newlines(source.get(at..).unwrap_or_default()));
    out.into()
}

/// Where a byte sits, lexically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Code,
    Quote(u8),
    Template,
    LineComment,
    BlockComment,
    Regex {
        class: bool,
    },
    /// Inside a JSX tag's `<…>`.
    JsxTag,
    /// Between an element's tags.
    JsxText,
}

/// A bracket open in code, and what its close returns to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Paren,
    Square,
    Brace,
    /// A template's `${`: its `}` returns to the template.
    Subst,
    /// A JSX element, from its `<` to its closing tag.
    Element,
    /// A `{` in JSX text, returning to the text.
    JsxChild,
    /// A `{` in a JSX tag, returning to the tag.
    JsxAttr,
}

/// The token before the current one: whether a `/` starts a regex, and
/// whether a newline ends a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prev {
    Start,
    Punct(u8),
    /// A word; `operator` if an expression follows it (`return`, `typeof`).
    Word {
        operator: bool,
    },
    /// A literal, or a closed `)` or `]`.
    Value,
}

impl Prev {
    fn regex_follows(self) -> bool {
        match self {
            Prev::Start | Prev::Punct(_) => true,
            Prev::Word { operator } => operator,
            Prev::Value => false,
        }
    }

    fn ends_expression(self) -> bool {
        matches!(
            self,
            Prev::Value | Prev::Word { operator: false } | Prev::Punct(b'}')
        )
    }
}

/// What a statement is, which decides where it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `import …`: at its specifier, or the `)` closing `= require(…)`.
    Import,
    /// `export { … }`: at its `}`, unless a `from` follows.
    ExportList,
    /// `export … from '…'`, `export * …`: at its specifier.
    ExportFrom,
    /// `const`/`let`/`var`, `module.exports =`: where a newline ends it; kept
    /// only if its value calls `require(` at the top level.
    Declaration { require: bool },
}

#[derive(Debug, Clone, Copy)]
struct Statement {
    start: usize,
    kind: Kind,
}

/// One pass over a source, collecting its top-level module statements.
struct Scan<'a> {
    src: &'a [u8],
    /// A `<` where an expression starts opens a JSX element.
    jsx: bool,
    lex: Lex,
    stack: Vec<Bracket>,
    prev: Prev,
    /// Where the previous token ended.
    prev_end: usize,
    /// A newline since the previous token.
    newline: bool,
    /// Each line a statement's opening word starts at column 0 is code.
    resync: bool,
    /// A close with nothing, or the wrong thing, open.
    unbalanced: bool,
    open: Option<Statement>,
    spans: Vec<Range<usize>>,
}

const OPERATORS: [&str; 14] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

fn ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

impl<'a> Scan<'a> {
    fn new(src: &'a [u8], jsx: bool, resync: bool) -> Self {
        Scan {
            src,
            jsx,
            lex: Lex::Code,
            stack: Vec::new(),
            prev: Prev::Start,
            prev_end: 0,
            newline: false,
            resync,
            unbalanced: false,
            open: None,
            spans: Vec::new(),
        }
    }

    /// The statements' spans, or `None` if the pass ended out of balance.
    fn run(mut self) -> Option<Vec<Range<usize>>> {
        let src = self.src;
        let mut i = 0;
        if src.starts_with(b"#!") {
            i = src.iter().position(|&b| b == b'\n').unwrap_or(src.len());
        }
        while i < src.len() {
            if self.resync && (i == 0 || src[i - 1] == b'\n') && self.opens_line(i) {
                self.lex = Lex::Code;
                self.stack.clear();
                self.open = None;
                self.prev = Prev::Start;
            }
            let b = src[i];
            i = match self.lex {
                Lex::Code => self.code(i),
                Lex::Quote(q) => match b {
                    b'\\' => i + 2,
                    // a string can't span lines: a misread one ends here
                    b'\n' => {
                        self.lex = Lex::Code;
                        i
                    }
                    _ if b == q => self.close_literal(i, true),
                    _ => i + 1,
                },
                Lex::Template => match b {
                    b'\\' => i + 2,
                    b'`' => self.close_literal(i, false),
                    b'$' if src.get(i + 1) == Some(&b'{') => {
                        self.stack.push(Bracket::Subst);
                        self.lex = Lex::Code;
                        self.prev = Prev::Punct(b'{');
                        i + 2
                    }
                    _ => i + 1,
                },
                Lex::LineComment => {
                    if b == b'\n' {
                        self.lex = Lex::Code;
                        self.newline = true;
                    }
                    i + 1
                }
                Lex::BlockComment => {
                    if b == b'\n' {
                        self.newline = true;
                    }
                    if src[i..].starts_with(b"*/") {
                        self.lex = Lex::Code;
                        i + 2
                    } else {
                        i + 1
                    }
                }
                Lex::Regex { class } => match b {
                    b'\\' => i + 2,
                    b'\n' => {
                        self.lex = Lex::Code;
                        i
                    }
                    b'[' => {
                        self.lex = Lex::Regex { class: true };
                        i + 1
                    }
                    b']' => {
                        self.lex = Lex::Regex { class: false };
                        i + 1
                    }
                    b'/' if !class => self.close_literal(i, false),
                    _ => i + 1,
                },
                Lex::JsxTag => match b {
                    b'"' | b'\'' => {
                        // an attribute string: no escapes, and it may span lines
                        i + 1
                            + src[i + 1..]
                                .iter()
                                .position(|&c| c == b)
                                .map_or(src.len(), |p| p + 1)
                    }
                    b'{' => {
                        self.stack.push(Bracket::JsxAttr);
                        self.lex = Lex::Code;
                        self.token(Prev::Punct(b), i + 1);
                        i + 1
                    }
                    b'/' if src.get(i + 1) == Some(&b'>') => self.close_element(i + 2),
                    b'>' => {
                        self.lex = Lex::JsxText;
                        i + 1
                    }
                    _ => i + 1,
                },
                Lex::JsxText => match b {
                    b'{' => {
                        self.stack.push(Bracket::JsxChild);
                        self.lex = Lex::Code;
                        self.token(Prev::Punct(b), i + 1);
                        i + 1
                    }
                    b'<' if src.get(i + 1) == Some(&b'/') => {
                        let end = src[i..]
                            .iter()
                            .position(|&c| c == b'>')
                            .map_or(src.len(), |p| i + p + 1);
                        self.close_element(end)
                    }
                    b'<' => {
                        self.stack.push(Bracket::Element);
                        self.lex = Lex::JsxTag;
                        i + 1
                    }
                    _ => i + 1,
                },
            };
        }
        if let Some(Statement {
            kind: Kind::Declaration { .. },
            ..
        }) = self.open
        {
            self.end(self.prev_end);
        }
        let clean = !self.unbalanced
            && self.stack.is_empty()
            && matches!(self.lex, Lex::Code | Lex::LineComment);
        (clean || self.resync).then_some(self.spans)
    }

    /// A line at `i` that starts a module statement at column 0.
    fn opens_line(&self, i: usize) -> bool {
        [
            &b"import"[..],
            b"export",
            b"const",
            b"let",
            b"var",
            b"module",
        ]
        .contains(&self.word_at(i))
    }

    /// A `<` before `i`, where an expression starts, opens an element: `<>`
    /// or a tag name, but not TSX's `<T,>` or `<T extends U>` type parameters.
    fn opens_element(&self, i: usize) -> bool {
        let name = self.word_at(i);
        if name.is_empty() {
            return self.src.get(i) == Some(&b'>');
        }
        let next = self.skip(i + name.len());
        !name[0].is_ascii_digit()
            && self.src.get(next) != Some(&b',')
            && self.word_at(next) != b"extends"
    }

    /// An element's close (its `/>` or closing tag) ending at `end`: back to
    /// its parent's text, or to code.
    fn close_element(&mut self, end: usize) -> usize {
        if self.stack.pop() != Some(Bracket::Element) {
            self.unbalanced = true;
        }
        if self.stack.last() == Some(&Bracket::Element) {
            self.lex = Lex::JsxText;
        } else {
            self.lex = Lex::Code;
            self.token(Prev::Value, end);
        }
        end
    }

    fn top_level(&self) -> bool {
        self.stack.is_empty()
    }

    /// A string, template or regex closing at `i`. A string at the top level
    /// is an import's specifier.
    fn close_literal(&mut self, i: usize, string: bool) -> usize {
        self.lex = Lex::Code;
        self.token(Prev::Value, i + 1);
        if string
            && self.top_level()
            && let Some(Statement {
                kind: Kind::Import | Kind::ExportFrom,
                ..
            }) = self.open
        {
            self.end(i + 1);
        }
        i + 1
    }

    /// The token just read: of kind `prev`, ending at `end`.
    fn token(&mut self, prev: Prev, end: usize) {
        self.prev = prev;
        self.prev_end = end;
        self.newline = false;
    }

    /// Before a token starting with `b`: a newline after a complete
    /// expression ends a declaration, unless `b` continues it.
    fn before(&mut self, b: u8) {
        if self.newline
            && self.top_level()
            && self.prev.ends_expression()
            && !b".,?:=+-*/%&|^<>([`".contains(&b)
            && let Some(Statement {
                kind: Kind::Declaration { .. },
                ..
            }) = self.open
        {
            self.end(self.prev_end);
        }
    }

    /// The open statement ends at `end`; kept if small and binding.
    fn end(&mut self, end: usize) {
        let Some(s) = self.open.take() else {
            return;
        };
        if !matches!(s.kind, Kind::Declaration { require: false }) {
            self.keep(s.start..end);
        }
    }

    /// A statement's span, with a `;` that follows it on a later line, as a
    /// parse reads it, if no bigger than [`PARSE_WHOLE`].
    fn keep(&mut self, span: Range<usize>) {
        let semi = self.skip(span.end);
        let end = if self.src.get(semi) == Some(&b';') {
            semi + 1
        } else {
            span.end
        };
        if end > span.start && end - span.start <= PARSE_WHOLE {
            self.spans.push(span.start..end);
        }
    }

    /// One token of code at `i`; returns where the next starts.
    fn code(&mut self, i: usize) -> usize {
        let src = self.src;
        let b = src[i];
        if b.is_ascii_whitespace() {
            if b == b'\n' {
                self.newline = true;
            }
            return i + 1;
        }
        match b {
            b'/' if src.get(i + 1) == Some(&b'/') => {
                self.lex = Lex::LineComment;
                return i + 2;
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                self.lex = Lex::BlockComment;
                return i + 2;
            }
            _ => {}
        }
        self.before(b);
        match b {
            b'/' if self.prev.regex_follows() => {
                self.lex = Lex::Regex { class: false };
                i + 1
            }
            b'\'' | b'"' => {
                self.lex = Lex::Quote(b);
                i + 1
            }
            b'`' => {
                self.lex = Lex::Template;
                i + 1
            }
            b'<' if self.jsx && self.prev.regex_follows() && self.opens_element(i + 1) => {
                self.stack.push(Bracket::Element);
                self.lex = Lex::JsxTag;
                i + 1
            }
            b'(' | b'[' | b'{' => {
                self.stack.push(match b {
                    b'(' => Bracket::Paren,
                    b'[' => Bracket::Square,
                    _ => Bracket::Brace,
                });
                self.token(Prev::Punct(b), i + 1);
                i + 1
            }
            b')' | b']' | b'}' => self.close(i),
            b';' => {
                self.token(Prev::Punct(b), i + 1);
                if self.top_level() {
                    self.end(i + 1);
                }
                i + 1
            }
            _ if ident(b) && !b.is_ascii_digit() => self.word(i),
            _ if b.is_ascii_digit() => {
                let end = i + src[i..]
                    .iter()
                    .position(|&c| !(ident(c) || c == b'.'))
                    .unwrap_or(src.len() - i);
                self.token(Prev::Value, end);
                end
            }
            _ => {
                self.token(Prev::Punct(b), i + 1);
                i + 1
            }
        }
    }

    /// A `)`, `]` or `}` at `i`.
    fn close(&mut self, i: usize) -> usize {
        let b = self.src[i];
        let want = match b {
            b')' => Bracket::Paren,
            b']' => Bracket::Square,
            _ => Bracket::Brace,
        };
        match self.stack.pop() {
            Some(Bracket::Subst) if b == b'}' => {
                self.lex = Lex::Template;
                return i + 1;
            }
            Some(Bracket::JsxChild) if b == b'}' => {
                self.lex = Lex::JsxText;
                return i + 1;
            }
            Some(Bracket::JsxAttr) if b == b'}' => {
                self.lex = Lex::JsxTag;
                return i + 1;
            }
            Some(open) if open == want => {}
            other => {
                // put back what this close doesn't match
                self.stack.extend(other);
                self.unbalanced = true;
            }
        }
        self.token(
            if b == b'}' {
                Prev::Punct(b)
            } else {
                Prev::Value
            },
            i + 1,
        );
        if self.top_level()
            && let Some(s) = self.open
        {
            match (s.kind, b) {
                (Kind::Import, b')') => self.end(i + 1),
                (Kind::ExportList, b'}') => {
                    if self.word_at(self.skip(i + 1)) == b"from" {
                        self.open = Some(Statement {
                            kind: Kind::ExportFrom,
                            ..s
                        });
                    } else {
                        self.end(i + 1);
                    }
                }
                _ => {}
            }
        }
        i + 1
    }

    /// The word starting at `i`, if one does.
    fn word_at(&self, i: usize) -> &'a [u8] {
        let src = self.src;
        let rest = src.get(i..).unwrap_or_default();
        &rest[..rest.iter().position(|&c| !ident(c)).unwrap_or(rest.len())]
    }

    /// The next byte at or after `i` that isn't whitespace or a comment.
    fn skip(&self, mut i: usize) -> usize {
        let src = self.src;
        while i < src.len() {
            if src[i].is_ascii_whitespace() {
                i += 1;
            } else if src[i..].starts_with(b"//") {
                i += src[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .unwrap_or(src.len() - i);
            } else if src[i..].starts_with(b"/*") {
                i += src[i + 2..]
                    .windows(2)
                    .position(|w| w == b"*/")
                    .map_or(src.len() - i, |p| p + 4);
            } else {
                break;
            }
        }
        i
    }

    /// A word at `i`: maybe a statement opening at the top level.
    fn word(&mut self, i: usize) -> usize {
        let word = self.word_at(i);
        let end = i + word.len();
        let member = self.prev == Prev::Punct(b'.');
        if self.top_level() && !member {
            self.opens(word, i, end);
        }
        let operator = OPERATORS.iter().any(|o| o.as_bytes() == word);
        // a binding's `require(` is the declaration's own value, not one nested in it
        if word == b"require"
            && self.top_level()
            && self.src.get(self.skip(end)) == Some(&b'(')
            && let Some(Statement {
                kind: Kind::Declaration { require },
                ..
            }) = &mut self.open
        {
            *require = true;
        }
        self.token(Prev::Word { operator }, end);
        end
    }

    /// `word` at `start..end`, at the top level: open the statement it starts.
    fn opens(&mut self, word: &[u8], start: usize, end: usize) {
        let next = self.skip(end);
        let after = self.word_at(next);
        let kind = match word {
            b"import" if !matches!(self.src.get(next), Some(b'(' | b'.')) => Kind::Import,
            b"export" => match self.src.get(next) {
                Some(b'{') => Kind::ExportList,
                Some(b'*') => Kind::ExportFrom,
                _ if after == b"type" => match self.src.get(self.skip(next + 4)) {
                    Some(b'{') => Kind::ExportList,
                    Some(b'*') => Kind::ExportFrom,
                    _ => return,
                },
                _ if after == b"default" => {
                    self.export_default(start, next + after.len());
                    return;
                }
                _ => return,
            },
            b"const" | b"var" => Kind::Declaration { require: false },
            b"let" if self.src.get(end).is_some_and(u8::is_ascii_whitespace) => {
                Kind::Declaration { require: false }
            }
            b"module" if self.src[end..].starts_with(b".exports") => {
                Kind::Declaration { require: false }
            }
            _ => return,
        };
        // a statement still open ends where the one after it starts
        self.end(self.prev_end);
        self.open = Some(Statement { start, kind });
    }

    /// `export default name` alone on its line: kept whole.
    fn export_default(&mut self, start: usize, after: usize) {
        const DECLARES: [&[u8]; 6] = [
            b"function",
            b"class",
            b"async",
            b"abstract",
            b"interface",
            b"enum",
        ];
        let at = self.skip_spaces(after);
        let name = self.word_at(at);
        if name.is_empty() || name[0].is_ascii_digit() || DECLARES.contains(&name) {
            return;
        }
        let end = at + name.len();
        let rest = self.skip_spaces(end);
        if matches!(self.src.get(rest), None | Some(b'\n' | b'\r' | b';'))
            || self.src[rest..].starts_with(b"//")
        {
            self.end(self.prev_end);
            self.keep(start..end);
        }
    }

    fn skip_spaces(&self, mut i: usize) -> usize {
        while matches!(self.src.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        i
    }
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
        let kept = module_statements("big.ts", &late);
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

    fn named_locals(source: &str) -> Vec<String> {
        bindings("big.ts", &module_statements("big.ts", source))
            .into_iter()
            .filter_map(|b| match b {
                Binding::Named { local, .. } => Some(local),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_template_across_the_first_16_kb_hides_nothing() {
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
            let kept = module_statements("bundle.js", source);
            assert_eq!(kept.lines().count(), source.lines().count(), "lines kept");
            let text: usize = kept.lines().map(str::len).sum();
            assert!(text <= PARSE_WHOLE, "{text}");
        }
        let names: Vec<String> =
            bindings("bundle.js", &module_statements("bundle.js", &unminified))
                .into_iter()
                .filter_map(|b| match b {
                    Binding::Namespace { local, .. } => Some(local),
                    _ => None,
                })
                .collect();
        assert_eq!(names, ["dep"], "a require past the header still binds");
    }

    #[test]
    fn a_statement_over_16_kb_is_skipped() {
        // the bound that keeps a bundle's one line from being parsed whole
        let names: String = (0..2_000).map(|i| format!("  Name{i:04},\n")).collect();
        let body = "function filler() {\n  return 1;\n}\n".repeat(500);
        let source =
            format!("{body}import {{\n{names}}} from './many'\nimport {{ Late }} from './late'\n");
        assert_eq!(named_locals(&source), ["Late"]);
    }
}
