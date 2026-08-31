//! Rust plugin, end to end: index a fixture file and assert the *ordering* — the
//! exact-name definition wins, kinds are classified, and a query that is only a
//! substring of another name doesn't outrank the thing named for it.

use std::fs;

use crate::search::{self, ActiveFiles};
use crate::tests::support::{indexed, top};

/// The fixture source, embedded at compile time so there's no runtime path to
/// resolve. Written into a throwaway repo dir the test indexes.
const WIDGET_RS: &str = include_str!("fixtures/rust/widget.rs");

#[test]
fn ranks_the_named_type_first_and_classifies_kinds() {
    let (store, dir) = indexed("ranks", "widget.rs", WIDGET_RS);

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
    let (store, dir) = indexed("kinds", "widget.rs", WIDGET_RS);

    let structs: Vec<_> = search::search(&store, "widget", None, None, &ActiveFiles::default(), 10)
        .unwrap()
        .hits
        .into_iter()
        .filter(|h| h.kind == "struct")
        .collect();
    assert_eq!(structs.len(), 1);
    assert_eq!(structs[0].name, "Widget");

    fs::remove_dir_all(&dir).ok();
}
