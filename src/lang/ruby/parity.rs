//! Prism's extraction against the Tree-sitter walk it replaced: alike on every
//! file both parse. Where prism reports a syntax error they part by design —
//! prism keeps what's around the broken spot (D70).

use std::path::{Path, PathBuf};

use super::{prism, tree_sitter};
use crate::core::Symbol;

/// What the store keeps of each symbol, one line apiece, sorted: emission
/// order isn't stored.
fn rows(symbols: &[Symbol]) -> Vec<String> {
    let mut rows: Vec<String> = symbols
        .iter()
        .map(|s| {
            format!(
                "{}-{} {} {} parent={:?} vis={:?} singleton={}",
                s.line, s.end_line, s.kind, s.name, s.parent, s.visibility, s.singleton
            )
        })
        .collect();
    rows.sort();
    rows
}

/// The rows only one side extracted, each marked with its side; empty when
/// they agree.
fn diff(file: &str, source: &str) -> Vec<String> {
    let ours = rows(&prism::extract(file, source));
    let oracle = rows(&tree_sitter::extract(file, source));
    let only = |a: &[String], b: &[String], side: &str| {
        a.iter()
            .filter(|r| !b.contains(r))
            .map(|r| format!("{side} {r}"))
            .collect::<Vec<_>>()
    };
    let mut out = only(&ours, &oracle, "prism      ");
    out.extend(only(&oracle, &ours, "tree-sitter"));
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
    for path in ruby_files(&root) {
        let name = path.file_name().unwrap().to_str().unwrap();
        let source = crate::index::read_source(&path).unwrap();
        if !prism::parses_cleanly(&source) {
            broken.push(name.to_string());
            continue;
        }
        assert_eq!(diff(name, &source), Vec::<String>::new(), "{name}");
    }
    // the one fixture written broken; any other is a typo in a fixture
    assert_eq!(broken, ["truncated.rb"]);
}

/// Every Ruby file under `$RQ_RUBY_PARITY`, both ways: `make ruby-parity
/// DIR=<checkout>`. Fails on a file prism parses cleanly whose extraction
/// differs; a broken file's difference is listed apart, as expected.
#[test]
#[ignore = "reads a corpus named by RQ_RUBY_PARITY"]
fn ruby_parity_corpus() {
    let root = PathBuf::from(std::env::var_os("RQ_RUBY_PARITY").expect("RQ_RUBY_PARITY=<dir>"));
    let (mut files, mut symbols, mut broken) = (0, 0, 0);
    let mut mismatched = Vec::new();
    for path in ruby_files(&root) {
        let Ok(source) = crate::index::read_source(&path) else {
            continue;
        };
        let name = path.strip_prefix(&root).unwrap().to_string_lossy();
        files += 1;
        symbols += prism::extract(&name, &source).len();
        let diff = diff(&name, &source);
        if diff.is_empty() {
            continue;
        }
        let clean = prism::parses_cleanly(&source);
        println!(
            "\n{name} ({})",
            if clean {
                "parses cleanly"
            } else {
                "syntax error"
            }
        );
        for row in &diff {
            println!("  {row}");
        }
        if clean {
            mismatched.push(name.into_owned());
        } else {
            broken += 1;
        }
    }
    println!(
        "\n{files} files, {symbols} symbols (prism); {} clean files differ, {broken} broken files differ",
        mismatched.len()
    );
    assert!(mismatched.is_empty(), "{mismatched:?}");
}
