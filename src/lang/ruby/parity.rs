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
}

impl Intended {
    const ALL: [Intended; 1] = [Intended::NonAsciiConstant];

    /// Why a row only prism extracted is right.
    fn prism_only(row: &Row) -> Option<Intended> {
        (row.kind == Kind::Constant.as_str() && row.name.starts_with(|c: char| !c.is_ascii()))
            .then_some(Intended::NonAsciiConstant)
    }
}

/// How the two extractions of one file part.
#[derive(Default)]
struct Diff {
    /// Each difference [`Intended`] explains, by what explains it.
    explained: Vec<(Intended, String)>,
    /// Each difference nothing explains: a parity bug.
    unexplained: Vec<String>,
}

fn diff(file: &str, source: &str) -> Diff {
    let rows = |symbols: Vec<Symbol>| -> BTreeSet<Row> { symbols.iter().map(Row::of).collect() };
    let ours = rows(prism::extract(file, source));
    let oracle = rows(tree_sitter::extract(file, source));
    let mut out = Diff::default();
    for row in ours.difference(&oracle) {
        match Intended::prism_only(row) {
            Some(why) => out.explained.push((why, format!("prism       {row}"))),
            None => out.unexplained.push(format!("prism       {row}")),
        }
    }
    for row in oracle.difference(&ours) {
        out.unexplained.push(format!("tree-sitter {row}"));
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
