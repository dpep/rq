//! Candidate retrieval's layers: exact and prefix matches always reach the
//! scorer, and fuzzy recall is scoped and skipped where it can't change the
//! answer.

use crate::core::{Kind, RepoIdentity, Symbol};
use crate::search::Probe;
use crate::store::Store;

fn sym(name: &str) -> Symbol {
    Symbol {
        name: name.into(),
        kind: Kind::Function,
        language: "rust".into(),
        file: "a.rs".into(),
        line: 1,
        end_line: 1,
        parent: None,
        visibility: None,
        stub: false,
    }
}

#[test]
fn an_exact_match_survives_a_fuzzy_recall_that_fills_the_cap() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store.test_checkout(&RepoIdentity::local("/tmp/x"));

    // 50 fuzzy matches for "mango", plus the exact target. With a tiny cap,
    // fuzzy recall alone would keep 5 of the 51; the exact layer must still
    // return it.
    let mut syms: Vec<Symbol> = (1..=50).map(|i| sym(&format!("ma_n_go{i:03}"))).collect();
    syms.push(sym("mango"));
    store
        .replace_file_symbols(repo, "a.rs", "rust", None, "h", &syms)
        .unwrap();

    let cands = store
        .search_candidates("mango", 5, true, None, None, &Probe::new("mango"))
        .unwrap();
    assert!(
        cands.iter().any(|c| c.name == "mango"),
        "exact match dropped by the cap; got {:?}",
        cands.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

#[test]
fn a_strong_match_short_circuits_fuzzy_recall() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store.test_checkout(&RepoIdentity::local("/tmp/x"));

    // "User" is a prefix match for "user"; "Peruser" matches only fuzzily.
    // When a strong match exists fuzzy recall is skipped — the relevance gate
    // would drop its hits — so "Peruser" doesn't come back. A wildcard query
    // forces it on.
    store
        .replace_file_symbols(
            repo,
            "a.rs",
            "rust",
            None,
            "h",
            &[sym("User"), sym("Peruser")],
        )
        .unwrap();

    let probe = Probe::new("user");
    let strong_only = store
        .search_candidates("user", 50, false, None, None, &probe)
        .unwrap();
    assert!(strong_only.iter().any(|c| c.name == "User"), "prefix kept");
    assert!(
        !strong_only.iter().any(|c| c.name == "Peruser"),
        "fuzzy-only candidate skipped when a strong match exists"
    );

    let forced = store
        .search_candidates("user", 50, true, None, None, &probe)
        .unwrap();
    assert!(
        forced.iter().any(|c| c.name == "Peruser"),
        "force_fuzzy still recalls the fuzzy candidate"
    );
}

#[test]
fn a_repo_scoped_cap_is_filled_by_that_repo_alone() {
    let mut store = Store::open_in_memory().unwrap();
    let here = store.test_checkout(&RepoIdentity::local("/tmp/here"));
    let other = store.test_checkout(&RepoIdentity::local("/tmp/other"));

    // The other repo floods every layer; with the cap shared across repos,
    // this repo's fuzzy match would never be reached.
    let flood: Vec<Symbol> = (1..=50)
        .map(|i| sym(&format!("aaa_widget{i:03}")))
        .collect();
    store
        .replace_file_symbols(other, "a.rs", "rust", None, "h", &flood)
        .unwrap();
    store
        .replace_file_symbols(here, "a.rs", "rust", None, "h", &[sym("MyWidget")])
        .unwrap();

    let cands = store
        .search_candidates("widget", 5, false, Some(here), None, &Probe::new("widget"))
        .unwrap();
    let names: Vec<&str> = cands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["MyWidget"],
        "only this repo's rows, and its match fits"
    );
}
