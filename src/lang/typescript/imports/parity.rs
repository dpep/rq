//! The large-source walk ([`module_statements`]) against a whole parse of the
//! same source: a truth the walk didn't produce. A row the walk reads wrong is
//! listed by name, so a fix turns it green visibly and a regression fails.

use super::walk::PARSE_WHOLE;
use super::*;

/// What the walk binds, and what a whole parse binds.
fn walk_and_truth(source: &str) -> (Vec<Binding>, Vec<Binding>) {
    (
        bindings("big.tsx", &module_statements(source)),
        bindings("big.tsx", source),
    )
}

/// Top-level code no module statement is in: `n` small functions.
fn filler(n: usize) -> String {
    (0..n)
        .map(|i| format!("function filler{i}() {{\n  return {i};\n}}\n"))
        .collect()
}

/// Statements that bind a module, each naming its own bindings.
const STATEMENTS: [(&str, &str); 14] = [
    ("named", "import { Alpha, Beta as Gamma } from './named'"),
    (
        "list",
        "import {\n  fromThing,\n  transformFrom,\n  Target,\n} from './list'",
    ),
    ("default", "import Widget from './widget'"),
    ("namespace", "import * as ns from './ns'"),
    ("export-from", "export { Delta } from './delta'"),
    (
        "export-list",
        "export {\n  Epsilon,\n  Zeta as Eta,\n} from './greek'",
    ),
    ("export-star-as", "export * as tools from './tools'"),
    ("export-type", "export type { Shape } from './shape'"),
    ("export-default", "const Panel = 1\nexport default Panel"),
    ("require-one", "const one = require('./one')"),
    (
        "require-list",
        "const {\n  alpha,\n  beta: gamma,\n} = require('./req')",
    ),
    ("module-exports", "module.exports = require('./mod')"),
    (
        "list-comment-semi",
        "import {\n  Legacy, // legacy;\n  Kept,\n} from './legacy'",
    ),
    (
        "list-comment-from",
        "import {\n  Moved, // moved from 'old'\n  Kept2,\n} from './moved'",
    ),
];

/// Text before (and, for a pair, after) a statement that a line reading can
/// mistake for code or for text.
const HAZARDS: [(&str, &str, &str); 17] = [
    ("none", "", ""),
    ("string-backtick", "const FENCE = '```'", ""),
    ("comment-backtick", "const q = 1 // don't use ` here", ""),
    ("regex-backtick", "const RE = /`/g", ""),
    ("regex-pair", "const RE = /[`]/g", "const RE2 = /[`]/g"),
    (
        "block-comment-pair",
        "let c = 1 /* ` */",
        "let d = 2 /* ` */",
    ),
    (
        "jsx-text",
        "export const C = () => <p>press ` to open</p>",
        "",
    ),
    (
        "jsx-pair",
        "export const C = () => <p>press ` to open</p>",
        "export const D = () => <p>or ` to close</p>",
    ),
    (
        "template-import",
        "export function gen() {\n  return `\nimport { Fake } from './fake'\n`\n}",
        "",
    ),
    (
        "template-slashes",
        "const doc = `\n// not a comment\nhttp://example.com\n`",
        "",
    ),
    (
        "nested-template",
        "const s = `a ${\n  `\nimport { Inner } from './inner'\n`\n} b`",
        "",
    ),
    (
        "block-comment-import",
        "/*\nimport { Commented } from './commented'\n*/",
        "",
    ),
    (
        "continued-string",
        "const s = 'one \\\nimport { Quoted } from \"./quoted\"'",
        "",
    ),
    (
        "type-alias",
        "export type Big = {\n  a: number\n  b: string\n}",
        "",
    ),
    ("division", "const half = total / 2; const r = '`'", ""),
    (
        "jsx-apostrophe",
        "export const E = () => <p>Don't {x}</p>",
        "",
    ),
    (
        "declare-module",
        "declare module 'ambient' {\n  import { Ambient } from './ambient'\n}",
        "",
    ),
];

/// Where a statement sits against the 16 KB header cut.
#[derive(Debug, Clone, Copy)]
enum Position {
    /// In the header, before any code.
    Header,
    /// Started inside the header, ended past its cap.
    Straddle,
    /// Past the cut, after code.
    Late,
}

fn source(position: Position, (before, after): (&str, &str), statement: &str) -> String {
    let around = format!("{before}\n{statement}\n{after}\n");
    let late = "import { Sentinel } from './sentinel'\n";
    match position {
        Position::Header => format!(
            "import {{ Early }} from './early'\n{around}{}{late}",
            filler(450)
        ),
        Position::Straddle => {
            let fill: String = (0..)
                .map(|i| format!("import {{ Fill{i:03} }} from './fill'\n"))
                .scan(0, |len, l| {
                    *len += l.len();
                    (*len < PARSE_WHOLE - 8).then_some(l)
                })
                .collect();
            format!("{fill}{around}{}{late}", filler(100))
        }
        Position::Late => format!(
            "import {{ Early }} from './early'\n{}{around}{}{late}",
            filler(450),
            filler(5)
        ),
    }
}

/// Statement/hazard pairs the walk reads differently from a whole parse, with
/// the positions (Header, Straddle, Late) where it does. Fixing one changes
/// its entry here; a new misread fails the test. The line walk's causes:
/// `export default X` is code to it; a multi-line `require` past the cut is
/// missed; a `;` or `from '…'` in a comment ends a statement; two misread
/// backticks (a regex, JSX text, `/* ` */`) balance and hide what's between;
/// an import in a block comment or a `${…}`-nested template is kept; a header
/// cut inside a template holding `//` leaves it open; and a hazard that is
/// code ends the header, putting a later statement past the cut.
const KNOWN_WRONG: &[&str] = &[
    "export-default/none/HSL",
    "require-list/none/SL",
    "list-comment-semi/none/SL",
    "list-comment-from/none/SL",
    "export-default/string-backtick/HSL",
    "require-list/string-backtick/SL",
    "list-comment-semi/string-backtick/SL",
    "list-comment-from/string-backtick/SL",
    "export-default/comment-backtick/HSL",
    "require-list/comment-backtick/SL",
    "list-comment-semi/comment-backtick/SL",
    "list-comment-from/comment-backtick/SL",
    "export-default/regex-backtick/HSL",
    "require-list/regex-backtick/SL",
    "list-comment-semi/regex-backtick/SL",
    "list-comment-from/regex-backtick/SL",
    "named/regex-pair/SL",
    "list/regex-pair/SL",
    "default/regex-pair/SL",
    "namespace/regex-pair/SL",
    "export-from/regex-pair/SL",
    "export-list/regex-pair/SL",
    "export-star-as/regex-pair/SL",
    "export-type/regex-pair/SL",
    "export-default/regex-pair/HSL",
    "require-one/regex-pair/SL",
    "require-list/regex-pair/SL",
    "module-exports/regex-pair/SL",
    "list-comment-semi/regex-pair/SL",
    "list-comment-from/regex-pair/SL",
    "named/block-comment-pair/SL",
    "list/block-comment-pair/SL",
    "default/block-comment-pair/SL",
    "namespace/block-comment-pair/SL",
    "export-from/block-comment-pair/SL",
    "export-list/block-comment-pair/SL",
    "export-star-as/block-comment-pair/SL",
    "export-type/block-comment-pair/SL",
    "export-default/block-comment-pair/HSL",
    "require-one/block-comment-pair/SL",
    "require-list/block-comment-pair/SL",
    "module-exports/block-comment-pair/SL",
    "list-comment-semi/block-comment-pair/SL",
    "list-comment-from/block-comment-pair/SL",
    "export-default/jsx-text/HSL",
    "require-list/jsx-text/HSL",
    "list-comment-semi/jsx-text/HSL",
    "list-comment-from/jsx-text/HSL",
    "named/jsx-pair/HSL",
    "list/jsx-pair/HSL",
    "default/jsx-pair/HSL",
    "namespace/jsx-pair/HSL",
    "export-from/jsx-pair/HSL",
    "export-list/jsx-pair/HSL",
    "export-star-as/jsx-pair/HSL",
    "export-type/jsx-pair/HSL",
    "export-default/jsx-pair/HSL",
    "require-one/jsx-pair/HSL",
    "require-list/jsx-pair/HSL",
    "module-exports/jsx-pair/HSL",
    "list-comment-semi/jsx-pair/HSL",
    "list-comment-from/jsx-pair/HSL",
    "export-default/template-import/HSL",
    "require-list/template-import/HSL",
    "list-comment-semi/template-import/HSL",
    "list-comment-from/template-import/HSL",
    "named/template-slashes/S",
    "list/template-slashes/S",
    "default/template-slashes/S",
    "namespace/template-slashes/S",
    "export-from/template-slashes/S",
    "export-list/template-slashes/S",
    "export-star-as/template-slashes/S",
    "export-type/template-slashes/S",
    "export-default/template-slashes/HSL",
    "require-one/template-slashes/S",
    "require-list/template-slashes/SL",
    "module-exports/template-slashes/S",
    "list-comment-semi/template-slashes/SL",
    "list-comment-from/template-slashes/SL",
    "named/nested-template/SL",
    "list/nested-template/SL",
    "default/nested-template/SL",
    "namespace/nested-template/SL",
    "export-from/nested-template/SL",
    "export-list/nested-template/SL",
    "export-star-as/nested-template/SL",
    "export-type/nested-template/SL",
    "export-default/nested-template/HSL",
    "require-one/nested-template/SL",
    "require-list/nested-template/SL",
    "module-exports/nested-template/SL",
    "list-comment-semi/nested-template/SL",
    "list-comment-from/nested-template/SL",
    "named/block-comment-import/SL",
    "list/block-comment-import/SL",
    "default/block-comment-import/SL",
    "namespace/block-comment-import/SL",
    "export-from/block-comment-import/SL",
    "export-list/block-comment-import/SL",
    "export-star-as/block-comment-import/SL",
    "export-type/block-comment-import/SL",
    "export-default/block-comment-import/HSL",
    "require-one/block-comment-import/SL",
    "require-list/block-comment-import/SL",
    "module-exports/block-comment-import/SL",
    "list-comment-semi/block-comment-import/SL",
    "list-comment-from/block-comment-import/SL",
    "export-default/continued-string/HSL",
    "require-list/continued-string/SL",
    "list-comment-semi/continued-string/SL",
    "list-comment-from/continued-string/SL",
    "export-default/type-alias/HSL",
    "require-list/type-alias/SL",
    "list-comment-semi/type-alias/SL",
    "list-comment-from/type-alias/SL",
    "export-default/division/HSL",
    "require-list/division/SL",
    "list-comment-semi/division/SL",
    "list-comment-from/division/SL",
    "export-default/jsx-apostrophe/HSL",
    "require-list/jsx-apostrophe/HSL",
    "list-comment-semi/jsx-apostrophe/HSL",
    "list-comment-from/jsx-apostrophe/HSL",
    "export-default/declare-module/HSL",
    "require-list/declare-module/HSL",
    "list-comment-semi/declare-module/HSL",
    "list-comment-from/declare-module/HSL",
];

#[test]
fn the_walk_reads_a_large_source_as_a_whole_parse_does() {
    let mut wrong = Vec::new();
    for (hazard, before, after) in HAZARDS {
        for (statement, text) in STATEMENTS {
            let positions: String = [Position::Header, Position::Straddle, Position::Late]
                .into_iter()
                .filter(|&position| {
                    let source = source(position, (before, after), text);
                    assert!(source.len() > PARSE_WHOLE);
                    let (walk, truth) = walk_and_truth(&source);
                    walk != truth
                })
                .map(|position| format!("{position:?}").remove(0))
                .collect();
            if !positions.is_empty() {
                wrong.push(format!("{statement}/{hazard}/{positions}"));
            }
        }
    }
    assert_eq!(wrong, KNOWN_WRONG);
}

fn xorshift(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// Statements and hazards the walk misreads somewhere in a random source; the
/// property test leaves them out until it reads them right.
const OPEN: &[&str] = &[
    "export-default",
    "require-list",
    "list-comment-semi",
    "list-comment-from",
    "regex-backtick",
    "regex-pair",
    "block-comment-pair",
    "jsx-text",
    "jsx-pair",
    "template-slashes",
    "nested-template",
    "block-comment-import",
    "continued-string",
];

/// Random large sources from the statements, hazards and filler above, each
/// read by the walk as a whole parse reads it. `RQ_FUZZ_SOURCES` and
/// `RQ_FUZZ_SEED` run it larger or elsewhere.
#[test]
fn random_large_sources_read_as_a_whole_parse_does() {
    fn env<T: std::str::FromStr>(key: &str, default: T) -> T {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    let count: usize = env("RQ_FUZZ_SOURCES", 40);
    let mut seed: u64 = env("RQ_FUZZ_SEED", 0x5EED_1A2B_3C4D_5E6F);
    let statements: Vec<(&str, &str)> = STATEMENTS
        .into_iter()
        .filter(|(name, _)| !OPEN.contains(name))
        .collect();
    let hazards: Vec<(&str, &str, &str)> = HAZARDS
        .into_iter()
        .filter(|(name, ..)| !OPEN.contains(name))
        .collect();
    for case in 0..count {
        let mut source = String::new();
        let mut parts = Vec::new();
        while source.len() <= PARSE_WHOLE * 2 {
            let (name, text) = match xorshift(&mut seed) % 4 {
                0 => {
                    let (name, text) = statements[xorshift(&mut seed) as usize % statements.len()];
                    (name, text.to_string())
                }
                1 => {
                    let (name, before, after) =
                        hazards[xorshift(&mut seed) as usize % hazards.len()];
                    (name, format!("{before}\n{after}"))
                }
                _ => ("filler", filler(1 + xorshift(&mut seed) as usize % 120)),
            };
            parts.push(name);
            source.push_str(&text);
            source.push('\n');
        }
        let (walk, truth) = walk_and_truth(&source);
        assert_eq!(walk, truth, "case {case}: {parts:?}");
    }
}

/// A binding as one stable line, comparable across versions of [`Binding`].
fn row(b: &Binding) -> String {
    match b {
        Binding::Named {
            local,
            imported,
            spec,
            rows,
        } => format!("named {local} {imported} {spec} {}-{}", rows.0, rows.1),
        Binding::Default { local, spec } => format!("default {local} {spec}"),
        Binding::Namespace { local, spec } => format!("namespace {local} {spec}"),
        Binding::Star { spec } => format!("star {spec}"),
        Binding::Module { spec } => format!("module {spec}"),
        Binding::Local { local, name } => format!("local {local} {name}"),
    }
}

/// Every source over the cut under `RQ_IMPORTS_CORPUS` (`:`-separated
/// checkouts), walked and parsed whole. Prints what the walk misses and adds
/// against the whole parse; with `RQ_IMPORTS_BASE`, a previous run's
/// `RQ_IMPORTS_OUT`, also what changed since that run. See docs/RECALL.md.
#[test]
#[ignore = "reads real checkouts; run by hand on walk changes"]
fn corpus_scan() {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;

    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name == ".git" {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                sources(&path, out);
            } else if EXTS.iter().any(|x| name.ends_with(x)) {
                out.push(path);
            }
        }
    }

    let corpus = std::env::var("RQ_IMPORTS_CORPUS").expect("RQ_IMPORTS_CORPUS=<dir>:<dir>");
    let mut files = Vec::new();
    for dir in corpus.split(':') {
        sources(Path::new(dir), &mut files);
    }
    files.sort();
    let (mut walked, mut whole) = (BTreeSet::new(), BTreeSet::new());
    let mut scanned = 0;
    for file in &files {
        // as resolution reads it: regular files, at most MAX_BYTES
        let Some(source) = read(file).filter(|s| s.len() > PARSE_WHOLE) else {
            continue;
        };
        scanned += 1;
        let name = file.to_string_lossy();
        let (walk, truth) = walk_and_truth(&source);
        walked.extend(walk.iter().map(|b| format!("{name}\t{}", row(b))));
        whole.extend(truth.iter().map(|b| format!("{name}\t{}", row(b))));
    }
    let missed: Vec<&String> = whole.difference(&walked).collect();
    let added: Vec<&String> = walked.difference(&whole).collect();
    let mut report = format!(
        "{scanned} sources over {PARSE_WHOLE} bytes: {} bindings whole, walk misses {}, adds {}\n",
        whole.len(),
        missed.len(),
        added.len()
    );
    for m in &missed {
        writeln!(report, "  missed\t{m}").unwrap();
    }
    for a in &added {
        writeln!(report, "  added\t{a}").unwrap();
    }
    if let Ok(base) = std::env::var("RQ_IMPORTS_BASE") {
        let base: BTreeSet<String> = crate::index::read_source(Path::new(&base))
            .expect("RQ_IMPORTS_BASE")
            .lines()
            .map(str::to_string)
            .collect();
        let count = |pick: &dyn Fn(&String) -> bool, set: &BTreeSet<String>| {
            set.iter().filter(|b| pick(b)).cloned().collect::<Vec<_>>()
        };
        let lost = count(&|b| whole.contains(b) && !walked.contains(b), &base);
        let gained = count(&|b| whole.contains(b) && !base.contains(b), &walked);
        let false_dropped = count(&|b| !whole.contains(b) && !walked.contains(b), &base);
        let false_added = count(&|b| !whole.contains(b) && !base.contains(b), &walked);
        writeln!(
            report,
            "against the base: real lost {}, gained {}, false dropped {}, false added {}",
            lost.len(),
            gained.len(),
            false_dropped.len(),
            false_added.len()
        )
        .unwrap();
        for l in &lost {
            writeln!(report, "  LOST\t{l}").unwrap();
        }
    }
    if let Ok(out) = std::env::var("RQ_IMPORTS_OUT") {
        let lines: String = walked.iter().map(|b| format!("{b}\n")).collect();
        std::fs::write(out, lines).expect("RQ_IMPORTS_OUT");
    }
    eprintln!("{report}");
}
