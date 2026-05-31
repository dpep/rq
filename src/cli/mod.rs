//! Command-line surface. Search is the default action: `rq <query>`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "rq",
    version,
    about = "A code navigation engine — reach the definition you want, fast.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Search query (the default action when no subcommand is given).
    #[arg(value_name = "QUERY")]
    query: Option<String>,

    /// Show the score breakdown for each result.
    #[arg(long)]
    explain: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Index a repository (incremental; safe to re-run).
    Index {
        /// Path to index (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// Show indexing coverage per known repository.
    Status,
    /// Record an interaction (editor/shell hook): which result you opened for a
    /// query. Feeds ranking's behavioral learning.
    Record {
        /// File that was opened/selected (absolute or relative to the cwd).
        #[arg(long)]
        file: String,
        /// The query that led there, if any.
        #[arg(long)]
        query: Option<String>,
        /// Line landed on (used to attribute the selection to a definition).
        #[arg(long)]
        line: Option<i64>,
        /// Event kind.
        #[arg(long, default_value = "select")]
        kind: String,
    },
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Index { path }) => cmd_index(path),
        Some(Command::Status) => cmd_status(),
        Some(Command::Record {
            file,
            query,
            line,
            kind,
        }) => cmd_record(&kind, query.as_deref(), &file, line),
        None => match cli.query {
            Some(query) => cmd_search(&query, cli.explain),
            None => {
                eprintln!("rq: no query given (try `rq --help`)");
                ExitCode::FAILURE
            }
        },
    }
}

/// Default action: search the index and print ranked results.
fn cmd_search(query: &str, explain: bool) -> ExitCode {
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
        if store.coverage_status(&identity).ok().flatten().as_deref() != Some("complete") {
            let _ = crate::index::index_path(&mut store, cwd);
        }
    }

    let current = current_repo_id(&store);
    let mut hits = match crate::search::search(&store, query, current, 10) {
        Ok(h) => h,
        Err(e) => return fail(format_args!("rq: {e}")),
    };

    // Staleness: lazily revalidate the files behind the top hits; if any changed
    // on disk, refresh them and re-rank once.
    if !hits.is_empty() && revalidate_top(&mut store, &hits) {
        hits = match crate::search::search(&store, query, current, 10) {
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
        hits = crate::search::live_search(cwd, query, 10);
    }

    if hits.is_empty() {
        eprintln!("no matches for {query:?}");
        return ExitCode::FAILURE;
    }
    for hit in &hits {
        let qualified = match &hit.parent {
            Some(p) => format!("{} · {p}", hit.name),
            None => hit.name.clone(),
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

    // Results are out — now do the cheap deferred work, amortized across
    // interactions: log this search and roll a batch of events into the
    // learning rollup. Best-effort; never fail the command on it.
    let _ = store.record_event(
        "search",
        Some(&query.to_ascii_lowercase()),
        current,
        None,
        None,
        None,
    );
    let _ = store.aggregate_events(AGGREGATE_BATCH);

    ExitCode::SUCCESS
}

/// How many events to roll up per interaction. Bounded so the deferred pass
/// after a command stays quick.
const AGGREGATE_BATCH: usize = 256;

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
    let _ = store.aggregate_events(AGGREGATE_BATCH);
    ExitCode::SUCCESS
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

/// The repository id for the current working directory, if it's indexed —
/// used to boost results from the repo you're in.
fn current_repo_id(store: &Store) -> Option<i64> {
    let cwd = std::env::current_dir().ok()?;
    let identity = crate::index::detect_identity(&cwd);
    store.repository_id(&identity.to_string()).ok().flatten()
}

fn cmd_index(path: Option<PathBuf>) -> ExitCode {
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    match crate::index::index_path(&mut store, &path) {
        Ok(stats) => {
            println!(
                "indexed {} file(s) ({} seen), {} symbol(s)",
                stats.files_indexed, stats.files_seen, stats.symbols
            );
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
                    "{:<10} {:>6} symbols  {}/{} files  {}",
                    r.status, r.symbols, r.files_indexed, r.files_seen, r.identity
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
