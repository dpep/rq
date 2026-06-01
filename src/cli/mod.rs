//! Command-line surface. Search is the default action: `rq <query>`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::store::Store;

/// Search is the default action (`rq <query>`). Operations are flags rather
/// than subcommands so no word is reserved — `rq index`, `rq status`, and
/// `rq record` all search for those symbols. This also matches the rg/fd feel.
#[derive(Parser)]
#[command(
    name = "rq",
    version,
    about = "Reference Query — find the code you're looking for.",
    long_about = "Reference Query (rq) finds the code you're looking for. It ranks aggressively \
to surface the one definition you most likely want, rather than listing every match.\n\n\
Search is the default action — operations are flags, not subcommands, so every \
word (including \"index\", \"status\", \"record\") stays searchable. Ranking \
learns from the results you open (see RECORDING below) and favors your current \
repo and recently-active files. Run `rq <query> --explain` to see the score \
behind each result.",
    after_help = "EXAMPLES:\n  \
rq thing                  search for a definition named or like \"thing\"\n  \
rq wibble --explain       same, plus the score behind each result\n  \
rq thing --json           machine-readable results (for editors/agents)\n  \
rq thing app/web          restrict to a directory (rg-style)\n  \
rq perform -k method      restrict to a symbol kind (c/mod/m/f)\n  \
rq --index                index the current repository\n  \
rq --status               show indexing coverage\n\n\
RECORDING (editor/shell hook):\n  \
rq --record --file <path> --line <n> <query>\n  \
Tells rq which result you opened for a query, so ranking learns. Editors and \
the script/rq-open wrapper call this for you.\n\n\
The index is a SQLite file at $RQ_DB (default ~/.local/share/rq/rq.db); it warms \
automatically on the first search in a git repo."
)]
struct Cli {
    /// Search query. With --index, the path to index; with --record, the query
    /// the selection was made for.
    #[arg(value_name = "TARGET")]
    target: Option<String>,

    /// Directories to restrict results to (rg-style; same as repeated --path).
    #[arg(value_name = "PATH")]
    dirs: Vec<String>,

    /// Show the score breakdown for each result.
    #[arg(short = 'e', long)]
    explain: bool,

    /// Don't record this search as a behavioral signal (for agents/scripts).
    #[arg(long)]
    no_record: bool,

    /// Emit results as a JSON array (for editors and scripts).
    #[arg(short = 'j', long)]
    json: bool,

    /// Emit results as newline-delimited JSON, one object per line.
    #[arg(short = 'J', long, conflicts_with = "json")]
    ndjson: bool,

    /// Restrict results to files under this repo-relative directory (repeatable).
    #[arg(short = 'p', long, value_name = "DIR")]
    path: Vec<String>,

    /// Maximum number of results to show.
    #[arg(short = 'l', long, value_name = "N", default_value_t = 10)]
    limit: usize,

    /// Restrict to symbol kinds: class, module, method, function
    /// (shortcuts: c, mod, m, f). Repeatable or comma-separated.
    #[arg(short = 'k', long, value_name = "KIND", value_delimiter = ',')]
    kind: Vec<String>,

    /// Index a repository (TARGET path, or the current directory).
    #[arg(long, conflicts_with_all = ["status", "record"])]
    index: bool,

    /// Show indexing coverage per known repository.
    #[arg(long, conflicts_with_all = ["index", "record"])]
    status: bool,

    /// Record an interaction (editor/shell hook): the result opened for a query.
    /// Requires --file.
    #[arg(long, requires = "file", conflicts_with_all = ["index", "status"])]
    record: bool,

    /// (--record) File that was opened/selected.
    #[arg(long)]
    file: Option<String>,

    /// (--record) Line landed on (attributes the selection to a definition).
    #[arg(long)]
    line: Option<i64>,

    /// (--record) Event kind (select or open).
    #[arg(long, default_value = "select")]
    event: String,

    /// Print a shell completion script (bash, zsh, fish, elvish, powershell).
    #[arg(long, value_name = "SHELL")]
    completions: Option<Shell>,
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "rq", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }
    if cli.index {
        // index TARGET (else cwd); with --path, only those subtrees (partial)
        return cmd_index(cli.target.map(PathBuf::from), &cli.path);
    }
    if cli.status {
        return cmd_status();
    }
    if cli.record {
        // clap guarantees --file is present via `requires`
        let file = cli.file.expect("--record requires --file");
        return cmd_record(&cli.event, cli.target.as_deref(), &file, cli.line);
    }
    let out = output_format(&cli);
    // path filters: trailing positionals (rg-style) plus any --path flags
    let mut paths = cli.path.clone();
    paths.extend(cli.dirs.clone());
    let kinds: Vec<String> = cli.kind.iter().map(|k| canonical_kind(k)).collect();
    match cli.target {
        Some(query) => cmd_search(
            &query,
            cli.explain,
            out,
            &paths,
            &kinds,
            cli.limit,
            cli.no_record,
        ),
        // bare `rq` (or just flags like --explain with no query): show help
        None => {
            let _ = Cli::command().print_long_help();
            ExitCode::SUCCESS
        }
    }
}

/// How results are rendered.
#[derive(Clone, Copy, PartialEq)]
enum Output {
    Text,
    Json,
    Ndjson,
}

fn output_format(cli: &Cli) -> Output {
    if cli.ndjson {
        Output::Ndjson
    } else if cli.json {
        Output::Json
    } else {
        Output::Text
    }
}

/// Minimum headroom to rank before a `--path` filter (so filtered-in results
/// aren't lost to the cutoff).
const PATH_HEADROOM: usize = 200;

/// Default action: search the index and print ranked results. `want` is the
/// number of results to show (`--limit`).
#[allow(clippy::too_many_arguments)]
fn cmd_search(
    query: &str,
    explain: bool,
    out: Output,
    paths: &[String],
    kinds: &[String],
    want: usize,
    no_record: bool,
) -> ExitCode {
    // post-filters (--path, --kind) need headroom before the cutoff so a
    // filtered-in result isn't lost to the top-N truncation
    let limit = if paths.is_empty() && kinds.is_empty() {
        want
    } else {
        (want * 20).max(PATH_HEADROOM)
    };
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = std::env::current_dir().ok();
    let cwd_is_git = cwd.as_deref().is_some_and(crate::index::is_git_repo);

    // Layer 5: opportunistically index the working repo the first time we see
    // it, so the index warms through normal use. Gated to git repos so a stray
    // query never walks an arbitrary directory.
    if let Some(cwd) = &cwd
        && cwd_is_git
    {
        let identity = crate::index::detect_identity(cwd).to_string();
        // only auto-index a repo we've never seen — a deliberate partial index
        // (`--index --path …`, status "partial") is respected, not clobbered
        if store.coverage_status(&identity).ok().flatten().is_none() {
            let _ = crate::index::index_path(&mut store, cwd);
        }
    }

    let current = current_repo_id(&store, cwd.as_deref());

    // Branch awareness: files you're changing on this feature branch (and their
    // directory neighbors) are where you're most likely looking.
    let active = match &cwd {
        Some(c) if cwd_is_git => {
            crate::search::ActiveFiles::new(crate::index::branch_changed_files(c))
        }
        _ => crate::search::ActiveFiles::default(),
    };

    // A repeated search (same query, nothing opened since) means last time
    // missed — decay this query's learned boost before ranking so a stale
    // learned pick stops dominating and alternatives surface. Skipped under
    // --no-record so an agent's searches don't perturb the learned signal.
    if !no_record && let Some(repo) = current {
        let qn = query.to_ascii_lowercase();
        if store.is_repeat_search(repo, &qn).unwrap_or(false) {
            let _ = store.decay_selections(repo, &qn);
        }
    }

    let mut hits = match crate::search::search(&store, query, current, &active, limit) {
        Ok(h) => h,
        Err(e) => return fail(format_args!("rq: {e}")),
    };

    // Staleness: lazily revalidate the files behind the top hits; if any changed
    // on disk, refresh them and re-rank once.
    if !hits.is_empty() && revalidate_top(&mut store, &hits) {
        hits = match crate::search::search(&store, query, current, &active, limit) {
            Ok(h) => h,
            Err(e) => return fail(format_args!("rq: {e}")),
        };
    }

    // Layer 4: a non-git directory isn't persisted, so fall back to a live scan
    // there — search still answers at zero coverage.
    if hits.is_empty()
        && !cwd_is_git
        && let Some(cwd) = &cwd
    {
        hits = crate::search::live_search(cwd, query, limit);
    }

    // post-filters: keep only results under a --path dir and/or of a --kind,
    // then trim to the requested count.
    if !paths.is_empty() {
        hits.retain(|h| under_any(&h.file, paths));
    }
    if !kinds.is_empty() {
        hits.retain(|h| kinds.iter().any(|k| k == &h.kind));
    }
    if !paths.is_empty() || !kinds.is_empty() {
        hits.truncate(want);
    }

    if hits.is_empty() {
        match out {
            Output::Json => println!("[]"),
            Output::Ndjson => {}
            Output::Text => eprintln!("no matches for {query:?}"),
        }
        return ExitCode::FAILURE;
    }

    // For machine-readable output, attach each result's definition line so a
    // consumer sees `def perform(refund)`, not just the name. Cheap: only the
    // displayed results, only in JSON modes.
    if matches!(out, Output::Json | Output::Ndjson) {
        for hit in &mut hits {
            hit.signature = read_signature(&store, hit, cwd.as_deref());
        }
    }

    match out {
        Output::Ndjson => {
            for hit in &hits {
                match serde_json::to_string(hit) {
                    Ok(line) => println!("{line}"),
                    Err(e) => return fail(format_args!("rq: {e}")),
                }
            }
        }
        Output::Json => match serde_json::to_string_pretty(&hits) {
            Ok(s) => println!("{s}"),
            Err(e) => return fail(format_args!("rq: {e}")),
        },
        Output::Text => {
            let color = match_color();
            for hit in &hits {
                // highlight the chars the query actually matched (great for fuzzy)
                let positions = crate::search::match_positions(query, &hit.name);
                let name = match &color {
                    Some(c) => highlight(&hit.name, &positions, c),
                    None => hit.name.clone(),
                };
                let qualified = match &hit.parent {
                    Some(p) => format!("{name} · {p}"),
                    None => name,
                };
                println!("{}:{}  {} {}", hit.file, hit.line, hit.kind, qualified);
                if explain {
                    let parts: Vec<String> = hit
                        .features
                        .iter()
                        .map(|f| format!("{} {:.0}", f.name, f.value))
                        .collect();
                    println!("    score {:.0} = {}", hit.score, parts.join(" + "));
                }
            }
        }
    }

    // Results are out — now do the cheap deferred work, amortized across
    // interactions. Under --no-record we skip logging this search (so it isn't a
    // behavioral signal) but still run maintenance, which only rolls up and
    // prunes pre-existing events.
    if !no_record {
        let _ = store.record_event(
            "search",
            Some(&query.to_ascii_lowercase()),
            current,
            None,
            None,
            None,
        );
    }
    deferred_maintenance(&mut store);

    ExitCode::SUCCESS
}

/// How many events to roll up per interaction. Bounded so the deferred pass
/// after a command stays quick.
const AGGREGATE_BATCH: usize = 256;

/// Recent raw events to retain after rollup (enough for repeat detection); the
/// rest, once aggregated, are pruned to keep the log from growing unbounded.
const KEEP_RECENT_EVENTS: i64 = 200;

/// The bounded background work run after a user interaction, once results are
/// out: roll new events into the learning rollup, then prune the raw log.
fn deferred_maintenance(store: &mut Store) {
    let _ = store.aggregate_events(AGGREGATE_BATCH);
    let _ = store.prune_events(KEEP_RECENT_EVENTS);
}

/// Hook entry point: record that `file` was opened/selected for `query`, then
/// amortize a chunk of event aggregation.
fn cmd_record(kind: &str, query: Option<&str>, file: &str, line: Option<i64>) -> ExitCode {
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let identity = crate::index::detect_identity(&cwd).to_string();
    let repo_id = store.repository_id(&identity).ok().flatten();

    // Store the path repo-relative so the rollup can resolve it against indexed
    // files.
    let rel = match repo_id.and_then(|id| store.checkout_root(id).ok().flatten()) {
        Some(root) => repo_relative(std::path::Path::new(&root), &cwd, file),
        None => file.to_string(),
    };
    let query_norm = query.map(|q| q.to_ascii_lowercase());

    if let Err(e) = store.record_event(kind, query_norm.as_deref(), repo_id, Some(&rel), line, None)
    {
        return fail(format_args!("rq record: {e}"));
    }
    deferred_maintenance(&mut store);
    ExitCode::SUCCESS
}

/// The definition's source line (trimmed) for a hit — read from disk, resolving
/// the repo root from the store (or the cwd for live results). Best-effort.
fn read_signature(
    store: &Store,
    hit: &crate::search::Hit,
    cwd: Option<&std::path::Path>,
) -> Option<String> {
    let root = store
        .repository_id(&hit.repo_identity)
        .ok()
        .flatten()
        .and_then(|id| store.checkout_root(id).ok().flatten())
        .map(PathBuf::from)
        .or_else(|| cwd.map(std::path::Path::to_path_buf))?;
    let content = std::fs::read_to_string(root.join(&hit.file)).ok()?;
    let idx = usize::try_from(hit.line).ok()?.checked_sub(1)?;
    let line = content.lines().nth(idx)?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Normalize a `--kind` value (name or shortcut) to a canonical symbol kind.
/// Unknown values pass through lowercased (so they simply match nothing).
fn canonical_kind(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "c" | "class" => "class",
        "m" | "method" => "method",
        "f" | "fn" | "func" | "function" => "function",
        "mod" | "module" => "module",
        other => return other.to_string(),
    }
    .to_string()
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

/// Wrap the matched character positions of `text` in an ANSI color run.
/// Consecutive matched chars share one escape sequence.
fn highlight(text: &str, positions: &[usize], color: &str) -> String {
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

/// Whether a repo-relative `file` sits under one of the `--path` directories
/// (prefix match on a path boundary). `app/services` matches
/// `app/services/refund.rb` but not `app/services_old/x.rb`.
fn under_any(file: &str, paths: &[String]) -> bool {
    paths.iter().any(|p| {
        let p = p.trim_start_matches("./").trim_end_matches('/');
        p.is_empty() || file == p || file.starts_with(&format!("{p}/"))
    })
}

/// Resolve a possibly-absolute or cwd-relative path to a repo-relative one.
fn repo_relative(root: &std::path::Path, cwd: &std::path::Path, file: &str) -> String {
    let p = std::path::Path::new(file);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    abs.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file.to_string())
}

/// Revalidate the files behind the top hits against disk, refreshing any that
/// changed and forgetting any that were deleted. Returns true if anything
/// changed (so the caller re-runs the search).
fn revalidate_top(store: &mut Store, hits: &[crate::search::Hit]) -> bool {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut changed = false;
    for hit in hits {
        if !seen.insert((hit.repo_identity.clone(), hit.file.clone())) {
            continue;
        }
        let Some(repo_id) = store.repository_id(&hit.repo_identity).ok().flatten() else {
            continue;
        };
        let Some(root) = store.checkout_root(repo_id).ok().flatten() else {
            continue;
        };
        if let Ok(crate::index::Refresh::Updated | crate::index::Refresh::Deleted) =
            crate::index::refresh_file(store, repo_id, std::path::Path::new(&root), &hit.file)
        {
            changed = true;
        }
    }
    changed
}

/// The repository id for the working directory, if it's indexed — used to
/// boost results from the repo you're in.
fn current_repo_id(store: &Store, cwd: Option<&std::path::Path>) -> Option<i64> {
    let identity = crate::index::detect_identity(cwd?);
    store.repository_id(&identity.to_string()).ok().flatten()
}

fn cmd_index(path: Option<PathBuf>, subdirs: &[String]) -> ExitCode {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    match crate::index::index_under(&mut store, &root, subdirs) {
        Ok(stats) => {
            let scope = if subdirs.is_empty() { "" } else { " (partial)" };
            // distinguish this run's incremental work from the index totals
            let totals = store
                .repository_id(&crate::index::detect_identity(&root).to_string())
                .ok()
                .flatten()
                .and_then(|id| store.repo_totals(id).ok());
            match totals {
                Some((files, symbols)) => println!(
                    "{} file(s)/{} symbol(s) added this run; index{scope} now {files} files, {symbols} symbols",
                    stats.files_indexed, stats.symbols
                ),
                None => println!(
                    "{} file(s)/{} symbol(s) added this run{scope}",
                    stats.files_indexed, stats.symbols
                ),
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format_args!("rq index: {e}")),
    }
}

fn cmd_status() -> ExitCode {
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    match store.coverage_overview() {
        Ok(rows) if rows.is_empty() => {
            println!("no repositories indexed yet (try `rq index`)");
            ExitCode::SUCCESS
        }
        Ok(rows) => {
            for r in rows {
                println!(
                    "{:<10} {:>6} files  {:>7} symbols  {}",
                    r.status, r.files, r.symbols, r.identity
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format_args!("rq status: {e}")),
    }
}

/// Open the rq database, honoring `RQ_DB` and creating parent dirs.
fn open_store() -> Result<Store, Box<dyn std::error::Error>> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Store::open(&path)?)
}

/// Resolve the database path: `$RQ_DB`, else `$HOME/.local/share/rq/rq.db`.
fn db_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(p) = std::env::var("RQ_DB") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home).join(".local/share/rq/rq.db"))
}

fn fail(args: std::fmt::Arguments) -> ExitCode {
    eprintln!("{args}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_wraps_matched_runs() {
        assert_eq!(
            highlight("FooThing", &[0, 1, 2], "1;31"),
            "\u{1b}[1;31mFoo\u{1b}[0mThing"
        );
        // scattered matches get separate runs
        assert_eq!(
            highlight("FooThing", &[0, 3], "1"),
            "\u{1b}[1mF\u{1b}[0moo\u{1b}[1mT\u{1b}[0mhing"
        );
        // nothing matched → unchanged
        assert_eq!(highlight("FooThing", &[], "1;31"), "FooThing");
    }
}
