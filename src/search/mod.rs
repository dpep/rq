//! Search — the staged ranking pipeline.
//!
//! Layers 1–3 (exact/prefix, abbreviation-aware fuzzy, path) over the index,
//! scored by an additive, `--explain`-able scorer. Layers 4–5 (live scan,
//! opportunistic extraction) and true streaming/early-exit arrive in phase 2;
//! for now the candidate set is gathered once and ranked.

mod names;
mod score;

pub(crate) use names::{Probe, SIG_BYTES, Signature};
pub(crate) use score::{
    Boosts, Feature, NAME_INDEX_FORMAT, confidence, is_literal, is_primary, joiners_eq,
    match_positions, match_quality, path_stem,
};

use std::collections::HashSet;
use std::time::Instant;

use crate::core::now_unix;
use crate::store::{Checkout, Store, SymbolRow};

/// Per-layer cap on candidates pulled from the store before ranking. Exact and
/// prefix matches are guaranteed in full (see `Store::search_candidates`); this
/// bounds the rows fuzzy recall fetches from the name index's matches.
/// Scoring is linear and cheap, so this sits well under the latency budget.
const CANDIDATE_LIMIT: usize = 8000;

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

/// The directories leading to a repo-relative file, outermost first.
fn dir_segments(path: &str) -> Vec<&str> {
    parent_dir(path).map_or_else(Vec::new, |d| d.split('/').collect())
}

/// The directory portion of a repo-relative path (`app/models/user.rb` →
/// `app/models`), or `None` for a top-level file.
fn parent_dir(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// Enclosing-scope boost per scope level the candidate's parent shares with
/// the anchor's: sharing the innermost class outranks sharing only its module.
const ENCLOSING_STEP: f64 = 60.0;
/// Cap on the enclosing-scope boost — below a branch file's, and far below the
/// gap between match tiers, so it reorders equals rather than overruling a
/// better name match.
const MAX_ENCLOSING: f64 = 180.0;
/// Proximity boost for a candidate in the anchor's own file.
const SAME_FILE_BOOST: f64 = 90.0;
/// Proximity boost for a candidate in the anchor's directory; it halves with
/// each directory step between the two, and stops counting below
/// [`MIN_PROXIMITY`].
const SAME_DIR_BOOST: f64 = 60.0;
const MIN_PROXIMITY: f64 = 5.0;
/// Boost for a candidate in a language the anchor's file can refer to. As
/// large as the secondary-path penalty, so an unreachable definition can't
/// outrank a reachable one on that alone; larger measured the same (D59).
const REACHABLE_BOOST: f64 = 400.0;

/// Where a query was asked from (`--anchor FILE:LINE`): an editor's cursor, or
/// the file an agent is reading. Ranking context only — it never filters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Anchor {
    /// Root of the checkout the anchor's file belongs to.
    root: String,
    /// The anchor's file, relative to that root.
    file: String,
    /// Lowercased scope chain of the innermost definition enclosing the
    /// anchor's line (`Foo::Widget#save` → `[foo, widget, save]`); empty when
    /// the line sits outside every definition.
    scope: Vec<String>,
    /// The language tags the anchor's file can refer to definitions in, as its
    /// plugin declares them; empty when no plugin handles the file.
    reach: Vec<&'static str>,
    /// The test or example tree holding the anchor's file (`examples/blog/`),
    /// whose definitions take no secondary penalty.
    tree: Option<String>,
}

impl Anchor {
    /// An anchor at `line` of `file`, given that file's definitions as they
    /// stand now. Language-blind: only spans and recorded parents are read.
    pub(crate) fn new(root: String, file: String, line: i64, defs: &[SymbolRow]) -> Self {
        let innermost = defs
            .iter()
            .filter(|d| d.line <= line && line <= d.end_line.unwrap_or(d.line))
            .max_by_key(|d| (d.line, std::cmp::Reverse(d.end_line)));
        let scope = innermost.map_or_else(Vec::new, |d| {
            let mut scope = d.parent.as_deref().map_or_else(Vec::new, score::segments);
            scope.push(d.name.to_ascii_lowercase());
            scope
        });
        let reach = crate::lang::reachable_from(&file);
        let tree = score::secondary_tree(&file).map(str::to_string);
        Anchor {
            root,
            file,
            scope,
            reach,
            tree,
        }
    }

    /// The candidate is in the anchor's own file, or under the anchor's test or
    /// example tree and callable from it: what is secondary elsewhere is the
    /// context there (D31, D60). A private definition stays secondary unless it
    /// sits beside the anchor: privacy is file-wide in some languages but
    /// directory-wide in others (a Go package), so only farther off is it
    /// surely out of the anchor's reach.
    fn in_tree(&self, root: &str, file: &str, visibility: Option<&str>) -> bool {
        root == self.root
            && (file == self.file
                || ((visibility != Some("private") || parent_dir(file) == parent_dir(&self.file))
                    && self.tree.as_deref().is_some_and(|t| file.starts_with(t))))
    }

    /// Code at the anchor can refer to a definition in `language`: the same
    /// language, or one its plugin names as reachable (TS from JS). Any
    /// checkout, since the language decides it, not the repo.
    fn reachable(&self, language: &str) -> f64 {
        if self.reach.contains(&language) {
            REACHABLE_BOOST
        } else {
            0.0
        }
    }

    /// The candidate is defined inside a scope enclosing the anchor — its parent
    /// is a leading run of the anchor's scope chain — graded by how much of the
    /// chain it shares. A bare `save` inside `Widget` thereby prefers
    /// `Widget#save`. rq records no inheritance, so a `save` the class inherits
    /// from an ancestor earns nothing here.
    fn enclosing(&self, parent: Option<&str>) -> f64 {
        let Some(parent) = parent else {
            return 0.0;
        };
        let p = score::segments(parent);
        if p.is_empty() || p.len() > self.scope.len() || p[..] != self.scope[..p.len()] {
            return 0.0;
        }
        (ENCLOSING_STEP * p.len() as f64).min(MAX_ENCLOSING)
    }

    /// Same file, then same directory, decaying with each directory step
    /// between the two. Only within the anchor's own checkout.
    fn proximity(&self, root: &str, file: &str) -> f64 {
        if root != self.root {
            return 0.0;
        }
        if file == self.file {
            return SAME_FILE_BOOST;
        }
        let (a, b) = (dir_segments(&self.file), dir_segments(file));
        let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let steps = (a.len() - common) + (b.len() - common);
        let boost = SAME_DIR_BOOST * 0.5_f64.powi(steps as i32);
        if boost < MIN_PROXIMITY { 0.0 } else { boost }
    }
}

/// Where a search is asked from, beyond the query: the files the branch is
/// changing, and the anchor position when the caller gave one.
#[derive(Debug, Default, Clone)]
pub(crate) struct Context {
    pub active: ActiveFiles,
    pub anchor: Option<Anchor>,
}

impl Context {
    /// Whether reach separates anything in this candidate set: some candidate
    /// is out of the anchor's reach. When none is, the boost would lift every
    /// one alike, changing no order and only diluting confidence.
    fn reach_splits(&self, rows: &[SymbolRow]) -> bool {
        self.anchor
            .as_ref()
            .is_some_and(|a| rows.iter().any(|r| a.reachable(&r.language) == 0.0))
    }

    /// The context-dependent boosts for one candidate, of a set for which
    /// [`Context::reach_splits`] said `reach`.
    fn boosts(&self, c: &SymbolRow, recency: f64, reach: bool) -> Boosts {
        let (enclosing, proximity, reachable, anchor_tree) =
            self.anchor.as_ref().map_or((0.0, 0.0, 0.0, false), |a| {
                (
                    a.enclosing(c.parent.as_deref()),
                    a.proximity(&c.root, &c.file),
                    if reach { a.reachable(&c.language) } else { 0.0 },
                    a.in_tree(&c.root, &c.file, c.visibility.as_deref()),
                )
            });
        Boosts {
            recency,
            branch: if self.active.is_empty() {
                0.0
            } else {
                self.active.boost(&c.file)
            },
            enclosing,
            proximity,
            reachable,
            anchor_tree,
        }
    }
}

/// What a search found, as its exit code and the usage counters tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// An answer (0).
    Hit,
    /// The index is complete, and nothing matched (1): definitive.
    Miss,
    /// The index couldn't yet say (2): ask again.
    Warming,
}

impl Verdict {
    /// The `status` a structured answer carries. A hit's rows carry none;
    /// `hit` names it for completeness.
    pub(crate) fn status(self) -> &'static str {
        match self {
            Verdict::Hit => "hit",
            Verdict::Miss => "no_match",
            Verdict::Warming => "warming",
        }
    }
}

/// Where a result was read from: the persisted index, or a live scan of a
/// directory rq doesn't track. Carried per result, since the two blend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    #[default]
    Index,
    Live,
}

/// A ranked search result. Serializes for `--json` / `--ndjson`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Hit {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file: String,
    /// Absolute checkout root `file` is relative to — per result, since
    /// `--all-repos` spans repos and one repo may have several checkouts.
    /// Filled before output; omitted when rq knows no checkout for the repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub line: i64,
    /// 1-based last line of the definition — read `line..=end_line` for the whole
    /// span. Omitted in JSON when unknown (a row indexed before end-line tracking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Access level (`public`/`crate`/`private`/`protected`/`local`) when the language
    /// expresses one. Omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// The type's own member rather than its instances' — a class method, a
    /// `static` member (D55). Omitted when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub singleton: bool,
    #[serde(rename = "repo")]
    pub repo_identity: String,
    pub source: Source,
    /// Raw additive score — the ranking key and the `--explain` breakdown source.
    /// Not serialized: JSON exposes the normalized `confidence` instead.
    #[serde(skip)]
    pub score: f64,
    /// Normalized match confidence in `[0, 1]`, filled before output (see
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
    /// Set when the checkout this hit came from is still being indexed: a
    /// better match may be in a file not read yet (D52).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warming: Option<Warming>,
}

/// How far the index behind an answer has got, on a checkout still being
/// indexed. The same shape trekr reports for the same question.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Warming {
    /// Files of this checkout the index holds.
    pub read: i64,
    /// Source files the tree spans, when a pass has enumerated it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<i64>,
    /// No process is indexing the checkout any more, nor will this one leave
    /// a warm behind: the gap stays until a search or `rq --index` fills it.
    pub interrupted: bool,
    /// What a live pass over the checkout is doing (`reading`, or
    /// `finishing` while `read` stands still), and for how many seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_secs: Option<i64>,
    pub hint: String,
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
/// `current` (a checkout id, if any) boosts results from the checkout you're
/// in; `only` (if any) restricts results to that checkout, so a search inside
/// a repo answers about *that* tree rather than leaking others you've indexed;
/// `ctx` carries where the search is asked from: the branch's changed files
/// and an optional anchor position.
pub(crate) fn search(
    store: &Store,
    query: &str,
    current: Option<i64>,
    only: Option<Checkout>,
    ctx: &Context,
    limit: usize,
) -> crate::store::Result<Matches> {
    let run = |q: &str, typo: bool| search_query(store, q, current, only, ctx, limit, typo);
    if !query.contains('.') {
        return run(query, true);
    }
    // `.` reads as a scope first (`Foo.bar`). When no scope answers, fall back
    // to its older meaning as a one-char wildcard (`find.controller`) — a
    // literal match — before guessing at typos.
    let m = run(query, false)?;
    if found(&m) {
        return Ok(m);
    }
    // `Foo.new` where Foo declares no constructor (inherited, or implicit):
    // answer with Foo itself — one hop from the real one — rather than letting
    // the typo retry pick a similarly named class's constructor.
    if let (leaf, Some(owner)) = score::parse_qualified(query)
        && leaf.eq_ignore_ascii_case("new")
    {
        let mut m = run(owner, false)?;
        constructor_owner(&mut m.hits);
        m.total = m.hits.len();
        if found(&m) {
            return Ok(m);
        }
    }
    let glob = run(&query.replace('.', "?"), false)?;
    if found(&glob) {
        return Ok(glob);
    }
    run(query, true)
}

/// Keep only exact class/struct matches from an owner search, flagged so
/// `--explain` and confidence show they stand in for a constructor.
fn constructor_owner(hits: &mut Vec<Hit>) {
    hits.retain(|h| {
        matches!(h.kind.as_str(), "class" | "struct")
            && h.features.iter().any(|f| f.name == "exact")
    });
    for h in hits {
        h.features.push(score::Feature {
            name: "constructor_owner",
            value: 0.0,
        });
    }
}

/// Asked for by name alone, a type nothing encloses is the one its name means
/// when the same name is also nested somewhere: `Account` the model over
/// `Billing::Providers::Account`, which `depth` leaves tied (DECISIONS D43).
/// Relative to the results, not a bonus for any top-level type: in Rust or Go
/// every type is top-level, and a flat bonus ranked `Copy` over `copy`.
/// Types only, since a function's parent is its owner, not a namespace.
fn top_level(hits: &mut [Hit]) {
    let is_type = |h: &Hit| score::is_primary_kind(&h.kind);
    let nested: std::collections::HashSet<String> = hits
        .iter()
        .filter(|h| is_type(h) && h.parent.is_some())
        .map(|h| h.name.clone())
        .collect();
    for h in hits.iter_mut() {
        if is_type(h) && h.parent.is_none() && nested.contains(&h.name) {
            let value = TOP_LEVEL_BONUS * score::match_quality(&h.features);
            h.features.push(score::Feature {
                name: "top_level",
                value,
            });
            h.score += value;
        }
    }
}

/// Sized below a kind step (15): past that, Ruby's reopened builtins (`class
/// String` in a core extension) outrank the real namespaced classes.
const TOP_LEVEL_BONUS: f64 = 10.0;

/// Anything above zero is worth showing; below it, only a wrong answer.
fn found(m: &Matches) -> bool {
    m.hits.iter().any(|h| h.score > 0.0)
}

fn search_query(
    store: &Store,
    query: &str,
    current: Option<i64>,
    only: Option<Checkout>,
    ctx: &Context,
    limit: usize,
    // retry as a near miss when nothing matches outright
    typo: bool,
) -> crate::store::Result<Matches> {
    // Recall keys off the leaf name only — a `Foo::Bar` qualifier targets the
    // parent during scoring, and the store indexes `name`, not `parent`. A
    // wildcard query's exact and prefix layers key off its literal chars; the
    // name index reads the glob itself, and scoring matches it precisely.
    let (leaf, qualifier) = score::parse_qualified(query);
    let stripped;
    let recall = if score::has_wildcard(leaf) {
        stripped = score::strip_wildcards(leaf);
        stripped.as_str()
    } else {
        leaf
    };
    let trace_on = crate::trace::enabled();
    let t = std::time::Instant::now();
    // Checkout scope: outside `--all-repos`, a search inside a repo returns
    // only that checkout's definitions — never another indexed tree's.
    let mut candidates = store.search_candidates(
        recall,
        CANDIDATE_LIMIT,
        score::has_wildcard(leaf),
        only,
        current,
        &Probe::new(leaf),
    )?;
    // `Foo.new` runs a constructor the store knows by another name
    if qualifier.is_some() && leaf.eq_ignore_ascii_case("new") {
        for name in crate::lang::constructors() {
            candidates.extend(store.search_candidates(
                name,
                CANDIDATE_LIMIT,
                false,
                only,
                current,
                &Probe::new(name),
            )?);
        }
    }
    let n_candidates = candidates.len();
    let t_recall = t.elapsed();
    let t = std::time::Instant::now();
    let now = now_unix();
    let reach = ctx.reach_splits(&candidates);

    // Borrows rather than consumes, so the retry below can re-rank the same
    // candidates instead of asking the store for them again.
    let rank = |candidates: &[SymbolRow], near_miss: bool| -> Vec<Hit> {
        candidates
            .iter()
            .filter_map(|c| {
                // prefer whichever recency signal is more recent: a recent edit
                // (mtime, stored in nanoseconds — convert to seconds) or a
                // recent commit (git_ts, seconds)
                let recency = recency_boost(c.git_ts.max(c.mtime.map(|n| n / 1_000_000_000)), now);
                let boosts = ctx.boosts(c, recency, reach);
                rank_one(query, c, current, boosts, near_miss)
            })
            .collect()
    };
    let mut hits = rank(&candidates, false);
    // A near miss competes with fuzzy matches, never with a literal one: once
    // the query spelled a name outright (exact, prefix, glob) the typo reading
    // is moot. Short of that, every near miss joins and ranks on its score:
    // `sleect` means `Select` more surely than `IsolatedExecutionState` merely
    // holding those letters in order, and a name that holds them better still
    // ranks above the typo. Side features scale with match quality, so a weak
    // guess can't ride in on them. Candidates that already matched keep the
    // score they had.
    let literal = hits
        .iter()
        .any(|h| h.score > 0.0 && score::is_literal(&h.features));
    if typo && qualifier.is_none() && !literal {
        let near: Vec<SymbolRow> = candidates
            .iter()
            .filter(|c| score::near_miss_possible(query, &c.name))
            .cloned()
            .collect();
        hits.extend(
            rank(&near, true)
                .into_iter()
                .filter(|h| h.features.iter().any(|f| f.name == "typo")),
        );
    }
    // A named scope that answered nothing may itself be the slip. That is a
    // guess about the question, not the name, so it stays a last resort:
    // only when nothing above zero turned up.
    if typo && qualifier.is_some() && hits.iter().all(|h| h.score <= 0.0) {
        // Only candidates that could *be* a near miss are worth re-scoring —
        // the alternative is paying the whole name-match chain a second time
        // for ten thousand rows to serve a few hundred.
        let retried = rank(&candidates, true);
        // keep the first pass's answer if the retry turns up nothing
        if !retried.is_empty() {
            hits = retried;
        }
    }
    if qualifier.is_none() {
        top_level(&mut hits);
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

/// Where the *unqualified* name lives, when a query named a scope and nothing
/// in that scope matched — so a caller can be told "not there, but here"
/// instead of a bare "no match".
///
/// Only a definition of that very name counts: a fuzzy or near-miss hit for the
/// leaf is some other name, and reporting it as where "that name" lives sent
/// callers to an unrelated symbol.
///
/// Returns `None` when the query named no scope, or when the bare name isn't
/// defined anywhere either (an ordinary miss). Runs only on the miss path.
pub(crate) fn scope_miss_owner(
    store: &Store,
    query: &str,
    current: Option<i64>,
    only: Option<Checkout>,
    ctx: &Context,
) -> Option<String> {
    let (leaf, qualifier) = score::parse_qualified(query);
    qualifier?;
    // a few, not one: an exact name under a test path can rank below a prefix
    let bare = search(store, leaf, current, only, ctx, 10).ok()?;
    let hit = bare
        .hits
        .iter()
        .find(|h| h.features.iter().any(|f| f.name == "exact"))?;
    Some(match &hit.parent {
        Some(parent) => format!("{parent} ({}:{})", hit.file, hit.line),
        None => format!("{}:{}", hit.file, hit.line),
    })
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

/// Text that any file holding an exact or prefix match for `query` contains: the
/// leaf name (`Foo::bar` → `bar`), which indexing uses to parse those files
/// first. Not a filter — a separator-less `parsefile` still matches `parse_file`
/// exactly, and those files are indexed later, not skipped. `None` for a
/// wildcard, whose literal text the user split with gaps.
pub(crate) fn literal_leaf(query: &str) -> Option<&str> {
    let (leaf, _) = score::parse_qualified(query);
    (!leaf.is_empty() && !score::has_wildcard(leaf)).then_some(leaf)
}

/// Layer 4: scan `tree` live (no index required) and return ranked hits.
/// Results are treated as the current repo, so the current-repo boost applies.
/// `skip` names already-indexed files to ignore, and `deadline` bounds the scan
/// — both empty/`None` for an unbounded scan of a never-indexed directory. When
/// `prefilter` is set, only files containing the query (substring) are parsed —
/// fast for exact/prefix/substring queries, but blind to fuzzy abbreviations, so
/// callers retry with `prefilter = false` if a filtered scan finds nothing.
pub(crate) fn live_search(
    tree: &crate::index::LiveTree,
    query: &str,
    limit: usize,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
    prefilter: bool,
    ctx: &Context,
) -> LiveScan {
    let needle = prefilter.then_some(query.as_bytes());
    let identity = &tree.identity;
    let root_str = tree.root.to_string_lossy();
    let files = crate::index::scan(tree, skip, deadline, needle);
    let scanned = files.len();
    let rows: Vec<SymbolRow> = files
        .into_iter()
        .flat_map(|fs| {
            let generated = fs.generated;
            fs.symbols
                .unwrap_or_default()
                .into_iter()
                .map(move |s| (s, generated))
        })
        .map(|(s, generated)| SymbolRow::live(s, identity, &root_str, generated))
        .collect();
    let reach = ctx.reach_splits(&rows);
    let rank = |q: &str| -> Vec<Hit> {
        rows.iter()
            .filter_map(|row| {
                rank_one(
                    q,
                    row,
                    Some(crate::store::LIVE),
                    ctx.boosts(row, 0.0, reach),
                    false,
                )
            })
            .map(|hit| Hit {
                source: Source::Live,
                ..hit
            })
            .collect()
    };
    let mut hits = rank(query);
    // `Foo.new` with no constructor in Foo: the class answers, as in `search`
    if hits.is_empty()
        && let (leaf, Some(owner)) = score::parse_qualified(query)
        && leaf.eq_ignore_ascii_case("new")
    {
        hits = rank(owner);
        constructor_owner(&mut hits);
    }
    sort_and_truncate(&mut hits, limit);
    LiveScan {
        hits,
        files: scanned,
    }
}

/// What a live scan found, and how many files it parsed to find it.
pub(crate) struct LiveScan {
    pub hits: Vec<Hit>,
    pub files: usize,
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

/// Scope gate for a qualified query (`Foo::Bar#baz`). The scorer keeps only
/// results whose scope matched: by the recorded parent (`parent`), or by the
/// file's path (`path_scope`), which is how a Go package, a Python module or a
/// Rust `mod` file names a scope no parent records. Of those, only the results
/// the scope matched best stay — the same way the relevance gate drops fuzzy
/// near-matches beside an exact hit. A parent is a stronger claim than a
/// directory, and a scope's own directory than one of its subdirectories: in
/// `gin`, `gin.Default` is the one in `gin.go`, not `binding.Default`.
pub(crate) fn apply_scope_gate(query: &str, hits: &mut Vec<Hit>) {
    if score::parse_qualified(query).1.is_none() {
        return; // unqualified query — nothing to gate on
    }
    let scoped = |h: &Hit| -> f64 {
        h.features
            .iter()
            .filter(|f| matches!(f.name, "parent" | "path_scope" | "scope_typo"))
            .map(|f| f.value)
            .sum()
    };
    let best = hits.iter().map(scoped).fold(f64::NEG_INFINITY, f64::max);
    hits.retain(|h| scoped(h) >= best);
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

/// Fold repeat declarations of one name into a single result.
///
/// Ruby reopens a module across files and Rust spreads `impl` blocks the same
/// way, so a name can be declared a dozen times: `rq Middleware` spent its whole
/// first page on four declarations of `ActiveRecord::Middleware`, one of them a
/// six-line autoload stub. Four rows, one answer — the opposite of what a
/// navigation tool is for.
///
/// The survivor is the best-ranked declaration, which `extent` and `stub`
/// already bias toward the one with a real body; the rest are recorded on it so
/// nothing is lost. An unqualified name folds only within one file: two
/// top-level `Widget`s are the same reopened class in Ruby but two unrelated
/// types in Rust, and showing one row too many is the cheaper mistake. In one
/// file they are one definition declared more than once: TypeScript's overload
/// signatures, Rust's `#[cfg]` alternatives.
fn collapse_declarations(hits: &mut Vec<Hit>) {
    use std::collections::HashMap;
    // a class method and an instance method of one name are two definitions
    type Key = (String, Option<String>, Option<String>, String, String, bool);
    let mut first: HashMap<Key, usize> = HashMap::new();
    let mut folded: Vec<Vec<String>> = vec![Vec::new(); hits.len()];
    let mut keep = Vec::with_capacity(hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let key = (
            hit.root.clone().unwrap_or_default(),
            hit.parent.clone(),
            hit.parent.is_none().then(|| hit.file.clone()),
            hit.name.clone(),
            hit.kind.clone(),
            hit.singleton,
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
    current: Option<i64>,
    boosts: Boosts,
    near_miss: bool,
) -> Option<Hit> {
    // Borrowed, so a candidate that doesn't score costs nothing; the clones
    // below happen only for the few that become results.
    let scored = score::score(query, c, current, boosts, near_miss)?;
    Some(Hit {
        name: c.name.clone(),
        kind: c.kind.clone(),
        language: c.language.clone(),
        file: c.file.clone(),
        root: Some(c.root.clone()),
        line: c.line,
        end_line: c.end_line,
        parent: c.parent.clone(),
        visibility: c.visibility.clone(),
        singleton: c.singleton,
        repo_identity: c.repo_identity.clone(),
        source: Source::Index,
        score: scored.total,
        confidence: 0.0, // filled from the final result set before output
        features: scored.features,
        signature: None,
        body: None,
        declarations: 1,
        also_in: Vec::new(),
        total: 0, // filled from the final result set before output
        explain: None,
        warming: None,
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
        // two kinds, so the pair in one file stays two results rather than
        // folding into one declaration
        let hit = |file: &str, line: i64| Hit {
            name: "Transaction".into(),
            kind: if line == 9 { "module" } else { "class" }.into(),
            language: "ruby".into(),
            file: file.into(),
            root: None,
            line,
            end_line: None,
            parent: None,
            visibility: None,
            singleton: false,
            score: 1.0,
            confidence: 0.5,
            signature: None,
            repo_identity: "local:/tmp/x".into(),
            source: Source::Index,
            features: Vec::new(),
            body: None,
            declarations: 1,
            also_in: Vec::new(),
            total: 0,
            explain: None,
            warming: None,
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
            stub: false,
            singleton: false,
        }
    }

    fn store_with(symbols: &[Symbol]) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        store
            .replace_file_symbols(repo, "app/x.rb", "ruby", None, "h", symbols)
            .unwrap();
        store
    }

    #[test]
    fn recall_keeps_everything_the_scorer_would_accept() {
        // One shape per way `score` can match: letters in order, a primary
        // definition named only by its file, a transposition, a glob, a scope,
        // a sigil, non-ASCII.
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        let files = [
            (
                "lib/connection_pool.rb",
                vec![
                    sym("ConnectionPool", Kind::Class),
                    sym("Base", Kind::Module),
                    sym("checkout", Kind::Method),
                ],
            ),
            (
                "lib/widget_controller.rb",
                vec![sym("Widgets", Kind::Class), sym("render", Kind::Method)],
            ),
            (
                "lib/user.rb",
                vec![
                    sym("User", Kind::Class),
                    sym("_private_user", Kind::Method),
                    sym("Überuser", Kind::Class),
                    sym("select", Kind::Method),
                    sym("Scheduler", Kind::Class),
                    sym("consolidate_all", Kind::Method),
                ],
            ),
        ];
        for (file, syms) in &files {
            store
                .replace_file_symbols(repo, file, "ruby", None, "h", syms)
                .unwrap();
        }
        let boosts = Boosts::default;
        // every row in the store: what the scorer would accept with no recall
        let every: Vec<SymbolRow> = files
            .iter()
            .flat_map(|(file, _)| store.symbols_in_file(repo.id, file).unwrap())
            .collect();
        let queries = [
            "conpool",
            "usr",
            "sleect",
            "widgetcontroller",
            "con*pool",
            "Base::chckout",
            "Über",
            "privuser",
            "zzz",
        ];
        for query in queries {
            let leaf = score::parse_qualified(query).0;
            let recall = score::strip_wildcards(leaf);
            // forced, so fuzzy recall runs even where a prefix matched
            let indexed = store
                .search_candidates(&recall, 1000, true, None, None, &Probe::new(leaf))
                .unwrap();
            for near_miss in [false, true] {
                let accepted =
                    |cands: &[SymbolRow]| -> std::collections::BTreeSet<(String, String)> {
                        cands
                            .iter()
                            .filter(|c| score::score(query, c, None, boosts(), near_miss).is_some())
                            .map(|c| (c.file.clone(), c.name.clone()))
                            .collect()
                    };
                assert_eq!(
                    accepted(&every),
                    accepted(&indexed),
                    "{query} (near miss: {near_miss})"
                );
            }
        }
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    /// Two repos, each with its own symbol, so scoping can be exercised.
    fn store_two_repos() -> (Store, Checkout, Checkout) {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/a"));
        let b = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/b"));
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
            Some(a.id),
            Some(a),
            &Context::default(),
            10,
        )
        .unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].repo_identity, "local:/tmp/a");
        // no scope (--all-repos): both repos' Widgets surface
        let all = search(&store, "Widget", Some(a.id), None, &Context::default(), 10).unwrap();
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
            Some(a.id),
            Some(a),
            &Context::default(),
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
        let hits = search(&store, "user", None, None, &Context::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn the_other_case_convention_surfaces_beside_a_literal_match() {
        // a literal `widget_handle` (and a prefix match) used to hide the type
        let store = store_with(&[
            sym("widget_handle", Kind::Method),
            sym("widget_handles", Kind::Method),
            sym("WidgetHandle", Kind::Struct),
            sym("WidgetHandler", Kind::Struct),
        ]);
        let hits = search(&store, "widget_handle", None, None, &Context::default(), 10).unwrap();
        assert_eq!(names(&hits)[..2], ["widget_handle", "WidgetHandle"]);
        // and the other way round
        let hits = search(&store, "WidgetHandle", None, None, &Context::default(), 10).unwrap();
        assert_eq!(names(&hits)[..2], ["WidgetHandle", "widget_handle"]);
    }

    #[test]
    fn abbreviation_finds_the_intended_symbol() {
        let store = store_with(&[
            sym("RefundProcessor", Kind::Class),
            sym("Refund", Kind::Class),
            sym("Payment", Kind::Class),
        ]);
        let hits = search(&store, "refundproc", None, None, &Context::default(), 10).unwrap();
        assert_eq!(hits[0].name, "RefundProcessor");
        assert!(!names(&hits).contains(&"Payment"));
    }

    #[test]
    fn short_fuzzy_query_still_resolves() {
        let store = store_with(&[sym("User", Kind::Class), sym("Account", Kind::Class)]);
        let hits = search(&store, "usr", None, None, &Context::default(), 10).unwrap();
        assert_eq!(hits[0].name, "User");
    }

    #[test]
    fn a_transposition_joins_beside_a_name_holding_the_letters_in_order() {
        // `test_sub_regions` holds `tets_br` in order and outscores the one-swap
        // reading, which must still be offered rather than dropped
        let store = store_with(&[
            sym("test_sub_regions", Kind::Method),
            sym("test_br", Kind::Method),
        ]);
        let hits = search(&store, "tets_br", None, None, &Context::default(), 10).unwrap();
        assert!(names(&hits).contains(&"test_br"), "{:?}", names(&hits));
    }

    #[test]
    fn no_match_returns_empty() {
        let store = store_with(&[sym("User", Kind::Class)]);
        let hits = search(&store, "zzzzz", None, None, &Context::default(), 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn merge_dedups_by_location_keeping_higher_score() {
        let mk = |name: &str, score: f64| Hit {
            name: name.into(),
            kind: "class".into(),
            language: "ruby".into(),
            file: "a.rb".into(),
            root: None,
            line: 1,
            end_line: Some(1),
            parent: None,
            visibility: None,
            singleton: false,
            repo_identity: "r".into(),
            source: Source::Index,
            score,
            confidence: 0.0,
            features: vec![],
            signature: None,
            body: None,
            declarations: 1,
            also_in: Vec::new(),
            total: 0,
            explain: None,
            warming: None,
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
    fn a_test_beside_the_code_ranks_below_the_code() {
        // a longer body and an earlier row would otherwise carry the test
        let mut in_tests = nested("widget_totals", Kind::Function, "tests");
        in_tests.end_line = 30;
        let store = store_with(&[in_tests, sym("WidgetTotals", Kind::Struct)]);
        let hits = search(&store, "widtot", None, None, &Context::default(), 10).unwrap();
        assert_eq!(names(&hits), ["WidgetTotals", "widget_totals"]);
    }

    #[test]
    fn qualified_query_ranks_the_definition_in_the_named_scope() {
        let store = store_with(&[
            nested("Config", Kind::Class, "Baz"),
            nested("Config", Kind::Class, "Foo"),
            nested("Config", Kind::Class, "Qux"),
        ]);
        // `Foo::Config` should surface the Config nested under Foo first
        let hits = search(&store, "Foo::Config", None, None, &Context::default(), 10).unwrap();
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
            &Context::default(),
            10,
        )
        .unwrap();
        assert_eq!(m[0].kind, "method");
        assert_eq!(m[0].parent.as_deref(), Some("Bar::Worker"));
        // a module qualified by its enclosing scope
        let w = search(&store, "Bar::Worker", None, None, &Context::default(), 10).unwrap();
        assert_eq!(w[0].name, "Worker");
        assert_eq!(w[0].parent.as_deref(), Some("Bar"));
    }

    fn hit(name: &str, in_scope: bool) -> Hit {
        Hit {
            name: name.into(),
            kind: "method".into(),
            language: "ruby".into(),
            file: "a.rb".into(),
            root: None,
            line: 1,
            end_line: Some(1),
            parent: None,
            visibility: None,
            singleton: false,
            repo_identity: "r".into(),
            source: Source::Index,
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
            warming: None,
        }
    }

    #[test]
    fn unqualified_declarations_fold_only_within_a_file() {
        let at = |file: &str, line: i64, score: f64| Hit {
            file: file.into(),
            line,
            score,
            ..hit("encode", false)
        };
        // overload signatures and their implementation, in one file
        let mut hits = vec![at("a.ts", 9, 3.0), at("a.ts", 2, 2.0), at("a.ts", 5, 1.0)];
        collapse_declarations(&mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].line, hits[0].declarations), (9, 3));
        assert_eq!(hits[0].also_in, ["a.ts:2", "a.ts:5"]);
        // the same top-level name in two files is two definitions
        let mut hits = vec![at("a.ts", 9, 2.0), at("b.ts", 9, 1.0)];
        collapse_declarations(&mut hits);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn scope_gate_keeps_only_in_scope_results_when_some_match() {
        let mut hits = vec![hit("baz", true), hit("baz", false), hit("baz", false)];
        apply_scope_gate("Foo::Bar#baz", &mut hits);
        assert_eq!(hits.len(), 1, "out-of-scope baz methods are dropped");
        assert!(hits[0].features.iter().any(|f| f.name == "parent"));
    }

    #[test]
    fn scope_gate_keeps_the_best_scoped_results() {
        let scoped = |parts: &[(&'static str, f64)]| Hit {
            features: parts
                .iter()
                .map(|&(name, value)| Feature { name, value })
                .collect(),
            ..hit("Default", false)
        };
        // a directory is a weaker claim than a parent
        let mut hits = vec![
            scoped(&[("parent", 180.0)]),
            scoped(&[("path_scope", 30.0)]),
        ];
        apply_scope_gate("gin.Default", &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].features[0].name, "parent");
        // and a scope's own directory a stronger one than its subdirectory
        let mut hits = vec![
            scoped(&[("path_scope", 15.0)]),
            scoped(&[("path_scope", 30.0)]),
        ];
        apply_scope_gate("gin.Default", &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].features[0].value, 30.0);
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

    fn row(name: &str, parent: Option<&str>, file: &str, span: (i64, i64)) -> SymbolRow {
        SymbolRow {
            name: name.into(),
            kind: "method".into(),
            language: "ruby".into(),
            file: file.into(),
            line: span.0,
            end_line: Some(span.1),
            parent: parent.map(str::to_string),
            repository_id: 1,
            repo_identity: "local:/tmp/x".into(),
            checkout_id: 1,
            root: "/tmp/x".into(),
            mtime: None,
            git_ts: None,
            visibility: None,
            stub: false,
            singleton: false,
            generated: false,
        }
    }

    fn anchor_in_widget(line: i64) -> Anchor {
        // module Shop; class Widget; def persist ... end; end; end
        let defs = [
            row("Shop", None, "app/shop/widget.rb", (1, 20)),
            row("Widget", Some("Shop"), "app/shop/widget.rb", (2, 19)),
            row(
                "persist",
                Some("Shop::Widget"),
                "app/shop/widget.rb",
                (5, 9),
            ),
        ];
        Anchor::new(
            "local:/tmp/x".into(),
            "app/shop/widget.rb".into(),
            line,
            &defs,
        )
    }

    #[test]
    fn an_anchor_takes_the_innermost_enclosing_definition() {
        assert_eq!(anchor_in_widget(7).scope, ["shop", "widget", "persist"]);
        assert_eq!(anchor_in_widget(3).scope, ["shop", "widget"]);
        assert!(anchor_in_widget(40).scope.is_empty(), "outside everything");
    }

    #[test]
    fn enclosing_grades_by_how_much_scope_is_shared() {
        let a = anchor_in_widget(7);
        let cases = [
            (Some("Shop::Widget"), 2.0 * ENCLOSING_STEP),
            (Some("Shop"), ENCLOSING_STEP),
            (Some("Shop::Gadget"), 0.0),
            (Some("Other::Shop::Widget"), 0.0),
            (Some("Shop::Widget::persist::Inner::Deeper"), 0.0),
            (None, 0.0),
        ];
        for (parent, want) in cases {
            assert_eq!(a.enclosing(parent), want, "{parent:?}");
        }
    }

    #[test]
    fn proximity_decays_with_directory_distance() {
        let a = anchor_in_widget(7);
        let id = "local:/tmp/x";
        let cases = [
            (id, "app/shop/widget.rb", SAME_FILE_BOOST),
            (id, "app/shop/gadget.rb", SAME_DIR_BOOST),
            (id, "app/shop/parts/gear.rb", SAME_DIR_BOOST / 2.0),
            (id, "app/other/gear.rb", SAME_DIR_BOOST / 4.0),
            (id, "lib/deep/down/gear.rb", 0.0),
            ("local:/tmp/elsewhere", "app/shop/widget.rb", 0.0),
        ];
        for (identity, file, want) in cases {
            assert_eq!(a.proximity(identity, file), want, "{file}");
        }
    }

    #[test]
    fn an_anchor_prefers_the_definition_in_its_enclosing_class() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        for (file, parent) in [
            ("app/a/gadget.rb", "Gadget"),
            ("app/shop/widget.rb", "Shop::Widget"),
            ("lib/z/zeta.rb", "Zeta"),
        ] {
            store
                .replace_file_symbols(
                    repo,
                    file,
                    "ruby",
                    None,
                    "h",
                    &[nested("save", Kind::Method, parent)],
                )
                .unwrap();
        }
        let plain = search(&store, "save", None, None, &Context::default(), 10).unwrap();
        assert_eq!(
            plain[0].parent.as_deref(),
            Some("Gadget"),
            "unanchored: by path"
        );
        assert!(plain.iter().all(|h| {
            h.features
                .iter()
                .all(|f| !matches!(f.name, "enclosing" | "proximity"))
        }));

        // asked from a file elsewhere, but inside Shop::Widget: scope wins
        let mut anchor = anchor_in_widget(7);
        anchor.file = "app/b/other.rb".into();
        let ctx = Context {
            anchor: Some(anchor),
            ..Context::default()
        };
        let hits = search(&store, "save", None, None, &ctx, 10).unwrap();
        assert_eq!(hits[0].parent.as_deref(), Some("Shop::Widget"));
        assert!(hits[0].features.iter().any(|f| f.name == "enclosing"));
    }

    #[test]
    fn a_bare_type_name_prefers_the_top_level_definition() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        // the nested ones have the bigger bodies, which used to decide it
        let account = |parent: Option<&str>, end_line| Symbol {
            parent: parent.map(str::to_string),
            end_line,
            ..sym("Account", Kind::Class)
        };
        for (file, parent, end_line) in [
            (
                "app/services/billing/providers/account.rb",
                Some("Billing::Providers"),
                12,
            ),
            ("app/models/account.rb", None, 6),
            ("lib/admin/account.rb", Some("Admin"), 9),
        ] {
            store
                .replace_file_symbols(repo, file, "ruby", None, "h", &[account(parent, end_line)])
                .unwrap();
        }

        let plain = search(&store, "Account", None, None, &Context::default(), 10).unwrap();
        assert_eq!(plain[0].file, "app/models/account.rb");
        assert!(plain[0].features.iter().any(|f| f.name == "top_level"));

        // a named scope asks for a nested one, and gets it
        let scoped = search(
            &store,
            "Admin::Account",
            None,
            None,
            &Context::default(),
            10,
        )
        .unwrap();
        assert_eq!(scoped[0].parent.as_deref(), Some("Admin"));
        assert!(
            scoped
                .iter()
                .all(|h| h.features.iter().all(|f| f.name != "top_level"))
        );

        // so does a query asked from inside the namespace
        let defs = [
            row("Billing", None, "app/services/billing/charge.rb", (1, 20)),
            row(
                "Providers",
                Some("Billing"),
                "app/services/billing/charge.rb",
                (2, 19),
            ),
        ];
        let ctx = Context {
            anchor: Some(Anchor::new(
                "local:/tmp/x".into(),
                "app/services/billing/charge.rb".into(),
                5,
                &defs,
            )),
            ..Context::default()
        };
        let anchored = search(&store, "Account", None, None, &ctx, 10).unwrap();
        assert_eq!(anchored[0].parent.as_deref(), Some("Billing::Providers"));
    }

    #[test]
    fn an_anchor_prefers_definitions_its_language_can_reach() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        for (file, language) in [
            ("a/range.rs", "rust"),
            ("b/range.rb", "ruby"),
            ("c/range.ts", "typescript"),
        ] {
            store
                .replace_file_symbols(
                    repo,
                    file,
                    language,
                    None,
                    "h",
                    &[Symbol {
                        language: language.into(),
                        ..sym("range", Kind::Function)
                    }],
                )
                .unwrap();
        }
        let plain = search(&store, "range", None, None, &Context::default(), 10).unwrap();
        assert_eq!(plain[0].file, "a/range.rs", "unanchored: by path");
        assert!(
            plain
                .iter()
                .all(|h| h.features.iter().all(|f| f.name != "reachable"))
        );

        // JS reaches TS, and nothing else
        let ctx = |file: &str| Context {
            anchor: Some(Anchor::new(
                "local:/tmp/elsewhere".into(),
                file.into(),
                1,
                &[],
            )),
            ..Context::default()
        };
        let hits = search(&store, "range", None, None, &ctx("web/page.jsx"), 10).unwrap();
        assert_eq!(hits[0].file, "c/range.ts");
        let reached: Vec<_> = hits
            .iter()
            .filter(|h| h.features.iter().any(|f| f.name == "reachable"))
            .map(|h| h.file.as_str())
            .collect();
        assert_eq!(reached, ["c/range.ts"]);

        let hits = search(&store, "range", None, None, &ctx("lib/task.rb"), 10).unwrap();
        assert_eq!(hits[0].file, "b/range.rb");

        // an anchor no plugin reads leaves the order alone
        let hits = search(&store, "range", None, None, &ctx("docs/notes.md"), 10).unwrap();
        assert_eq!(hits[0].file, "a/range.rs");
    }

    #[test]
    fn reach_adds_nothing_when_every_candidate_is_reachable() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        for (file, kind) in [("a/parse.ts", Kind::Function), ("b/parse.js", Kind::Method)] {
            let language = if file.ends_with(".ts") {
                "typescript"
            } else {
                "javascript"
            };
            store
                .replace_file_symbols(
                    repo,
                    file,
                    language,
                    None,
                    "h",
                    &[Symbol {
                        language: language.into(),
                        ..sym("parse", kind)
                    }],
                )
                .unwrap();
        }
        let ctx = Context {
            anchor: Some(Anchor::new(
                "local:/tmp/elsewhere".into(),
                "web/page.tsx".into(),
                1,
                &[],
            )),
            ..Context::default()
        };
        let plain = search(&store, "parse", None, None, &Context::default(), 10).unwrap();
        let anchored = search(&store, "parse", None, None, &ctx, 10).unwrap();
        // the same order and the same scores, so the same confidence
        let scores = |hits: &[Hit]| -> Vec<(String, f64)> {
            hits.iter().map(|h| (h.file.clone(), h.score)).collect()
        };
        assert_eq!(scores(&anchored), scores(&plain));
        assert!(
            anchored
                .iter()
                .all(|h| h.features.iter().all(|f| f.name != "reachable"))
        );
    }

    #[test]
    fn an_anchor_in_an_example_app_waives_the_penalty_there() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        for file in ["examples/blog/utils/range.rb", "lib/core/range.rb"] {
            store
                .replace_file_symbols(repo, file, "ruby", None, "h", &[sym("range", Kind::Method)])
                .unwrap();
        }
        let plain = search(&store, "range", None, None, &Context::default(), 10).unwrap();
        assert_eq!(plain[0].file, "lib/core/range.rb");

        let ctx = |file: &str| Context {
            anchor: Some(Anchor::new("local:/tmp/x".into(), file.into(), 1, &[])),
            ..Context::default()
        };
        let inside = search(
            &store,
            "range",
            None,
            None,
            &ctx("examples/blog/app/page.rb"),
            10,
        )
        .unwrap();
        assert_eq!(inside[0].file, "examples/blog/utils/range.rb");
        assert!(inside[0].features.iter().all(|f| f.name != "example_path"));

        // from library code the example keeps its penalty
        let outside = search(&store, "range", None, None, &ctx("lib/app/page.rb"), 10).unwrap();
        assert_eq!(outside[0].file, "lib/core/range.rb");
        let example = outside
            .iter()
            .find(|h| h.file.starts_with("examples/"))
            .unwrap();
        assert!(example.features.iter().any(|f| f.name == "example_path"));
    }

    #[test]
    fn the_anchor_tree_is_its_own_app_and_waives_nothing_private_elsewhere() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store.test_checkout(&crate::core::RepoIdentity::local("/tmp/x"));
        for (file, visibility) in [
            ("examples/blog/utils/range.rb", Some("public")),
            ("examples/blog/utils/helpers.rb", Some("private")),
            ("examples/blog/app/helpers.rb", Some("private")),
            ("examples/shop/range.rb", Some("public")),
        ] {
            let name = match file {
                "examples/blog/utils/helpers.rb" => "range_helper",
                "examples/blog/app/helpers.rb" => "range_sibling",
                _ => "range",
            };
            store
                .replace_file_symbols(
                    repo,
                    file,
                    "ruby",
                    None,
                    "h",
                    &[Symbol {
                        visibility,
                        ..sym(name, Kind::Method)
                    }],
                )
                .unwrap();
        }
        let ctx = Context {
            anchor: Some(Anchor::new(
                "local:/tmp/x".into(),
                "examples/blog/app/page.rb".into(),
                1,
                &[],
            )),
            ..Context::default()
        };
        let penalized = |query: &str, file: &str| {
            search(&store, query, None, None, &ctx, 10)
                .unwrap()
                .iter()
                .find(|h| h.file == file)
                .unwrap()
                .features
                .iter()
                .any(|f| f.name == "example_path")
        };
        assert!(!penalized("range", "examples/blog/utils/range.rb"));
        // another app under the same `examples/` is no context for this one
        assert!(penalized("range", "examples/shop/range.rb"));
        // a private helper in another file is nothing the anchor can call
        assert!(penalized("range_helper", "examples/blog/utils/helpers.rb"));
        // ...but beside the anchor it may be: Go's unexported names are package-wide
        assert!(!penalized("range_sibling", "examples/blog/app/helpers.rb"));
    }

    #[test]
    fn branch_boost_lifts_an_active_file() {
        let store = store_with(&[sym("User", Kind::Class)]); // lives in app/x.rb
        let ctx = Context {
            active: ActiveFiles::new(["app/x.rb".to_string()]),
            anchor: None,
        };
        let hits = search(&store, "user", None, None, &ctx, 10).unwrap();
        assert!(hits[0].features.iter().any(|f| f.name == "branch"));
    }
}
