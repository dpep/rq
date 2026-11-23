//! Prism's extraction against the Tree-sitter walk it replaced: alike on every
//! file both parse, but for the few [`Intended`] differences where prism's
//! answer is Ruby's and the oracle's isn't. Where prism reports a syntax error
//! they part by design — prism keeps what's around the broken spot (D70).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{prism, tree_sitter};
use crate::core::{Kind, Symbol, Visibility};

/// What the store keeps of a symbol, ordered for a sorted diff: emission
/// order isn't stored.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    line: u32,
    end_line: u32,
    kind: &'static str,
    name: String,
    parent: Option<String>,
    visibility: Option<&'static str>,
    singleton: bool,
}

impl Row {
    fn of(s: &Symbol) -> Row {
        Row {
            line: s.line,
            end_line: s.end_line,
            kind: s.kind.as_str(),
            name: s.name.clone(),
            parent: s.parent.clone(),
            visibility: s.visibility.map(Visibility::as_str),
            singleton: s.singleton,
        }
    }
}

impl std::fmt::Display for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}-{} {} {} parent={:?} vis={:?} singleton={}",
            self.line,
            self.end_line,
            self.kind,
            self.name,
            self.parent,
            self.visibility,
            self.singleton
        )
    }
}

/// A difference from the oracle that is prism reading Ruby right. Each is
/// recognized narrowly, so any other drift still fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Intended {
    /// `ÜBER = 1` assigns a constant: Ruby's constants start with any
    /// uppercase letter, the grammar's with an ASCII one.
    NonAsciiConstant,
    /// A string names a method by its content: `%q(x)` and `"a\tb"` name
    /// `x` and `a<tab>b`, where the oracle kept the source spelling.
    StringContent,
    /// A name built by interpolation (`"get_#{x}"`) can't be known
    /// statically, so it's skipped, as any computed name is.
    InterpolatedName,
    /// A quoted symbol (`:"x?"`, `%s(x)`) or one in non-ASCII letters (`:☠`)
    /// names a method; the grammar read only plain ASCII symbols.
    SymbolName,
    /// The grammar fell into an error on Ruby prism parses cleanly, and lost
    /// what follows it: parents, a class's end. Only definitions reaching the
    /// first error are excused; those wholly before it must still agree.
    OracleMisparse,
}

/// The delimiter pairs a `%` literal may use.
const PERCENT: [(&str, &str); 5] = [("(", ")"), ("[", "]"), ("{", "}"), ("<", ">"), ("|", "|")];

/// Whether `text` spells `name` as a symbol the grammar didn't read.
fn symbol_spelled(text: &str, name: &str) -> bool {
    text.contains(&format!(":\"{name}\""))
        || text.contains(&format!(":'{name}'"))
        || PERCENT
            .iter()
            .any(|(open, close)| text.contains(&format!("%s{open}{name}{close}")))
        || (!name.is_ascii() && text.contains(&format!(":{name}")))
}

impl Intended {
    const ALL: [Intended; 5] = [
        Intended::NonAsciiConstant,
        Intended::StringContent,
        Intended::InterpolatedName,
        Intended::SymbolName,
        Intended::OracleMisparse,
    ];

    /// Why a row only prism extracted is right.
    fn prism_only(row: &Row, source: &str) -> Option<Intended> {
        if row.kind == Kind::Constant.as_str() && row.name.starts_with(|c: char| !c.is_ascii()) {
            return Some(Intended::NonAsciiConstant);
        }
        // the lines of the definition: the macro call that spells the name
        let lines: Vec<&str> = source
            .lines()
            .skip(row.line as usize - 1)
            .take((row.end_line - row.line) as usize + 1)
            .collect();
        (row.kind == Kind::Method.as_str() && symbol_spelled(&lines.join("\n"), &row.name))
            .then_some(Intended::SymbolName)
    }

    /// Why a row only the oracle extracted is wrong.
    fn oracle_only(row: &Row) -> Option<Intended> {
        (row.kind == Kind::Method.as_str() && row.name.contains("#{"))
            .then_some(Intended::InterpolatedName)
    }

    /// Why prism names the oracle's definition otherwise.
    fn renamed(oracle: &Row, ours: &Row) -> Option<Intended> {
        let (was, is) = (oracle.name.as_str(), ours.name.as_str());
        // `%q(x)` kept whole, or an escape kept as its backslash
        let source_spelling = (was.starts_with('%') && was.contains(is)) || was.contains('\\');
        // `alias :"x y" z` kept its quotes
        let quoted = was.len() == is.len() + 2 && was[1..was.len() - 1] == *is;
        if oracle.kind != Kind::Method.as_str() {
            None
        } else if source_spelling {
            Some(Intended::StringContent)
        } else if quoted && (was.starts_with('"') || was.starts_with('\'')) {
            Some(Intended::SymbolName)
        } else {
            None
        }
    }

    /// Why prism gives the oracle's definition another visibility: an access
    /// call named it by a literal the oracle misread.
    fn revisited(ours: &Row, source: &str) -> Option<Intended> {
        let name = &ours.name;
        let spelled = |open: &str, close: &str| {
            ["%q", "%Q", "%"]
                .iter()
                .any(|p| source.contains(&format!("{p}{open}{name}{close}")))
        };
        if PERCENT.iter().any(|(open, close)| spelled(open, close)) {
            Some(Intended::StringContent)
        } else if symbol_spelled(source, name) {
            Some(Intended::SymbolName)
        } else {
            None
        }
    }
}

/// How the two extractions of one file part.
#[derive(Default)]
struct Diff {
    /// Each difference [`Intended`] explains, by what explains it.
    explained: Vec<(Intended, String)>,
    /// Each difference nothing explains: a parity bug.
    unexplained: Vec<String>,
    /// The row being noted reaches the oracle's first parse error.
    misparsed: bool,
}

impl Diff {
    fn note(&mut self, why: Option<Intended>, row: String) {
        match why.or_else(|| self.misparsed.then_some(Intended::OracleMisparse)) {
            Some(why) => self.explained.push((why, row)),
            None => self.unexplained.push(row),
        }
    }
}

fn diff(file: &str, source: &str) -> Diff {
    let rows = |symbols: Vec<Symbol>| -> BTreeSet<Row> { symbols.iter().map(Row::of).collect() };
    let ours = rows(prism::extract(file, source));
    let oracle = rows(tree_sitter::extract(file, source));
    let mut ours_only: Vec<Row> = ours.difference(&oracle).cloned().collect();
    let mut out = Diff::default();
    let error = tree_sitter::first_error_line(source);
    let reaches_error = |r: &Row| error.is_some_and(|line| r.end_line >= line);
    let blank_name = |r: &Row| Row {
        name: String::new(),
        ..r.clone()
    };
    let blank_vis = |r: &Row| Row {
        visibility: None,
        ..r.clone()
    };
    for o in oracle.difference(&ours) {
        // the same definition on both sides, named or made visible otherwise
        // several definitions can share a place (`attr_reader :a, :b`): pair
        // with the one a rule explains, if any
        let renamed = ours_only
            .iter()
            .position(|p| blank_name(p) == blank_name(o) && Intended::renamed(o, p).is_some())
            .or_else(|| {
                ours_only
                    .iter()
                    .position(|p| blank_name(p) == blank_name(o))
            });
        let revisited = ours_only.iter().position(|p| blank_vis(p) == blank_vis(o));
        let (why, pair) = match (renamed, revisited) {
            (Some(i), _) => (Intended::renamed(o, &ours_only[i]), i),
            (None, Some(i)) => (Intended::revisited(&ours_only[i], source), i),
            (None, None) => {
                out.misparsed = reaches_error(o);
                out.note(Intended::oracle_only(o), format!("tree-sitter {o}"));
                continue;
            }
        };
        let p = ours_only.remove(pair);
        out.misparsed = reaches_error(o) || reaches_error(&p);
        out.note(why, format!("tree-sitter {o}\n  prism       {p}"));
    }
    for p in &ours_only {
        out.misparsed = reaches_error(p);
        out.note(Intended::prism_only(p, source), format!("prism       {p}"));
    }
    out
}

fn ruby_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
        .map(ignore::DirEntry::into_path)
        .filter(|p| p.extension().is_some_and(|e| e == "rb") && p.is_file())
        .collect();
    files.sort();
    files
}

#[test]
fn prism_matches_tree_sitter_on_every_fixture_both_parse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/fixtures/ruby");
    let mut broken = Vec::new();
    let mut seen = BTreeSet::new();
    for path in ruby_files(&root) {
        let name = path.file_name().unwrap().to_str().unwrap();
        let source = crate::index::read_source(&path).unwrap();
        if !prism::parses_cleanly(&source) {
            broken.push(name.to_string());
            continue;
        }
        let diff = diff(name, &source);
        assert_eq!(diff.unexplained, Vec::<String>::new(), "{name}");
        seen.extend(diff.explained.iter().map(|(why, _)| *why));
    }
    // the one fixture written broken; any other is a typo in a fixture
    assert_eq!(broken, ["truncated.rb"]);
    // a difference no fixture shows any more is no longer one to excuse
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        Intended::ALL,
        "every intended difference has a fixture"
    );
}

/// Every Ruby file under `$RQ_RUBY_PARITY`, both ways: `make ruby-parity
/// DIR=<checkout>`. Fails on a file prism parses cleanly with a difference
/// [`Intended`] doesn't explain; a broken file's are listed apart, as expected.
#[test]
#[ignore = "reads a corpus named by RQ_RUBY_PARITY"]
fn ruby_parity_corpus() {
    let root = PathBuf::from(std::env::var_os("RQ_RUBY_PARITY").expect("RQ_RUBY_PARITY=<dir>"));
    let (mut files, mut symbols, mut broken) = (0, 0, 0);
    let mut mismatched = Vec::new();
    let mut explained = std::collections::BTreeMap::<Intended, usize>::new();
    for path in ruby_files(&root) {
        let Ok(source) = crate::index::read_source(&path) else {
            continue;
        };
        let name = path.strip_prefix(&root).unwrap().to_string_lossy();
        files += 1;
        symbols += prism::extract(&name, &source).len();
        let clean = prism::parses_cleanly(&source);
        let diff = diff(&name, &source);
        if clean {
            for (why, _) in &diff.explained {
                *explained.entry(*why).or_default() += 1;
            }
        }
        let shown: Vec<&String> = if clean {
            diff.unexplained.iter().collect()
        } else {
            diff.explained
                .iter()
                .map(|(_, r)| r)
                .chain(&diff.unexplained)
                .collect()
        };
        if shown.is_empty() {
            continue;
        }
        let state = if clean {
            "parses cleanly"
        } else {
            "syntax error"
        };
        println!("\n{name} ({state})");
        for row in shown {
            println!("  {row}");
        }
        if clean {
            mismatched.push(name.into_owned());
        } else {
            broken += 1;
        }
    }
    println!("\n{files} files, {symbols} symbols (prism)");
    for (why, n) in &explained {
        println!("  {n} intended: {why:?}");
    }
    println!(
        "  {} clean files differ unexplained, {broken} broken files differ",
        mismatched.len()
    );
    assert!(mismatched.is_empty(), "{mismatched:?}");
}
