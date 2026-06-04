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

    let cands = store.search_candidates("mango", 5).unwrap();
    assert!(
        cands.iter().any(|c| c.name == "mango"),
        "exact match dropped by the cap; got {:?}",
        cands.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}
