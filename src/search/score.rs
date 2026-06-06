//! Ranking: a simple, explainable, additive score.
//!
//! Match quality dominates (exact > prefix > abbreviation/subsequence), with
//! smaller additive features layered on (kind, current-repo). Every component
//! is recorded so `--explain` can show why a result ranked where it did.

use crate::store::SymbolRow;

/// One named contribution to a score.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Feature {
    pub name: &'static str,
    pub value: f64,
}

/// A scored candidate: total plus the per-feature breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub total: f64,
    pub features: Vec<Feature>,
}

/// Dynamic, context-dependent boosts computed by [`crate::search`] (which owns
/// the time math and store lookups). Kept out of the pure match scoring so each
/// signal can be added without threading more parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Boosts {
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
pub fn score(
    query: &str,
    cand: &SymbolRow,
    current_repo_id: Option<i64>,
    boosts: Boosts,
) -> Option<Scored> {
    let q = query.to_ascii_lowercase();
    let name_lower = cand.name.to_ascii_lowercase();

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
        true
    } else if name_lower.starts_with(&q) {
        // shorter remaining tail ranks higher
        let tail = cand.name.chars().count().saturating_sub(q.chars().count());
        features.push(Feature {
            name: "prefix",
            value: 700.0 - (tail as f64).min(100.0),
        });
        true
    } else if let Some(s) = subsequence_score(&q, &cand.name) {
        features.push(Feature {
            name: "fuzzy",
            value: s.min(600.0),
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
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    if q.len() > n {
        return None;
    }
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
pub fn match_positions(query: &str, name: &str) -> Vec<usize> {
    if has_wildcard(query) {
        return glob_positions(query, name).unwrap_or_default();
    }
    align(query, name).map(|a| a.positions).unwrap_or_default()
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
pub fn has_wildcard(query: &str) -> bool {
    query.contains(['*', '?', '.'])
}

/// A wildcard query's literal characters, metachars removed — used to seed the
/// store's candidate recall (which keys off literal trigrams) before the glob
/// does the precise matching. `find*controller` → `findcontroller`.
pub fn strip_wildcards(query: &str) -> String {
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
fn path_stem(path: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, repo: i64) -> SymbolRow {
        SymbolRow {
            name: name.into(),
            kind: kind.into(),
            language: "ruby".into(),
            file: "f.rb".into(),
            line: 1,
            parent: None,
            repository_id: repo,
            repo_identity: "r".into(),
            mtime: None,
            git_ts: None,
        }
    }

    fn total(query: &str, name: &str) -> Option<f64> {
        score(query, &row(name, "class", 1), None, Boosts::default()).map(|s| s.total)
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
            let pos = match_positions(q, name);
            assert_eq!(
                pos.len(),
                qchars.len(),
                "one highlight per query char: {q}/{name}"
            );
            assert!(
                pos.windows(2).all(|w| w[0] < w[1]),
                "strictly increasing: {q}/{name} {pos:?}"
            );
            for (qi, &p) in pos.iter().enumerate() {
                assert!(p < nchars.len(), "in bounds: {q}/{name}");
                assert_eq!(
                    nchars[p].to_ascii_lowercase(),
                    qchars[qi].to_ascii_lowercase(),
                    "highlighted char equals the query char: {q}/{name} at {p}"
                );
            }
        }
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
        let pre = score("employees", &prefixed, None, Boosts::default())
            .unwrap()
            .total;
        if let Some(s) = score("employees", &straggler, None, Boosts::default()) {
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
        let s = score("billing", &cand, None, Boosts::default()).expect("path match");
        assert!(s.features.iter().any(|f| f.name == "path"));

        // a method (not a primary definition) in the same file does NOT surface
        let mut method = row("compute", "method", 1);
        method.file = "app/models/billing.rb".into();
        assert!(score("billing", &method, None, Boosts::default()).is_none());
    }

    #[test]
    fn path_bonus_reinforces_a_name_match() {
        let mut named = row("User", "class", 1);
        named.file = "app/models/user.rb".into();
        let mut elsewhere = row("User", "class", 1);
        elsewhere.file = "app/lib/misc.rb".into();
        let with_path = score("user", &named, None, Boosts::default())
            .unwrap()
            .total;
        let without = score("user", &elsewhere, None, Boosts::default())
            .unwrap()
            .total;
        assert!(with_path > without, "{with_path} > {without}");
    }

    #[test]
    fn current_repo_boost_applies() {
        let cand = row("User", "class", 7);
        let in_repo = score("user", &cand, Some(7), Boosts::default())
            .unwrap()
            .total;
        let out_repo = score("user", &cand, Some(99), Boosts::default())
            .unwrap()
            .total;
        assert!(in_repo > out_repo);
        assert_eq!(in_repo - out_repo, 200.0);
    }

    #[test]
    fn learned_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default()).unwrap().total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                learned: 150.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(boosted.total - base, 150.0);
        assert!(boosted.features.iter().any(|f| f.name == "learned"));
    }

    #[test]
    fn recency_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default()).unwrap().total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                recency: 80.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(boosted.total - base, 80.0);
        assert!(boosted.features.iter().any(|f| f.name == "recency"));
    }

    #[test]
    fn branch_boost_adds_to_the_score() {
        let cand = row("User", "class", 1);
        let base = score("user", &cand, None, Boosts::default()).unwrap().total;
        let boosted = score(
            "user",
            &cand,
            None,
            Boosts {
                branch: 180.0,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(boosted.total - base, 180.0);
        assert!(boosted.features.iter().any(|f| f.name == "branch"));
    }
}
