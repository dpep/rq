//! The command line: flags, and parsing the values they take.

use super::*;

/// How results are rendered.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Output {
    Text,
    Json,
    Ndjson,
}

pub(super) fn output_format(cli: &Cli) -> Output {
    if cli.ndjson {
        Output::Ndjson
    } else if cli.json {
        Output::Json
    } else {
        Output::Text
    }
}

/// Results shown when `--limit` isn't given.
pub(super) const DEFAULT_LIMIT: usize = 10;

/// Minimum headroom to rank before a `--path` filter (so filtered-in results
/// aren't lost to the cutoff).
pub(super) const PATH_HEADROOM: usize = 200;

/// `--limit 0` means unlimited: every ranked hit, bounded only by how many
/// candidates recall returned.
pub(super) fn requested_limit(limit: usize) -> usize {
    if limit == 0 { usize::MAX } else { limit }
}

/// A parsed `--anchor FILE:LINE[:COL]`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AnchorSpec {
    pub(super) file: PathBuf,
    pub(super) line: i64,
}

/// Parse `FILE:LINE` or `FILE:LINE:COL` (1-based), splitting from the right so
/// a path holding a colon still parses.
pub(super) fn parse_anchor(s: &str) -> Result<AnchorSpec, String> {
    let num = |t: &str| t.parse::<i64>().ok().filter(|n| *n > 0);
    let bad = || format!("expected FILE:LINE[:COL], got {s:?}");
    let (rest, last) = s.rsplit_once(':').ok_or_else(bad)?;
    let last = num(last).ok_or_else(bad)?;
    // `FILE:LINE:COL` when what precedes the last number is itself a number
    let (file, line) = rest
        .rsplit_once(':')
        .and_then(|(file, line)| num(line).map(|line| (file, line)))
        .unwrap_or((rest, last));
    if file.is_empty() {
        return Err(bad());
    }
    Ok(AnchorSpec {
        file: PathBuf::from(file),
        line,
    })
}

/// Parse a `--wait` value into a duration: `<n>ms`, `<n>s`, `<n>m`, or a bare
/// `<n>` (seconds). Fractions are allowed (`1.5s`); `0` (any unit) means "don't
/// wait". A `clap` value parser, so an invalid duration is rejected at parse
/// time with a usage error.
pub(super) fn parse_wait(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("invalid duration {s:?} — use e.g. 50ms, 2s, 1m, or 0");
    // check "ms" before "s" so the "s" arm doesn't swallow it
    let (num, unit_ms) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1_000.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000.0)
    } else {
        // a bare number is seconds
        (s, 1_000.0)
    };
    let val: f64 = num.trim().parse().map_err(|_| bad())?;
    if !val.is_finite() || val < 0.0 {
        return Err(bad());
    }
    Ok(Duration::from_millis((val * unit_ms).round() as u64))
}

/// A leading positional that names a symbol kind — the shorthand behind
/// `rq class Foo` and `rq method zoom`. Only the full, unambiguous keyword forms
/// count (never the single-letter `-k` shortcuts, which are far likelier to be a
/// real query). Returns the canonical kind, so it filters exactly like `--kind`.
pub(super) fn keyword_kind(token: &str) -> Option<&'static [&'static str]> {
    let word = token.to_ascii_lowercase();
    // a kind's own name, or the keyword a language declares it with
    let keyword =
        Kind::from_tag(&word).is_some() || matches!(word.as_str(), "fn" | "interface" | "const");
    keyword.then(|| canonical_kind(&word)).flatten()
}

/// Peel a leading kind keyword off the query, so `rq class Foo` (or the quoted
/// `rq 'class Foo'`) means `-k class` + query `Foo`. The keyword must be followed
/// by a real query token — a bare `rq class` stays a search for a symbol literally
/// named `class`. Returns `(kind, query, trailing_path_dirs)`; the trailing dirs
/// are the rg-style positionals left after the query is consumed.
pub(super) fn split_kind_keyword(
    target: String,
    dirs: Vec<String>,
) -> (Option<&'static [&'static str]>, String, Vec<String>) {
    // Quoted form: the whole thing is one arg (`"class Foo"`), so peel the first
    // whitespace-separated word and keep the remainder as the query.
    if let Some((head, rest)) = target.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if let Some(k) = keyword_kind(head)
            && !rest.is_empty()
        {
            return (Some(k), rest.to_string(), dirs);
        }
    } else if let Some(k) = keyword_kind(&target)
        && let Some((query, extra)) = dirs.split_first()
    {
        // Unquoted form: `rq class Foo` — the next positional is the query.
        return (Some(k), query.clone(), extra.to_vec());
    }
    (None, target, dirs)
}

/// Normalize a `--kind` value (name or shortcut) to the canonical symbol kinds
/// it selects; `None` for an unknown one, which is a usage error.
pub(super) fn canonical_kind(s: &str) -> Option<&'static [&'static str]> {
    Some(match s.to_ascii_lowercase().as_str() {
        "c" | "class" => &["class"],
        "m" | "method" => &["method"],
        "f" | "fn" | "func" | "function" => &["function"],
        "mod" | "module" => &["module"],
        "s" | "struct" => &["struct"],
        // `type` is any named type: an alias, and the structs that languages
        // declare with the same keyword (Go's `type Foo struct`)
        "type" => &["type", "struct"],
        "alias" | "type_alias" => &["type"],
        "e" | "enum" => &["enum"],
        "t" | "trait" | "interface" => &["trait"],
        "const" | "constant" => &["constant"],
        "macro" | "macro_rules" => &["macro"],
        "v" | "variant" | "member" | "enum_member" => &["variant"],
        "field" | "property" | "prop" => &["field"],
        _ => return None,
    })
}

/// Expand a `--lang` value to the language tag(s) it selects: a **prefix** of any
/// known language name (so `r` → ruby+rust, `p`/`py` → python, `g` → go,
/// `t` → typescript, `j` → javascript), or one of a plugin's extensions
/// (`rb`, `tsx`) or aliases (`golang`). An unknown value selects none, which
/// is a usage error — the empty one too (`-x ruby,`), though it prefixes all.
pub(super) fn canonical_langs(s: &str) -> Vec<String> {
    let t = s.to_ascii_lowercase();
    if t.is_empty() {
        return Vec::new();
    }
    crate::lang::registry()
        .iter()
        .filter(|p| {
            p.language().starts_with(&t)
                || p.extensions().contains(&t.as_str())
                || p.aliases().contains(&t.as_str())
        })
        .map(|p| p.language().to_string())
        .collect()
}

/// The output mode argv asks for, without a full parse: `--json`/`--ndjson`,
/// or `-j`/`-J` alone or in a cluster of short flags (`-ej`). A cluster ends at
/// the first flag that takes a value, since the rest is that value (`-xj` is
/// `--lang j`). Nothing after `--` is a flag.
pub(super) fn requested_output(args: impl IntoIterator<Item = std::ffi::OsString>) -> Output {
    let cmd = Cli::command();
    let takes_value = |c: char| {
        cmd.get_arguments()
            .any(|a| a.get_short() == Some(c) && a.get_action().takes_values())
    };
    let (mut json, mut ndjson) = (false, false);
    for arg in args {
        let arg = arg.to_string_lossy();
        match arg.as_ref() {
            "--" => break,
            "--json" => json = true,
            "--ndjson" => ndjson = true,
            a if a.starts_with('-') && !a.starts_with("--") => {
                for c in a.chars().skip(1) {
                    match c {
                        'j' => json = true,
                        'J' => ndjson = true,
                        c if takes_value(c) => break,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // the same precedence `output_format` gives a parsed command line
    if ndjson {
        Output::Ndjson
    } else if json {
        Output::Json
    } else {
        Output::Text
    }
}
