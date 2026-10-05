//! A change to what a plugin extracts reaches existing indexes only through a
//! schema step that requeues that language's files; without one, every
//! checkout indexed before the change keeps the old rows until each file
//! happens to change. This guard hashes each language's extraction of its
//! fixtures — only what the store keeps — against `extraction.golden`, and
//! fails when a hash moves while the schema version it was recorded at hasn't.
//!
//! It sees only what the fixtures exercise: an extraction fix should add its
//! input to `fixtures/<lang>/` as well as its inline test.

use std::collections::BTreeMap;
use std::path::Path;

use crate::core::Symbol;
use crate::lang;
use crate::store::VERSION as SCHEMA;

const GOLDEN: &str = "src/tests/extraction.golden";
const REGENERATE: &str = "UPDATE_GOLDEN=1 cargo test extraction_guard";

/// One stored row as a line: the fields `replace_file_symbols` writes, a
/// default one left out so a new field moves only the languages that set it.
/// Destructured without `..`, so a new field can't compile until it's here.
fn row(file: &str, s: &Symbol) -> String {
    let Symbol {
        name,
        kind,
        line,
        end_line,
        parent,
        visibility,
        stub,
        singleton,
        // the plugin and the path the fixture sits at, not what it extracted
        language: _,
        file: _,
    } = s;
    let mut out = format!("{file} {name} {kind} {line}-{end_line}");
    if let Some(parent) = parent {
        out += &format!(" parent={parent}");
    }
    if let Some(vis) = visibility {
        out += &format!(" vis={vis}");
    }
    if *stub {
        out += " stub";
    }
    if *singleton {
        out += " singleton";
    }
    out
}

/// FNV-1a: stable across Rust releases, which `DefaultHasher` is not.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Each language's hash over every fixture its plugin reads.
fn hashes() -> BTreeMap<&'static str, u64> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/fixtures");
    let mut rows: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for dir in std::fs::read_dir(&root).unwrap() {
        for entry in std::fs::read_dir(dir.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let Some(plugin) = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(lang::plugin_for_extension)
            else {
                continue;
            };
            let source = crate::index::read_source(&path).unwrap();
            let out = rows.entry(plugin.language()).or_default();
            out.extend(plugin.extract(&name, &source).iter().map(|s| row(&name, s)));
        }
    }
    rows.into_iter()
        .map(|(language, mut lines)| {
            lines.sort();
            (language, fnv1a(lines.join("\n").as_bytes()))
        })
        .collect()
}

/// The golden file: the schema version it was recorded at, and each hash.
fn parse(golden: &str) -> (i64, BTreeMap<String, u64>) {
    let mut schema = 0;
    let mut hashes = BTreeMap::new();
    for line in golden.lines().filter(|l| !l.starts_with('#')) {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        if key == "schema" {
            schema = value.parse().expect("schema version");
        } else {
            let hash = u64::from_str_radix(value, 16).expect("hex hash");
            hashes.insert(key.to_string(), hash);
        }
    }
    (schema, hashes)
}

fn render(hashes: &BTreeMap<&str, u64>) -> String {
    let mut out = format!(
        "# Each language's extraction of src/tests/fixtures, hashed by\n\
         # src/tests/extraction_guard.rs. Regenerate: {REGENERATE}\n\
         schema {}\n",
        SCHEMA
    );
    for (language, hash) in hashes {
        out += &format!("{language} {hash:016x}\n");
    }
    out
}

/// What a mismatch asks for: a requeue when the output moved under the schema
/// it was recorded at, else only a re-record. A language the golden lacks is
/// new, with nothing stored to requeue.
fn verdict(
    now: &BTreeMap<&str, u64>,
    recorded_at: i64,
    recorded: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let moved: Vec<&str> = now
        .iter()
        .filter(|&(language, hash)| recorded.get(*language).is_some_and(|h| h != hash))
        .map(|(language, _)| *language)
        .collect();
    let new = now.keys().any(|language| !recorded.contains_key(*language));
    if !moved.is_empty() && recorded_at == SCHEMA {
        let moved = moved.join(", ");
        return Err(format!(
            "extraction output changed for {moved}: bump the schema with a requeue \
             migration for {moved} (src/store/schema.rs), then regenerate with `{REGENERATE}`; \
             if only the fixture changed, just regenerate"
        ));
    }
    if !moved.is_empty() || new {
        return Err(format!(
            "extraction output moved since schema {recorded_at} (now {}); if that \
             schema step requeues it, regenerate with `{REGENERATE}`",
            SCHEMA
        ));
    }
    Ok(())
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the golden file is the repo's own"
)]
fn extraction_guard() {
    let now = hashes();
    for language in lang::languages() {
        assert!(
            now.contains_key(language),
            "{language} has no fixture under src/tests/fixtures"
        );
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, render(&now)).unwrap();
        return;
    }
    let (recorded_at, recorded) = parse(&std::fs::read_to_string(&path).unwrap_or_default());
    if let Err(message) = verdict(&now, recorded_at, &recorded) {
        panic!("{message}");
    }
}

#[test]
fn a_moved_hash_asks_for_a_requeue_only_under_the_same_schema() {
    let now = BTreeMap::from([("go", 1u64), ("ruby", 2)]);
    let golden = |ruby: Option<u64>| {
        let mut recorded = BTreeMap::from([("go".to_string(), 1u64)]);
        recorded.extend(ruby.map(|h| ("ruby".to_string(), h)));
        recorded
    };
    let (at, before) = (SCHEMA, SCHEMA - 1);
    let cases = [
        (at, golden(Some(2)), None),
        (before, golden(Some(2)), None),
        (at, golden(Some(3)), Some("requeue migration for ruby")),
        (before, golden(Some(3)), Some("regenerate")),
        (at, golden(None), Some("regenerate")),
    ];
    for (recorded_at, recorded, want) in cases {
        match (verdict(&now, recorded_at, &recorded), want) {
            (Ok(()), None) => {}
            (Err(message), Some(want)) => assert!(message.contains(want), "{message}"),
            (got, want) => panic!("{recorded:?} at {recorded_at}: got {got:?}, want {want:?}"),
        }
    }
}
