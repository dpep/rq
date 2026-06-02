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
    let name_matched = if name_lower == q {
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

    // Layer 3: path / filename matching.
    let stem = path_stem(&cand.file);
    let path_match = subsequence_score(&q, stem);
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

/// The char indices in `name` that `query` matched — greedy, left-to-right,
/// separator-insensitive (same matching as the scorer). For highlighting *what*
/// matched (exact/prefix yield a leading run; fuzzy yields the scattered chars).
/// Empty if `query` isn't a subsequence of `name`.
pub fn match_positions(query: &str, name: &str) -> Vec<usize> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if q.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut qi = 0;
    for (i, c) in name.chars().enumerate() {
        if qi >= q.len() {
            break;
        }
        if c.to_ascii_lowercase() == q[qi] {
            positions.push(i);
            qi += 1;
        }
    }
    if qi == q.len() { positions } else { Vec::new() }
}

/// Score `query` as a subsequence of `name`, rewarding matches at word
/// boundaries (camelCase / underscore) and contiguous runs. `None` if `query`
/// is not a subsequence. Handles abbreviations like `refproc → RefundProcessor`,
/// `usr → User`, `paymnt → Payments`.
///
/// Separators in the query are ignored, so a snake_case (or kebab) query matches
/// a CamelCase name: `widget_controller` → `WidgetsController`. Candidate
/// separators are skipped naturally as the scan walks the name.
fn subsequence_score(query: &str, name: &str) -> Option<f64> {
    let q: Vec<char> = query.chars().filter(|c| c.is_alphanumeric()).collect();
    if q.is_empty() {
        return None;
    }
    let chars: Vec<char> = name.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let boundary = boundaries(&chars);

    let mut score = 0.0;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, &c) in lower.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if c != q[qi] {
            continue;
        }
        score += 10.0; // base per matched char
        if boundary[i] {
            score += 15.0; // aligned to a word boundary
        }
        match prev {
            Some(p) if p + 1 == i => score += 10.0,       // contiguous
            Some(p) => score -= (i - p - 1) as f64 * 0.5, // gap penalty
            None if i == 0 => score += 20.0,              // matches at the very start
            None => {}
        }
        prev = Some(i);
        qi += 1;
    }

    if qi == q.len() {
        Some(score.max(0.0))
    } else {
        None
    }
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
    fn snake_case_query_matches_camelcase_name() {
        // typed a snake_case query, want the CamelCase class — even when the
        // class is plural and you forgot the `s`
        assert!(total("widget_controller", "WidgetsController").is_some());
        assert!(total("widget_controller", "WidgetController").is_some());
        // unrelated controller still doesn't match
        assert!(total("widget_controller", "AdminController").is_none());
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
