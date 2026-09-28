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

impl Feature {
    /// The value `--explain` reports, in text and JSON alike: whole points,
    /// which is all a weight means to a reader. Ranking sums the unrounded
    /// values — rounding each term first turns near-ties into ties and moved
    /// 304 of 2372 recall top-10s.
    pub(crate) fn reported(&self) -> f64 {
        self.value.round() + 0.0 // `+ 0.0` turns -0 into 0
    }
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
    // a mistyped scope is as much a guess as a mistyped name
    if features.iter().any(|f| f.name == "scope_typo") {
        return 0.25;
    }
    // the right class, but where its constructor is inherited or implicit
    if features.iter().any(|f| f.name == "constructor_owner") {
        return 0.75;
    }
    for f in features {
        match f.name {
            "exact" | "constructor" => return 1.0,
            "prefix" => return 0.9,
            "wildcard" => return 0.7,
            // the fuzzy feature value is the alignment score (capped ~600)
            "fuzzy" | "typo" => return (0.30 + 0.35 * (f.value / 600.0)).clamp(0.30, 0.65),
            _ => {}
        }
    }
    0.25 // path-only, or no name match at all
}

/// Did the query spell the name outright — exactly, as a prefix, as a glob, or
/// as the constructor it names — rather than as scattered letters or a typo?
pub(crate) fn is_literal(features: &[Feature]) -> bool {
    features
        .iter()
        .any(|f| matches!(f.name, "exact" | "prefix" | "wildcard" | "constructor"))
}

/// How well the name alone answered the query — the fuzzy or near-miss
/// feature, before anything else is added. Zero for a path-only match.
pub(crate) fn name_evidence(features: &[Feature]) -> f64 {
    features
        .iter()
        .find(|f| matches!(f.name, "fuzzy" | "typo"))
        .map_or(0.0, |f| f.value)
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
    /// Git/filesystem signal: symbols in recently-modified files.
    pub recency: f64,
    /// Branch signal: symbols in files you're changing on this branch (or their
    /// directory neighbors) — where you're most likely working.
    pub branch: f64,
    /// Anchor signal: the candidate is defined in a scope that lexically
    /// encloses the position the query was asked from.
    pub enclosing: f64,
    /// Anchor signal: the candidate's file is the anchor's, or near it.
    pub proximity: f64,
    /// The candidate is in the anchor's own file.
    pub anchor_file: bool,
}

/// Score `cand` for `query`. Returns `None` when the candidate doesn't match at
/// all (not even as a subsequence).
///
/// `boosts` carries the dynamic signals (recency, branch) computed by
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
    let name_matched = if qualifier.is_some()
        && q == "new"
        && crate::lang::is_constructor(&cand.language, &cand.name)
    {
        // `Foo.new` runs the constructor, whatever the language names it
        features.push(Feature {
            name: "constructor",
            value: 1000.0,
        });
        true
    } else if wildcard {
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
    } else if joiners_eq(&name_lower, &q) {
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
    } else if let Some(value) = fuzzy_value(&q, &cand.name) {
        features.push(Feature {
            name: "fuzzy",
            value,
        });
        true
    } else if let Some(d) = near_miss
        .then(|| near_miss_distance(&q, &name_lower))
        .flatten()
    {
        // Only when nothing matched literally (the caller decides). A
        // subsequence match forgives typing too *little* and nothing else, so
        // the two commonest typos — swapping adjacent letters, doubling one —
        // were hard misses: `connectoin_pool` returned nothing while
        // `cnnection_pool` worked fine.
        features.push(Feature {
            name: "typo",
            value: near_miss_score(&q, &cand.name, d),
        });
        true
    } else {
        false
    };

    // What the definition looks like — its file, visibility, extent, depth and
    // kind — picks among names that answer the query about equally well, so it
    // counts in proportion to how surely the name answered it. At full weight it
    // outweighed the name itself on an approximate match (DECISIONS D24).
    let quality = match_quality(&features);

    // A capital typed into a query that the name only approximates says the
    // same thing it does on an exact match (`case` above): `Fraem` means the
    // type `Frame`, not the function `frame`. Graded by how many of the query's
    // cased letters the name agrees with, and scaled like the other features
    // an approximate match earns.
    if leaf != q {
        let matched = match features.first().map(|f| f.name) {
            Some("typo") => Some(common_subsequence(leaf, &cand.name)),
            Some("prefix" | "fuzzy") => Some(leaf.to_string()),
            _ => None,
        };
        if let Some(share) = matched.and_then(|m| case_agreement(&m, &cand.name)) {
            features.push(Feature {
                name: "case",
                value: CASE_MATCH * share * quality,
            });
        }
    }

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
                value: (ps * 0.2).min(50.0) * quality,
            });
        }
    } else {
        // no name match: a path hit only surfaces a file's primary definitions
        match path_match {
            Some(ps) if is_primary_kind(&cand.kind) => {
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
    //
    // A `local` definition (a closure, a function nested in another) is
    // reachable only from inside its enclosing definition, so it ranks below
    // every same-named definition that isn't, and above test code: sized, like
    // `separators`, past what `path`, extent and kind can add together.
    let visibility = match cand.visibility.as_deref() {
        Some("private" | "protected") => Some(("private", -15.0)),
        Some("local") => Some(("local", -LOCAL_PENALTY)),
        _ => None,
    };
    if let Some((name, value)) = visibility {
        features.push(Feature {
            name,
            value: value * quality,
        });
    }

    // A stub declares what is defined elsewhere (a `.d.ts` entry, an overload
    // signature): the implementation is the answer when it's indexed, and the
    // stub when it isn't. Sized as `local` is, for the same reason.
    if cand.stub {
        features.push(Feature {
            name: "stub",
            value: -STUB_PENALTY * quality,
        });
    }

    // Test/spec path — a fixture or a test double is rarely the definition you
    // meant, and on a large repo they collide head-on with the real ones: Rails
    // has 64 definitions of `save`, half of them fake models under `test/`,
    // every one scoring exactly what the real `ActiveRecord::Persistence#save`
    // scores. Without this the tie falls through to alphabetical path order.
    // A penalty, never a filter — when the test *is* what you're after, every
    // candidate takes it equally and the order among them is unchanged.
    //
    // A match the query only approximates (fuzzy or typo) gives up a share of
    // its name evidence instead of the flat cliff: a test definition that reads
    // as the query clearly better still ranks above a weak match outside tests.
    //
    // Generated code and example or docs apps are secondary the same way, and
    // take the same penalty: a stringer `String()`, or a docs site's wrapper of
    // the library's component, is rarely the definition meant when the library's
    // own shares its name, and is the only answer when not. So are tests that
    // live beside the code, in a scope named for them (Rust's `mod tests`).
    let secondary = if in_test_path(&cand.file) {
        Some("test_path")
    } else if in_test_scope(cand.parent.as_deref(), &cand.name, &cand.kind) {
        Some("test_scope")
    } else if cand.generated {
        Some("generated")
    } else if in_example_path(&cand.file) {
        Some("example_path")
    } else {
        None
    };
    // Asked from inside a test (or generated, or example) file, that file's
    // own definitions are the context, not secondary to it.
    if let Some(name) = secondary.filter(|_| !boosts.anchor_file) {
        let value = if features.iter().any(|f| matches!(f.name, "fuzzy" | "typo")) {
            (TEST_PATH_SHARE * name_evidence(&features).max(0.0)).min(TEST_PATH_PENALTY)
        } else {
            TEST_PATH_PENALTY
        };
        features.push(Feature {
            name,
            value: -value,
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
                value: (span.ln() * BODY_WEIGHT).min(MAX_BODY_BONUS) * quality,
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
            value: -(DEPTH_PENALTY * depth as f64).min(MAX_DEPTH_PENALTY) * quality,
        });
    }

    // Kind weight — definitions you navigate to most sit slightly higher.
    // Top-level types rank alongside classes; methods/functions stay neutral.
    let kind = match cand.kind.as_str() {
        "class" | "struct" | "trait" => 15.0,
        "module" | "enum" | "type" => 12.0,
        _ => 0.0,
    };
    if kind != 0.0 {
        features.push(Feature {
            name: "kind",
            value: kind * quality,
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
    //
    // The typo retry forgives a slip in the scope as it does in the name, so
    // `Widgit.new` still lands inside `Widget`.
    //
    // Scopes a language doesn't record as a parent — a Go package, a Python
    // module, a Rust `mod` file — are spelled by the file's path instead, so the
    // segments the parent doesn't hold may be found there (`path_scope`).
    if let Some(qual) = qualifier {
        if let Some((b, edits)) = parent_boost(qual, cand.parent.as_deref(), near_miss) {
            features.push(Feature {
                name: "parent",
                value: b,
            });
            if edits > 0 {
                features.push(Feature {
                    name: "scope_typo",
                    value: -NEAR_MISS_STEP * edits as f64,
                });
            }
        } else {
            let (owned, pathed, depth) = path_scope(qual, cand)?;
            if owned > 0 {
                features.push(Feature {
                    name: "parent",
                    value: parent_value(owned),
                });
            }
            features.push(Feature {
                name: "path_scope",
                value: PATH_SCOPE_STEP * pathed as f64 * 0.5_f64.powi(depth as i32),
            });
        }
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

    // Anchor boosts — where the query was asked from (`--anchor`).
    if boosts.enclosing > 0.0 {
        features.push(Feature {
            name: "enclosing",
            value: boosts.enclosing,
        });
    }
    if boosts.proximity > 0.0 {
        features.push(Feature {
            name: "proximity",
            value: boosts.proximity,
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

/// A word may be entered at its second letter when its first is a vowel: a
/// consonant skeleton drops it like any other (`prsnch` → `parse_anchor`).
/// The vowel is what the reader leaves out; skipping a consonant that starts a
/// word is reading coincidence into it.
fn droppable(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Reward for matching the case the query was typed in, when the query carries
/// any. It has to outweigh the spread in `recency` (0-120), or which of two
/// same-named symbols wins would come down to whichever file was touched more
/// recently — that made ranking depend on file mtimes, so a fresh checkout
/// ranked differently from a stale one.
const CASE_MATCH: f64 = 150.0;

/// Per qualifier segment found in the file's path rather than the recorded
/// parent: half an owner segment's weight, since a directory claims less than a
/// parent does. Halved again per directory between the scope and the file, so
/// the code the scope holds directly outranks code in its subdirectories.
/// `apply_scope_gate` keeps only the best-scoped results.
const PATH_SCOPE_STEP: f64 = 30.0;

/// Charged per edit in a mistyped scope.
const NEAR_MISS_STEP: f64 = 40.0;

/// How wrong a near miss may be. One edit is a slip; beyond two the "did you
/// mean" stops being a guess and starts being a different word.
const MAX_NEAR_MISS: usize = 2;

/// What a separator-insensitive exact match gives up to a literal one, so
/// `parse_file` still wins when the query spells it out. Sized above what a
/// definition's shape can add (path 50, extent 50, kind 15): Rails'
/// `SchemaCreation` classes each sit in a `schema_creation.rb`, and at 50 they
/// outranked the `schema_creation` method the query spelled.
const SEPARATOR_PENALTY: f64 = 150.0;

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

/// A definition local to another's body. See the visibility block in `score`.
const LOCAL_PENALTY: f64 = 150.0;

/// A declaration whose body is elsewhere. See the stub block in `score`.
const STUB_PENALTY: f64 = 150.0;

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

/// The share of an approximate match's name evidence a test definition gives
/// up: to outrank a match outside tests it must read as the query 1/(1 − share)
/// times better. The flat cliff let any weak match outside tests bury a strong
/// one inside them once recall was complete (DECISIONS D24).
const TEST_PATH_SHARE: f64 = 0.4;

/// Per leading sigil character a query shares with a name: `align`'s credit
/// for one matched letter.
const SIGIL_CREDIT: f64 = 10.0;

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
        .map(fold)
        .collect();
    if q.is_empty() {
        return None;
    }
    // cheap gate: most candidates aren't even a subsequence of the query, so
    // reject them with one linear scan before any of the DP allocations below
    let mut qi = 0;
    for c in name.chars() {
        if qi < q.len() && fold(c) == q[qi] {
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let lower: Vec<char> = chars.iter().copied().map(fold).collect();
    let boundary = boundaries(&chars);
    // prefix count of word boundaries, so we can ask "is a whole word skipped
    // between j and i?" in O(1) — the "only span adjacent words" rule
    let mut bnd_prefix = vec![0usize; n + 1];
    for i in 0..n {
        bnd_prefix[i + 1] = bnd_prefix[i] + boundary[i] as usize;
    }

    // Letters matched before the alignment first reaches a word start earn no
    // credit of their own: the `t…e` of `testag` inside `acTivE` is coincidence
    // until a word boundary confirms the reading. Only the first word can be
    // entered mid-way (every later one must be entered at its start, below), so
    // this charges exactly the query's leading letters spent inside a word it
    // didn't begin — the more of them, the less the match is worth.
    let credit = |anchored: bool, i: usize| {
        if anchored {
            10.0 + if boundary[i] { 15.0 } else { 0.0 }
        } else {
            0.0
        }
    };

    // table[qi][i][a] = best (score, backpointer) for aligning q[0..=qi] with
    // q[qi] landing on name position `i`, where `a` is 1 once the alignment has
    // matched a word start. The backpointer is where q[qi-1] matched, and in
    // which state (self for qi == 0).
    type Cell = Option<(f64, (usize, usize))>;
    let mut table: Vec<Vec<[Cell; 2]>> = vec![vec![[None; 2]; n]; q.len()];

    for (i, &c) in lower.iter().enumerate() {
        if c == q[0] {
            let a = usize::from(boundary[i]);
            // anchored at the very start
            let s = credit(a == 1, i) + if i == 0 { 20.0 } else { 0.0 };
            table[0][i][a] = Some((s, (i, a)));
        }
    }

    for qi in 1..q.len() {
        for i in qi..n {
            if lower[i] != q[qi] {
                continue;
            }
            // a non-boundary char can only follow within MAX_NONBOUNDARY_GAP;
            // a boundary char may follow from the previous word (scan back
            // further), and so may the second letter of a word whose first
            // vowel was dropped
            let past_vowel = i >= 2 && !boundary[i] && boundary[i - 1] && droppable(lower[i - 1]);
            let j_start = if boundary[i] || past_vowel {
                qi - 1
            } else {
                (qi - 1).max(i.saturating_sub(MAX_NONBOUNDARY_GAP + 1))
            };
            let mut best: [Cell; 2] = [None; 2];
            let prev_row = &table[qi - 1];
            for (j, cells) in prev_row.iter().enumerate().take(i).skip(j_start) {
                let trans = if j + 1 == i {
                    10.0 // contiguous run
                } else {
                    let gap = i - j - 1;
                    let crossed = bnd_prefix[i] - bnd_prefix[j + 1];
                    let crossed_word = crossed > 0;
                    if boundary[i] {
                        // entering a new word: only the *adjacent* one — reject if
                        // a whole word boundary sits between j and i (a word skipped)
                        if crossed_word {
                            continue;
                        }
                    } else if past_vowel && crossed == 1 {
                        // from anywhere in the word before, as a word start is
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
                for (pa, cell) in cells.iter().enumerate() {
                    let Some((pscore, _)) = cell else {
                        continue;
                    };
                    let a = usize::from(pa == 1 || boundary[i]);
                    let cand = pscore + trans;
                    if best[a].is_none_or(|(b, _)| cand > b) {
                        best[a] = Some((cand, (j, pa)));
                    }
                }
            }
            for (a, cell) in best.into_iter().enumerate() {
                if let Some((bscore, back)) = cell {
                    table[qi][i][a] = Some((bscore + credit(a == 1, i), back));
                }
            }
        }
    }

    // best end position for the final query char, then backtrack to collect indices
    let last = q.len() - 1;
    let (mut pos, mut state, score) = (0..n)
        .flat_map(|i| (0..2).map(move |a| (i, a)))
        .filter_map(|(i, a)| table[last][i][a].map(|(s, _)| (i, a, s)))
        .max_by(|x, y| x.2.total_cmp(&y.2))?;
    let mut positions = Vec::with_capacity(q.len());
    for qi in (0..q.len()).rev() {
        positions.push(pos);
        (pos, state) = table[qi][pos][state]
            .expect("backtrack hits a filled cell")
            .1;
    }
    positions.reverse();
    Some(Alignment {
        score: score.max(0.0),
        positions,
    })
}

/// Bump when [`transition_pairs`] or the name index's record layout changes.
const PAIRS_VERSION: u32 = 2;

/// Stamped on every repo's name index: [`transition_pairs`]' version and the
/// constant of [`align`] it encodes. An index written under another value is
/// rebuilt rather than read. A change to which transitions `align` accepts has
/// to change `transition_pairs` too (the name index's property test fails until
/// it does), and with it this.
pub(crate) const NAME_INDEX_FORMAT: u32 = PAIRS_VERSION * 100 + MAX_NONBOUNDARY_GAP as u32;

/// Letters and digits get a code of their own, any other alphanumeric shares
/// one; everything else is never matched by a query letter.
pub(super) const PAIR_CODES: usize = 37;

pub(super) fn pair_code(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a'),
        c @ '0'..='9' => Some(26 + c as u8 - b'0'),
        c if c.is_alphanumeric() => Some(36),
        _ => None,
    }
}

/// Every pair of codes `(a, b)` a query could step across in `name`, as
/// `a * PAIR_CODES + b`. [`align`] never skips a word and bounds a mid-word gap,
/// so each consecutive pair of query letters lands on one of these: a later
/// letter in the same word within the gap, or any letter of a word and the
/// start of the next (or its second letter, after a [`droppable`] first). Alphanumerics adjacent across separators are added too,
/// which is how an exact, prefix, separator-free or glob match steps. The name
/// index keeps these per name, so a query missing any is rejected unread.
pub(super) fn transition_pairs(chars: &[char], boundary: &[bool], out: &mut Vec<u16>) {
    let pair = |a: u8, b: u8| u16::from(a) * PAIR_CODES as u16 + u16::from(b);
    let codes: Vec<Option<u8>> = chars.iter().map(|&c| pair_code(c)).collect();
    let (mut word, mut prev_word) = (0, None);
    let mut last_alnum: Option<u8> = None;
    for i in 0..chars.len() {
        if boundary[i] {
            prev_word = (i > 0).then_some(word);
            word = i;
        }
        let Some(b) = codes[i] else { continue };
        let past_vowel = i >= 2 && word == i - 1 && droppable(fold(chars[i - 1]));
        let from = match (boundary[i], prev_word) {
            (true, Some(start)) => start,
            (true, None) => i,
            (false, Some(start)) if past_vowel => start,
            (false, _) => i.saturating_sub(MAX_NONBOUNDARY_GAP + 1).max(word),
        };
        out.extend(codes[from..i].iter().flatten().map(|&a| pair(a, b)));
        out.extend(last_alnum.map(|a| pair(a, b)));
        last_alnum = Some(b);
    }
}

/// Is there any alignment of `query` in `name` — the same answer as
/// `align(..).is_some()`, without scoring it? Bit-parallel over the name's
/// positions: `query` is its ASCII letters and digits lowercased, `name` the
/// lowercased ASCII name (at most 128 bytes) and `boundary` its word starts.
pub(super) fn aligns(query: &[u8], name: &[u8], boundary: u128) -> bool {
    let n = name.len();
    if n == 0 || n > 128 || query.is_empty() || query.len() > n {
        return false;
    }
    // where each of the query's letters sits in the name, in one pass over it
    let mut slot = [u8::MAX; 128];
    let mut masks = [0u128; PAIR_CODES];
    let mut distinct = 0;
    for &c in query {
        match slot.get_mut(usize::from(c)) {
            Some(s) if *s == u8::MAX && distinct < PAIR_CODES => {
                *s = distinct as u8;
                distinct += 1;
            }
            Some(_) => {}
            None => return false, // not ASCII: `align` never matches it here
        }
    }
    for (i, &c) in name.iter().enumerate() {
        if let Some(&k) = slot.get(usize::from(c))
            && k != u8::MAX
        {
            masks[usize::from(k)] |= 1 << i;
        }
    }
    let at = |c: u8| {
        let k = slot.get(usize::from(c)).copied().unwrap_or(u8::MAX);
        masks.get(usize::from(k)).copied().unwrap_or(0)
    };
    let live = if n == 128 { !0 } else { (1u128 << n) - 1 };
    let inner = !boundary & live;
    let vowels = name
        .iter()
        .enumerate()
        .filter(|&(_, &c)| droppable(char::from(c)))
        .fold(0u128, |m, (i, _)| m | 1 << i);
    let b = boundary;
    let mut s = at(query[0]);
    for &c in &query[1..] {
        if s == 0 {
            return false;
        }
        let m = at(c);
        // mid-word: one to three positions on, with no word start in between
        let near = (s << 1) | ((s << 2) & !(b << 1)) | ((s << 3) & !(b << 1) & !(b << 2));
        // a word start: any position the current word reaches, plus one —
        // `s` smeared forward through the positions that aren't word starts
        let (mut g, mut p) = (s, inner);
        for k in [1, 2, 4, 8, 16, 32, 64] {
            g |= p & (g << k);
            p &= p << k;
        }
        let starts = (g << 1) & b;
        let past_vowel = ((starts & vowels) << 1) & inner;
        s = (near & inner & m) | (starts & m) | (past_vowel & m);
    }
    s != 0
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
/// typed before it. The qualifier is everything before the last `::`/`#`/`.`
/// separator: `Foo::Bar` → (`Bar`, `Some("Foo")`), `Foo::Bar#baz` → (`baz`,
/// `Some("Foo::Bar")`), a plain `User` → (`User`, `None`). A leading or trailing
/// separator (`::Bar`, `Foo::`) is treated as an ordinary unqualified query.
pub(crate) fn parse_qualified(query: &str) -> (&str, Option<&str>) {
    let sep = query
        .rmatch_indices("::")
        .map(|(i, _)| (i, 2usize))
        .chain(query.rmatch_indices(['#', '.']).map(|(i, _)| (i, 1usize)))
        .max_by_key(|&(i, _)| i);
    match sep {
        Some((i, len)) if i > 0 && i + len < query.len() => (&query[i + len..], Some(&query[..i])),
        _ => (query, None),
    }
}

/// Lowercased scope segments of a (possibly qualified) name, split on `::`/`#`/`.`.
/// How many scopes a qualified name has, without building them. `segments`
/// allocates a `String` per scope, which is fine for the one query but not for
/// every candidate on a query that recalls thousands.
fn segment_count(s: &str) -> usize {
    s.split("::")
        .flat_map(|p| p.split(['#', '.']))
        .filter(|p| !p.is_empty())
        .count()
}

pub(crate) fn segments(s: &str) -> Vec<String> {
    s.split("::")
        .flat_map(|p| p.split(['#', '.']))
        // a scope is compared by its name, so receiver punctuation around it
        // goes: Go's godoc `(*HugoSites).Build` names `HugoSites`
        .map(|p| p.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_lowercase())
        .collect()
}

/// Boost a candidate whose enclosing scope matches a query's qualifier. The
/// qualifier must match the *innermost* segments of the candidate's `parent`
/// (a suffix): `Foo::Bar` (qualifier `Foo`) rewards a `Bar` whose parent is
/// `Foo` or `App::Foo`, but not one nested under some other scope. More matched
/// segments are stronger evidence of intent, so the boost grows with them.
///
/// With `near_miss`, segments may differ by up to [`MAX_NEAR_MISS`] edits in
/// total; the edit count comes back alongside the boost (0 for an exact scope).
fn parent_boost(qualifier: &str, parent: Option<&str>, near_miss: bool) -> Option<(f64, usize)> {
    let p = segments(parent?);
    let q = segments(qualifier);
    if q.is_empty() || q.len() > p.len() {
        return None;
    }
    let off = p.len() - q.len();
    let mut edits = 0;
    for (qs, ps) in q.iter().zip(&p[off..]) {
        if qs != ps {
            edits += near_miss.then(|| near_miss_distance(qs, ps)).flatten()?;
        }
    }
    (edits <= MAX_NEAR_MISS).then(|| (parent_value(q.len()), edits))
}

/// What a qualifier earns for naming `segments` of a candidate's recorded
/// parent: more segments are stronger evidence of intent.
fn parent_value(segments: usize) -> f64 {
    (120.0 + 60.0 * segments as f64).min(300.0)
}

/// A qualifier's scope found partly or wholly in the candidate's file path,
/// for scopes a language names by where code lives rather than by a recorded
/// parent. The innermost segments may still be the parent (`hugolib.HugoSites.Build`
/// has `HugoSites` as parent); the rest must appear in order among the repo's
/// name, the file's directories and its stem (`django.db.models` in
/// `django/db/models/query.py`, `mpsc` in `tokio/src/sync/mpsc/bounded.rs`, and
/// `gin` for a package at the root of the `gin` repo).
///
/// Returns how many segments the parent held, how many the path did, and how
/// many directories sit between the innermost one the path held and the file:
/// 0 when it names the file's own directory or stem. `gin.Default` is in
/// `gin.go` at the root; `binding/binding.go`'s `Default` is only inside it.
fn path_scope(qualifier: &str, cand: &SymbolRow) -> Option<(usize, usize, usize)> {
    let q = segments(qualifier);
    if q.is_empty() {
        return None;
    }
    let p = cand.parent.as_deref().map(segments).unwrap_or_default();
    let owned = q
        .iter()
        .rev()
        .zip(p.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (last, outer) = q[..q.len() - owned].split_last()?;
    let repo = cand.repo_identity.rsplit(['/', ':']).next();
    let (dirs, _) = cand.file.rsplit_once('/').unwrap_or_default();
    let chain: Vec<String> = repo
        .into_iter()
        .chain(dirs.split('/').filter(|d| !d.is_empty()))
        .chain([path_stem(&cand.file)])
        .map(str::to_lowercase)
        .collect();
    // in order, not contiguous: `tokio::sync` skips the `src` between them.
    // Joiners are ignored, so `tokio_util` finds the `tokio-util` directory.
    let mut at = 0;
    for seg in outer {
        at += chain[at..].iter().position(|c| joiners_eq(c, seg))? + 1;
    }
    // the innermost segment as near the file as it occurs
    let inner = at + chain[at..].iter().rposition(|c| joiners_eq(c, last))?;
    let own_dir = chain.len() - 2;
    Some((owned, outer.len() + 1, own_dir.saturating_sub(inner)))
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

/// The kinds a path match alone can surface: a file's primary definitions.
pub(crate) const PRIMARY_KINDS: [&str; 5] = ["class", "module", "struct", "enum", "trait"];

fn is_primary_kind(kind: &str) -> bool {
    PRIMARY_KINDS.contains(&kind)
}

/// A fuzzy match's value: the best alignment, less the same unmatched-tail
/// charge the prefix branch applies. The alignment counts matched query chars,
/// so without it a candidate's extra characters were free and `Validaton`
/// scored `ValidationError` exactly as well as `Validations`. Capped, and gentle
/// enough that an abbreviation still reaches a long name it barely covers
/// (`apc` → `ApplicationController`).
pub(super) fn fuzzy_value(q: &str, name: &str) -> Option<f64> {
    // `align` reads only letters and digits, so `_dshrz` found the public
    // `dasherize` first. A leading sigil the name shares is read as typed: both
    // are aligned without it, so the name's first letter keeps its start bonus,
    // and each sigil character earns what `align` credits a matched letter.
    // Only the score moves; what matches is unchanged, as the name index needs.
    let sigil = q.len() - q.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
    if sigil > 0 && name.get(..sigil) == Some(&q[..sigil]) {
        return fuzzy_value(&q[sigil..], &name[sigil..]).map(|v| v + SIGIL_CREDIT * sigil as f64);
    }
    let s = subsequence_score(q, name)?;
    let tail = name.chars().count().saturating_sub(q.chars().count());
    Some(s.min(600.0) - (tail as f64).min(100.0))
}

/// The share of `query`'s cased letters whose case the letter they align with
/// in `name` shares, or `None` without an alignment or a cased letter.
fn case_agreement(query: &str, name: &str) -> Option<f64> {
    let positions = align(query, name)?.positions;
    let chars: Vec<char> = name.chars().collect();
    let (mut agree, mut cased) = (0usize, 0usize);
    for (c, &i) in query
        .chars()
        .filter(|c| c.is_alphanumeric())
        .zip(&positions)
    {
        if c.is_uppercase() || c.is_lowercase() {
            cased += 1;
            agree += usize::from(c.is_uppercase() == chars[i].is_uppercase());
        }
    }
    (cased > 0).then(|| agree as f64 / cased as f64)
}

/// Score `query` as a subsequence of `name` (the best alignment's score), or
/// `None` if it isn't a subsequence.
pub(super) fn subsequence_score(query: &str, name: &str) -> Option<f64> {
    align(query, name).map(|a| a.score)
}

/// Does `query` use wildcard syntax — `*` (any run), `?` (one char)? When it
/// does, matching switches from fuzzy subsequence to an explicit glob: literal
/// chars match *contiguously*, and the only gaps are the ones the user marked.
/// `find*controller` keeps `FindController` and `FindUserController` but, unlike
/// fuzzy, won't reach into a scattered `FxIxNxDxController`.
pub(crate) fn has_wildcard(query: &str) -> bool {
    match query.strip_suffix('?') {
        // a lone trailing `?` ends a Ruby predicate's name (`empty?`), not a glob
        Some(body) if !body.contains(['*', '?']) => false,
        _ => query.contains(['*', '?']),
    }
}

/// A wildcard query's literal characters, metachars removed — the key for the
/// store's exact and prefix layers, before the glob does the precise matching.
/// `find*controller` → `findcontroller`.
pub(crate) fn strip_wildcards(query: &str) -> String {
    query.chars().filter(|c| !matches!(c, '*' | '?')).collect()
}

/// One token of a compiled wildcard pattern.
enum Glob {
    Lit(char), // a literal (lowercased) char — matches itself
    Any,       // `?` — exactly one char
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
            '?' => Some(Glob::Any),
            c if c.is_alphanumeric() => Some(Glob::Lit(fold(c))),
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

    let lower: Vec<char> = name.chars().map(fold).collect();
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
            // the query's separators were dropped at compile, so the name's are
            // transparent to a literal too — else `only_up*` never meets `only_uploads`
            Some(Glob::Lit(_)) if !lower[ni].is_alphanumeric() => {
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
pub(super) fn wildcard_score(query: &str, name: &str) -> Option<f64> {
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
pub(super) fn boundaries(chars: &[char]) -> Vec<bool> {
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
    let mut qc = leaf.chars().map(fold);
    let mut nc = name.chars().map(fold);
    match (qc.next(), qc.next(), nc.next(), nc.next()) {
        (Some(q0), Some(q1), Some(n0), Some(n1)) => q0 == n0 || (q0 == n1 && q1 == n0),
        _ => false,
    }
}

pub(super) fn near_miss_distance(q: &str, name: &str) -> Option<usize> {
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

/// A near miss scores as a fuzzy match of the part of the query that was right
/// — its longest common subsequence with the name — so the letters that agree
/// count as they would in any other fuzzy match. Each edit then takes back one
/// letter's share of that evidence: one slip in a long name costs little, two
/// in a four-letter query leave nothing. The same unmatched-tail charge applies.
fn near_miss_score(query: &str, name: &str, edits: usize) -> f64 {
    let right = common_subsequence(query, name);
    let len = right.chars().count();
    let aligned = align(&right, name).map_or(0.0, |a| a.score.min(600.0));
    let kept = len.saturating_sub(edits) as f64 / len.max(1) as f64;
    let tail = name.chars().count().saturating_sub(len);
    aligned * kept - (tail as f64).min(100.0)
}

/// The longest common subsequence of `a` and `b`, compared case-blind and
/// spelled as in `a`. Near misses are short, so the quadratic table is small.
fn common_subsequence(a: &str, b: &str) -> String {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().map(fold).collect();
    let eq = |i: usize, j: usize| fold(a[i]) == b[j];
    // len[i][j] = LCS length of a[i..] and b[j..]
    let mut len = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            len[i][j] = if eq(i, j) {
                len[i + 1][j + 1] + 1
            } else {
                len[i + 1][j].max(len[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, String::new());
    while i < a.len() && j < b.len() {
        if eq(i, j) {
            out.push(a[i]);
            (i, j) = (i + 1, j + 1);
        } else if len[i + 1][j] >= len[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// One character lowercased for matching, so `ΣΑΣ` in a name meets the `σασ`
/// a query is folded to. A letter whose lowercase is ASCII (`İ`, the Kelvin
/// sign) keeps its case: the name index codes it as non-ASCII ([`pair_code`]),
/// and the scorer must accept exactly what the index does (D23).
pub(super) fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    match c.to_lowercase().next() {
        // a string lowercases a word-final `Σ` to `ς`, a lone char to `σ`
        Some('ς') => 'σ',
        Some(l) if !l.is_ascii() => l,
        _ => c,
    }
}

/// Lowercase without allocating when there's nothing to change. Called once
/// per candidate for the name and — wastefully, since it's constant — once per
/// candidate for the query; most queries and most snake_case names are already
/// lowercase, so the copy was of something identical. Unicode folding, as
/// the store's `name_lower` is written.
pub(super) fn lower(s: &str) -> std::borrow::Cow<'_, str> {
    if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(s.to_lowercase())
    }
}

/// Are these the same identifier once separators are dropped? `_`, `-`, and
/// `.` are all word joiners across the languages rq indexes, and a query that
/// omits them is spelling the same name. Any other character is part of the
/// name: `save!` and `name=` are different methods from `save` and `name`.
pub(crate) fn joiners_eq(a: &str, b: &str) -> bool {
    // Compared in lockstep rather than by building two squashed Strings: this
    // runs against every candidate, and on a query that recalls thousands the
    // allocations cost more than everything else in scoring put together.
    let word = |c: &char| !matches!(c, '_' | '-' | '.');
    let mut sa = a.chars().filter(word);
    let mut sb = b.chars().filter(word);
    let mut any = false;
    loop {
        match (sa.next(), sb.next()) {
            (None, None) => return any,
            (Some(x), Some(y)) if x == y => any = true,
            _ => return false,
        }
    }
}

/// Is this repo-relative path an example, demo or docs app — code that shows
/// the library rather than being it? Whole directory segments, as for tests.
/// Not `doc/`: in Go and Rust that's usually the library's own package
/// (ripgrep's `flags/doc/`, tokio's `src/doc/`).
fn in_example_path(file: &str) -> bool {
    let dirs = file.rsplit_once('/').map_or("", |(d, _)| d);
    dirs.split('/').any(|seg| {
        matches!(
            seg,
            "example" | "examples" | "_examples" | "demo" | "demos" | "docs" | "dev-docs"
        )
    })
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

/// Whether a definition is test code by its enclosing scope rather than its
/// file: inside a scope named `tests`, `test` or `*_tests`, or such a module
/// itself. Only a lowercase segment counts, so a type or module that is part of
/// an API about testing (Ruby's `ActiveSupport::Testing`, `Minitest::Test`)
/// never does.
fn in_test_scope(parent: Option<&str>, name: &str, kind: &str) -> bool {
    let test = |seg: &str| matches!(seg, "tests" | "test") || seg.ends_with("_tests");
    parent.is_some_and(|p| p.split("::").flat_map(|s| s.split(['#', '.'])).any(test))
        || (kind == "module" && test(name))
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

    #[test]
    fn recognizes_test_scopes_by_their_lowercase_name() {
        for (parent, name, kind) in [
            (Some("tests"), "helper", "function"),
            (Some("store::tests"), "fixture", "function"),
            (Some("resolve::surface_tests"), "case", "function"),
            (None, "tests", "module"),
        ] {
            assert!(in_test_scope(parent, name, kind), "{parent:?} {name}");
        }
        for (parent, name, kind) in [
            (Some("Minitest::Test"), "assert", "method"),
            (Some("ActiveSupport::Testing"), "travel", "method"),
            (Some("testing"), "helper", "function"),
            (None, "tests", "function"),
            (None, "latest", "module"),
        ] {
            assert!(!in_test_scope(parent, name, kind), "{parent:?} {name}");
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
            stub: false,
            generated: false,
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
    fn an_implementation_outranks_its_declaration_elsewhere() {
        // the vendored-library shape: a `.d.ts` declares the class with every
        // shape signal in its favour — public, as long, in a file named for it —
        // against a CommonJS implementation that reads private
        let declared = SymbolRow {
            file: "types/widget.d.ts".into(),
            end_line: Some(200),
            visibility: Some("public".into()),
            stub: true,
            ..row("Widget", "class", 1)
        };
        let implemented = SymbolRow {
            file: "lib/index.js".into(),
            end_line: Some(40),
            visibility: Some("private".into()),
            ..row("Widget", "class", 1)
        };
        let total = |q: &str, c: &SymbolRow| score(q, c, None, Boosts::default(), false);
        let stub = total("Widget", &declared).unwrap();
        assert!(total("Widget", &implemented).unwrap().total > stub.total);
        assert!(stub.features.iter().any(|f| f.name == "stub"));
        // but a declaration that is the only exact match still beats a prefix
        let longer = row("WidgetBuilder", "class", 1);
        assert!(stub.total > total("Widget", &longer).unwrap().total);
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
    fn a_sigil_the_query_left_off_is_not_an_exact_match() {
        // `!`, `?` and `=` end a name rather than join its words: `save!` is a
        // different method from `save`, not the same one spelled with a separator
        let literal = |query: &str, name: &str| {
            score(
                query,
                &row(name, "method", 1),
                None,
                Boosts::default(),
                false,
            )
            .is_some_and(|s| s.features.iter().any(|f| f.name == "exact"))
        };
        for (query, name) in [("save", "save!"), ("valid", "valid?"), ("name", "name=")] {
            assert!(!literal(query, name), "{query} is not exactly {name}");
            // and it still wins the query that types it, by the full match tier
            assert!(literal(name, name));
            assert!(!literal(name, query), "{name} is not exactly {query}");
            let gap = total(query, query).unwrap() - total(query, name).unwrap();
            assert!(gap > 200.0, "{query}: {name} trails by {gap}");
        }
        // separators still join words, wherever they sit
        assert!(literal("isvalid?", "is_valid?"));
        assert!(literal("init", "__init__"));
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
    fn example_and_docs_apps_rank_below_the_library() {
        let at = |file: &str| SymbolRow {
            file: file.into(),
            ..row("Widget", "class", 1)
        };
        let total = |c: &SymbolRow| score("Widget", c, None, Boosts::default(), false).unwrap();
        let library = total(&at("packages/widget/index.tsx")).total;
        for file in [
            "examples/with-next/src/pages/widget.tsx",
            "dev-docs/src/theme/index.js",
            "docs/_ext/widget.py",
            "demo/widget.go",
        ] {
            let example = total(&at(file));
            assert!(library > example.total, "{file}");
            assert!(example.features.iter().any(|f| f.name == "example_path"));
        }
        // the library's own `doc` package, and a word merely containing one
        assert!(!in_example_path("crates/core/flags/doc/help.rs"));
        assert!(!in_example_path("lib/examples_helper.rb"));
    }

    #[test]
    fn generated_code_ranks_below_hand_written_code() {
        let hand = row("String", "method", 1);
        let generated = SymbolRow {
            generated: true,
            ..row("String", "method", 1)
        };
        let total = |c: &SymbolRow| score("String", c, None, Boosts::default(), false).unwrap();
        let g = total(&generated);
        assert!(total(&hand).total > g.total);
        assert!(g.features.iter().any(|f| f.name == "generated"));
    }

    #[test]
    fn a_strong_fuzzy_match_in_a_test_outranks_a_weak_one_outside() {
        let at = |name: &str, file: &str| {
            let mut r = row(name, "method", 1);
            r.file = file.into();
            score("coclfi", &r, None, Boosts::default(), false).unwrap()
        };
        let strong = at("conditional_class_filter", "test/filters_test.rb");
        let weak = at("remove_scoped_cable_files_if_skipped", "lib/generator.rb");
        assert!(
            strong.total > weak.total,
            "{} > {}",
            strong.total,
            weak.total
        );
        // an equally good match outside tests still wins
        let lib = at("conditional_class_filter", "lib/filters.rb");
        assert!(lib.total > strong.total);
        // and a literal match keeps the full cliff
        let exact = |file: &str| {
            let mut r = row("save", "method", 1);
            r.file = file.into();
            score("save", &r, None, Boosts::default(), false).unwrap()
        };
        let lib = exact("lib/persistence.rb").total;
        assert_eq!(lib - exact("test/fake_models.rb").total, TEST_PATH_PENALTY);
    }

    #[test]
    fn a_leading_underscore_asks_for_the_underscored_name() {
        let fz = |q: &str, name: &str| {
            let mut r = row(name, "method", 1);
            r.visibility = Some(
                if name.starts_with('_') {
                    "private"
                } else {
                    "public"
                }
                .into(),
            );
            score(q, &r, None, Boosts::default(), false).unwrap().total
        };
        assert!(fz("_frmtr", "_formatter") > fz("_frmtr", "formatter"));
        // without it, the public name still leads
        assert!(fz("frmtr", "formatter") > fz("frmtr", "_formatter"));
        // the sigil changes the score, never what matches
        assert!(fuzzy_value("_zq", "_formatter").is_none());
        assert!(fuzzy_value("_frmtr", "formatter").is_some());
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
    fn a_typed_capital_counts_on_an_approximate_match_too() {
        let typo = |q: &str, name: &str| {
            score(q, &row(name, "function", 1), None, Boosts::default(), true)
                .unwrap()
                .total
        };
        assert!(typo("Fraem", "Frame") > typo("Fraem", "frame"));
        assert!(typo("FRAEM", "FRAME") > typo("FRAEM", "Frame"));
        assert!(total("Widg", "WidgetBox") > total("Widg", "widget_box"));
        assert!(total("WdgBx", "WidgetBox") > total("WdgBx", "widget_box"));
        // lowercase stays casual here as well
        assert_eq!(typo("fraem", "Frame"), typo("fraem", "frame"));
    }

    #[test]
    fn a_lowercase_query_stays_case_agnostic() {
        // Lowercase is how people type casually — reading intent into it would
        // demote `User` for `user`, so neither spelling is rewarded.
        assert_eq!(total("symbol", "symbol"), total("symbol", "Symbol"));
        assert_eq!(total("user", "User"), total("user", "user"));
    }

    #[test]
    fn a_closer_name_outranks_a_bigger_definition() {
        // The method reads as the query; the class only holds its letters. Its
        // extent, kind and file name must not carry it past the better name.
        let method = row("find_public_node", "method", 1);
        let mut class = row("RefillPushNoticeLevel", "class", 1);
        class.file = "app/models/refill_push_notice_level.rb".into();
        class.end_line = Some(200);
        let at = |r: &SymbolRow| score("fipuno", r, None, Boosts::default(), false).unwrap();
        let (m, c) = (at(&method), at(&class));
        assert!(name_evidence(&m.features) > name_evidence(&c.features));
        assert!(m.total > c.total, "{} > {}", m.total, c.total);
        // the same features still order two exact matches at full weight
        let exact = |r: &SymbolRow| score(&r.name, r, None, Boosts::default(), false).unwrap();
        let extent = |s: &Scored| {
            s.features
                .iter()
                .find(|f| f.name == "extent")
                .unwrap()
                .value
        };
        assert_eq!(extent(&exact(&class)), MAX_BODY_BONUS);
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
    fn non_ascii_letters_match_across_case() {
        assert_eq!(match_positions("ΣΑΣprs", "ΣΑΣParser"), [0, 1, 2, 3, 5, 6]);
        assert_eq!(match_positions("σασprs", "ΣΑΣParser"), [0, 1, 2, 3, 5, 6]);
        assert_eq!(match_positions(&lower("ΣΑΣ"), "ΣΑΣParser"), [0, 1, 2]);
        assert_eq!(match_positions("grösse", "GRÖSSE"), [0, 1, 2, 3, 4, 5]);
        // lowercases to ASCII, which the name index can't see: left alone
        assert!(match_positions("istrtr", "İstanbulRouter").is_empty());
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
    fn a_consonant_skeleton_may_drop_a_word_initial_vowel() {
        // `n` is anchor's second letter: its `a` was left out, as vowels are
        assert_eq!(
            match_positions("prsnch", "parse_anchor"),
            [0, 2, 3, 7, 8, 9]
        );
        assert!(subsequence_score("mxncls", "MAX_ENCLOSING").is_some());
        // a dropped consonant is not a skeleton
        assert!(subsequence_score("prsnchr", "parse_branchor").is_none());
        // and the word dropped from must be the next one
        assert!(subsequence_score("prsnch", "parse_x_anchor").is_none());
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
                while qi < qchars.len() && fold(qchars[qi]) != fold(nchars[p]) {
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
    fn a_trailing_question_mark_is_a_predicate_name() {
        assert!(!has_wildcard("empty?"));
        assert!(has_wildcard("emp?y"));
        assert!(has_wildcard("emp?y?"));
        assert!(has_wildcard("find*?"));
    }

    #[test]
    fn wildcard_literals_step_over_the_names_separators() {
        assert!(total("only_up*s", "only_uploads").is_some());
        assert!(total("onlyup*s", "only_uploads").is_some());
    }

    #[test]
    fn wildcard_question_mark_matches_one_char() {
        // `?` consumes exactly one char
        assert!(total("find?controller", "FindXController").is_some());
        assert!(total("find?controller", "Find1Controller").is_some());
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
        // `.` too — a class method, or a Python/JS member
        assert_eq!(parse_qualified("Foo.bar"), ("bar", Some("Foo")));
        assert_eq!(parse_qualified("Foo::Bar.baz"), ("baz", Some("Foo::Bar")));
        // a leading or trailing separator is not a qualifier
        assert_eq!(parse_qualified("::Bar"), ("::Bar", None));
        assert_eq!(parse_qualified("Foo::"), ("Foo::", None));
    }

    #[test]
    fn a_receiver_in_parentheses_names_its_type() {
        assert_eq!(segments("(*HugoSites)"), ["hugosites"]);
        assert_eq!(segments("(*pkg.T)"), ["pkg", "t"]);
        assert!(parent_boost("(*Widget)", Some("Widget"), false).is_some());
    }

    #[test]
    fn parent_boost_matches_the_innermost_scopes() {
        // exact parent, and a qualifier naming only the immediate scope
        assert!(parent_boost("Foo", Some("Foo"), false).is_some());
        assert!(parent_boost("Foo", Some("App::Foo"), false).is_some());
        assert!(parent_boost("App::Foo", Some("App::Foo"), false).is_some());
        // more matched segments → a stronger boost
        let one = parent_boost("Foo", Some("App::Foo"), false).unwrap().0;
        let two = parent_boost("App::Foo", Some("App::Foo"), false).unwrap().0;
        assert!(two > one, "{two} > {one}");
        // the qualifier must be a suffix, not just any ancestor or sibling
        assert!(parent_boost("App", Some("App::Foo"), false).is_none());
        assert!(parent_boost("Foo", Some("Foo::Inner"), false).is_none());
        assert!(parent_boost("Foo", None, false).is_none());
        // a `.`-joined parent (Python, TypeScript) splits the same way
        assert!(parent_boost("Inner", Some("Outer.Inner"), false).is_some());
        // a typo'd scope matches only on the typo retry, and reports its edits
        assert!(parent_boost("Widgit", Some("Widget"), false).is_none());
        assert_eq!(parent_boost("Widgit", Some("Widget"), true).unwrap().1, 1);
        assert!(parent_boost("Gadget", Some("Widget"), true).is_none());
    }

    #[test]
    fn a_scope_no_parent_records_is_found_in_the_path() {
        let at = |file: &str, parent: Option<&str>| SymbolRow {
            file: file.into(),
            parent: parent.map(Into::into),
            repo_identity: "github.com/org/shop".into(),
            ..row("Widget", "class", 1)
        };
        let features = |q: &str, c: &SymbolRow| {
            score(q, c, None, Boosts::default(), false).map(|s| {
                s.features
                    .iter()
                    .filter(|f| matches!(f.name, "parent" | "path_scope"))
                    .map(|f| (f.name, f.value))
                    .collect::<Vec<_>>()
            })
        };
        let module = at("lib/shop/db/models/widget.py", None);
        // directories in order, gaps allowed; the stem counts too
        assert!(features("db.models.Widget", &module).is_some());
        assert!(features("shop.models.Widget", &module).is_some());
        assert!(features("models.widget.Widget", &module).is_some());
        // out of order, or absent, is not a scope match
        assert!(features("models.db.Widget", &module).is_none());
        assert!(features("orders.Widget", &module).is_none());
        // a package at the repo root is named by the repo; a file below the
        // root is inside that scope, but only indirectly
        let root = features("shop.Widget", &at("widget.go", None)).unwrap();
        let below = features("shop.Widget", &at("orders/widget.go", None)).unwrap();
        assert_eq!((root[0].1, below[0].1), (30.0, 15.0));
        // a directory spelled with a joiner the scope leaves out
        assert!(features("shop_util.Widget", &at("shop-util/src/a.rs", None)).is_some());
        // the parent holds the innermost segments, the path the rest, and each
        // segment the parent holds is worth more than one the path does
        let method = at("pkg/widgets/widget.go", Some("Catalog"));
        let mixed = features("widgets.Catalog.Widget", &method).unwrap();
        assert_eq!(mixed, [("parent", 180.0), ("path_scope", 30.0)]);
        let owned = features("Catalog.Widget", &method).unwrap();
        let total = |f: &[(&str, f64)]| f.iter().map(|x| x.1).sum::<f64>();
        let pathed = features("widgets.Widget", &method).unwrap();
        assert!(total(&owned) > total(&pathed));
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
        // inside anything, which is precisely what the query ruled out — unless
        // its file sits under `foo`, which is how a package or module says so
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
