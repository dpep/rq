//! TDD-style aspirations: queries a developer would actually type against rq's
//! own source, paired with the definition we *want* back. Some may fail — that's
//! the point: a failure is a ranking gap to discuss, not necessarily a bug.

use std::path::PathBuf;

use rq::index;
use rq::search::{self, ActiveFiles};
use rq::store::Store;

fn indexed_src() -> Store {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &src).unwrap();
    store
}

fn top(store: &Store, query: &str) -> (String, String) {
    search::search(store, query, None, &ActiveFiles::default(), 10)
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
