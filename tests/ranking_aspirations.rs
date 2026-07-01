//! TDD-style aspirations: queries a developer would actually type against rq's
//! own source, paired with the definition we *want* back. Some may fail — that's
//! the point: a failure is a ranking gap to discuss, not necessarily a bug.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use reference_query::index;
use reference_query::search::{self, ActiveFiles};
use reference_query::store::Store;

/// Index a copy of `src/` whose files all share one (old) mtime, so the recency
/// boost is uniform and cancels out. This test asserts *match-quality* ranking;
/// without normalizing it, the top result would flip based on which source file
/// the developer last edited — a freshly-touched file's recency boost can swamp a
/// thin prefix-tail margin.
fn indexed_src() -> Store {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let dir = std::env::temp_dir().join(format!("rq-aspirations-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    copy_with_uniform_mtime(&src, &dir);
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    let _ = fs::remove_dir_all(&dir);
    store
}

/// Recursively copy `src` → `dst`, stamping every file with the same epoch-old
/// mtime so no file looks "recently modified".
fn copy_with_uniform_mtime(src: &Path, dst: &Path) {
    let stamp = UNIX_EPOCH + Duration::from_secs(1_000_000);
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_with_uniform_mtime(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), &to).unwrap();
            File::open(&to).unwrap().set_modified(stamp).unwrap();
        }
    }
}

fn top(store: &Store, query: &str) -> (String, String) {
    search::search(store, query, None, None, &ActiveFiles::default(), 10)
        .unwrap()
        .first()
        .map(|h| (h.name.clone(), h.kind.clone()))
        .unwrap_or_else(|| ("<none>".into(), String::new()))
}

#[test]
fn aspirational_queries() {
    let store = indexed_src();
    // (query, wanted name, wanted kind) — what a developer typing this expects.
    let wants = [
        ("lp", "LanguagePlugin", "trait"), // acronym across a camel hump
        ("recency", "recency_boost", "function"), // prefix of the helper
        ("budgeted", "index_budgeted", "function"), // a trailing word
        ("parsefile", "parse_file", "function"), // snake target, no separator typed
        ("search", "search", "function"),  // the fn, not the bare `mod search;`
        ("store", "Store", "struct"),      // the struct, not the bare `mod store;`
        ("kind", "Kind", "enum"),          // the enum
    ];
    let mut misses = Vec::new();
    for (q, name, kind) in wants {
        let (got_name, got_kind) = top(&store, q);
        if got_name != name || got_kind != kind {
            misses.push(format!(
                "  {q:<10} want {name} ({kind})   got {got_name} ({got_kind})"
            ));
        }
    }
    assert!(
        misses.is_empty(),
        "ranking gaps ({} of {}):\n{}",
        misses.len(),
        wants.len(),
        misses.join("\n")
    );
}
