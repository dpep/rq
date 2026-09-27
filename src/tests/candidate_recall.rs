//! Candidate retrieval must not drop an exact match when its first-character
//! bucket overflows the per-layer cap — the failure mode on a huge repo.

use crate::core::{Kind, RepoIdentity, Symbol};
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
    }
}

#[test]
fn exact_match_survives_a_flooded_first_char_bucket() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store
        .upsert_repository(&RepoIdentity::local("/tmp/x"), None)
        .unwrap();

    // 50 names that all share "mango"'s first char AND sort before it, plus the
    // exact target. With a tiny cap, a broad first-char scan would truncate
    // "mango" away; the dedicated exact layer must still return it.
    let mut syms: Vec<Symbol> = (1..=50).map(|i| sym(&format!("manaa{i:03}"))).collect();
    syms.push(sym("mango"));
    store
        .replace_file_symbols(repo, "a.rs", "rust", None, "h", &syms)
        .unwrap();

    let cands = store
        .search_candidates("mango", 5, false, None, None, None)
        .unwrap();
    assert!(
        cands.iter().any(|c| c.name == "mango"),
        "exact match dropped by the cap; got {:?}",
        cands.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

#[test]
fn a_strong_match_short_circuits_the_broad_fuzzy_layers() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store
        .upsert_repository(&RepoIdentity::local("/tmp/x"), None)
        .unwrap();

    // "User" is a prefix match for "user"; "Peruser" matches only via trigram FTS
    // (it contains "user" but isn't a prefix). When a strong match exists the
    // broad fuzzy layers are skipped — the relevance gate would drop their hits —
    // so "Peruser" doesn't come back. A wildcard query forces them on.
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

    let strong_only = store
        .search_candidates("user", 50, false, None, None, None)
        .unwrap();
    assert!(strong_only.iter().any(|c| c.name == "User"), "prefix kept");
    assert!(
        !strong_only.iter().any(|c| c.name == "Peruser"),
        "fuzzy-only candidate skipped when a strong match exists"
    );

    let forced = store
        .search_candidates("user", 50, true, None, None, None)
        .unwrap();
    assert!(
        forced.iter().any(|c| c.name == "Peruser"),
        "force_fuzzy still recalls the fuzzy candidate"
    );
}

#[test]
fn a_repo_scoped_cap_is_filled_by_that_repo_alone() {
    let mut store = Store::open_in_memory().unwrap();
    let here = store
        .upsert_repository(&RepoIdentity::local("/tmp/here"), None)
        .unwrap();
    let other = store
        .upsert_repository(&RepoIdentity::local("/tmp/other"), None)
        .unwrap();

    // The other repo floods every layer with names that sort first; with the
    // cap shared across repos, this repo's fuzzy match would never be reached.
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
        .search_candidates("widget", 5, false, Some(here), None, None)
        .unwrap();
    let names: Vec<&str> = cands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["MyWidget"],
        "only this repo's rows, and its match fits"
    );
}

#[test]
fn a_filtered_net_reaches_past_rows_that_cannot_match() {
    let mut store = Store::open_in_memory().unwrap();
    let repo = store
        .upsert_repository(&RepoIdentity::local("/tmp/x"), None)
        .unwrap();

    // "maa" names fill the start of the `m` range; none holds "mgo" in order.
    // Unfiltered, a cap of 5 stops inside them; filtered, the net reads on.
    let mut syms: Vec<Symbol> = (1..=10).map(|i| sym(&format!("maa{i:03}"))).collect();
    syms.push(sym("mongo"));
    store
        .replace_file_symbols(repo, "a.rs", "rust", None, "h", &syms)
        .unwrap();

    let names = |filter: Option<crate::store::CandidateFilter>| -> Vec<String> {
        store
            .search_candidates("mgo", 5, false, None, filter, None)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    assert!(
        !names(None).contains(&"mongo".to_string()),
        "the cap binds unfiltered"
    );
    let in_order = Box::new(|name: &str, _: &str, _: &str| name.contains('o'));
    assert_eq!(names(Some(in_order)), vec!["mongo".to_string()]);
}
