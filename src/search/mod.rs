//! Search — the staged ranking pipeline.
//!
//! Layers 1–3 (exact/prefix, abbreviation-aware fuzzy, path) over the index,
//! scored by an additive, `--explain`-able scorer. Layers 4–5 (live scan,
//! opportunistic extraction) and true streaming/early-exit arrive in phase 2;
//! for now the candidate set is gathered once and ranked.

mod score;

pub(crate) use score::{Boosts, Feature, confidence, match_positions, match_quality, path_stem};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::store::{Store, SymbolRow};

/// Per-layer cap on candidates pulled from the store before ranking. Exact and
/// prefix matches are guaranteed in full (see `Store::search_candidates`); this
/// only bounds the broad first-char-anchor and trigram-fuzzy recall layers.
/// Scoring is linear and cheap, so this sits well under the latency budget.
const CANDIDATE_LIMIT: usize = 8000;

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
pub(crate) struct ActiveFiles {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

impl ActiveFiles {
    /// Build from a list of repo-relative paths changed on the branch.
    pub(crate) fn new<I: IntoIterator<Item = String>>(paths: I) -> Self {
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
pub(crate) struct Hit {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file: String,
    pub line: i64,
    /// 1-based last line of the definition — read `line..=end_line` for the whole
    /// span. Omitted in JSON when unknown (a row indexed before end-line tracking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Access level (`public`/`crate`/`private`/`protected`) when the language
    /// expresses one. Omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(rename = "repo")]
    pub repo_identity: String,
    /// Raw additive score — the ranking key and the `--explain` breakdown source.
    /// Not serialized: JSON exposes the normalized `confidence` instead.
    #[serde(skip)]
    pub score: f64,
    /// Normalized match confidence in [0,1], filled before output (see
    /// [`score::confidence`]). This is what JSON carries in place of the raw score.
    pub confidence: f64,
    /// The scoring features, serialized as their names in descending weight order
    /// (the raw values are low-signal unnormalized; `--explain` shows them in text).
    #[serde(serialize_with = "serialize_feature_names")]
    pub features: Vec<Feature>,
    /// The definition's source line (trimmed) — filled for displayed results in
    /// machine-readable output. Omitted when unread (matching `--symbols`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The full definition source (`line..=end_line`), filled only by `--show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// How many places declare this name, when more than one folded together
    /// (a reopened Ruby module, a Rust type with `impl` blocks in several
    /// files). Absent when the definition is declared once.
    #[serde(skip_serializing_if = "is_one")]
    pub declarations: usize,
    /// The `file:line` of the declarations that folded into this one, so the
    /// collapse loses nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_in: Vec<String>,
    /// Matches this window was drawn from, before `--limit`. Lets a caller tell
    /// it saw ten of a thousand rather than ten of ten. Filled before output.
    pub total: usize,
    /// Feature name → weight, filled only under `--explain`, so the breakdown
    /// text mode prints is reproducible from JSON too. `features` keeps its
    /// name-list shape so existing callers don't break.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<std::collections::BTreeMap<String, f64>>,
}

/// Serialize a hit's features as a name list, strongest first — the values are
/// unnormalized and low-signal, so the ordered names are the useful part.
fn serialize_feature_names<S: serde::Serializer>(
    features: &[Feature],
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize;
    let mut sorted: Vec<&Feature> = features.iter().collect();
    sorted.sort_by(|a, b| b.value.total_cmp(&a.value));
    let names: Vec<&str> = sorted.iter().map(|f| f.name).collect();
    names.serialize(s)
}

/// A ranked window plus how many matches it was drawn from.
pub(crate) struct Matches {
    pub hits: Vec<Hit>,
    /// Matches before `--limit` truncated them, capped by `CANDIDATE_LIMIT`.
    pub total: usize,
}

/// A definition declared exactly once needs no count in the output.
fn is_one(n: &usize) -> bool {
    *n <= 1
}

/// Read-through to the window, so a caller that only wants the results reads
/// like it always did.
impl std::ops::Deref for Matches {
    type Target = [Hit];
    fn deref(&self) -> &[Hit] {
        &self.hits
    }
}

/// Search the index for `query`, returning up to `limit` ranked hits.
/// `current_repo_id` (if any) boosts results from the repository you're in;
/// `only_repo` (if any) restricts results to that repository, so a search inside
/// a repo answers about *that* repo rather than leaking others you've indexed;
/// `active` boosts files you're changing on the current branch.
pub(crate) fn search(
    store: &Store,
    query: &str,
    current_repo_id: Option<i64>,
    only_repo: Option<i64>,
    active: &ActiveFiles,
    limit: usize,
) -> crate::store::Result<Matches> {
    // Recall keys off the leaf name only — a `Foo::Bar` qualifier targets the
    // parent during scoring, and the store indexes `name`, not `parent`. A
    // wildcard query then keys off its literal chars (the store indexes literal
    // trigrams); the glob matches precisely during scoring.
    let (leaf, _) = score::parse_qualified(query);
    let stripped;
    let recall = if score::has_wildcard(leaf) {
        stripped = score::strip_wildcards(leaf);
        stripped.as_str()
    } else {
        leaf
    };
    let trace_on = crate::trace::enabled();
    let t = std::time::Instant::now();
    let candidates = store.search_candidates(recall, CANDIDATE_LIMIT, score::has_wildcard(leaf))?;
    let n_candidates = candidates.len();
    let t_recall = t.elapsed();
    let t = std::time::Instant::now();
    let now = now_unix();
    let learned = learned_boosts(store, query, now)?;

    // Borrows rather than consumes, so the retry below can re-rank the same
    // candidates instead of asking the store for them again.
    let rank = |candidates: &[SymbolRow], near_miss: bool| -> Vec<Hit> {
        candidates
            .iter()
            .filter_map(|c| {
                // Repo scope: outside `--all-repos`, a search inside a repo returns
                // only that repo's definitions — never another indexed repo's.
                if only_repo.is_some_and(|r| r != c.repository_id) {
                    return None;
                }
                // learned is empty for most queries — skip the per-candidate
                // String clones the key would cost
                let learned_boost = if learned.is_empty() {
                    0.0
                } else {
                    let key = (c.repository_id, c.file.clone(), c.name.clone());
                    learned.get(&key).copied().unwrap_or(0.0)
                };
                let boosts = Boosts {
                    learned: learned_boost,
                    // prefer whichever recency signal is more recent: a recent edit
                    // (mtime, stored in nanoseconds — convert to seconds) or a
                    // recent commit (git_ts, seconds)
                    recency: recency_boost(c.git_ts.max(c.mtime.map(|n| n / 1_000_000_000)), now),
                    branch: if active.is_empty() {
                        0.0
                    } else {
                        active.boost(&c.file)
                    },
                };
                rank_one(query, c, current_repo_id, boosts, near_miss)
            })
            .collect()
    };
    // The typo pass is a retry, not a wider net: running it up front would let
    // a bounded edit-distance match outrank a candidate that genuinely contains
    // the query, and would pay for the edit distance on every search.
    let mut hits = rank(&candidates, false);
    // Nothing above zero means nothing worth showing — `ActiveRecrod` matched
    // only a test method whose name happens to contain `ActiveRecordRecord`,
    // scored into the negative by the test-path penalty. A wrong answer blocks
    // the retry just as surely as no answer, so treat them alike.
    if hits.iter().all(|h| h.score <= 0.0) {
        // Only candidates that could *be* a near miss are worth re-scoring —
        // the alternative is paying the whole name-match chain a second time
        // for ten thousand rows to serve a few hundred.
        let near: Vec<SymbolRow> = candidates
            .into_iter()
            .filter(|c| score::near_miss_possible(query, &c.name))
            .collect();
        let retried = rank(&near, true);
        // keep the first pass's answer if the retry turns up nothing
        if !retried.is_empty() {
            hits = retried;
        }
    }
    let n_hits = hits.len();
    let t_score = t.elapsed();

    let t = std::time::Instant::now();
    // Counted after folding repeat declarations but before truncation: a caller
    // shown ten of a thousand matches can't tell from the window alone.
    let total = sort_and_truncate(&mut hits, limit);
    // The search path already measures these for its trace line; profiling
    // records the same numbers rather than timing the work twice.
    crate::profile::record("recall", t_recall, || format!("{n_candidates} candidates"));
    crate::profile::record("score", t_score, || format!("{n_hits} hits"));
    crate::profile::record("sort", t.elapsed(), || format!("top {limit}"));
    if trace_on {
        crate::trace!(
            "search {query:?}: recall {n_candidates} cand in {} ms, score→{n_hits} hits in {} ms, sort {} ms",
            t_recall.as_millis(),
            t_score.as_millis(),
            t.elapsed().as_millis(),
        );
    }
    Ok(Matches { hits, total })
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
    now: i64,
) -> crate::store::Result<HashMap<(i64, String, String), f64>> {
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
/// ~5 selections; recency decays with a ~30-day half-life, all the way down.
///
/// No floor: a floor meant a pick could never be forgotten, only diminished, so
/// a choice made once a year ago kept nudging results forever. Letting the
/// half-life run to zero is how a wrong pick now expires — which matters more
/// since nothing else corrects one. (A repeated search used to decay the boost
/// on the theory that repeating meant the last answer missed; that inference
/// turned out to fire almost entirely on machine re-runs, so it was removed and
/// time is the only forgetting left.)
fn learned_boost(selections: i64, last_selected_at: i64, now: i64) -> f64 {
    if selections <= 0 {
        return 0.0;
    }
    let strength = (selections.min(5) as f64) / 5.0;
    let age_days = (now - last_selected_at).max(0) as f64 / 86_400.0;
    let recency = 0.5_f64.powf(age_days / 30.0);
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
/// `skip` names already-indexed files to ignore, and `deadline` bounds the scan
/// — both empty/`None` for an unbounded scan of a never-indexed directory. When
/// `prefilter` is set, only files containing the query (substring) are parsed —
/// fast for exact/prefix/substring queries, but blind to fuzzy abbreviations, so
/// callers retry with `prefilter = false` if a filtered scan finds nothing.
pub(crate) fn live_search(
    root: &Path,
    query: &str,
    limit: usize,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
    prefilter: bool,
) -> Vec<Hit> {
    let needle = prefilter.then_some(query.as_bytes());
    let identity = crate::index::detect_identity(root).to_string();
    let mut hits: Vec<Hit> = crate::index::scan(root, skip, deadline, needle)
        .into_iter()
        .flat_map(|fs| fs.symbols)
        .filter_map(|s| {
            let row = SymbolRow {
                name: s.name,
                kind: s.kind.as_str().to_string(),
                language: s.language,
                file: s.file,
                line: s.line as i64,
                end_line: Some(s.end_line as i64),
                parent: s.parent,
                repository_id: LIVE_REPO_ID,
                repo_identity: identity.clone(),
                mtime: None,
                git_ts: None,
                visibility: s.visibility.map(str::to_string),
            };
            rank_one(query, &row, Some(LIVE_REPO_ID), Boosts::default(), false)
        })
        .collect();
    sort_and_truncate(&mut hits, limit);
    hits
}

/// Merge two ranked lists, de-duplicating by location and name (keeping the
/// higher score), then re-rank and truncate. Used to blend index and live-scan
/// results.
pub(crate) fn merge(a: Vec<Hit>, b: Vec<Hit>, limit: usize) -> Vec<Hit> {
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

/// Scope gate for a qualified query (`Foo::Bar#baz`). When the user names an
/// enclosing scope and at least one result actually sits in it, drop the rest —
/// a `baz` outside `Foo::Bar` is noise next to the one inside it, the same way
/// the relevance gate drops fuzzy near-matches beside an exact hit. When
/// *nothing* matches the scope, the list is left untouched: the scope was a
/// hint, and the definition may simply live somewhere we didn't expect, so a
/// `baz` elsewhere still surfaces rather than returning empty.
///
/// An in-scope result is one the scorer gave the `parent` feature — i.e. its
/// recorded parent ends with the qualifier's scope chain.
pub(crate) fn apply_scope_gate(query: &str, hits: &mut Vec<Hit>) {
    if score::parse_qualified(query).1.is_none() {
        return; // unqualified query — nothing to gate on
    }
    let in_scope = |h: &Hit| h.features.iter().any(|f| f.name == "parent");
    if hits.iter().any(in_scope) {
        hits.retain(in_scope);
    }
}

/// Highest score first; ties broken toward shorter (more specific) names, then
/// by location so the order is total.
///
/// That last tiebreak is what makes an answer reproducible. A query like
/// `Transaction` in a large repo can turn up five definitions that share a
/// name, a length, and a score — every earlier comparison ties, and a stable
/// sort then just preserves whatever order the rows arrived in, which is the
/// database's business and not stable between runs. The same query would
/// answer differently each time, which is baffling from a terminal and worse
/// from an agent, and it means output can't be diffed to check a refactor.
fn sort_and_truncate(hits: &mut Vec<Hit>, limit: usize) -> usize {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| (&a.file, a.line).cmp(&(&b.file, b.line)))
    });
    collapse_declarations(hits);
    let total = hits.len();
    hits.truncate(limit);
    total
}

/// Fold repeat declarations of one qualified name into a single result.
///
/// Ruby reopens a module across files and Rust spreads `impl` blocks the same
/// way, so a name can be declared a dozen times: `rq Middleware` spent its whole
/// first page on four declarations of `ActiveRecord::Middleware`, one of them a
/// six-line autoload stub. Four rows, one answer — the opposite of what a
/// navigation tool is for.
///
/// The survivor is the best-ranked declaration, which `extent` already biases
/// toward the one with a real body; the rest are recorded on it so nothing is
/// lost. Only *qualified* names fold, deliberately: two unqualified `Widget`s
/// are the same reopened class in Ruby but two unrelated types in Rust, and
/// showing one row too many is the cheaper mistake.
fn collapse_declarations(hits: &mut Vec<Hit>) {
    use std::collections::HashMap;
    let mut first: HashMap<(String, String, String, String), usize> = HashMap::new();
    let mut folded: Vec<Vec<String>> = vec![Vec::new(); hits.len()];
    let mut keep = Vec::with_capacity(hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let Some(parent) = hit.parent.clone() else {
            keep.push(true);
            continue;
        };
        let key = (
            hit.repo_identity.clone(),
            parent,
            hit.name.clone(),
            hit.kind.clone(),
        );
        match first.get(&key) {
            Some(&at) => {
                folded[at].push(format!("{}:{}", hit.file, hit.line));
                keep.push(false);
            }
            None => {
                first.insert(key, i);
                keep.push(true);
            }
        }
    }
    let mut i = 0;
    hits.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
    // walk the survivors in their original order to reattach what folded in
    let mut survivors = keep.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i);
    for hit in hits.iter_mut() {
        let Some(src) = survivors.next() else { break };
        if !folded[src].is_empty() {
            hit.declarations = 1 + folded[src].len();
            hit.also_in = std::mem::take(&mut folded[src]);
        }
    }
}

fn rank_one(
    query: &str,
    c: &SymbolRow,
    current_repo_id: Option<i64>,
    boosts: Boosts,
    near_miss: bool,
) -> Option<Hit> {
    // Borrowed, so a candidate that doesn't score costs nothing; the clones
    // below happen only for the few that become results.
    let scored = score::score(query, c, current_repo_id, boosts, near_miss)?;
    Some(Hit {
        name: c.name.clone(),
        kind: c.kind.clone(),
        language: c.language.clone(),
        file: c.file.clone(),
        line: c.line,
        end_line: c.end_line,
        parent: c.parent.clone(),
        visibility: c.visibility.clone(),
        repo_identity: c.repo_identity.clone(),
        score: scored.total,
        confidence: 0.0, // filled from the final result set before output
        features: scored.features,
        signature: None,
        body: None,
        declarations: 1,
        also_in: Vec::new(),
        total: 0, // filled from the final result set before output
        explain: None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn identical_names_rank_in_a_stable_order() {
        // Five definitions sharing a name score the same and are the same
        // length, so every earlier tiebreak ties. Without a final total order
        // the winner is whatever order the rows arrived in — and the same query
        // answers differently between runs.
        let hit = |file: &str, line: i64| Hit {
            name: "Transaction".into(),
            kind: "class".into(),
            language: "ruby".into(),
            file: file.into(),
            line,
            end_line: None,
            parent: None,
            visibility: None,
            score: 1.0,
            confidence: 0.5,
            signature: None,
            repo_identity: "local:/tmp/x".into(),
            features: Vec::new(),
            body: None,
            declarations: 1,
            also_in: Vec::new(),
            total: 0,
            explain: None,
        };
        let ordered = |mut hits: Vec<Hit>| {
            sort_and_truncate(&mut hits, 10);
            hits.into_iter()
                .map(|h| (h.file, h.line))
                .collect::<Vec<_>>()
        };

        let a = ordered(vec![
            hit("app/models/b.rb", 1),
            hit("app/models/a.rb", 9),
            hit("app/models/a.rb", 2),
        ]);
        // the same set, arriving in a different order, must rank the same
        let b = ordered(vec![
            hit("app/models/a.rb", 2),
            hit("app/models/b.rb", 1),
            hit("app/models/a.rb", 9),
        ]);
        assert_eq!(a, b, "ranking must not depend on row order");
        assert_eq!(
            a,
            vec![
                ("app/models/a.rb".to_string(), 2),
                ("app/models/a.rb".to_string(), 9),
                ("app/models/b.rb".to_string(), 1),
            ]
        );
    }

    use super::*;
    use crate::core::{Kind, Symbol};

    fn sym(name: &str, kind: Kind) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            language: "ruby".into(),
            file: "app/x.rb".into(),
            line: 1,
            end_line: 1,
            parent: None,
            visibility: None,
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

    /// Two repos, each with its own symbol, so scoping can be exercised.
    fn store_two_repos() -> (Store, i64, i64) {
        let mut store = Store::open_in_memory().unwrap();
        let a = store
            .upsert_repository(&crate::core::RepoIdentity::local("/tmp/a"), None)
            .unwrap();
        let b = store
            .upsert_repository(&crate::core::RepoIdentity::local("/tmp/b"), None)
            .unwrap();
        store
            .replace_file_symbols(a, "a.rb", "ruby", None, "h", &[sym("Widget", Kind::Class)])
            .unwrap();
        store
            .replace_file_symbols(b, "b.rb", "ruby", None, "h", &[sym("Widget", Kind::Class)])
            .unwrap();
        (store, a, b)
    }

    #[test]
    fn only_repo_scopes_results_to_that_repo() {
        let (store, a, b) = store_two_repos();
        // scoped to repo A: only A's Widget, never B's
        let hits = search(
            &store,
            "Widget",
            Some(a),
            Some(a),
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].repo_identity, "local:/tmp/a");
        // no scope (--all-repos): both repos' Widgets surface
        let all = search(&store, "Widget", Some(a), None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(all.hits.len(), 2);
        let _ = b;
    }

    #[test]
    fn scoped_search_reports_no_match_rather_than_leaking_another_repo() {
        let (store, a, _b) = store_two_repos();
        // "Gadget" exists in neither; scoped to A it's simply absent (not B's)
        let hits = search(
            &store,
            "Gadget",
            Some(a),
            Some(a),
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn ranks_exact_match_first() {
        let store = store_with(&[
            sym("Users", Kind::Class),
            sym("User", Kind::Class),
            sym("UserMailer", Kind::Class),
        ]);
        let hits = search(&store, "user", None, None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn abbreviation_finds_the_intended_symbol() {
        let store = store_with(&[
            sym("RefundProcessor", Kind::Class),
            sym("Refund", Kind::Class),
            sym("Payment", Kind::Class),
        ]);
        let hits = search(
            &store,
            "refundproc",
            None,
            None,
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert_eq!(hits[0].name, "RefundProcessor");
        assert!(!names(&hits).contains(&"Payment"));
    }

    #[test]
    fn short_fuzzy_query_still_resolves() {
        let store = store_with(&[sym("User", Kind::Class), sym("Account", Kind::Class)]);
        let hits = search(&store, "usr", None, None, &ActiveFiles::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn no_match_returns_empty() {
        let store = store_with(&[sym("User", Kind::Class)]);
        let hits = search(&store, "zzzzz", None, None, &ActiveFiles::default(), 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn merge_dedups_by_location_keeping_higher_score() {
        let mk = |name: &str, score: f64| Hit {
            name: name.into(),
            kind: "class".into(),
            language: "ruby".into(),
            file: "a.rb".into(),
            line: 1,
            end_line: Some(1),
            parent: None,
            visibility: None,
            repo_identity: "r".into(),
            score,
            confidence: 0.0,
            features: vec![],
            signature: None,
            body: None,
            declarations: 1,
            also_in: Vec::new(),
            total: 0,
            explain: None,
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

    fn nested(name: &str, kind: Kind, parent: &str) -> Symbol {
        Symbol {
            parent: Some(parent.into()),
            ..sym(name, kind)
        }
    }

    #[test]
    fn qualified_query_ranks_the_definition_in_the_named_scope() {
        let store = store_with(&[
            nested("Config", Kind::Class, "Baz"),
            nested("Config", Kind::Class, "Foo"),
            nested("Config", Kind::Class, "Qux"),
        ]);
        // `Foo::Config` should surface the Config nested under Foo first
        let hits = search(
            &store,
            "Foo::Config",
            None,
            None,
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert_eq!(hits[0].parent.as_deref(), Some("Foo"));
        assert!(hits[0].features.iter().any(|f| f.name == "parent"));
    }

    #[test]
    fn qualifier_resolves_modules_and_methods_too() {
        let store = store_with(&[
            nested("perform", Kind::Method, "Bar::Worker"),
            nested("perform", Kind::Method, "Other::Worker"),
            nested("Worker", Kind::Module, "Bar"),
        ]);
        // a method qualified by its full scope chain
        let m = search(
            &store,
            "Bar::Worker#perform",
            None,
            None,
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert_eq!(m[0].kind, "method");
        assert_eq!(m[0].parent.as_deref(), Some("Bar::Worker"));
        // a module qualified by its enclosing scope
        let w = search(
            &store,
            "Bar::Worker",
            None,
            None,
            &ActiveFiles::default(),
            10,
        )
        .unwrap();
        assert_eq!(w[0].name, "Worker");
        assert_eq!(w[0].parent.as_deref(), Some("Bar"));
    }

    fn hit(name: &str, in_scope: bool) -> Hit {
        Hit {
            name: name.into(),
            kind: "method".into(),
            language: "ruby".into(),
            file: "a.rb".into(),
            line: 1,
            end_line: Some(1),
            parent: None,
            visibility: None,
            repo_identity: "r".into(),
            score: 1.0,
            confidence: 0.0,
            features: if in_scope {
                vec![Feature {
                    name: "parent",
                    value: 180.0,
                }]
            } else {
                vec![]
            },
            signature: None,
            body: None,
            declarations: 1,
            also_in: Vec::new(),
            total: 0,
            explain: None,
        }
    }

    #[test]
    fn scope_gate_keeps_only_in_scope_results_when_some_match() {
        let mut hits = vec![hit("baz", true), hit("baz", false), hit("baz", false)];
        apply_scope_gate("Foo::Bar#baz", &mut hits);
        assert_eq!(hits.len(), 1, "out-of-scope baz methods are dropped");
        assert!(hits[0].features.iter().any(|f| f.name == "parent"));
    }

    #[test]
    fn scope_gate_falls_back_when_nothing_matches_the_scope() {
        // no result is in `Foo::Bar`, so a `baz` defined elsewhere still surfaces
        let mut hits = vec![hit("baz", false), hit("baz", false)];
        apply_scope_gate("Foo::Bar#baz", &mut hits);
        assert_eq!(hits.len(), 2, "fall back rather than return empty");
    }

    #[test]
    fn scope_gate_is_a_noop_for_an_unqualified_query() {
        let mut hits = vec![hit("baz", true), hit("baz", false)];
        apply_scope_gate("baz", &mut hits);
        assert_eq!(hits.len(), 2, "no qualifier — nothing to gate on");
    }

    #[test]
    fn branch_boost_lifts_an_active_file() {
        let store = store_with(&[sym("User", Kind::Class)]); // lives in app/x.rb
        let active = ActiveFiles::new(["app/x.rb".to_string()]);
        let hits = search(&store, "user", None, None, &active, 10).unwrap();
        assert!(hits[0].features.iter().any(|f| f.name == "branch"));
    }
}
