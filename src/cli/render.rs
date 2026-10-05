//! Printing results to a terminal: highlighted rows, `--show`'s body, and the progress line.

use super::*;

/// Print the ranked results (JSON array, NDJSON lines, or highlighted text).
/// `unshown`: a `--show` found no single confident match to print instead.
pub(super) fn render_hits(
    args: &SearchArgs,
    hits: &[crate::search::Hit],
    unshown: bool,
) -> Result<(), Failure> {
    // Time to the first printed result, not to the last: rq streams, and the
    // sub-50 ms budget is about the first answer. A change that speeds the
    // total while delaying this one is a regression.
    let render_span = crate::profile::span("render");
    if args.batch {
        // One stream, many questions: tag each row with the query it answers,
        // the same way `no_match_code` already tags a miss.
        #[derive(serde::Serialize)]
        struct Tagged<'a> {
            query: &'a str,
            #[serde(flatten)]
            hit: &'a crate::search::Hit,
        }
        let rows: Vec<Tagged> = hits
            .iter()
            .map(|hit| Tagged {
                query: args.query,
                hit,
            })
            .collect();
        emit_rows(args.out, &rows)?;
    } else {
        emit_rows(args.out, hits)?;
    }
    if args.out != Output::Text {
        return Ok(());
    }
    drop(render_span);
    let color = match_color();
    let c = color.as_deref();
    let query = args.query;
    if unshown {
        // fell through from --show: no single confident match to print
        let total = hits.first().map_or(hits.len(), |h| h.total);
        eprintln!(
            "rq: no single confident match for {query:?} — {} of {total} candidates below; narrow the query to --show one",
            hits.len()
        );
    }
    for hit in hits {
        // highlight the chars the query matched — in the name, the
        // filename, and the definition line (great for fuzzy matches)
        let name = hl(&hit.name, query, c);
        let qualified = match &hit.parent {
            Some(p) => format!("{name} · {p}"),
            None => name,
        };
        println!(
            "{}:{}  {} {}",
            hl_path(&hit.file, query, c),
            hit.line,
            kind_label(&hit.kind, hit.singleton),
            qualified
        );
        if let Some(sig) = &hit.signature {
            println!("    {}", hl(sig, query, c));
        }
        if args.explain {
            let parts: Vec<String> = hit
                .features
                .iter()
                .map(|f| format!("{} {}", f.name, f.reported()))
                .collect();
            println!(
                "    confidence {:.2} · score {:.0} = {}",
                hit.confidence,
                hit.score,
                parts.join(" + ")
            );
        }
    }
    Ok(())
}

/// Is a human watching stderr? True for a real terminal; `RQ_ASSUME_INTERACTIVE`
/// forces it on so the progress/Ctrl-C path is exercisable under test (where
/// stderr is a pipe), mirroring the `RQ_*_BUDGET_MS` testing knobs.
pub(super) fn stderr_interactive() -> bool {
    std::io::stderr().is_terminal() || std::env::var_os("RQ_ASSUME_INTERACTIVE").is_some()
}

/// Whether to show the live "indexing…" progress heads-up and handle Ctrl-C
/// gracefully while a cold repo blocks — a human watching a plain-text terminal.
/// Piped / `--json` / `--ndjson` callers block silently instead (no line to draw,
/// no one to interrupt); the *decision to block* is the same for both.
pub(super) fn show_progress(out: Output, interactive: bool) -> bool {
    interactive && matches!(out, Output::Text)
}

/// A short, friendly name for the repo being indexed — its directory name, for
/// the progress line.
pub(super) fn repo_label(root: Option<&std::path::Path>) -> String {
    root.and_then(|r| r.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// Redraw the in-place "indexing…" progress line on stderr (kept off stdout so
/// piped/`--json` output stays clean). The file count comes from the index the
/// background pass is filling, so it climbs as warming proceeds.
pub(super) fn draw_progress(store: &Store, checkout: Option<Checkout>, label: &str) {
    let files = checkout
        .and_then(|c| store.checkout_totals(c.id).ok())
        .map_or(0, |(f, _)| f);
    eprint!("\r\x1b[Krq: indexing {label}… {files} files");
    let _ = std::io::stderr().flush();
}

/// Erase the progress line so results print to a clean terminal.
pub(super) fn clear_progress() {
    eprint!("\r\x1b[K");
    let _ = std::io::stderr().flush();
}

/// The definition's source line (trimmed) at `line` of `path`. Best-effort.
pub(super) fn read_signature(path: &std::path::Path, line: i64) -> Option<String> {
    let src = crate::index::read_source(path).ok()?;
    signature_in(&src.lines().collect::<Vec<_>>(), line)
}

/// Confidence at or above which `--show` prints a body instead of a list. Exact
/// (1.0) and a unique prefix (0.9) clear it; a fuzzy or tied match does not — so
/// `--show` never prints a definition it isn't sure about.
const SHOW_CONFIDENCE: f64 = 0.85;

/// `--show`: if the top hit is confident, read and print its full source span
/// and return the exit code; otherwise return `None` to fall through to the
/// ranked list. Emits a single object in JSON/NDJSON (with a `body` field).
pub(super) fn show_top_definition(
    hits: &mut [crate::search::Hit],
    query: &str,
    out: Output,
    ranked: f64,
) -> Option<Result<(), Failure>> {
    let top = hits.first()?;
    if ranked < SHOW_CONFIDENCE {
        return None; // ambiguous / weak — let the caller list candidates
    }
    let end = top.end_line.unwrap_or(top.line);
    let body = top.root.as_deref().and_then(|root| {
        let src = crate::index::read_source(&std::path::Path::new(root).join(&top.file)).ok()?;
        span_in(&src, top.line, end)
    });
    hits[0].body = body;
    let top = &hits[0];
    let code = match out {
        Output::Json | Output::Ndjson => {
            // fail loudly on a serialize error, like every other JSON path
            emit_json(out, top)
        }
        Output::Text => {
            let color = match_color();
            let c = color.as_deref();
            let name = hl(&top.name, query, c);
            let qualified = match &top.parent {
                Some(p) => format!("{name} · {p}"),
                None => name,
            };
            println!(
                "{}:{}  {} {}",
                hl_path(&top.file, query, c),
                top.line,
                kind_label(&top.kind, top.singleton),
                qualified
            );
            match (&top.body, &top.signature) {
                (Some(body), _) => println!("{body}"),
                // end_line unknown (pre-v4 row) → at least the definition line
                (None, Some(sig)) => println!("{sig}"),
                (None, None) => {}
            }
            Ok(())
        }
    };

    Some(code)
}

/// Lines `start..=end` (1-based, inclusive) of already-read `content`, joined —
/// clamped to the file's bounds. `None` if `start` is past the end.
fn span_in(content: &str, start: i64, end: i64) -> Option<String> {
    let s = usize::try_from(start).ok()?.checked_sub(1)?;
    let lines: Vec<&str> = content.lines().collect();
    if s >= lines.len() {
        return None;
    }
    let e = usize::try_from(end).ok()?.clamp(s + 1, lines.len());
    Some(lines[s..e].join("\n"))
}

/// The trimmed source line `line` (1-based) of a file already split into
/// `lines`, if non-empty — a symbol's definition line. Takes the split rather
/// than the text so `--symbols` splits once: re-scanning from the top per
/// symbol is quadratic in a large file.
pub(super) fn signature_in(lines: &[&str], line: i64) -> Option<String> {
    let idx = usize::try_from(line).ok()?.checked_sub(1)?;
    let l = lines.get(idx)?.trim();
    (!l.is_empty()).then(|| l.to_string())
}

/// A result's kind as text prints it: a type's own member (a class method, a
/// `static` one) says so, as JSON's `singleton` does.
pub(super) fn kind_label(kind: &str, singleton: bool) -> std::borrow::Cow<'_, str> {
    if singleton {
        format!("singleton {kind}").into()
    } else {
        kind.into()
    }
}

/// The ANSI SGR code for highlighting matches, or `None` to disable color.
/// Off unless stdout is a terminal; honors `NO_COLOR`; takes the match style
/// from `GREP_COLORS` (`mt`/`ms`) when set, else grep's default bold red.
fn match_color() -> Option<String> {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        return None;
    }
    let style = std::env::var("GREP_COLORS").ok().and_then(|gc| {
        gc.split(':').find_map(|e| {
            e.strip_prefix("mt=")
                .or_else(|| e.strip_prefix("ms="))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
    });
    Some(style.unwrap_or_else(|| "1;31".to_string()))
}

/// Highlight the chars of `text` that `query` matched (no-op when `color` is
/// `None`, e.g. piped output).
fn hl(text: &str, query: &str, color: Option<&str>) -> String {
    match color {
        Some(c) => highlight(text, &crate::search::match_positions(query, text), c),
        None => text.to_string(),
    }
}

/// Like [`hl`], but only over a path's filename — so matched chars light up in
/// `payrolls_controller.rb`, not scattered across the directory parts.
pub(super) fn hl_path(path: &str, query: &str, color: Option<&str>) -> String {
    let Some(c) = color else {
        return path.to_string();
    };
    let base_byte = path.rfind('/').map(|b| b + 1).unwrap_or(0);
    let base_start = path[..base_byte].chars().count();
    // align on the filename *stem* (drop the extension), the same string the
    // scorer matched — so the query can't straggle into `.rb` instead of lighting
    // up the logical name (`employees_controller`)
    let stem = crate::search::path_stem(path);
    let positions: Vec<usize> = crate::search::match_positions(query, stem)
        .into_iter()
        .map(|p| p + base_start)
        .collect();
    highlight(path, &positions, c)
}

/// Wrap the matched character positions of `text` in an ANSI color run.
/// Consecutive matched chars share one escape sequence.
pub(super) fn highlight(text: &str, positions: &[usize], color: &str) -> String {
    if positions.is_empty() {
        return text.to_string();
    }
    let matched: std::collections::HashSet<usize> = positions.iter().copied().collect();
    let mut out = String::new();
    let mut on = false;
    for (i, c) in text.chars().enumerate() {
        match (matched.contains(&i), on) {
            (true, false) => {
                out.push_str("\x1b[");
                out.push_str(color);
                out.push('m');
                on = true;
            }
            (false, true) => {
                out.push_str("\x1b[0m");
                on = false;
            }
            _ => {}
        }
        out.push(c);
    }
    if on {
        out.push_str("\x1b[0m");
    }
    out
}
