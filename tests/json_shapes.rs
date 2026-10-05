//! The shape of every structured answer, pinned: each `--json`/`--ndjson`
//! output reduced to `path: types` and compared with `json_shapes.golden`.
//!
//! Output shape is rq's public API, and spot asserts only check the fields a
//! test thought of. A renamed field, a type that flips (an int that becomes an
//! object), a field that vanishes from one path but not another, or a `null`
//! where the README promises absence fails here and shows in review.
//! Values aren't pinned — paths and timings vary — only their shape.
//!
//! Regenerate after an intended change: `UPDATE_GOLDEN=1 cargo test --test json_shapes`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use common::{Scratch, git_cmd, rq_cmd};

const GOLDEN: &str = "tests/json_shapes.golden";

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir).args(args).output().expect("run git");
    assert!(out.status.success(), "git {args:?}");
}

/// Run rq with `stdin`, returning stdout.
fn run(db: &Path, cwd: &Path, args: &[&str], stdin: &str) -> String {
    let mut child = rq_cmd(db, cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run rq");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for rq");
    String::from_utf8(out.stdout).unwrap()
}

/// Fold `value` into `shape` as `path → types`. An object's keys extend the
/// path; an array's elements share `[]`; `explain`'s keys are feature names,
/// data rather than shape, so they fold into `*`.
fn fold(
    shape: &mut BTreeMap<String, BTreeSet<&'static str>>,
    path: &str,
    value: &serde_json::Value,
) {
    use serde_json::Value;
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    shape.entry(path.to_string()).or_default().insert(kind);
    match value {
        Value::Array(items) => {
            for item in items {
                fold(shape, &format!("{path}[]"), item);
            }
        }
        Value::Object(fields) => {
            let dynamic = path.ends_with(".explain");
            for (key, v) in fields {
                let key = if dynamic { "*" } else { key.as_str() };
                fold(shape, &format!("{path}.{key}"), v);
            }
        }
        _ => {}
    }
}

/// One output's shape: a pretty document, or one object per ndjson line.
fn shape_of(label: &str, out: &str) -> String {
    let docs: Vec<serde_json::Value> = match serde_json::from_str(out) {
        Ok(doc) => vec![doc],
        Err(_) => out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{label}: {e}: {l:?}")))
            .collect(),
    };
    assert!(!docs.is_empty(), "{label}: no output");
    let mut shape = BTreeMap::new();
    for doc in &docs {
        fold(&mut shape, "", doc);
    }
    let mut text = format!("## {label}\n");
    for (path, kinds) in shape {
        let path = if path.is_empty() { "." } else { &path };
        let kinds: Vec<&str> = kinds.into_iter().collect();
        text += &format!("{path}: {}\n", kinds.join(" | "));
    }
    text
}

#[test]
#[allow(clippy::disallowed_methods, reason = "the golden file this test owns")]
fn every_structured_answer_keeps_its_shape() {
    let dir = Scratch::new("json-shapes");
    let db = dir.db();
    fs::write(
        dir.join("a.rb"),
        "module Shop\n  class Widget\n    def run\n    end\n  end\nend\n",
    )
    .unwrap();
    fs::write(dir.join("empty.rb"), "\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=t@e.st",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ],
    );
    let plain = Scratch::new("json-shapes-plain");
    fs::write(plain.join("b.rb"), "class Gadget\nend\n").unwrap();

    let (dir, plain): (&Path, &Path) = (&dir, &plain);
    // (label, cwd, args, stdin), in order: each run may depend on the last
    let runs: Vec<(&str, &Path, Vec<&str>, &str)> = vec![
        (
            "search: warming miss",
            dir,
            vec!["Widget", "--no-wait", "--json"],
            "",
        ),
        ("index", dir, vec!["--index", "--json"], ""),
        ("status", dir, vec!["--status", "--json"], ""),
        ("search: hit", dir, vec!["Widget", "--json"], ""),
        (
            "search: hit --explain",
            dir,
            vec!["Widget", "--explain", "--json"],
            "",
        ),
        (
            "search: --show",
            dir,
            vec!["Widget", "--show", "--json"],
            "",
        ),
        ("search: miss", dir, vec!["NoSuchThing", "--json"], ""),
        (
            "search: scope miss",
            dir,
            vec!["Nowhere::Widget", "--json"],
            "",
        ),
        ("search: live hit", plain, vec!["Gadget", "--json"], ""),
        ("batch", dir, vec!["--ndjson"], "Widget\nNoSuchThing\n"),
        ("symbols", dir, vec!["--symbols", "a.rb", "--json"], ""),
        (
            "symbols: none",
            dir,
            vec!["--symbols", "empty.rb", "--json"],
            "",
        ),
        ("usage", dir, vec!["--usage", "--json"], ""),
        ("error", dir, vec!["Widget", "-k", "banana", "--json"], ""),
        ("drop", dir, vec!["--drop", "--json"], ""),
        ("drop: not indexed", dir, vec!["--drop", "--json"], ""),
        (
            "drop: unknown repo",
            dir,
            vec!["--drop", "github.com/none/such", "--json"],
            "",
        ),
    ];
    let actual: String = runs
        .iter()
        .map(|(label, cwd, args, stdin)| shape_of(label, &run(&db, cwd, args, stdin)))
        .collect::<Vec<_>>()
        .join("\n");

    let header = "# Shape of each --json/--ndjson answer: path: types. Generated by\n\
                  # tests/json_shapes.rs; regenerate with UPDATE_GOLDEN=1 cargo test --test json_shapes\n\n";
    let actual = format!("{header}{actual}");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(GOLDEN, &actual).unwrap();
        return;
    }
    let golden = fs::read_to_string(GOLDEN).unwrap_or_default();
    assert!(
        golden == actual,
        "structured output changed shape; if intended, regenerate with \
         UPDATE_GOLDEN=1 cargo test --test json_shapes and review the diff\n\n{actual}"
    );
}
