//! Ranking regression guard, dogfooded on rq's own source: index `src/` and
//! assert the obvious query lands its definition first, with the right kind.
//!
//! The names here are rq's own public symbols — a rename that breaks navigation
//! also breaks this test, which is the point. Queries are exact matches for a
//! unique symbol, so the result is recency-independent (an exact match outscores
//! any prefix/fuzzy competitor by far more than the recency boost can move).

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

fn top(store: &Store, query: &str) -> search::Hit {
    let hits = search::search(store, query, None, &ActiveFiles::default(), 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits.into_iter().next().unwrap()
}

#[test]
fn obvious_queries_land_their_definition_first() {
    let store = indexed_src();

    // exact match beats prefix competitors (e.g. SymbolRow, FileSymbols)
    let symbol = top(&store, "Symbol");
    assert_eq!(symbol.name, "Symbol");
    assert_eq!(symbol.kind, "struct");

    // kinds are classified across the model
    assert_eq!(top(&store, "LanguagePlugin").kind, "trait");

    // a unique function and struct resolve to themselves
    assert_eq!(top(&store, "index_budgeted").name, "index_budgeted");
    let fs = top(&store, "FileSymbols");
    assert_eq!(fs.name, "FileSymbols");
    assert_eq!(fs.kind, "struct");
}
