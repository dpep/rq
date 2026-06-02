//! Rust plugin, end to end: index a fixture file and assert the *ordering* — the
//! exact-name definition wins, kinds are classified, and a query that is only a
//! substring of another name doesn't outrank the thing named for it.

use std::fs;
use std::path::PathBuf;

use rq::index;
use rq::search::{self, ActiveFiles};
use rq::store::Store;

/// The fixture source, embedded at compile time so there's no runtime path to
/// resolve. Written into a throwaway repo dir the test indexes.
const WIDGET_RS: &str = include_str!("fixtures/rust/widget.rs");

fn indexed_fixture() -> (Store, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rq-rust-fixture-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("widget.rs"), WIDGET_RS).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    (store, dir)
}

fn top(store: &Store, query: &str) -> search::Hit {
    let hits = search::search(store, query, None, &ActiveFiles::default(), 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits.into_iter().next().unwrap()
}

#[test]
fn ranks_the_named_type_first_and_classifies_kinds() {
    let (store, dir) = indexed_fixture();

    // exact name wins over `build_widget`, which merely contains "widget"
    let widget = top(&store, "widget");
    assert_eq!(widget.name, "Widget");
    assert_eq!(widget.kind, "struct");

    // the trait and enum are extracted with the right kinds
    assert_eq!(top(&store, "Render").kind, "trait");
    assert_eq!(top(&store, "Shape").kind, "enum");

    // a method defined in an impl is a method, qualified by its type
    let resize = top(&store, "resize");
    assert_eq!(resize.kind, "method");
    assert_eq!(resize.parent.as_deref(), Some("Widget"));

    // a free function is a function
    assert_eq!(top(&store, "build_widget").kind, "function");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn kind_filter_narrows_to_struct() {
    let (store, dir) = indexed_fixture();

    let structs: Vec<_> = search::search(&store, "widget", None, &ActiveFiles::default(), 10)
        .unwrap()
        .into_iter()
        .filter(|h| h.kind == "struct")
        .collect();
    assert_eq!(structs.len(), 1);
    assert_eq!(structs[0].name, "Widget");

    fs::remove_dir_all(&dir).ok();
}
