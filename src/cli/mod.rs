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
    #[arg(value_name = "QUERY", trailing_var_arg = true)]
    query: Vec<String>,

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
        None => {
            if cli.query.is_empty() {
                eprintln!("rq: no query given (try `rq --help`)");
                return ExitCode::FAILURE;
            }
            let query = cli.query.join(" ");
            // TODO(phase-1): drive crate::search and stream ranked results.
            eprintln!(
                "rq {query}: search not yet implemented (explain={})",
                cli.explain
            );
            ExitCode::FAILURE
        }
    }
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
