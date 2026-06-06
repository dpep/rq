//! Candidate retrieval must not drop an exact match when its first-character
//! bucket overflows the per-layer cap — the failure mode on a huge repo.

use rq::core::{Kind, RepoIdentity, Symbol};
use rq::store::Store;

fn sym(name: &str) -> Symbol {
    Symbol {
        name: name.into(),
        kind: Kind::Function,
        language: "rust".into(),
        file: "a.rs".into(),
        line: 1,
        parent: None,
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

    let cands = store.search_candidates("mango", 5, false).unwrap();
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

    let strong_only = store.search_candidates("user", 50, false).unwrap();
    assert!(strong_only.iter().any(|c| c.name == "User"), "prefix kept");
    assert!(
        !strong_only.iter().any(|c| c.name == "Peruser"),
        "fuzzy-only candidate skipped when a strong match exists"
    );

    let forced = store.search_candidates("user", 50, true).unwrap();
    assert!(
        forced.iter().any(|c| c.name == "Peruser"),
        "force_fuzzy still recalls the fuzzy candidate"
    );
}
