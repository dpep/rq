//! Ranking: a simple, explainable, additive score.
//!
//! Match quality dominates (exact > prefix > abbreviation/subsequence), with
//! smaller additive features layered on (kind, current-repo). Every component
//! is recorded so `--explain` can show why a result ranked where it did.

use crate::store::SymbolRow;

/// One named contribution to a score.
#[derive(Debug, Clone, PartialEq)]
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

/// Score `cand` for `query`. Returns `None` when the candidate doesn't match at
/// all (not even as a subsequence), filtering FTS trigram noise.
pub fn score(query: &str, cand: &SymbolRow, current_repo_id: Option<i64>) -> Option<Scored> {
    let q = query.to_ascii_lowercase();
    let name_lower = cand.name.to_ascii_lowercase();

    let mut features = Vec::new();

    // Match quality — the dominant term.
    if name_lower == q {
        features.push(Feature {
            name: "exact",
            value: 1000.0,
        });
    } else if name_lower.starts_with(&q) {
        // shorter remaining tail ranks higher
        let tail = cand.name.chars().count().saturating_sub(q.chars().count());
        let value = 700.0 - (tail as f64).min(100.0);
        features.push(Feature {
            name: "prefix",
            value,
        });
    } else {
        let s = subsequence_score(&q, &cand.name)?;
        features.push(Feature {
            name: "fuzzy",
            value: s.min(600.0),
        });
    }

    // Kind weight — definitions you navigate to most sit slightly higher.
    let kind = match cand.kind.as_str() {
        "class" => 15.0,
        "module" => 12.0,
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

    let total = features.iter().map(|f| f.value).sum();
    Some(Scored { total, features })
}

/// Score `query` as a subsequence of `name`, rewarding matches at word
/// boundaries (camelCase / underscore) and contiguous runs. `None` if `query`
/// is not a subsequence. Handles abbreviations like `refproc → RefundProcessor`,
/// `usr → User`, `paymnt → Payments`.
fn subsequence_score(query: &str, name: &str) -> Option<f64> {
    let q: Vec<char> = query.chars().collect();
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
        }
    }

    fn total(query: &str, name: &str) -> Option<f64> {
        score(query, &row(name, "class", 1), None).map(|s| s.total)
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
    fn current_repo_boost_applies() {
        let cand = row("User", "class", 7);
        let in_repo = score("user", &cand, Some(7)).unwrap().total;
        let out_repo = score("user", &cand, Some(99)).unwrap().total;
        assert!(in_repo > out_repo);
        assert_eq!(in_repo - out_repo, 200.0);
    }
}
