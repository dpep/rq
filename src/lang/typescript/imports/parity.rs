//! The large-source walk ([`module_statements`]) against a whole parse of the
//! same source: a truth the walk didn't produce. A row the walk reads wrong is
//! listed by name, so a fix turns it green visibly and a regression fails.

use super::walk::PARSE_WHOLE;
use super::*;

/// What the walk binds, and what a whole parse binds.
fn walk_and_truth(source: &str) -> (Vec<Binding>, Vec<Binding>) {
    (
        bindings("big.tsx", &module_statements("big.tsx", source)),
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
const HAZARDS: [(&str, &str, &str); 26] = [
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
    ("tsx-generic", "const id = <T,>(x: T) => x", ""),
    (
        "jsx-url",
        "export const U = () => <a href=\"x\">http://{host}</a>",
        "",
    ),
    (
        "jsx-nested",
        "export const N = () => (\n  <ul>\n    {items.map((i) => <li key={i}>it's `{i}`</li>)}\n  </ul>\n)",
        "",
    ),
    ("import-alias", "import Foo = Bar.Baz", ""),
    (
        "import-attributes",
        "import data from './data.json' with { type: 'json' }",
        "",
    ),
    (
        "minified",
        "import{a as b}from\"./min\";export{c as d};var e=require(\"./e\"),f=1",
        "",
    ),
    ("division-paren", "const ratio = (width) / 2 / (height)", ""),
    // read as division: the brackets in it unbalance the pass
    ("regex-after-paren", "if (ok) /[(]/.test(s)", ""),
    // and then the line walk's reading, which takes a template's line for code
    (
        "fallback-template",
        "if (ok) /[(]/.test(s)",
        "export function gen() {\n  return `\nimport { Fake } from './fake'\n`\n}",
    ),
];

/// Where a statement sits against the first 16 KB, where the line walk cut.
#[derive(Debug, Clone, Copy)]
enum Position {
    /// Before any code.
    Header,
    /// Started within the first 16 KB, ended past them.
    Straddle,
    /// Past the first 16 KB, after code.
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
/// its entry here; a new misread fails the test. What's left is the fallback:
/// a pass a misread unbalanced trusts every column-0 statement line, a
/// template's too.
const KNOWN_WRONG: &[&str] = &[
    "named/fallback-template/HSL",
    "list/fallback-template/HSL",
    "default/fallback-template/HSL",
    "namespace/fallback-template/HSL",
    "export-from/fallback-template/HSL",
    "export-list/fallback-template/HSL",
    "export-star-as/fallback-template/HSL",
    "export-type/fallback-template/HSL",
    "export-default/fallback-template/HSL",
    "require-one/fallback-template/HSL",
    "require-list/fallback-template/HSL",
    "module-exports/fallback-template/HSL",
    "list-comment-semi/fallback-template/HSL",
    "list-comment-from/fallback-template/HSL",
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
    // an unbalanced pass falls back to trusting column-0 lines
    "regex-after-paren",
    "fallback-template",
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

/// Every source over 16 KB under `RQ_IMPORTS_CORPUS` (`:`-separated
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
