//! `rq --symbols FILE`: a file's outline.

use super::*;

/// One symbol in `rq --symbols` output. Same field names as a search hit
/// (`repo`, `signature`) for agent consistency, but no score/features — an
/// outline is structural, not ranked.
#[derive(serde::Serialize)]
struct SymbolOut {
    name: String,
    kind: String,
    language: String,
    file: String,
    /// Absolute checkout root `file` is relative to, as on a search hit.
    root: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    singleton: bool,
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

/// `rq --symbols <file>`: list a file's symbols in line order — a structural
/// outline, not a ranked search. Warms the file's repo if it's cold/incomplete or
/// changed (same gate as search), then reads straight from the index. Honors
/// --kind/--lang filters and --json/--ndjson.
pub(super) fn cmd_symbols(
    file_arg: &str,
    kinds: &[String],
    langs: &[String],
    out: Output,
) -> ExitCode {
    let open_span = crate::profile::span("store open");
    let mut store = match open_store_or_fail(out, "rq --symbols") {
        Ok(s) => s,
        Err(code) => return code,
    };
    drop(open_span);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let here = Here::at(&store, &cwd);
    let warming_ok = here.warms();
    let Here {
        root,
        is_git,
        coverage,
        checkout: current,
    } = here;
    let rel = repo_relative(&root, &cwd, file_arg);
    // Outside git, a file the index doesn't hold may be one its walk skips
    // (`.ignore`d): stored, it would read as a tree change on every miss. So
    // it's read live, and a new one is left to the walk.
    let stored = |store: &Store, checkout: Checkout| {
        is_git || matches!(store.file_mtime(checkout.id, &rel), Ok(Some(_)))
    };
    let path = root.join(&rel);
    if !path.is_file() {
        return fail(
            out,
            Failure::NotFound,
            format_args!("rq --symbols: no such file: {file_arg}"),
        );
    }
    let indexable = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| crate::lang::plugin_for_extension(e).is_some());
    if indexable && crate::index::oversized(&path) {
        // "no symbols" would claim it was read
        return fail(
            out,
            Failure::Usage,
            format_args!(
                "rq --symbols: {file_arg} is over the 64 MiB size cap; rq doesn't parse it"
            ),
        );
    }
    match current {
        // An outline depends on this one file, so on a complete index freshness
        // is just re-extracting it if it moved — no `git status` over the whole
        // worktree, and a new untracked file is picked up too.
        Some(checkout) if coverage == Some(Coverage::Complete) => {
            if indexable && stored(&store, checkout) {
                let _span = crate::profile::span("symbols: refresh");
                let _ = crate::index::refresh_file(&mut store, checkout, &root, &rel);
            }
        }
        // Not fully indexed yet: warm synchronously — there's no answer to get
        // out of the way of here — with this file as an active one, so it's
        // indexed first whatever the budget.
        _ if warming_ok => {
            let budget = answer_warm_budget() + deferred_warm_budget();
            let _span = crate::profile::span("symbols: warm");
            let active = [rel.clone()];
            let _ = crate::index::index_budgeted(&mut store, &root, &active, budget, None);
        }
        _ => {}
    }

    let mut query_span = crate::profile::span("symbols: query");
    let rows = match checkout_at(&store, &root) {
        Some(checkout) if stored(&store, checkout) => store.symbols_in_file(checkout.id, &rel),
        // a dir rq doesn't warm, or a file it doesn't hold: read the file
        // live, as an anchor is
        _ => {
            let identity = resolve_identity(&store, &root);
            let mut defs = crate::index::current_definitions(&store, None, &identity, &root, &rel);
            defs.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
            Ok(defs)
        }
    };
    let mut rows = match rows {
        Ok(r) => r,
        Err(e) => return fail(out, Failure::Database, format_args!("rq --symbols: {e}")),
    };
    query_span.note(|| format!("{} rows", rows.len()));
    drop(query_span);
    if !kinds.is_empty() {
        rows.retain(|r| kinds.iter().any(|k| k == &r.kind));
    }
    if !langs.is_empty() {
        rows.retain(|r| langs.iter().any(|l| l == &r.language));
    }

    // Read the source once for signatures (every row is the same file), from
    // the checkout we're in — it's the one the outline was refreshed from.
    let signatures_span = crate::profile::span("symbols: signatures");
    let content = crate::index::read_source(&path).ok();
    let lines: Vec<&str> = content
        .as_deref()
        .map_or_else(Vec::new, |c| c.lines().collect());
    let syms: Vec<SymbolOut> = rows
        .into_iter()
        .map(|r| SymbolOut {
            signature: signature_in(&lines, r.line),
            name: r.name,
            kind: r.kind,
            language: r.language,
            file: r.file,
            root: r.root,
            line: r.line,
            end_line: r.end_line,
            parent: r.parent,
            visibility: r.visibility,
            singleton: r.singleton,
            repo: r.repo_identity,
        })
        .collect();
    drop(signatures_span);
    let _span = crate::profile::span("render");
    emit_symbols(out, &syms)
}

/// Render the outline. Exit 0 if any symbols, non-zero if none — rq's exit-code
/// convention, matching how search reports an empty result per format.
fn emit_symbols(out: Output, syms: &[SymbolOut]) -> ExitCode {
    if syms.is_empty() {
        match out {
            Output::Json | Output::Ndjson => {
                #[derive(serde::Serialize)]
                struct NoSymbols {
                    status: &'static str,
                }
                let obj = NoSymbols {
                    status: Verdict::Miss.status(),
                };
                let _ = emit_json(out, &obj); // exit code below carries the miss
            }
            Output::Text => eprintln!("no symbols"),
        }
        return Outcome::Miss.into();
    }
    if let Err(failed) = emit_rows(out, syms) {
        return failed.into();
    }
    match out {
        Output::Json | Output::Ndjson => {}
        Output::Text => {
            for s in syms {
                let qualified = match &s.parent {
                    Some(p) => format!("{} · {p}", s.name),
                    None => s.name.clone(),
                };
                let kind = kind_label(&s.kind, s.singleton);
                println!("{}:{}  {kind} {qualified}", s.file, s.line);
                if let Some(sig) = &s.signature {
                    println!("    {sig}");
                }
            }
        }
    }
    ExitCode::SUCCESS
}

/// Resolve a possibly-absolute or cwd-relative path to a repo-relative one.
pub(super) fn repo_relative(root: &std::path::Path, cwd: &std::path::Path, file: &str) -> String {
    let p = std::path::Path::new(file);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    crate::index::rel_key(root, &abs).unwrap_or_else(|| file.to_string())
}
