//! Search — the staged ranking pipeline.
//!
//! Layers 1–3 (exact/prefix, abbreviation-aware fuzzy, path) over the index,
//! scored by an additive, `--explain`-able scorer. Layers 4–5 (live scan,
//! opportunistic extraction) and true streaming/early-exit arrive in phase 2;
//! for now the candidate set is gathered once and ranked.

mod score;

pub use score::{Boosts, Feature, Scored};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::{Store, SymbolRow};

/// How many candidates to pull from the store before ranking.
const CANDIDATE_LIMIT: usize = 4000;

/// Sentinel repository id for live-scan (Layer 4) results — distinct from any
/// real row id, and treated as "the current repo" so the boost applies.
const LIVE_REPO_ID: i64 = -1;

/// Boost for a symbol whose file you're actively changing on this branch.
const BRANCH_FILE_BOOST: f64 = 180.0;
/// Smaller boost for a symbol in a directory you're changing (a neighbor).
const BRANCH_DIR_BOOST: f64 = 60.0;

/// Files you're working on this branch — those that differ from the trunk —
/// plus the directories holding them. Symbols in those files (or their
/// directory neighbors) get a branch boost. Empty on the trunk / outside git.
#[derive(Debug, Default, Clone)]
pub struct ActiveFiles {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

impl ActiveFiles {
    /// Build from a list of repo-relative paths changed on the branch.
    pub fn new<I: IntoIterator<Item = String>>(paths: I) -> Self {
        let files: HashSet<String> = paths.into_iter().collect();
        let dirs = files
            .iter()
            .filter_map(|f| parent_dir(f))
            .map(str::to_string)
            .collect();
        ActiveFiles { files, dirs }
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The branch boost for a candidate's file: full if the file itself is
    /// changing, smaller if a sibling in the same directory is.
    fn boost(&self, path: &str) -> f64 {
        if self.files.contains(path) {
            BRANCH_FILE_BOOST
        } else if parent_dir(path).is_some_and(|d| self.dirs.contains(d)) {
            BRANCH_DIR_BOOST
        } else {
            0.0
        }
    }
}

/// The directory portion of a repo-relative path (`app/models/user.rb` →
/// `app/models`), or `None` for a top-level file.
fn parent_dir(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// A ranked search result. Serializes for `--json` / `--ndjson`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Hit {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: i64,
    pub parent: Option<String>,
    #[serde(rename = "repo")]
    pub repo_identity: String,
    pub score: f64,
    pub features: Vec<Feature>,
    /// The definition's source line (trimmed) — filled for displayed results in
    /// machine-readable output. `None` when unread or in text mode.
    pub signature: Option<String>,
}

/// Search the index for `query`, returning up to `limit` ranked hits.
/// `current_repo_id` (if any) boosts results from the repository you're in;
/// `active` boosts files you're changing on the current branch.
pub fn search(
    store: &Store,
    query: &str,
    current_repo_id: Option<i64>,
    active: &ActiveFiles,
    limit: usize,
) -> crate::store::Result<Vec<Hit>> {
    let candidates = store.search_candidates(query, CANDIDATE_LIMIT)?;
    let learned = learned_boosts(store, query)?;
    let now = now_unix();

    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .filter_map(|c| {
            let key = (c.repository_id, c.file.clone(), c.name.clone());
            let boosts = Boosts {
                learned: learned.get(&key).copied().unwrap_or(0.0),
                // prefer whichever recency signal is more recent: a recent edit
                // (mtime) or a recent commit (git_ts)
                recency: recency_boost(c.git_ts.max(c.mtime), now),
                branch: if active.is_empty() {
                    0.0
                } else {
                    active.boost(&c.file)
                },
            };
            rank_one(query, c, current_repo_id, boosts)
        })
        .collect();

    sort_and_truncate(&mut hits, limit);
    Ok(hits)
}

/// Symbols in recently-modified files rank higher. ~14-day half-life and no
/// floor, so files untouched for a while contribute nothing.
fn recency_boost(mtime: Option<i64>, now: i64) -> f64 {
    let Some(mtime) = mtime else {
        return 0.0;
    };
    let age_days = (now - mtime).max(0) as f64 / 86_400.0;
    let boost = 120.0 * 0.5_f64.powf(age_days / 14.0);
    if boost < 1.0 { 0.0 } else { boost }
}

/// Decay-weighted learned boosts for a query, keyed by `(repo, file, name)`.
fn learned_boosts(
    store: &Store,
    query: &str,
) -> crate::store::Result<HashMap<(i64, String, String), f64>> {
    let now = now_unix();
    let q = query.to_ascii_lowercase();
    let mut map: HashMap<(i64, String, String), f64> = HashMap::new();
    for s in store.selections_for(&q)? {
        // several stored queries can match (e.g. "han" and "handler"); keep the
        // strongest boost for each candidate
        let boost = learned_boost(s.selections, s.last_selected_at, now);
        let entry = map.entry((s.repository_id, s.file, s.name)).or_insert(0.0);
        *entry = entry.max(boost);
    }
    Ok(map)
}

/// Turn a selection count + recency into a ranking boost. Evidence ramps over
/// ~5 selections; recency decays with a ~30-day half-life, floored so old picks
/// still count for something.
fn learned_boost(selections: i64, last_selected_at: i64, now: i64) -> f64 {
    if selections <= 0 {
        return 0.0;
    }
    let strength = (selections.min(5) as f64) / 5.0;
    let age_days = (now - last_selected_at).max(0) as f64 / 86_400.0;
    let recency = 0.5_f64.powf(age_days / 30.0).max(0.25);
    260.0 * strength * recency
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Layer 4: scan `root` live (no index required) and return ranked hits.
/// Results are treated as the current repo, so the current-repo boost applies.
pub fn live_search(root: &Path, query: &str, limit: usize) -> Vec<Hit> {
    let identity = crate::index::detect_identity(root).to_string();
    let mut hits: Vec<Hit> = crate::index::scan_symbols(root)
        .into_iter()
        .filter_map(|s| {
            let row = SymbolRow {
                name: s.name,
                kind: s.kind.as_str().to_string(),
                language: s.language,
                file: s.file,
                line: s.line as i64,
                parent: s.parent,
                repository_id: LIVE_REPO_ID,
                repo_identity: identity.clone(),
                mtime: None,
                git_ts: None,
            };
            rank_one(query, row, Some(LIVE_REPO_ID), Boosts::default())
        })
        .collect();
    sort_and_truncate(&mut hits, limit);
    hits
}

/// Merge two ranked lists, de-duplicating by location and name (keeping the
/// higher score), then re-rank and truncate. Used to blend index and live-scan
/// results.
pub fn merge(a: Vec<Hit>, b: Vec<Hit>, limit: usize) -> Vec<Hit> {
    use std::collections::HashMap;
    let mut by_key: HashMap<(String, i64, String), Hit> = HashMap::new();
    for hit in a.into_iter().chain(b) {
        let key = (hit.file.clone(), hit.line, hit.name.clone());
        match by_key.get(&key) {
            Some(existing) if existing.score >= hit.score => {}
            _ => {
                by_key.insert(key, hit);
            }
        }
    }
    let mut hits: Vec<Hit> = by_key.into_values().collect();
    sort_and_truncate(&mut hits, limit);
    hits
}

/// Highest score first; ties broken toward shorter (more specific) names.
fn sort_and_truncate(hits: &mut Vec<Hit>, limit: usize) {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(limit);
}

fn rank_one(
    query: &str,
    c: SymbolRow,
    current_repo_id: Option<i64>,
    boosts: Boosts,
) -> Option<Hit> {
    let scored = score::score(query, &c, current_repo_id, boosts)?;
    Some(Hit {
        name: c.name,
        kind: c.kind,
        file: c.file,
        line: c.line,
        parent: c.parent,
        repo_identity: c.repo_identity,
        score: scored.total,
        features: scored.features,
        signature: None,
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
        let hits = search(&store, "user", None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn abbreviation_finds_the_intended_symbol() {
        let store = store_with(&[
            sym("RefundProcessor", Kind::Class),
            sym("Refund", Kind::Class),
            sym("Payment", Kind::Class),
        ]);
        let hits = search(&store, "refundproc", None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(hits[0].name, "RefundProcessor");
        assert!(!names(&hits).contains(&"Payment"));
    }

    #[test]
    fn short_fuzzy_query_still_resolves() {
        let store = store_with(&[sym("User", Kind::Class), sym("Account", Kind::Class)]);
        let hits = search(&store, "usr", None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn no_match_returns_empty() {
        let store = store_with(&[sym("User", Kind::Class)]);
        let hits = search(&store, "zzzzz", None, &ActiveFiles::default(), 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn merge_dedups_by_location_keeping_higher_score() {
        let mk = |name: &str, score: f64| Hit {
            name: name.into(),
            kind: "class".into(),
            file: "a.rb".into(),
            line: 1,
            parent: None,
            repo_identity: "r".into(),
            score,
            features: vec![],
            signature: None,
        };
        let from_index = vec![mk("User", 100.0)];
        let from_live = vec![mk("User", 500.0), mk("Account", 200.0)];
        let merged = merge(from_index, from_live, 10);
        assert_eq!(merged.len(), 2, "the duplicate User is collapsed");
        assert_eq!(merged[0].name, "User");
        assert_eq!(merged[0].score, 500.0, "the higher-scored duplicate wins");
    }

    #[test]
    fn active_files_boosts_the_file_and_its_neighbors() {
        let active = ActiveFiles::new(["app/services/refund.rb".to_string()]);
        // the changed file itself: full boost
        assert_eq!(active.boost("app/services/refund.rb"), BRANCH_FILE_BOOST);
        // a sibling in the same directory: neighbor boost
        assert_eq!(active.boost("app/services/charge.rb"), BRANCH_DIR_BOOST);
        // unrelated directory: nothing
        assert_eq!(active.boost("app/models/user.rb"), 0.0);
    }

    #[test]
    fn branch_boost_lifts_an_active_file() {
        let store = store_with(&[sym("User", Kind::Class)]); // lives in app/x.rb
        let active = ActiveFiles::new(["app/x.rb".to_string()]);
        let hits = search(&store, "user", None, &active, 10).unwrap();
        assert!(hits[0].features.iter().any(|f| f.name == "branch"));
    }
}
