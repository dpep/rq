//! Rust plugin, end to end: index a fixture file and assert the *ordering* — the
//! exact-name definition wins, kinds are classified, and a query that is only a
//! substring of another name doesn't outrank the thing named for it.

use std::fs;

use crate::search::{self, Context};
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

    // an associated fn without `self` is the type's own; a free fn isn't
    // anyone's, and a `self` method is an instance's
    let new = top(&store, "Widget::new");
    assert_eq!((new.kind.as_str(), new.singleton), ("function", true));
    assert!(!top(&store, "build_widget").singleton);
    assert!(!resize.singleton);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_field_is_found_by_name_and_scope_below_a_same_named_method() {
    let (store, dir) = indexed("fields", "widget.rs", WIDGET_RS);

    // the field is the only exact `size`: it beats `resize`
    let size = top(&store, "size");
    assert_eq!((size.name.as_str(), size.kind.as_str()), ("size", "field"));
    assert_eq!(size.parent.as_deref(), Some("Widget"));
    assert_eq!(top(&store, "Widget::size").kind, "field");
    assert_eq!(top(&store, "Widget.size").kind, "field");

    // the accessor is what `label` means; its field is still found, second
    let label = search::search(&store, "label", None, None, &Context::default(), 10)
        .unwrap()
        .hits;
    let kinds: Vec<_> = label.iter().map(|h| h.kind.as_str()).collect();
    assert_eq!(kinds, ["method", "field"]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn kind_filter_narrows_to_struct() {
    let (store, dir) = indexed("kinds", "widget.rs", WIDGET_RS);

    let structs: Vec<_> = search::search(&store, "widget", None, None, &Context::default(), 10)
        .unwrap()
        .hits
        .into_iter()
        .filter(|h| h.kind == "struct")
        .collect();
    assert_eq!(structs.len(), 1);
    assert_eq!(structs[0].name, "Widget");

    fs::remove_dir_all(&dir).ok();
}
