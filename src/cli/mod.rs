//! Command-line surface. Search is the default action: `rq <query>`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
        Some(Command::Index { path }) => {
            let path = path.unwrap_or_else(|| PathBuf::from("."));
            // TODO(phase-1): drive crate::index.
            eprintln!("rq index {}: not yet implemented", path.display());
            ExitCode::FAILURE
        }
        Some(Command::Status) => {
            // TODO(phase-1): report crate::store coverage.
            eprintln!("rq status: not yet implemented");
            ExitCode::FAILURE
        }
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
