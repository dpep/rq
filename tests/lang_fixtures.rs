//! Go and Python plugins, end to end: index a fixture file and assert the
//! ordering — the named definition wins, with the right kind and qualification.

use std::fs;
use std::path::PathBuf;

use reference_query::index;
use reference_query::search::{self, ActiveFiles};
use reference_query::store::Store;

const WIDGET_GO: &str = include_str!("fixtures/go/widget.go");
const ACCOUNT_PY: &str = include_str!("fixtures/python/account.py");

fn indexed(tag: &str, name: &str, source: &str) -> (Store, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rq-lang-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), source).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    index::index_path(&mut store, &dir).unwrap();
    (store, dir)
}

fn top(store: &Store, query: &str) -> search::Hit {
    let hits = search::search(store, query, None, None, &ActiveFiles::default(), 10).unwrap();
    assert!(!hits.is_empty(), "no hits for {query:?}");
    hits.into_iter().next().unwrap()
}

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
