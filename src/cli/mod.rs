//! Command-line surface. Search is the default action: `rq <query>`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::store::Store;

/// Search is the default action (`rq <query>`). Operations are flags rather
/// than subcommands so no word is reserved — `rq index`, `rq status`, and
/// `rq record` all search for those symbols. This also matches the rg/fd feel.
#[derive(Parser)]
#[command(
    name = "rq",
    version,
    about = "A code navigation engine — reach the definition you want, fast."
)]
struct Cli {
    /// Search query. With --index, the path to index; with --record, the query
    /// the selection was made for.
    #[arg(value_name = "TARGET")]
    target: Option<String>,

    /// Show the score breakdown for each result.
    #[arg(long)]
    explain: bool,

    /// Run as if invoked from DIR (like `git -C`) — sets which repository is
    /// searched/indexed and where a recorded path is resolved.
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,

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

    /// (--record) Event kind.
    #[arg(long, default_value = "select")]
    kind: String,
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    if cli.index {
        // index TARGET if given, else -C dir, else the current directory
        return cmd_index(cli.target.map(PathBuf::from).or(cli.cwd));
    }
    if cli.status {
        return cmd_status();
    }
    if cli.record {
        // clap guarantees --file is present via `requires`
        let file = cli.file.expect("--record requires --file");
        return cmd_record(&cli.kind, cli.target.as_deref(), &file, cli.line, cli.cwd);
    }
    match cli.target {
        Some(query) => cmd_search(&query, cli.explain, cli.cwd),
        None => {
            eprintln!("rq: no query given (try `rq --help`)");
            ExitCode::FAILURE
        }
    }
}

/// Default action: search the index and print ranked results.
fn cmd_search(query: &str, explain: bool, cwd: Option<PathBuf>) -> ExitCode {
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = cwd.or_else(|| std::env::current_dir().ok());
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

    let current = current_repo_id(&store, cwd.as_deref());

    // A repeated search (same query, nothing opened since) means last time
    // missed — decay this query's learned boost before ranking so a stale
    // learned pick stops dominating and alternatives surface.
    if let Some(repo) = current {
        let qn = query.to_ascii_lowercase();
        if store.is_repeat_search(repo, &qn).unwrap_or(false) {
            let _ = store.decay_selections(repo, &qn);
        }
    }

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
fn cmd_record(
    kind: &str,
    query: Option<&str>,
    file: &str,
    line: Option<i64>,
    cwd: Option<PathBuf>,
) -> ExitCode {
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = cwd
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
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
