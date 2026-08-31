//! Go, Python, TypeScript and JavaScript plugins, end to end: index a fixture
//! file and assert the ordering — the named definition wins, with the right
//! kind and qualification.

use std::fs;

use crate::tests::support::{indexed, top};

const WIDGET_GO: &str = include_str!("fixtures/go/widget.go");
const ACCOUNT_PY: &str = include_str!("fixtures/python/account.py");
const WIDGET_TS: &str = include_str!("fixtures/typescript/widget.ts");
const ACCOUNT_JSX: &str = include_str!("fixtures/javascript/account.jsx");

#[test]
fn go_definitions_rank_and_classify() {
    let (store, dir) = indexed("go", "widget.go", WIDGET_GO);

    let widget = top(&store, "Widget");
    assert_eq!(widget.name, "Widget");
    assert_eq!(widget.kind, "struct");
    assert_eq!(top(&store, "Renderer").kind, "trait");

    let resize = top(&store, "Resize");
    assert_eq!(resize.kind, "method");
    assert_eq!(resize.parent.as_deref(), Some("Widget"));

    assert_eq!(top(&store, "BuildWidget").kind, "function");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn python_definitions_rank_and_classify() {
    let (store, dir) = indexed("py", "account.py", ACCOUNT_PY);

    let account = top(&store, "Account");
    assert_eq!(account.name, "Account");
    assert_eq!(account.kind, "class");

    let deposit = top(&store, "deposit");
    assert_eq!(deposit.kind, "method");
    assert_eq!(deposit.parent.as_deref(), Some("Account"));

    assert_eq!(top(&store, "build_account").kind, "function");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn typescript_definitions_rank_and_classify() {
    let (store, dir) = indexed("ts", "widget.ts", WIDGET_TS);

    let widget = top(&store, "Widget");
    assert_eq!(widget.name, "Widget");
    assert_eq!(widget.kind, "class");
    assert_eq!(widget.language, "typescript");

    assert_eq!(top(&store, "Renderer").kind, "trait");
    assert_eq!(top(&store, "WidgetSize").kind, "struct");
    assert_eq!(top(&store, "WidgetColor").kind, "enum");

    let resize = top(&store, "resize");
    assert_eq!(resize.kind, "method");
    assert_eq!(resize.parent.as_deref(), Some("Widget"));

    assert_eq!(top(&store, "buildWidget").kind, "function");
    // an arrow assigned to a const is a function like any other
    assert_eq!(top(&store, "defaultWidget").kind, "function");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn javascript_definitions_rank_and_classify() {
    let (store, dir) = indexed("jsx", "account.jsx", ACCOUNT_JSX);

    let account = top(&store, "Account");
    assert_eq!(account.name, "Account");
    assert_eq!(account.kind, "class");
    assert_eq!(account.language, "javascript");

    let deposit = top(&store, "deposit");
    assert_eq!(deposit.kind, "method");
    assert_eq!(deposit.parent.as_deref(), Some("Account"));

    assert_eq!(top(&store, "buildAccount").kind, "function");
    // a JSX-returning component in a `.jsx` file still parses
    assert_eq!(top(&store, "AccountBadge").kind, "function");

    fs::remove_dir_all(&dir).ok();
}
