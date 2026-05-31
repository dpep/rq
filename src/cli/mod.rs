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
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Index { path }) => cmd_index(path),
        Some(Command::Status) => cmd_status(),
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
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let current = current_repo_id(&store);
    let mut hits = match crate::search::search(&store, query, current, 10) {
        Ok(h) => h,
        Err(e) => return fail(format_args!("rq: {e}")),
    };

    // Layer 4: if the index has no confident answer (thin or absent coverage),
    // fall back to a live scan of the current repository and blend results.
    if !confident(&hits)
        && let Ok(cwd) = std::env::current_dir()
    {
        let live = crate::search::live_search(&cwd, query, 10);
        hits = crate::search::merge(hits, live, 10);
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
    ExitCode::SUCCESS
}

/// A result set is "confident" when its top hit is at least prefix-quality —
/// good enough to skip the Layer 4 live-scan fallback.
fn confident(hits: &[crate::search::Hit]) -> bool {
    hits.first().is_some_and(|h| h.score >= 700.0)
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
