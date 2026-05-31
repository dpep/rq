//! Search — the staged ranking pipeline.
//!
//! Layers 1–3 (exact/prefix, abbreviation-aware fuzzy, path) over the index,
//! scored by an additive, `--explain`-able scorer. Layers 4–5 (live scan,
//! opportunistic extraction) and true streaming/early-exit arrive in phase 2;
//! for now the candidate set is gathered once and ranked.

mod score;

pub use score::{Feature, Scored};

use crate::store::{Store, SymbolRow};

/// How many candidates to pull from the store before ranking.
const CANDIDATE_LIMIT: usize = 4000;

/// A ranked search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: i64,
    pub parent: Option<String>,
    pub repo_identity: String,
    pub score: f64,
    pub features: Vec<Feature>,
}

/// Search the index for `query`, returning up to `limit` ranked hits.
/// `current_repo_id` (if any) boosts results from the repository you're in.
pub fn search(
    store: &Store,
    query: &str,
    current_repo_id: Option<i64>,
    limit: usize,
) -> crate::store::Result<Vec<Hit>> {
    let candidates = store.search_candidates(query, CANDIDATE_LIMIT)?;

    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .filter_map(|c| rank_one(query, c, current_repo_id))
        .collect();

    // Highest score first; break ties toward shorter (more specific) names.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(limit);
    Ok(hits)
}

fn rank_one(query: &str, c: SymbolRow, current_repo_id: Option<i64>) -> Option<Hit> {
    let scored = score::score(query, &c, current_repo_id)?;
    Some(Hit {
        name: c.name,
        kind: c.kind,
        file: c.file,
        line: c.line,
        parent: c.parent,
        repo_identity: c.repo_identity,
        score: scored.total,
        features: scored.features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Kind, Symbol};

    fn sym(name: &str, kind: Kind) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            language: "ruby".into(),
            file: "app/x.rb".into(),
            line: 1,
            parent: None,
        }
    }

    fn store_with(symbols: &[Symbol]) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&crate::core::RepoIdentity::local("/tmp/x"), None)
            .unwrap();
        store
            .replace_file_symbols(repo, "app/x.rb", "ruby", None, "h", symbols)
            .unwrap();
        store
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn ranks_exact_match_first() {
        let store = store_with(&[
            sym("Users", Kind::Class),
            sym("User", Kind::Class),
            sym("UserMailer", Kind::Class),
        ]);
        let hits = search(&store, "user", None, 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn abbreviation_finds_the_intended_symbol() {
        let store = store_with(&[
            sym("RefundProcessor", Kind::Class),
            sym("Refund", Kind::Class),
            sym("Payment", Kind::Class),
        ]);
        let hits = search(&store, "refundproc", None, 10).unwrap();
        assert_eq!(hits[0].name, "RefundProcessor");
        assert!(!names(&hits).contains(&"Payment"));
    }

    #[test]
    fn short_fuzzy_query_still_resolves() {
        let store = store_with(&[sym("User", Kind::Class), sym("Account", Kind::Class)]);
        let hits = search(&store, "usr", None, 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn no_match_returns_empty() {
        let store = store_with(&[sym("User", Kind::Class)]);
        let hits = search(&store, "zzzzz", None, 10).unwrap();
        assert!(hits.is_empty());
    }
}
