//! The name index's matcher: a fixed-size [`Signature`] per name, and the
//! [`Probe`] a query compiles to. Recall screens every signature in a repo
//! with a few AND instructions, then verifies the few survivors exactly, so the
//! names it hands on are the names [`score`] accepts — no more,
//! no fewer (docs/NAME_INDEX.md, DECISIONS D23).
//!
//! Language-blind: it sees a name's characters and word boundaries, and the
//! scorer's own rules for stepping across them.

use super::score::{self, PAIR_CODES, pair_code};

/// A signature's size on disk.
pub(crate) const SIG_BYTES: usize = 40;

/// Code for a character with no pair code, in the typo key.
const OTHER: u8 = PAIR_CODES as u8;
/// Code for a character past the end of a short name, in the typo key.
const NONE: u8 = OTHER + 1;

/// What a name could match, in 40 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Signature {
    /// Which pair codes the name holds, one bit each.
    chars: u64,
    /// A 256-bit Bloom filter of the name's transition pairs.
    pairs: [u64; 4],
    /// The typo key, from the lowercased name: its length (saturating) and the
    /// codes of its first two characters. A near miss needs both to be close.
    len: u8,
    first: [u8; 2],
}

fn bloom(pair: u16) -> (usize, u64) {
    let h = u32::from(pair).wrapping_mul(0x9E37_79B1) >> 24;
    ((h >> 6) as usize, 1 << (h & 63))
}

fn typo_code(c: Option<char>) -> u8 {
    c.map_or(NONE, |c| pair_code(c).unwrap_or(OTHER))
}

impl Signature {
    pub(crate) fn of(name: &str) -> Signature {
        let mut sig = Signature::default();
        let mut pairs = Vec::new();
        let chars: Vec<char> = name.chars().collect();
        score::transition_pairs(&chars, &score::boundaries(&chars), &mut pairs);
        sig.add_chars(&chars);
        let lower = score::lower(name);
        if !name.is_ascii() {
            // exact and prefix compare the Unicode-lowercased name, which can
            // differ from the name letter by letter; adjacency is all they need
            let lower: Vec<char> = lower.chars().collect();
            let no_words = vec![false; lower.len()];
            score::transition_pairs(&lower, &no_words, &mut pairs);
            sig.add_chars(&lower);
        }
        for p in pairs {
            let (w, bit) = bloom(p);
            sig.pairs[w] |= bit;
        }
        let mut lc = lower.chars();
        sig.len = lower.chars().count().min(255) as u8;
        sig.first = [typo_code(lc.next()), typo_code(lc.next())];
        sig
    }

    fn add_chars(&mut self, chars: &[char]) {
        for c in chars.iter().filter_map(|&c| pair_code(c)) {
            self.chars |= 1 << c;
        }
    }

    /// Little-endian: the character mask with the typo key in its spare high
    /// bits, then the Bloom filter.
    pub(crate) fn to_bytes(self) -> [u8; SIG_BYTES] {
        let head = self.chars
            | u64::from(self.len) << 40
            | u64::from(self.first[0]) << 48
            | u64::from(self.first[1]) << 56;
        let mut out = [0; SIG_BYTES];
        out[..8].copy_from_slice(&head.to_le_bytes());
        for (i, w) in self.pairs.iter().enumerate() {
            out[8 + 8 * i..16 + 8 * i].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub(crate) fn from_bytes(b: &[u8; SIG_BYTES]) -> Signature {
        let words = b.as_chunks::<8>().0;
        let word = |i: usize| u64::from_le_bytes(words[i]);
        let head = word(0);
        Signature {
            chars: head & ((1 << PAIR_CODES) - 1),
            pairs: [word(1), word(2), word(3), word(4)],
            len: (head >> 40) as u8,
            first: [(head >> 48) as u8, (head >> 56) as u8],
        }
    }
}

/// The near-miss side of a probe: what a name within two edits must share.
struct Typo {
    len: u8,
    first: [u8; 2],
    chars: u64,
}

/// A query compiled for the name index: what a signature must hold for the
/// name to be worth verifying, and the exact verification.
pub(crate) struct Probe {
    /// The lowercased leaf, as `score` compares it.
    q: String,
    glob: bool,
    chars: u64,
    pairs: [u64; 4],
    /// The query's letters and digits, lowercased, when all are ASCII: what
    /// [`score::aligns`] reads.
    ascii: Option<Vec<u8>>,
    typo: Option<Typo>,
}

impl Probe {
    /// `leaf` is the query's leaf name, as typed (a qualifier already split
    /// off, wildcards kept).
    pub(crate) fn new(leaf: &str) -> Probe {
        let q = score::lower(leaf).into_owned();
        let glob = score::has_wildcard(&q);
        let (mut chars, mut pairs) = (0u64, [0u64; 4]);
        let mut prev: Option<u8> = None;
        for c in q.chars() {
            if glob && matches!(c, '*' | '?') {
                prev = None; // a wildcard's gap is the user's, not a transition
                continue;
            }
            let Some(k) = pair_code(c) else { continue };
            chars |= 1 << k;
            if let Some(p) = prev {
                let (w, bit) = bloom(u16::from(p) * PAIR_CODES as u16 + u16::from(k));
                pairs[w] |= bit;
            }
            prev = Some(k);
        }
        let alnum: Vec<char> = q.chars().filter(|c| c.is_alphanumeric()).collect();
        let ascii = alnum.iter().all(char::is_ascii).then(|| {
            alnum
                .iter()
                .map(|&c| c.to_ascii_lowercase() as u8)
                .collect()
        });
        let len = q.chars().count();
        let typo = (!glob && len >= 4).then(|| {
            let mut qc = q.chars();
            Typo {
                len: len.min(255) as u8,
                first: [typo_code(qc.next()), typo_code(qc.next())],
                chars,
            }
        });
        Probe {
            q,
            glob,
            chars,
            pairs,
            ascii,
            typo,
        }
    }

    /// Could the name behind `sig` match? Never false for a name `accepts`
    /// takes; usually false for the rest.
    pub(crate) fn screen(&self, sig: &Signature) -> bool {
        self.screen_stem(sig) || self.typo.as_ref().is_some_and(|t| t.screen(sig))
    }

    /// [`Probe::screen`] for a file's stem, which a near miss never matches.
    pub(crate) fn screen_stem(&self, sig: &Signature) -> bool {
        let missing = (self.chars & !sig.chars)
            | (self.pairs[0] & !sig.pairs[0])
            | (self.pairs[1] & !sig.pairs[1])
            | (self.pairs[2] & !sig.pairs[2])
            | (self.pairs[3] & !sig.pairs[3]);
        missing == 0
    }

    /// Does `score` accept `name` on its name alone — exactly, as a prefix,
    /// without separators, as an alignment, a glob or a near miss? `sig` is
    /// the name's signature, which spares most names the near-miss check.
    pub(crate) fn accepts(&self, name: &str, sig: &Signature) -> bool {
        if self.glob {
            return score::wildcard_score(&self.q, name).is_some();
        }
        let ascii = Ascii::of(name);
        let lower = match &ascii {
            Some(n) => std::borrow::Cow::Borrowed(n.text()),
            None => score::lower(name),
        };
        lower.starts_with(self.q.as_str())
            || score::joiners_eq(&lower, &self.q)
            || self.aligns(name, ascii.as_ref())
            || (self.typo.as_ref().is_some_and(|t| t.screen(sig))
                && score::near_miss_distance(&self.q, &lower).is_some())
    }

    /// Does a file named `stem` answer the query — the path match `score`
    /// allows a file's primary definitions?
    pub(crate) fn accepts_stem(&self, stem: &str) -> bool {
        if self.glob {
            return score::wildcard_score(&self.q, stem).is_some();
        }
        self.aligns(stem, Ascii::of(stem).as_ref())
    }

    /// `align(..).is_some()`, bit-parallel when both sides are ASCII.
    fn aligns(&self, name: &str, ascii: Option<&Ascii>) -> bool {
        match (ascii, &self.ascii) {
            (Some(n), Some(q)) => score::aligns(q, n.text().as_bytes(), n.boundary),
            _ => score::subsequence_score(&self.q, name).is_some(),
        }
    }

    /// How well an accepted `name` matches, for choosing which names' rows a
    /// capped recall keeps: the glob or alignment value `score` gives it, and
    /// nothing for a near miss.
    pub(crate) fn rank(&self, name: &str) -> f64 {
        let value = if self.glob {
            score::wildcard_score(&self.q, name).map(|s| s.min(600.0))
        } else {
            score::fuzzy_value(&self.q, name)
        };
        value.unwrap_or(0.0)
    }
}

impl Typo {
    fn screen(&self, sig: &Signature) -> bool {
        let [q0, q1] = self.first;
        let [n0, n1] = sig.first;
        // at most two edits: a length within two, the first letter right or
        // swapped with the second, and at most two of the query's letters absent
        (sig.len == 255 || self.len.abs_diff(sig.len) <= 2)
            && (q0 == n0 || (q0 == n1 && q1 == n0))
            && (self.chars & !sig.chars).count_ones() <= 2
    }
}

/// An ASCII name of at most 128 bytes, lowercased on the stack with its word
/// starts as a bit mask: the form [`score::aligns`] reads.
struct Ascii {
    lower: [u8; 128],
    len: usize,
    boundary: u128,
}

impl Ascii {
    fn of(name: &str) -> Option<Ascii> {
        let b = name.as_bytes();
        if b.len() > 128 || !name.is_ascii() {
            return None;
        }
        let mut out = Ascii {
            lower: [0; 128],
            len: b.len(),
            boundary: 0,
        };
        // `score::boundaries` on ASCII, without collecting chars
        for (i, &c) in b.iter().enumerate() {
            out.lower[i] = c.to_ascii_lowercase();
            let starts = i == 0 || {
                let prev = b[i - 1];
                let upper = c.is_ascii_uppercase();
                !prev.is_ascii_alphanumeric()
                    || (upper && prev.is_ascii_lowercase())
                    || (upper
                        && prev.is_ascii_uppercase()
                        && b.get(i + 1).is_some_and(u8::is_ascii_lowercase))
            };
            out.boundary |= u128::from(starts) << i;
        }
        Some(out)
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.lower[..self.len]).expect("lowercased ASCII")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::score::{Boosts, score};
    use crate::store::SymbolRow;

    /// Does rq's own `score` accept `name` for `leaf`, on the name alone —
    /// first pass or typo retry? The file matches nothing, so the path can't.
    fn scored(leaf: &str, name: &str) -> bool {
        let row = SymbolRow {
            name: name.into(),
            kind: "function".into(),
            language: "rust".into(),
            file: String::new(),
            line: 1,
            end_line: None,
            parent: None,
            repository_id: 1,
            repo_identity: "r".into(),
            checkout_id: 1,
            root: "/tmp/x".into(),
            mtime: None,
            git_ts: None,
            visibility: None,
            stub: false,
            singleton: false,
            generated: false,
        };
        [false, true]
            .into_iter()
            .any(|near| score(leaf, &row, None, Boosts::default(), near).is_some())
    }

    /// Every way the probe and `score` could disagree about `leaf` over
    /// `names`: a name `score` takes that the screen or the verifier drops, or
    /// one the verifier takes that `score` doesn't.
    fn disagreements(leaf: &str, names: &[(String, Signature)]) -> Vec<String> {
        let probe = Probe::new(leaf);
        let mut out = Vec::new();
        for (name, sig) in names {
            let truth = scored(leaf, name);
            let screened = probe.screen(&Signature::from_bytes(&sig.to_bytes()));
            let verified = probe.accepts(name, sig);
            if truth != (screened && verified) || (verified && !screened) {
                out.push(format!(
                    "{leaf:?} vs {name:?}: score {truth}, screen {screened}, verify {verified}"
                ));
            }
        }
        out
    }

    /// The same for file stems: `score` takes a primary definition whose
    /// name can't match when its file's stem does.
    fn stem_disagreements(leaf: &str, stems: &[String]) -> Vec<String> {
        let probe = Probe::new(leaf);
        let mut out = Vec::new();
        for stem in stems {
            let row = SymbolRow {
                name: String::new(),
                kind: "class".into(),
                language: "rust".into(),
                file: format!("lib/{stem}.rb"),
                line: 1,
                end_line: None,
                parent: None,
                repository_id: 1,
                repo_identity: "r".into(),
                checkout_id: 1,
                root: "/tmp/x".into(),
                mtime: None,
                git_ts: None,
                visibility: None,
                stub: false,
                singleton: false,
                generated: false,
            };
            let truth = score(leaf, &row, None, Boosts::default(), true).is_some();
            let stem = score::path_stem(&row.file);
            let sig = Signature::from_bytes(&Signature::of(stem).to_bytes());
            let found = probe.screen_stem(&sig) && probe.accepts_stem(stem);
            if truth != found || (probe.accepts_stem(stem) && !probe.screen_stem(&sig)) {
                out.push(format!(
                    "{leaf:?} vs stem {stem:?}: score {truth}, index {found}"
                ));
            }
        }
        out
    }

    fn harness() -> Vec<(String, String)> {
        let tsv = include_str!("../../script/recall/queries.tsv");
        tsv.lines()
            .skip(1)
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f[1].to_string(), f.get(3).unwrap_or(&"").to_string())
            })
            .collect()
    }

    /// Names that stress the edges: acronyms, sigils, suffixes that aren't
    /// separators, digits, non-ASCII (including letters whose lowercase is two
    /// characters), and a name too long for the bit-parallel verifier.
    const EDGES: &[&str] = &[
        "HTTPParser",
        "getHTTPResponseCode",
        "parse_file",
        "save!",
        "empty?",
        "name=",
        "__init__",
        "_private",
        "@ivar",
        "$global",
        "i18n",
        "v2_api",
        "A1B2",
        "CONSTANT_VALUE",
        "value-with-dash",
        "dotted.name",
        "x",
        "ab",
        "ÉcoleNormale",
        "straße",
        "İstanbul",
        "naïve_café",
        "Ωmega",
        "日本語Name",
        "mixedCASEName",
        "Widget",
        "widget",
        "WidgetsController",
        "widget_controller",
    ];

    fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Queries a person might type for `name`: word initials, a prefix with a
    /// dropped letter, an adjacent swap, a glob, a case change.
    fn derived(name: &str, seed: &mut u64) -> Vec<String> {
        let chars: Vec<char> = name.chars().collect();
        let n = chars.len();
        let mut out = vec![name.to_uppercase(), name.to_lowercase()];
        let bounds = score::boundaries(&chars);
        out.push(
            (0..n)
                .filter(|&i| bounds[i])
                .flat_map(|i| chars[i..(i + 2).min(n)].iter())
                .collect(),
        );
        if n >= 3 {
            let i = (xorshift(seed) as usize) % (n - 1);
            let mut swapped = chars.clone();
            swapped.swap(i, i + 1);
            out.push(swapped.iter().collect());
            let drop = (xorshift(seed) as usize) % n;
            out.push(
                chars
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != drop)
                    .map(|(_, c)| c)
                    .collect(),
            );
            out.push(format!(
                "{}*{}",
                chars[..2].iter().collect::<String>(),
                chars[n - 2..].iter().collect::<String>()
            ));
            out.push(format!(
                "{}?{}",
                chars[0],
                chars[2..].iter().collect::<String>()
            ));
        }
        out
    }

    /// Every recall query, and queries derived from every name it targets.
    /// `cargo test` checks every `RQ_FUZZ_STRIDE`th (8) of each, since the
    /// corpus grows with every recall case; `make fuzz` sweeps all of them in
    /// release.
    #[test]
    fn the_index_takes_exactly_what_score_accepts() {
        let stride: usize = std::env::var("RQ_FUZZ_STRIDE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
            .max(1);
        let harness = harness();
        let mut words: Vec<String> = harness
            .iter()
            .map(|(_, source)| source.clone())
            .filter(|s| !s.is_empty())
            .chain(EDGES.iter().map(|s| s.to_string()))
            .collect();
        words.push("a".repeat(70) + "_" + &"b".repeat(70));
        words.sort();
        words.dedup();
        let names: Vec<(String, Signature)> = words
            .iter()
            .map(|n| (n.clone(), Signature::of(n)))
            .collect();

        let mut seed = 0x9E37_79B9_7F4A_7C15;
        // every harness query against its own source and a sample of the rest
        let mut bad = Vec::new();
        for (i, (query, source)) in harness.iter().enumerate().step_by(stride) {
            let leaf = score::parse_qualified(query).0;
            let mut sample: Vec<(String, Signature)> =
                names.iter().skip(i % 16).step_by(16).cloned().collect();
            if !source.is_empty() {
                sample.push((source.clone(), Signature::of(source)));
            }
            bad.extend(disagreements(leaf, &sample));
        }
        // and against file stems named like the sources
        let stems: Vec<String> = words.iter().step_by(4).cloned().collect();
        for (query, _) in harness.iter().step_by(8 * stride) {
            bad.extend(stem_disagreements(score::parse_qualified(query).0, &stems));
        }
        // queries derived from every name, against it and a sample of the rest
        let picked = words
            .iter()
            .enumerate()
            .filter(|&(i, w)| i % stride == 0 || EDGES.contains(&w.as_str()));
        for (i, w) in picked {
            let mut sample: Vec<(String, Signature)> =
                names.iter().skip(i % 8).step_by(8).cloned().collect();
            sample.push((w.clone(), Signature::of(w)));
            for q in derived(w, &mut seed) {
                bad.extend(disagreements(score::parse_qualified(&q).0, &sample));
            }
        }
        assert!(
            bad.is_empty(),
            "{} disagreements:\n{}",
            bad.len(),
            bad[..bad.len().min(20)].join("\n")
        );
    }

    /// Random names built from words that stress the normal form (non-ASCII
    /// case mappings, a ligature, the Kelvin sign, separators, the 128-byte
    /// signature edge), each against queries derived from it and a fixed-size
    /// sample of the rest. `make fuzz` runs it large, in release, with a
    /// fresh seed: `RQ_FUZZ_NAMES`, `RQ_FUZZ_SEED`.
    #[test]
    fn the_index_takes_exactly_what_score_accepts_on_random_names() {
        const WORDS: &[&str] = &[
            "get",
            "set",
            "http",
            "Parser",
            "URL",
            "id",
            "x",
            "widget",
            "café",
            "straße",
            "İd",
            "Ω",
            "日本",
            "v2",
            "a1b",
            "K\u{212A}",
            "ﬁle",
            "ΣΑΣ",
            "qq",
            "zz",
            "ab",
            "Io",
            "ioS",
            "HTML5",
            "to",
            "do",
        ];
        const SEPS: &[&str] = &["", "", "", "_", "-", ".", "?", "!", "="];
        let env = |key, default| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let count = env("RQ_FUZZ_NAMES", 300) as usize;
        let mut seed = env("RQ_FUZZ_SEED", 0x1234_5678_9ABC_DEF1);
        let pick = |n: usize, seed: &mut u64| (xorshift(seed) as usize) % n;

        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            let mut name = String::new();
            if pick(10, &mut seed) == 0 {
                name.push(['_', '@', '$'][pick(3, &mut seed)]);
            }
            for w in 0..1 + pick(6, &mut seed) {
                if w > 0 {
                    name.push_str(SEPS[pick(SEPS.len(), &mut seed)]);
                }
                let word = WORDS[pick(WORDS.len(), &mut seed)];
                match pick(4, &mut seed) {
                    0 => name.push_str(&word.to_uppercase()),
                    1 => {
                        let mut chars = word.chars();
                        name.extend(chars.next().into_iter().flat_map(char::to_uppercase));
                        name.push_str(chars.as_str());
                    }
                    _ => name.push_str(word),
                }
            }
            if pick(8, &mut seed) == 0 {
                let target = 124 + pick(9, &mut seed);
                while name.len() < target {
                    name.push_str(["a", "B", "_c", "9"][pick(4, &mut seed)]);
                }
            }
            names.push((name.clone(), Signature::of(&name)));
        }

        let step = (count / 16).max(1);
        let mut bad = Vec::new();
        for (i, (name, sig)) in names.iter().enumerate() {
            let mut sample: Vec<(String, Signature)> =
                names.iter().skip(i % step).step_by(step).cloned().collect();
            sample.push((name.clone(), *sig));
            let chars: Vec<char> = name.chars().collect();
            let mut queries = derived(name, &mut seed);
            for _ in 0..3 {
                let mut at: Vec<usize> = (0..2 + pick(7, &mut seed))
                    .map(|_| pick(chars.len(), &mut seed))
                    .collect();
                at.sort();
                at.dedup();
                queries.push(at.iter().map(|&j| chars[j]).collect());
            }
            if chars.len() >= 4 {
                let mut typo = chars.clone();
                for _ in 0..2 {
                    typo[1 + pick(chars.len() - 1, &mut seed)] = 'q';
                }
                queries.push(typo.iter().collect());
                let mut extra = chars.clone();
                extra.insert(1 + pick(chars.len() - 1, &mut seed), 'z');
                queries.push(extra.iter().collect());
            }
            for q in queries.iter().filter(|q| !q.contains('#')) {
                let leaf = score::parse_qualified(q).0;
                if !leaf.is_empty() {
                    bad.extend(disagreements(leaf, &sample));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "{} disagreements:\n{}",
            bad.len(),
            bad[..bad.len().min(20)].join("\n")
        );
    }

    /// The same check over a real index, every harness query against every
    /// name in its repo: `RQ_NAME_INDEX_DB=<db with rails and discourse>`,
    /// indexed at the recall pins (docs/RECALL.md). Minutes in a debug build.
    #[test]
    #[ignore = "needs RQ_NAME_INDEX_DB; run with --release --ignored"]
    fn the_index_takes_exactly_what_score_accepts_on_a_real_index() {
        let db = std::env::var("RQ_NAME_INDEX_DB").expect("RQ_NAME_INDEX_DB");
        let conn = rusqlite::Connection::open(db).unwrap();
        let names_of = |repo: &str| -> Vec<(String, Signature)> {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT s.name FROM symbols s \
                     JOIN repositories r ON r.id = s.repository_id WHERE r.identity = ?1",
                )
                .unwrap();
            let rows = stmt.query_map([format!("github.com/{repo}")], |r| r.get(0));
            rows.unwrap()
                .map(Result::unwrap)
                .map(|n: String| (n.clone(), Signature::of(&n)))
                .collect()
        };
        let corpora = [
            ("rails", names_of("rails/rails")),
            ("discourse", names_of("discourse/discourse")),
        ];
        let harness = harness();
        let tsv = include_str!("../../script/recall/queries.tsv");
        let repos: Vec<&str> = tsv
            .lines()
            .skip(1)
            .map(|l| l.split('\t').next().unwrap())
            .collect();
        let bad = std::sync::Mutex::new(Vec::new());
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        std::thread::scope(|scope| {
            for t in 0..threads {
                let (harness, repos, corpora, bad) = (&harness, &repos, &corpora, &bad);
                scope.spawn(move || {
                    for (i, (query, _)) in harness.iter().enumerate().skip(t).step_by(threads) {
                        // the harness spans more corpora than these two
                        let corpus = repos
                            .get(i)
                            .and_then(|repo| corpora.iter().find(|(r, _)| r == repo));
                        let Some((_, names)) = corpus else { continue };
                        let found = disagreements(score::parse_qualified(query).0, names);
                        bad.lock().unwrap().extend(found);
                    }
                });
            }
        });
        let bad = bad.into_inner().unwrap();
        let n: usize = corpora.iter().map(|(_, n)| n.len()).sum();
        assert!(n > 70_000, "expected rails and discourse, got {n} names");
        assert!(
            bad.is_empty(),
            "{} disagreements:\n{}",
            bad.len(),
            bad[..bad.len().min(20)].join("\n")
        );
    }

    #[test]
    fn a_signature_survives_its_bytes() {
        for name in EDGES {
            let sig = Signature::of(name);
            assert_eq!(Signature::from_bytes(&sig.to_bytes()), sig, "{name}");
        }
    }

    #[test]
    fn the_screen_rejects_most_names() {
        // a query with letters in the wrong transitions is never verified
        let probe = Probe::new("wdgctl");
        assert!(probe.screen(&Signature::of("WidgetController")));
        assert!(!probe.screen(&Signature::of("ControllerWidget")));
        assert!(!probe.screen(&Signature::of("parse_file")));
    }
}
