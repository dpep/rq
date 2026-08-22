//! Ranking: a simple, explainable, additive score.
//!
//! Match quality dominates (exact > prefix > abbreviation/subsequence), with
//! smaller additive features layered on (kind, current-repo). Every component
//! is recorded so `--explain` can show why a result ranked where it did.

use crate::store::SymbolRow;

/// One named contribution to a score.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Feature {
    pub name: &'static str,
    pub value: f64,
}

/// A scored candidate: total plus the per-feature breakdown.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scored {
    pub total: f64,
    pub features: Vec<Feature>,
}

/// Absolute match quality in [0,1] — how good the match *itself* is, independent
/// of ranking boosts. The dominant term in [`confidence`]. Exact is certain; a
/// prefix nearly so; a fuzzy/abbreviation match scales with its alignment; a
/// path-only match (name didn't match) is weak.
pub(crate) fn match_quality(features: &[Feature]) -> f64 {
    for f in features {
        match f.name {
            "exact" => return 1.0,
            "prefix" => return 0.9,
            "wildcard" => return 0.7,
            // the fuzzy feature value is the alignment score (capped ~600)
            "fuzzy" => return (0.30 + 0.35 * (f.value / 600.0)).clamp(0.30, 0.65),
            _ => {}
        }
    }
    0.25 // path-only, or no name match at all
}

/// Presented confidence in [0,1]: match quality scaled by *dominance* — how much
/// this result leads the strongest other one. A unique strong match → ~1.0;
/// evenly-tied candidates → ~0.5 (rq isn't sure which you mean); a lone weak
/// fuzzy match stays low. `best_other` is the top score among the other results
/// (`None` when this is the only one). Rounded to two decimals.
pub(crate) fn confidence(score: f64, quality: f64, best_other: Option<f64>) -> f64 {
    let lead = match best_other {
        None => 1.0,
        Some(_) if score <= 0.0 => 0.5,
        // a modest score lead already signals dominance, so ramp steeply: an
        // even tie sits at 0.5, and pulling ~15%+ ahead saturates to 1.0.
        Some(other) => (0.5 + 3.0 * (score - other) / score).clamp(0.0, 1.0),
    };
    ((quality * lead) * 100.0).round() / 100.0
}

/// Dynamic, context-dependent boosts computed by [`crate::search`] (which owns
/// the time math and store lookups). Kept out of the pure match scoring so each
/// signal can be added without threading more parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Boosts {
    /// Behavioral signal: results chosen before for this query.
    pub learned: f64,
    /// Git/filesystem signal: symbols in recently-modified files.
    pub recency: f64,
    /// Branch signal: symbols in files you're changing on this branch (or their
    /// directory neighbors) — where you're most likely working.
    pub branch: f64,
}

/// Score `cand` for `query`. Returns `None` when the candidate doesn't match at
/// all (not even as a subsequence), filtering FTS trigram noise.
///
/// `boosts` carries the dynamic signals (behavioral, recency) computed by
/// [`crate::search`], which owns the time math.
pub(crate) fn score(
    query: &str,
    cand: &SymbolRow,
    current_repo_id: Option<i64>,
    boosts: Boosts,
    // Allow a bounded typo match — only set on a retry that found nothing.
    near_miss: bool,
) -> Option<Scored> {
    // A qualified query (`Foo::Bar`, `Foo::Bar#baz`) names an enclosing scope:
    // match the leaf against the name, and reward a matching `parent` below.
    let (leaf, qualifier) = parse_qualified(query);
    let q = lower(leaf);
    let name_lower = lower(&cand.name);

    let mut features = Vec::new();

    // Match quality on the symbol name — the dominant term.
    let wildcard = has_wildcard(&q);
    let name_matched = if wildcard {
        // explicit glob: literal segments separated by the user's `*`/`?` gaps
        if let Some(s) = wildcard_score(&q, &cand.name) {
            features.push(Feature {
                name: "wildcard",
                value: s.min(600.0),
            });
            true
        } else {
            false
        }
    } else if name_lower == q {
        features.push(Feature {
            name: "exact",
            value: 1000.0,
        });
        // Typing a capital is a deliberate signal: `Symbol` means the type, not
        // the `symbol` method that happens to share the name case-insensitively.
        // Only when the query carries case, though — an all-lowercase query is
        // how people type casually, and reading intent into it would demote
        // `User` for `user`.
        if leaf != q && cand.name == leaf {
            features.push(Feature {
                name: "case",
                value: CASE_MATCH,
            });
        }
        true
    } else if alnum_eq(&name_lower, &q) {
        // The same identifier with the separators left out — `parsefile` for
        // `parse_file`, `usercontroller` for `user-controller`. That's a
        // deliberate abbreviation of an exact match, not a fuzzy one, and it
        // must not be decided by which candidate has the bigger body.
        features.push(Feature {
            name: "exact",
            value: 1000.0,
        });
        features.push(Feature {
            name: "separators",
            value: -SEPARATOR_PENALTY,
        });
        true
    } else if name_lower.starts_with(q.as_ref()) {
        // shorter remaining tail ranks higher
        let tail = cand.name.chars().count().saturating_sub(q.chars().count());
        features.push(Feature {
            name: "prefix",
            value: 700.0 - (tail as f64).min(100.0),
        });
        true
    } else if let Some(s) = subsequence_score(&q, &cand.name) {
        // Same unmatched-tail penalty the prefix branch applies: the alignment
        // score counts matched query chars, so a candidate's extra characters
        // were free and `Validaton` scored `ValidationError` exactly as well as
        // `Validations`. Capped, and gentle enough that an abbreviation still
        // reaches a long name it barely covers (`apc` → `ApplicationController`).
        let tail = cand.name.chars().count().saturating_sub(q.chars().count());
        features.push(Feature {
            name: "fuzzy",
            value: s.min(600.0) - (tail as f64).min(100.0),
        });
        true
    } else if let Some(d) = near_miss
        .then(|| near_miss_distance(&q, &name_lower))
        .flatten()
    {
        // Last resort, and only on a query that found nothing any other way. A
        // subsequence match forgives typing too *little* and nothing else, so
        // the two commonest typos — swapping adjacent letters, doubling one —
        // were hard misses: `connectoin_pool` returned nothing while
        // `cnnection_pool` worked fine.
        features.push(Feature {
            name: "typo",
            value: NEAR_MISS_SCORE - NEAR_MISS_STEP * d as f64,
        });
        true
    } else {
        false
    };

    // Layer 3: path / filename matching (same glob/fuzzy split as the name).
    let stem = path_stem(&cand.file);
    let path_match = if wildcard {
        wildcard_score(&q, stem)
    } else {
        subsequence_score(&q, stem)
    };
    if name_matched {
        // a file named after the query reinforces a name match (small bonus)
        if let Some(ps) = path_match {
            features.push(Feature {
                name: "path",
                value: (ps * 0.2).min(50.0),
            });
        }
    } else {
        // no name match: a path hit only surfaces a file's primary definitions
        match path_match {
            Some(ps)
                if matches!(
                    cand.kind.as_str(),
                    "class" | "module" | "struct" | "enum" | "trait"
                ) =>
            {
                features.push(Feature {
                    name: "path",
                    value: (ps * 0.6).min(300.0),
                });
            }
            _ => return None,
        }
    }

    // Visibility — a definition the language marks private/protected is less
    // likely the navigation target than public API. A small penalty (never a
    // filter): it breaks ties among comparable matches without overriding
    // match quality, and unknown visibility (pre-v9 rows, or languages that
    // don't express one) carries no signal at all.
    if matches!(
        cand.visibility.as_deref(),
        Some("private") | Some("protected")
    ) {
        features.push(Feature {
            name: "private",
            value: -15.0,
        });
    }

    // Test/spec path — a fixture or a test double is rarely the definition you
    // meant, and on a large repo they collide head-on with the real ones: Rails
    // has 64 definitions of `save`, half of them fake models under `test/`,
    // every one scoring exactly what the real `ActiveRecord::Persistence#save`
    // scores. Without this the tie falls through to alphabetical path order.
    // A penalty, never a filter — when the test *is* what you're after, every
    // candidate takes it equally and the order among them is unchanged.
    if in_test_path(&cand.file) {
        features.push(Feature {
            name: "test_path",
            value: -TEST_PATH_PENALTY,
        });
    }

    // Body extent — a definition with a real body is more often the one you
    // meant than a one-line stub, an `alias_method`, or an autoload
    // declaration. Log-scaled and capped: 3 lines versus 30 is a real
    // difference, 300 versus 3000 isn't.
    if let Some(end) = cand.end_line {
        let span = (end - cand.line + 1).max(1) as f64;
        if span > 1.0 {
            features.push(Feature {
                // not "body": that's the field `--show` fills with source, and
                // a feature sharing the name reads as the same thing in JSON
                name: "extent",
                value: (span.ln() * BODY_WEIGHT).min(MAX_BODY_BONUS),
            });
        }
    }

    // Namespace depth — among equally-good matches the shallower definition is
    // usually the canonical one: `ActiveRecord::Persistence#save` over
    // `ActiveRecord::Middleware::DatabaseSelector::Resolver::Session#save`.
    // Sized as a tiebreaker and deliberately below every real signal — on a
    // large repo the whole visible result set routinely scores identically, and
    // this decides it by something better than alphabetical path order.
    // A language whose plugin records no parent reads as depth 0 and takes no
    // penalty; that only matters when one query spans several languages.
    let depth = cand
        .parent
        .as_deref()
        .map_or(0, segment_count)
        .saturating_sub(FREE_DEPTH);
    if depth > 0 {
        features.push(Feature {
            name: "depth",
            value: -(DEPTH_PENALTY * depth as f64).min(MAX_DEPTH_PENALTY),
        });
    }

    // Kind weight — definitions you navigate to most sit slightly higher.
    // Top-level types rank alongside classes; methods/functions stay neutral.
    let kind = match cand.kind.as_str() {
        "class" | "struct" | "trait" => 15.0,
        "module" | "enum" => 12.0,
        _ => 0.0,
    };
    if kind != 0.0 {
        features.push(Feature {
            name: "kind",
            value: kind,
        });
    }

    // Qualifier — the user named an enclosing scope (`Foo::Bar`, `Foo#bar`).
    // A candidate outside that scope is not an answer to the question asked, so
    // it drops out entirely rather than ranking on its name alone. Scoring it
    // anyway meant a made-up owner returned the same definition as the real one
    // at the same confidence 1.0, with only a missing `--explain` feature to
    // tell them apart — the strongest signal of certainty on exactly the query
    // whose constraint had been discarded.
    //
    // A candidate with no recorded parent drops out too, and that's correct:
    // `Foo::Bar` asserts Bar sits inside Foo, and a top-level Bar does not.
    if let Some(qual) = qualifier {
        let b = parent_boost(qual, cand.parent.as_deref())?;
        features.push(Feature {
            name: "parent",
            value: b,
        });
    }

    // Current-repo boost — the repo you're in dominates other repos.
    if let Some(cur) = current_repo_id
        && cur == cand.repository_id
    {
        features.push(Feature {
            name: "current_repo",
            value: 200.0,
        });
    }

    // Learned boost — results you've chosen before for this query rank higher.
    if boosts.learned > 0.0 {
        features.push(Feature {
            name: "learned",
            value: boosts.learned,
        });
    }

    // Recency boost — symbols in recently-modified files rank higher.
    if boosts.recency > 0.0 {
        features.push(Feature {
            name: "recency",
            value: boosts.recency,
        });
    }

    // Branch boost — symbols in files you're changing on this branch (or nearby).
    if boosts.branch > 0.0 {
        features.push(Feature {
            name: "branch",
            value: boosts.branch,
        });
    }

    let total = features.iter().map(|f| f.value).sum();
    Some(Scored { total, features })
}

/// Largest gap (chars skipped) allowed between two matched query chars that land
/// *mid-word* (not at a word boundary). Boundary jumps are how abbreviations work
/// and stay unlimited; off-boundary we tolerate a couple of skipped chars — a
/// consonant run like `ctrl`→`Controller` (the `c`→`t` skips `on`), or a typo —
/// but no more. A bigger gap (the `s` in `employeescontroller` reaching past
/// `XYZ`, three chars) is coincidence, not a match.
const MAX_NONBOUNDARY_GAP: usize = 2;

/// Reward for matching the case the query was typed in, when the query carries
/// any. It has to outweigh the spread in `recency` (0-120), or which of two
/// same-named symbols wins would come down to whichever file was touched more
/// recently — that made ranking depend on file mtimes, so a fresh checkout
/// ranked differently from a stale one.
const CASE_MATCH: f64 = 150.0;

/// Ceiling for a near-miss match. Below the weakest real fuzzy match, so a
/// candidate that genuinely contains the query always wins; this exists to turn
/// a hard miss into a ranked guess, not to compete.
const NEAR_MISS_SCORE: f64 = 120.0;

/// Charged per edit between the query and the name.
const NEAR_MISS_STEP: f64 = 40.0;

/// How wrong a near miss may be. One edit is a slip; beyond two the "did you
/// mean" stops being a guess and starts being a different word.
const MAX_NEAR_MISS: usize = 2;

/// What a separator-insensitive exact match gives up to a literal one, so
/// `parse_file` still wins when the query spells it out.
const SEPARATOR_PENALTY: f64 = 50.0;

/// Per natural-log line of a definition's body. Small and log-scaled — this
/// separates an implementation from a stub, not a big file from a small one.
const BODY_WEIGHT: f64 = 10.0;

/// Cap on the body bonus, so a huge class can't outweigh match quality.
const MAX_BODY_BONUS: f64 = 50.0;

/// Levels of scope that cost nothing. Ordinary namespacing has to be free:
/// Ruby and Rust nest library code two deep where JavaScript and Go leave it at
/// the top level, so charging per level made this a penalty on *languages* —
/// `ActionController::Metal#dispatch` lost to eight compiled `.esm.js` bundles
/// whose classes happen to be top-level. Only nesting past the normal range
/// says anything about how canonical a definition is.
const FREE_DEPTH: usize = 2;

/// Per level of enclosing scope beyond [`FREE_DEPTH`]. Small: this exists to
/// order results that are otherwise identical, not to outweigh how well a name
/// matched.
const DEPTH_PENALTY: f64 = 15.0;

/// Cap on the depth penalty. Past a few levels everything is equally
/// un-canonical, and without a cap a deeply nested match would start losing to
/// signals it should never lose to.
const MAX_DEPTH_PENALTY: f64 = 60.0;

/// How far a definition under a test/spec path drops. Sized to clear the gap
/// between an exact match (1000) and a prefix one (~700): below that, a
/// three-line private helper in a test still beat the obvious answer, because
/// no other feature can cross that cliff. A name that only lives in tests is
/// unaffected — every candidate takes the same penalty.
const TEST_PATH_PENALTY: f64 = 400.0;

/// Penalty per skipped char between two matched chars. Strong enough that a
/// closer match wins over a farther one — so the query's trailing chars don't
/// straggle to a distant word boundary (the `r` of a query landing in `.rb`
/// instead of `controller`) — but not so strong it lets a scattered mid-word
/// alignment outrank a boundary-aligned abbreviation.
const GAP_PENALTY: f64 = 3.0;

/// One way `query` lines up against `name`: its score and the matched indices.
struct Alignment {
    score: f64,
    positions: Vec<usize>,
}

/// Find the **best** alignment of `query` as a subsequence of `name`, maximizing
/// matches at word boundaries (camelCase / underscore) and contiguous runs while
/// penalizing gaps. `None` if `query` isn't a subsequence. Handles abbreviations
/// (`refproc → RefundProcessor`, `usr → User`, `paymnt → Payments`) and ignores
/// separators in the query, so a snake_case query matches CamelCase
/// (`widget_controller → WidgetsController`).
///
/// This is a small dynamic program rather than a greedy left-to-right scan: greedy
/// takes the *first* candidate for each query char, which mis-aligns (matching the
/// `e` in `xxxe_employee` instead of the contiguous `employee`, or letting a
/// trailing char straggle to a far position). The DP considers every placement and
/// keeps the highest-scoring one, so the score and the highlight reflect the match
/// a human would read.
fn align(query: &str, name: &str) -> Option<Alignment> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if q.is_empty() {
        return None;
    }
    // cheap gate: most candidates aren't even a subsequence of the query, so
    // reject them with one linear scan before any of the DP allocations below
    let mut qi = 0;
    for c in name.chars() {
        if qi < q.len() && c.to_ascii_lowercase() == q[qi] {
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let boundary = boundaries(&chars);
    // prefix count of word boundaries, so we can ask "is a whole word skipped
    // between j and i?" in O(1) — the "only span adjacent words" rule
    let mut bnd_prefix = vec![0usize; n + 1];
    for i in 0..n {
        bnd_prefix[i + 1] = bnd_prefix[i] + boundary[i] as usize;
    }

    // table[qi][i] = best (score, backpointer) for aligning q[0..=qi] with q[qi]
    // landing on name position `i`; `None` if q[qi] can't end there. The
    // backpointer is the position where q[qi-1] matched (self for qi == 0).
    let mut table: Vec<Vec<Option<(f64, usize)>>> = vec![vec![None; n]; q.len()];

    for (i, &c) in lower.iter().enumerate() {
        if c == q[0] {
            let mut s = 10.0;
            if boundary[i] {
                s += 15.0;
            }
            if i == 0 {
                s += 20.0; // anchored at the very start
            }
            table[0][i] = Some((s, i));
        }
    }

    for qi in 1..q.len() {
        for i in qi..n {
            if lower[i] != q[qi] {
                continue;
            }
            let base = 10.0 + if boundary[i] { 15.0 } else { 0.0 };
            // a non-boundary char can only follow within MAX_NONBOUNDARY_GAP;
            // a boundary char may follow from the previous word (scan back further)
            let j_start = if boundary[i] {
                qi - 1
            } else {
                (qi - 1).max(i.saturating_sub(MAX_NONBOUNDARY_GAP + 1))
            };
            let mut best: Option<(f64, usize)> = None;
            let prev_row = &table[qi - 1];
            for (j, cell) in prev_row.iter().enumerate().take(i).skip(j_start) {
                let Some((pscore, _)) = cell else {
                    continue;
                };
                let trans = if j + 1 == i {
                    10.0 // contiguous run
                } else {
                    let gap = i - j - 1;
                    let crossed_word = bnd_prefix[i] - bnd_prefix[j + 1] > 0;
                    if boundary[i] {
                        // entering a new word: only the *adjacent* one — reject if
                        // a whole word boundary sits between j and i (a word skipped)
                        if crossed_word {
                            continue;
                        }
                    } else if gap > MAX_NONBOUNDARY_GAP || crossed_word {
                        // a mid-word target may follow only a small same-word gap (a
                        // dropped vowel). A larger gap, or one that crosses into a
                        // new word, is scatter — you enter a new word at its
                        // boundary, never mid-word (the `ees` of `employees`
                        // threading employee→b[e]fore→[s]tarting).
                        continue;
                    }
                    -(gap as f64) * GAP_PENALTY
                };
                let cand = pscore + trans;
                if best.is_none_or(|(b, _)| cand > b) {
                    best = Some((cand, j));
                }
            }
            if let Some((bscore, j)) = best {
                table[qi][i] = Some((bscore + base, j));
            }
        }
    }

    // best end position for the final query char, then backtrack to collect indices
    let last = q.len() - 1;
    let (mut pos, score) = (0..n)
        .filter_map(|i| table[last][i].map(|(s, _)| (i, s)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let mut positions = Vec::with_capacity(q.len());
    for qi in (0..q.len()).rev() {
        positions.push(pos);
        pos = table[qi][pos].expect("backtrack hits a filled cell").1;
    }
    positions.reverse();
    Some(Alignment {
        score: score.max(0.0),
        positions,
    })
}

/// The char indices in `name` that `query` matched, from the best alignment —
/// for highlighting *what* matched. Empty if `query` isn't a subsequence.
pub(crate) fn match_positions(query: &str, name: &str) -> Vec<usize> {
    // highlight what the *leaf* matched; a qualifier targets the parent, not the name
    let (leaf, _) = parse_qualified(query);
    if has_wildcard(leaf) {
        // a wildcard's gaps are deliberate, so highlight every literal as-is
        return glob_positions(leaf, name).unwrap_or_default();
    }
    let positions = align(leaf, name).map(|a| a.positions).unwrap_or_default();
    contiguous_highlight(positions, name)
}

/// Split a query into its leaf name and the optional enclosing scope the user
/// typed before it. The qualifier is everything before the last `::`/`#`
/// separator: `Foo::Bar` → (`Bar`, `Some("Foo")`), `Foo::Bar#baz` → (`baz`,
/// `Some("Foo::Bar")`), a plain `User` → (`User`, `None`). A leading or trailing
/// separator (`::Bar`, `Foo::`) is treated as an ordinary unqualified query.
pub(crate) fn parse_qualified(query: &str) -> (&str, Option<&str>) {
    let sep = query
        .rmatch_indices("::")
        .map(|(i, _)| (i, 2usize))
        .chain(query.rmatch_indices('#').map(|(i, _)| (i, 1usize)))
        .max_by_key(|&(i, _)| i);
    match sep {
        Some((i, len)) if i > 0 && i + len < query.len() => (&query[i + len..], Some(&query[..i])),
        _ => (query, None),
    }
}

/// Lowercased scope segments of a (possibly qualified) name, split on `::`/`#`.
/// How many scopes a qualified name has, without building them. `segments`
/// allocates a `String` per scope, which is fine for the one query but not for
/// every candidate on a query that recalls thousands.
fn segment_count(s: &str) -> usize {
    s.split("::")
        .flat_map(|p| p.split('#'))
        .filter(|p| !p.is_empty())
        .count()
}

fn segments(s: &str) -> Vec<String> {
    s.split("::")
        .flat_map(|p| p.split('#'))
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_lowercase())
        .collect()
}

/// Boost a candidate whose enclosing scope matches a query's qualifier. The
/// qualifier must match the *innermost* segments of the candidate's `parent`
/// (a suffix): `Foo::Bar` (qualifier `Foo`) rewards a `Bar` whose parent is
/// `Foo` or `App::Foo`, but not one nested under some other scope. More matched
/// segments are stronger evidence of intent, so the boost grows with them.
fn parent_boost(qualifier: &str, parent: Option<&str>) -> Option<f64> {
    let p = segments(parent?);
    let q = segments(qualifier);
    if q.is_empty() || q.len() > p.len() {
        return None;
    }
    let off = p.len() - q.len();
    (p[off..] == q[..]).then(|| (120.0 + 60.0 * q.len() as f64).min(300.0))
}

/// Trim a fuzzy match's highlight so it reads cleanly. We keep contiguous runs of
/// two or more matched chars, and a lone matched char only when it sits on a word
/// boundary (an acronym/abbreviation initial — the `U`/`C` of `UserController`).
/// Isolated mid-word matches — single lit letters with dark gaps on both sides —
/// are dropped even though they technically matched: they're visually noisy and
/// carry no navigational signal. Separate clumps each survive, so a vowel-dropped
/// abbreviation still lights both halves (`Paym`e`nt`s).
fn contiguous_highlight(positions: Vec<usize>, name: &str) -> Vec<usize> {
    if positions.is_empty() {
        return positions;
    }
    let boundary = boundaries(&name.chars().collect::<Vec<_>>());
    let mut out = Vec::with_capacity(positions.len());
    let mut i = 0;
    while i < positions.len() {
        // positions are strictly increasing; extend a run of adjacent indices
        let mut j = i;
        while j + 1 < positions.len() && positions[j + 1] == positions[j] + 1 {
            j += 1;
        }
        if j > i {
            out.extend_from_slice(&positions[i..=j]); // a clump of >= 2
        } else if boundary[positions[i]] {
            out.push(positions[i]); // a lone match, but a word-boundary initial
        }
        i = j + 1;
    }
    out
}

/// Score `query` as a subsequence of `name` (the best alignment's score), or
/// `None` if it isn't a subsequence.
fn subsequence_score(query: &str, name: &str) -> Option<f64> {
    align(query, name).map(|a| a.score)
}

/// Does `query` use wildcard syntax — `*` (any run), `?`/`.` (one char)? When it
/// does, matching switches from fuzzy subsequence to an explicit glob: literal
/// chars match *contiguously*, and the only gaps are the ones the user marked.
/// `find*controller` keeps `FindController` and `FindUserController` but, unlike
/// fuzzy, won't reach into a scattered `FxIxNxDxController`.
pub(crate) fn has_wildcard(query: &str) -> bool {
    query.contains(['*', '?', '.'])
}

/// A wildcard query's literal characters, metachars removed — used to seed the
/// store's candidate recall (which keys off literal trigrams) before the glob
/// does the precise matching. `find*controller` → `findcontroller`.
pub(crate) fn strip_wildcards(query: &str) -> String {
    query
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '.'))
        .collect()
}

/// One token of a compiled wildcard pattern.
enum Glob {
    Lit(char), // a literal (lowercased) char — matches itself
    Any,       // `?` / `.` — exactly one char
    Star,      // `*` — zero or more chars
}

/// Compile a wildcard query into glob tokens. The query's own separators
/// (`_`, `-`, …) are ignored, like the fuzzy matcher, so `emp_*_ctrl` and
/// `emp*ctrl` compile alike.
fn compile_glob(query: &str) -> Vec<Glob> {
    query
        .chars()
        .filter_map(|c| match c {
            '*' => Some(Glob::Star),
            '?' | '.' => Some(Glob::Any),
            c if c.is_alphanumeric() => Some(Glob::Lit(c.to_ascii_lowercase())),
            _ => None,
        })
        .collect()
}

/// Match a wildcard `query` against `name`, unanchored (the pattern may match any
/// substring — implicit `*` at both ends). Returns the indices the *literal*
/// chars matched (the highlight), or `None` if it doesn't match. Classic
/// two-pointer glob with `*` backtracking; literal positions are recorded and
/// rolled back on each backtrack.
fn glob_positions(query: &str, name: &str) -> Option<Vec<usize>> {
    let mut toks = vec![Glob::Star];
    toks.extend(compile_glob(query));
    toks.push(Glob::Star);

    let lower: Vec<char> = name.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut ti = 0;
    let mut ni = 0;
    let mut positions: Vec<usize> = Vec::new();
    // the last `*` to fall back to: (token index after it, name index, #positions)
    let mut star: Option<(usize, usize, usize)> = None;

    while ni < lower.len() {
        match toks.get(ti) {
            Some(Glob::Lit(c)) if lower[ni] == *c => {
                positions.push(ni);
                ti += 1;
                ni += 1;
            }
            Some(Glob::Any) => {
                ti += 1;
                ni += 1;
            }
            Some(Glob::Star) => {
                star = Some((ti + 1, ni, positions.len()));
                ti += 1;
            }
            // mismatch, or pattern ran out with chars left: extend the last star
            // by one char and retry from just after it; no star to fall back to
            // means no match
            _ => match star {
                Some((sti, sni, plen)) => {
                    ti = sti;
                    ni = sni + 1;
                    star = Some((sti, sni + 1, plen));
                    positions.truncate(plen);
                }
                None => return None,
            },
        }
    }
    while matches!(toks.get(ti), Some(Glob::Star)) {
        ti += 1;
    }
    (ti == toks.len()).then_some(positions)
}

/// Score a wildcard match from its literal positions — the same boundary /
/// contiguity / start signals as the fuzzy scorer, but no gap penalty: the gaps
/// are the `*`/`?` the user placed deliberately. `None` when it doesn't match,
/// or when nothing literal matched (an all-wildcard query like `*`).
fn wildcard_score(query: &str, name: &str) -> Option<f64> {
    let positions = glob_positions(query, name)?;
    if positions.is_empty() {
        return None;
    }
    let chars: Vec<char> = name.chars().collect();
    let boundary = boundaries(&chars);
    let mut score = 0.0;
    let mut prev: Option<usize> = None;
    for &i in &positions {
        score += 10.0;
        if boundary[i] {
            score += 15.0;
        }
        match prev {
            Some(p) if p + 1 == i => score += 10.0, // contiguous literal run
            None if i == 0 => score += 20.0,        // anchored at the very start
            _ => {}
        }
        prev = Some(i);
    }
    Some(score)
}

/// The filename stem of a repo-relative path: last segment, extension dropped.
/// `app/models/user.rb` → `user`.
pub(crate) fn path_stem(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rfind('.') {
        Some(i) if i > 0 => &base[..i],
        _ => base,
    }
}

/// Mark word-boundary positions: index 0, anything after `_`/non-alphanumeric,
/// and camelCase humps (lower→Upper, and the last cap of an ACRONYMWord run).
fn boundaries(chars: &[char]) -> Vec<bool> {
    let mut out = vec![false; chars.len()];
    for i in 0..chars.len() {
        let c = chars[i];
        out[i] = if i == 0 {
            true
        } else {
            let prev = chars[i - 1];
            // start of a word: after a separator, a lower→Upper hump, or the
            // tail cap of an acronym run (the `P` in `HTTPParser`)
            !prev.is_alphanumeric()
                || (c.is_uppercase() && prev.is_lowercase())
                || (c.is_uppercase()
                    && prev.is_uppercase()
                    && chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
        };
    }
    out
}

/// Damerau-Levenshtein distance between query and name, or `None` past
/// [`MAX_NEAR_MISS`] — "Damerau" meaning it counts a swap of two adjacent
/// characters as one edit rather than two, because that's what a typo is.
///
/// Bounded hard before doing any work: recall hands over thousands of
/// candidates, and comparing the query against all of them is only affordable
/// because a length difference alone rules out almost every one.
/// The cheap half of [`near_miss_distance`], so a retry can skip candidates
/// that could never be a near miss rather than scoring all of them: on Rails a
/// transposed query recalls ten thousand candidates and six hundred survive
/// this.
pub(crate) fn near_miss_possible(query: &str, name: &str) -> bool {
    let leaf = parse_qualified(query).0;
    let (qlen, nlen) = (leaf.chars().count(), name.chars().count());
    if qlen < 4 || qlen.abs_diff(nlen) > MAX_NEAR_MISS {
        return false;
    }
    let low = |c: char| c.to_ascii_lowercase();
    let mut qc = leaf.chars().map(low);
    let mut nc = name.chars().map(low);
    match (qc.next(), qc.next(), nc.next(), nc.next()) {
        (Some(q0), Some(q1), Some(n0), Some(n1)) => q0 == n0 || (q0 == n1 && q1 == n0),
        _ => false,
    }
}

fn near_miss_distance(q: &str, name: &str) -> Option<usize> {
    // Every gate here reads the strings directly. Collecting into `Vec<char>`
    // first cost two allocations per candidate across thousands of them, which
    // swamped the comparisons meant to avoid the work — a gate below an
    // allocation isn't a gate.
    let (qlen, nlen) = (q.chars().count(), name.chars().count());
    // a short query is all typo — one edit in three characters is a different
    // word, not a slip
    if qlen < 4 || qlen.abs_diff(nlen) > MAX_NEAR_MISS {
        return None;
    }
    // The first letter is the one people get right, so checking it (or its swap
    // with the second) discards almost every candidate for two comparisons.
    // Missing a first-character typo costs one query that stays a miss.
    let mut qc = q.chars();
    let mut nc = name.chars();
    let (q0, q1) = (qc.next()?, qc.next()?);
    let (n0, n1) = (nc.next()?, nc.next()?);
    if q0 != n0 && !(q0 == n1 && q1 == n0) {
        return None;
    }
    let (a, b): (Vec<char>, Vec<char>) = (q.chars().collect(), name.chars().collect());
    // three rows, allocated once: the inner loop runs over thousands of
    // candidates, and a per-row allocation dominated everything else
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut best = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            // the Damerau step: an adjacent swap costs one, not two
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
            best = best.min(cur[j]);
        }
        // every alignment on this row is already too far gone
        if best > MAX_NEAR_MISS {
            return None;
        }
        // rotate: cur becomes prev, prev becomes prev2, and the old prev2's
        // buffer is reused as the next cur (every cell is rewritten below)
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d > 0 && d <= MAX_NEAR_MISS).then_some(d)
}

/// Lowercase without allocating when there's nothing to change. Called once
/// per candidate for the name and — wastefully, since it's constant — once per
/// candidate for the query; most queries and most snake_case names are already
/// lowercase, so the copy was of something identical.
fn lower(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(s.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Are these the same identifier once separators are dropped? `_`, `-`, and
/// `.` are all word joiners across the languages rq indexes, and a query that
/// omits them is spelling the same name.
fn alnum_eq(a: &str, b: &str) -> bool {
    // Compared in lockstep rather than by building two squashed Strings: this
    // runs against every candidate, and on a query that recalls thousands the
    // allocations cost more than everything else in scoring put together.
    let mut sa = a.chars().filter(char::is_ascii_alphanumeric);
    let mut sb = b.chars().filter(char::is_ascii_alphanumeric);
    let mut any = false;
    loop {
        match (sa.next(), sb.next()) {
            (None, None) => return any,
            (Some(x), Some(y)) if x == y => any = true,
            _ => return false,
        }
    }
}

/// Does this repo-relative path look like test/spec code?
///
/// Directory names are matched as whole segments, and only suffix conventions
/// are read off the filename. A `test_*` prefix rule was tried and dropped: it
/// wrongly caught a pile of genuine library files (`active_support/test_case.rb`,
/// `action_view/test_case.rb` — public API people search for). Missing a stray
/// `test_foo.py` beside its source is the cheaper error, and the `tests/`
/// directory those normally live in is caught anyway.
fn in_test_path(file: &str) -> bool {
    let (dirs, name) = match file.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", file),
    };
    // whole segments only, so a library *about* testing (`.../testing/`) stays
    // unpenalized
    if dirs.split('/').any(|seg| {
        matches!(
            seg,
            "test"
                | "tests"
                | "spec"
                | "specs"
                | "__tests__"
                | "__mocks__"
                | "testdata"
                | "fixtures"
        )
    }) {
        return true;
    }
    if name == "conftest.py" {
        return true;
    }
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    // `foo_test.go`, `foo_spec.rb`, `foo.test.ts`, `foo.spec.tsx`
    stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_test_paths_without_catching_libraries_about_testing() {
        for p in [
            "actionpack/test/lib/controller/fake_models.rb",
            "spec/models/widget_spec.rb",
            "pkg/thing/thing_test.go",
            "src/__tests__/widget.ts",
            "src/widget.test.tsx",
            "tests/conftest.py",
            "internal/testdata/sample.go",
        ] {
            assert!(in_test_path(p), "should be a test path: {p}");
        }
        for p in [
            // public API that merely *mentions* testing — the case a `test_`
            // prefix rule got wrong
            "activesupport/lib/active_support/test_case.rb",
            "activesupport/lib/active_support/testing/assertions.rb",
            "activejob/lib/active_job/test_helper.rb",
            "src/search/score.rs",
            "lib/latest.rb",
        ] {
            assert!(!in_test_path(p), "should not be a test path: {p}");
        }
    }

    fn row(name: &str, kind: &str, repo: i64) -> SymbolRow {
        SymbolRow {
            name: name.into(),
            kind: kind.into(),
            language: "ruby".into(),
            file: "f.rb".into(),
            line: 1,
            end_line: Some(1),
            parent: None,
            repository_id: repo,
            repo_identity: "r".into(),
            mtime: None,
            git_ts: None,
            visibility: None,
        }
    }

    fn total(query: &str, name: &str) -> Option<f64> {
        score(
            query,
            &row(name, "class", 1),
            None,
            Boosts::default(),
            false,
        )
        .map(|s| s.total)
    }

    #[test]
    fn a_typo_prefers_the_tight_match_over_a_longer_superstring() {
        // `Validaton` is a subsequence of both; the alignment score only counts
        // matched query chars, so without a tail penalty the extra characters
        // of `ValidationError` cost it nothing and Rails answered with that.
        let tight = total("Validaton", "Validations").unwrap();
        let longer = total("Validaton", "ValidationError").unwrap();
        assert!(tight > longer, "{tight} > {longer}");
        // but an abbreviation must still reach a name it barely covers
        assert!(total("apc", "ApplicationController").is_some());
    }

    #[test]
    fn a_near_miss_catches_the_typos_a_subsequence_cannot() {
        let d = |q: &str, name: &str| near_miss_distance(q, name);
        // subsequence matching forgives typing too little and nothing else
        assert_eq!(d("connectoin_pool", "connection_pool"), Some(1)); // swap
        assert_eq!(d("connection_poool", "connection_pool"), Some(1)); // doubled
        assert_eq!(d("activerecrod", "activerecord"), Some(1));
        // too far gone to be a slip
        assert_eq!(d("connection_pool", "widget_factory"), None);
        // a short query is all typo — one edit in three chars is another word
        assert_eq!(d("cat", "car"), None);
        // an exact match isn't a near miss
        assert_eq!(d("widget", "widget"), None);
    }

    #[test]
    fn a_real_body_outranks_a_stub_of_the_same_name() {
        let span = |lines: i64| {
            let mut r = row("where", "method", 1);
            r.end_line = Some(r.line + lines - 1);
            score("where", &r, None, Boosts::default(), false)
                .unwrap()
                .total
        };
        // the Rails case: a 3-line stub and the 9-line implementation scored
        // identically, so alphabetical file order picked the stub
        assert!(span(9) > span(3), "a real body should outrank a stub");
        // log-scaled and capped — a huge class can't outweigh match quality
        assert!(span(4000) - span(40) < CASE_MATCH);
    }

    #[test]
    fn separators_left_out_still_read_as_an_exact_match() {
        // typing `parsefile` for `parse_file` is an abbreviation of an exact
        // match, not a fuzzy one — it must not lose to whichever similar name
        // happens to have more lines
        let exact = total("parsefile", "parse_file").unwrap();
        let plural = total("parsefile", "parse_files").unwrap();
        assert!(exact > plural, "{exact} > {plural}");
        // spelling it out in full still wins over leaving separators off
        assert!(total("parse_file", "parse_file").unwrap() > exact);
    }

    #[test]
    fn the_shallower_of_two_identical_matches_wins() {
        let nested = |parent: &str| {
            let mut r = row("save", "method", 1);
            r.parent = Some(parent.into());
            score("save", &r, None, Boosts::default(), false)
                .unwrap()
                .total
        };
        // the Rails case: both exact matches on the same name, and before this
        // the tie fell through to alphabetical file order
        let shallow = nested("ActiveRecord::Persistence");
        let deep = nested("ActiveRecord::Middleware::DatabaseSelector::Resolver::Session");
        assert!(shallow > deep, "{shallow} > {deep}");
        // ordinary namespacing is free, or this becomes a penalty on languages
        // that namespace at all: a two-deep Ruby method would lose to a
        // top-level JavaScript one for no reason but the language
        assert_eq!(nested("ActiveRecord::Persistence"), nested("Widget"));
        // small enough to stay a tiebreaker — a case match is worth more than
        // several levels of nesting
        assert!(shallow - deep < CASE_MATCH, "depth outweighs match quality");
    }

    #[test]
    fn source_outranks_an_identical_match_in_a_test() {
        let at = |file: &str| {
            let mut r = row("save", "method", 1);
            r.file = file.into();
            score("save", &r, None, Boosts::default(), false)
                .unwrap()
                .total
        };
        // the Rails case: identical exact matches, decided by path alone
        let lib = at("activerecord/lib/active_record/persistence.rb");
        let fixture = at("actionpack/test/lib/controller/fake_models.rb");
        assert!(lib > fixture, "{lib} > {fixture}");
        // still a match, not a filter — a name that only lives in tests is
        // penalized uniformly, so the ordering among those is untouched
        assert!(fixture > 0.0);
        assert_eq!(fixture, at("spec/models/widget_spec.rb"));
    }

    #[test]
    fn a_typed_capital_picks_the_matching_case() {
        // `Symbol` and `symbol` are both exact matches case-insensitively.
        // Which one wins used to fall through to recency, i.e. to file mtimes,
        // so a fresh checkout ranked differently from a stale one.
        let upper = total("Symbol", "Symbol").unwrap();
        let lower = total("Symbol", "symbol").unwrap();
        assert!(upper > lower, "{upper} > {lower}");
        // by enough to outweigh the whole recency range, or mtime decides again
        assert!(upper - lower > 120.0, "margin {} too small", upper - lower);
    }

    #[test]
    fn a_lowercase_query_stays_case_agnostic() {
        // Lowercase is how people type casually — reading intent into it would
        // demote `User` for `user`, so neither spelling is rewarded.
        assert_eq!(total("symbol", "symbol"), total("symbol", "Symbol"));
        assert_eq!(total("user", "User"), total("user", "user"));
    }

    #[test]
    fn private_ranks_below_public_on_an_equal_match() {
        let mut public = row("save", "method", 1);
        public.visibility = Some("public".into());
        let mut private = row("save", "method", 1);
        private.visibility = Some("private".into());
        let unknown = row("save", "method", 1); // pre-v9 row: no signal

        let pub_score = score("save", &public, None, Boosts::default(), false).unwrap();
        let priv_score = score("save", &private, None, Boosts::default(), false).unwrap();
        let unk_score = score("save", &unknown, None, Boosts::default(), false).unwrap();
        assert!(pub_score.total > priv_score.total);
        assert_eq!(
            pub_score.total, unk_score.total,
            "unknown carries no penalty"
        );
        // the penalty is a tiebreaker, never bigger than a match-quality step
        assert!(priv_score.total > 700.0, "still comfortably above a prefix");
    }

    #[test]
    fn exact_beats_prefix_beats_fuzzy() {
        let exact = total("user", "user").unwrap();
        let prefix = total("user", "users").unwrap();
        let fuzzy = total("usr", "user").unwrap();
        assert!(exact > prefix, "{exact} > {prefix}");
        assert!(prefix > fuzzy, "{prefix} > {fuzzy}");
    }

    #[test]
    fn abbreviations_match() {
        assert!(total("refundproc", "RefundProcessor").is_some());
        assert!(total("refproc", "RefundProcessor").is_some());
        assert!(total("paymnt", "Payments").is_some());
        assert!(total("perf", "perform").is_some());
        assert!(total("usr", "User").is_some());
        // a consonant run skipping a couple of chars (gap 2) still matches
        assert!(total("ctrl", "Controller").is_some());
    }

    #[test]
    fn rejects_scattered_midword_matches() {
        // the trailing `s` of the query landed past `XYZ` mid-word — coincidence,
        // not a match. The clean plural (boundary/contiguous `s`) still matches.
        assert!(total("employeescontroller", "EmployeeXYZsController").is_none());
        assert!(total("employeescontroller", "EmployeesController").is_some());
        // a single skipped char off-boundary is tolerated (looks like a typo)
        assert!(total("employescontroller", "EmployeesController").is_some());
    }

    #[test]
    fn match_positions_report_what_matched() {
        assert_eq!(match_positions("foo", "FooThing"), vec![0, 1, 2]);
        assert_eq!(match_positions("ft", "FooThing"), vec![0, 3]); // F, T
        // separator-insensitive: snake query highlights across CamelCase
        assert_eq!(match_positions("wc", "WidgetController"), vec![0, 6]); // W, C
        assert!(match_positions("xyz", "FooThing").is_empty());
    }

    #[test]
    fn prefers_the_contiguous_run_over_an_earlier_scattered_match() {
        // the bug: a greedy scan anchored on the first `e` (in `xxxe`) and lit up
        // a scattered match; the best alignment is the contiguous `employee`.
        assert_eq!(
            match_positions("employee", "xxxe_employee"),
            vec![5, 6, 7, 8, 9, 10, 11, 12]
        );
        // align to the `controller` word, not a stray earlier `c` in `calc`
        assert_eq!(
            match_positions("controller", "calc_controller"),
            (5..15).collect::<Vec<_>>()
        );
        // and to the camelCase humps across the whole name
        assert_eq!(
            match_positions("widgetcontroller", "WidgetController"),
            (0..16).collect::<Vec<_>>()
        );
    }

    #[test]
    fn matches_only_span_adjacent_words() {
        // a query char may jump to the *next* word but not skip a whole one
        assert_eq!(
            match_positions("employeescontroller", "employees_controller"),
            // employees (0-8) + controller (10-19); the `_` at 9 is skipped
            vec![
                0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19
            ]
        );
        // the trailing `s` would have to skip the `x` word to reach `syy` — reject
        assert!(subsequence_score("employees", "employee_x_syy").is_none());
        // skipping a whole middle word isn't a match either
        assert!(subsequence_score("rndsvc", "RefundProcessingService").is_none());
        // adjacent-word abbreviations still match
        assert!(subsequence_score("refproc", "RefundProcessor").is_some());
        assert!(subsequence_score("refprocsvc", "RefundProcessingService").is_some());
    }

    #[test]
    fn a_contiguous_match_beats_a_farther_boundary_jump() {
        // both `r`s are reachable; the closer contiguous one wins, so the query
        // doesn't straggle to a separated boundary `r` (e.g. a file extension)
        assert_eq!(match_positions("car", "car_r"), vec![0, 1, 2]);
    }

    #[test]
    fn acronyms_highlight_word_initials_across_adjacent_words() {
        // crossing word boundaries IS correct for an acronym — each query char
        // lands on a word start (`uc` → the U and C humps of UserController)
        assert_eq!(match_positions("uc", "UserController"), vec![0, 4]);
        assert_eq!(
            match_positions("abc", "alpha_bravo_charlie"),
            vec![0, 6, 12] // a, b, c — each a word initial
        );
        // but only *adjacent* words — skipping a whole word is not a match
        assert!(subsequence_score("payrollcontroller", "payroll_runs_controller").is_none());
        assert!(subsequence_score("apc", "alpha_bravo_charlie").is_none()); // alpha→charlie skips bravo
    }

    #[test]
    fn a_gap_cannot_cross_a_word_boundary_into_a_mid_word_char() {
        // the reported scatter: `employeescontroller` threaded its `ees` through
        // employee → b[e]fore → [s]tarting (small gaps crossing word boundaries
        // into mid-word chars). You enter a new word at its boundary, not mid-word.
        assert!(
            subsequence_score("employeescontroller", "employee_before_starting_controller")
                .is_none()
        );
        // the clean target still matches
        assert!(subsequence_score("employeescontroller", "employees_controller").is_some());
        // and within-word vowel drops still match (the gap stays in one word)
        assert!(subsequence_score("usr", "user").is_some());
        assert!(subsequence_score("cfg", "config").is_some());
    }

    #[test]
    fn a_contiguous_word_match_outranks_a_scattered_cross_word_one() {
        // `test` scatters across `the`+`settings` (jump + dropped vowel — the same
        // shape as a real abbreviation, so it still matches), but a clean
        // contiguous match must rank well above it. Ranking, not rejection, is the
        // defense against scatter.
        let contiguous = total("test", "test_helper").unwrap(); // prefix
        let scattered = total("test", "the_settings_store");
        if let Some(s) = scattered {
            assert!(contiguous > s, "contiguous {contiguous} > scattered {s}");
        }
    }

    #[test]
    fn score_and_positions_come_from_the_same_alignment() {
        // a match yields a score and exactly one highlight per query char
        assert!(subsequence_score("refproc", "RefundProcessor").is_some());
        assert_eq!(match_positions("refproc", "RefundProcessor").len(), 7);
        // a non-match yields neither
        assert!(subsequence_score("xyz", "RefundProcessor").is_none());
        assert!(match_positions("xyz", "RefundProcessor").is_empty());
    }

    #[test]
    fn highlights_are_ordered_in_bounds_and_correct_across_varied_inputs() {
        let cases = [
            ("usr", "UserService"),
            ("paymnt", "Payments"),
            ("wc", "WidgetController"),
            ("ctrl", "Controller"),
            ("gp", "get_post"),
            ("ab", "alpha_beta"),
            ("refproc", "RefundProcessor"),
            ("emp", "EmployeesController"),
            ("http", "HTTPParser"),
        ];
        for (q, name) in cases {
            let nchars: Vec<char> = name.chars().collect();
            let qchars: Vec<char> = q.chars().filter(|c| c.is_alphanumeric()).collect();
            let boundary = boundaries(&nchars);
            let pos = match_positions(q, name);
            assert!(
                pos.windows(2).all(|w| w[0] < w[1]),
                "strictly increasing: {q}/{name} {pos:?}"
            );
            // highlights are a subsequence of the query, each in bounds
            let mut qi = 0;
            for &p in &pos {
                assert!(p < nchars.len(), "in bounds: {q}/{name}");
                while qi < qchars.len() && !qchars[qi].eq_ignore_ascii_case(&nchars[p]) {
                    qi += 1;
                }
                assert!(
                    qi < qchars.len(),
                    "highlight maps to a query char: {q}/{name}"
                );
                qi += 1;
            }
            // every highlight is part of a clump (>= 2 adjacent) or a boundary initial
            for (idx, &p) in pos.iter().enumerate() {
                let clumped = (idx > 0 && pos[idx - 1] + 1 == p)
                    || (idx + 1 < pos.len() && p + 1 == pos[idx + 1]);
                assert!(
                    clumped || boundary[p],
                    "no isolated mid-word highlight: {q}/{name} at {p} {pos:?}"
                );
            }
        }
    }

    #[test]
    fn highlights_avoid_isolated_single_chars() {
        // a vowel-dropped abbreviation lights both clumps across the dark gap
        assert_eq!(
            match_positions("paymnt", "Payments"),
            vec![0, 1, 2, 3, 5, 6]
        );
        // the straggling `r` of `usr` (mid-word, gap before it) is dropped, not lit
        assert_eq!(match_positions("usr", "UserService"), vec![0, 1]);
        // boundary initial `C` stays; the contiguous `tr` stays; the lone `l` drops
        assert_eq!(match_positions("ctrl", "Controller"), vec![0, 3, 4]);
        // two scattered mid-word singles leave nothing to highlight
        assert!(match_positions("rp", "wrapper").is_empty());
        // a pure boundary acronym is all single chars, but each is a real initial
        assert_eq!(match_positions("uc", "UserController"), vec![0, 4]);
    }

    #[test]
    fn an_acronym_at_boundaries_outranks_a_mid_word_alignment() {
        // both letters on word boundaries (acronym) beats them landing mid-word
        let acronym = subsequence_score("wc", "WidgetController").unwrap();
        let midword = subsequence_score("wc", "switchcase").unwrap();
        assert!(acronym > midword, "{acronym} > {midword}");
    }

    #[test]
    fn a_far_path_straggler_never_outranks_a_prefix_match() {
        // "employees" can match the stem `employee_x_syy` only via a trailing `s`
        // straggling to a far word boundary — a weak match. The real target, where
        // "employees" is a prefix, dominates via the prefix layer.
        let mut straggler = row("Thing", "class", 1);
        straggler.file = "app/employee_x_syy.rb".into();
        let prefixed = row("EmployeesController", "class", 1);
        let pre = score("employees", &prefixed, None, Boosts::default(), false)
            .unwrap()
            .total;
        if let Some(s) = score("employees", &straggler, None, Boosts::default(), false) {
            assert!(pre > s.total, "prefix {pre} > path straggler {}", s.total);
        }
    }

    #[test]
    fn snake_case_query_matches_camelcase_name() {
        // typed a snake_case query, want the CamelCase class — even when the
        // class is plural and you forgot the `s`
        assert!(total("widget_controller", "WidgetsController").is_some());
        assert!(total("widget_controller", "WidgetController").is_some());
        // unrelated controller still doesn't match
        assert!(total("widget_controller", "AdminController").is_none());
    }

    #[test]
    fn wildcard_star_spans_an_explicit_gap() {
        // `*` bridges any run, so the scattered tail the fuzzy gate rejects is
        // exactly what an explicit star asks for
        assert!(total("find*controller", "FindController").is_some());
        assert!(total("find*controller", "FindUserController").is_some());
        assert!(total("find*controller", "FindUserAccountController").is_some());
        // but the literals must still appear contiguously — `controller` is a
        // literal, not an abbreviation
        assert!(total("find*ctrlr", "FindController").is_none());
        // and a name missing a literal segment doesn't match
        assert!(total("find*controller", "FindService").is_none());
    }

    #[test]
    fn wildcard_question_mark_matches_one_char() {
        // `?` and `.` each consume exactly one char
        assert!(total("find?controller", "FindXController").is_some());
        assert!(total("find.controller", "Find1Controller").is_some());
        // zero chars or two chars in the slot don't fit a single `?`
        assert!(total("find?controller", "FindController").is_none());
        assert!(total("find?controller", "FindXyController").is_none());
    }

    #[test]
    fn wildcard_highlights_only_the_literals() {
        // the gap chars aren't highlighted, only the literals the user typed
        assert_eq!(
            match_positions("find*er", "FindController"),
            vec![0, 1, 2, 3, 12, 13] // Find + er
        );
    }

    #[test]
    fn wildcard_prefers_boundary_aligned_matches() {
        // a star landing the second literal on a word boundary outranks one
        // landing it mid-word
        let boundary = total("a*b", "Alpha_Bravo").unwrap();
        let midword = total("a*b", "Alphabet").unwrap();
        assert!(boundary > midword, "{boundary} > {midword}");
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(total("xyz", "RefundProcessor").is_none());
        assert!(total("zzz", "User").is_none());
    }

    #[test]
    fn confidence_reflects_quality_and_dominance() {
        let exact = vec![Feature {
            name: "exact",
            value: 1000.0,
        }];
        let fuzzy = vec![Feature {
            name: "fuzzy",
            value: 300.0,
        }];
        // a unique exact match is fully confident
        assert_eq!(confidence(1000.0, match_quality(&exact), None), 1.0);
        // a lone fuzzy match is mid/low even though it's the only result
        let f = confidence(300.0, match_quality(&fuzzy), None);
        assert!(f > 0.3 && f < 0.65, "fuzzy confidence {f}");
        // three evenly-tied exacts: the leader isn't dominant → ~0.5, well below a
        // unique exact
        let tied = confidence(1000.0, match_quality(&exact), Some(1000.0));
        assert!(tied < 0.6, "tied exact confidence {tied}");
        // a clear leader (big gap to #2) stays near the top
        let dominant = confidence(1000.0, match_quality(&exact), Some(300.0));
        assert!(dominant > 0.9, "dominant confidence {dominant}");
    }

    #[test]
    fn parse_qualified_splits_on_scope_separators() {
        assert_eq!(parse_qualified("User"), ("User", None));
        assert_eq!(parse_qualified("Foo::Bar"), ("Bar", Some("Foo")));
        assert_eq!(parse_qualified("App::Foo::Bar"), ("Bar", Some("App::Foo")));
        // a `#` is the innermost separator (Ruby instance method)
        assert_eq!(parse_qualified("Foo::Bar#baz"), ("baz", Some("Foo::Bar")));
        // a leading or trailing separator is not a qualifier
        assert_eq!(parse_qualified("::Bar"), ("::Bar", None));
        assert_eq!(parse_qualified("Foo::"), ("Foo::", None));
    }

    #[test]
    fn parent_boost_matches_the_innermost_scopes() {
        // exact parent, and a qualifier naming only the immediate scope
        assert!(parent_boost("Foo", Some("Foo")).is_some());
        assert!(parent_boost("Foo", Some("App::Foo")).is_some());
        assert!(parent_boost("App::Foo", Some("App::Foo")).is_some());
        // more matched segments → a stronger boost
        let one = parent_boost("Foo", Some("App::Foo")).unwrap();
        let two = parent_boost("App::Foo", Some("App::Foo")).unwrap();
        assert!(two > one, "{two} > {one}");
        // the qualifier must be a suffix, not just any ancestor or sibling
        assert!(parent_boost("App", Some("App::Foo")).is_none());
        assert!(parent_boost("Foo", Some("Foo::Inner")).is_none());
        assert!(parent_boost("Foo", None).is_none());
    }

    #[test]
    fn a_named_scope_excludes_candidates_outside_it() {
        // two classes both named `Bar`; the qualifier picks the one inside `Foo`
        let in_foo = SymbolRow {
            parent: Some("Foo".into()),
            ..row("Bar", "class", 1)
        };
        let in_baz = SymbolRow {
            parent: Some("Baz".into()),
            ..row("Bar", "class", 1)
        };
        // the named scope is a constraint, not a preference: a `Bar` somewhere
        // else is not an answer to `Foo::Bar`. It used to merely rank lower,
        // which meant a made-up owner returned the real definition at full
        // confidence whenever the leaf name was unique.
        assert!(score("Foo::Bar", &in_foo, None, Boosts::default(), false).is_some());
        assert!(score("Foo::Bar", &in_baz, None, Boosts::default(), false).is_none());
        // nor does a top-level `Bar` answer `Foo::Bar` — no parent means not
        // inside anything, which is precisely what the query ruled out
        let top_level = row("Bar", "class", 1);
        assert!(score("Foo::Bar", &top_level, None, Boosts::default(), false).is_none());
        // unqualified, all three are candidates again
        assert!(score("Bar", &in_baz, None, Boosts::default(), false).is_some());
        assert!(score("Bar", &top_level, None, Boosts::default(), false).is_some());
        // a wrong leaf still doesn't match, qualifier or not
        assert!(score("Foo::Zzz", &in_foo, None, Boosts::default(), false).is_none());
    }

    #[test]
    fn boundary_alignment_outranks_scattered() {
        // "rp" aligned to Refund/Processor humps should beat an incidental match
        let aligned = total("rp", "RefundProcessor").unwrap();
        let scattered = total("rp", "wrapper").unwrap();
        assert!(aligned > scattered, "{aligned} > {scattered}");
    }

    #[test]
    fn path_only_match_surfaces_a_class_in_a_named_file() {
        // name "Invoice" doesn't match "billing", but the file does
        let mut cand = row("Invoice", "class", 1);
        cand.file = "app/models/billing.rb".into();
        let s = score("billing", &cand, None, Boosts::default(), false).expect("path match");
        assert!(s.features.iter().any(|f| f.name == "path"));

        // a method (not a primary definition) in the same file does NOT surface
        let mut method = row("compute", "method", 1);
        method.file = "app/models/billing.rb".into();
        assert!(score("billing", &method, None, Boosts::default(), false).is_none());
    }

    #[test]
    fn path_bonus_reinforces_a_name_match() {
        let mut named = row("User", "class", 1);
        named.file = "app/models/user.rb".into();
        let mut elsewhere = row("User", "class", 1);
        elsewhere.file = "app/lib/misc.rb".into();
        let with_path = score("user", &named, None, Boosts::default(), false)
            .unwrap()
            .total;
        let without = score("user", &elsewhere, None, Boosts::default(), false)
            .unwrap()
            .total;
        assert!(with_path > without, "{with_path} > {without}");
    }

    #[test]
    fn current_repo_boost_applies() {
        let cand = row("User", "class", 7);
        let in_repo = score("user", &cand, Some(7), Boosts::default(), false)
            .unwrap()
            .total;
        let out_repo = score("user", &cand, Some(99), Boosts::default(), false)
            .unwrap()
            .total;
        assert!(in_repo > out_repo);
        assert_eq!(in_repo - out_repo, 200.0);
    }

    #[test]
    fn learned_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default(), false)
            .unwrap()
            .total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                learned: 150.0,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(boosted.total - base, 150.0);
        assert!(boosted.features.iter().any(|f| f.name == "learned"));
    }

    #[test]
    fn recency_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default(), false)
            .unwrap()
            .total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                recency: 80.0,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(boosted.total - base, 80.0);
        assert!(boosted.features.iter().any(|f| f.name == "recency"));
    }

    #[test]
    fn branch_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default(), false)
            .unwrap()
            .total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                branch: 180.0,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(boosted.total - base, 180.0);
        assert!(boosted.features.iter().any(|f| f.name == "branch"));
    }
}
