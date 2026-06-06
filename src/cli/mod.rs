//! Command-line surface. Search is the default action: `rq <query>`.

use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

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
rq perform -k method      restrict to a symbol kind (c/mod/m/f/s/e/t)\n  \
rq thing -x rust          restrict to a language (ruby/rust/go/python)\n  \
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

    /// Restrict to symbol kinds: class, module, method, function, struct, enum,
    /// trait (shortcuts: c, mod, m, f, s, e, t). Repeatable or comma-separated.
    #[arg(short = 'k', long, value_name = "KIND", value_delimiter = ',')]
    kind: Vec<String>,

    /// Restrict to languages: ruby, rust, go, python. Prefix-matched, so `r`
    /// means ruby+rust and `p` means python; aliases rb, rs, golang. Repeatable
    /// or comma-separated.
    #[arg(short = 'x', long = "lang", value_name = "LANG", value_delimiter = ',')]
    lang: Vec<String>,

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

    /// Trace what rq decides (root, coverage, warming, reconcile) to stderr —
    /// for debugging. `RQ_LOG=1` does the same for an installed binary.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Parse worker threads the background indexer uses (0 = auto). (`-j` is
    /// taken by `--json`, so this is `--jobs` only.) `RQ_JOBS` works too.
    #[arg(long, value_name = "N", default_value_t = 0)]
    jobs: usize,
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    crate::trace::enable_from(cli.verbose);
    crate::index::set_parse_jobs(cli.jobs);

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
    // a language token can expand to several tags (`r` → ruby + rust)
    let langs: Vec<String> = cli.lang.iter().flat_map(|x| canonical_langs(x)).collect();
    match cli.target {
        Some(query) => cmd_search(
            &query,
            cli.explain,
            out,
            &paths,
            &kinds,
            &langs,
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

/// How often the search re-checks the index while a cold repo warms on the
/// background thread — short enough to feel instant, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Default action: search the index and print ranked results. `want` is the
/// number of results to show (`--limit`).
#[allow(clippy::too_many_arguments)]
fn cmd_search(
    query: &str,
    explain: bool,
    out: Output,
    paths: &[String],
    kinds: &[String],
    langs: &[String],
    want: usize,
    no_record: bool,
) -> ExitCode {
    // post-filters (--path, --kind, --lang) need headroom before the cutoff so a
    // filtered-in result isn't lost to the top-N truncation
    let limit = if paths.is_empty() && kinds.is_empty() && langs.is_empty() {
        want
    } else {
        (want * 20).max(PATH_HEADROOM)
    };
    let _timer = crate::trace::Timer::start("search done");
    let t_setup = std::time::Instant::now();
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = std::env::current_dir().ok();
    let cwd_is_git = cwd.as_deref().is_some_and(crate::index::is_git_repo);

    // Index relative to the repo ROOT, not wherever the search happens to run.
    // Paths and the stored checkout root must be repo-root-relative and stable, or
    // a search from a subdirectory would re-key the same repo under subdir-relative
    // paths — and the deletion reconcile / staleness revalidation would then forget
    // everything indexed from the root. Outside git, the root is just the cwd.
    let root = cwd
        .as_deref()
        .map(|c| crate::index::repo_root(c).unwrap_or_else(|| c.to_path_buf()));

    // Files you're changing on this feature branch (and their directory
    // neighbors): the branch ranking boost, and the warm pass's priority set.
    let active_paths: Vec<String> = match &root {
        Some(c) if cwd_is_git => crate::index::branch_changed_files(c),
        _ => Vec::new(),
    };

    // Resolve identity from the repo root, cache-first: looked up by checkout root
    // (no `git remote` fork), falling back to git only the first time we see a
    // repo. Computed even for non-git dirs so an explicitly `--index`ed one is
    // still recognized as the current repo below.
    let identity = root.as_deref().map(|c| resolve_identity(&store, c));
    let coverage = identity
        .as_deref()
        .and_then(|id| store.coverage_status(id).ok())
        .flatten();

    // Opportunistic indexing (Layer 5), time-bounded so the first query in a
    // large repo never blocks on a full walk. We may warm a git work tree (safe
    // to auto-discover) *or* any dir we already track — one earns tracking by
    // being explicitly `--index`ed, which opts a non-git dir in. We never warm an
    // unknown non-git dir (don't walk a random directory) or a deliberate partial
    // subset (`--index --path …`, status "partial").
    let known = coverage.is_some();
    let warming_ok = (cwd_is_git || known) && coverage.as_deref() != Some("partial");
    if crate::trace::enabled() {
        crate::trace!(
            "query {query:?}: root={} identity={} coverage={} warming_ok={warming_ok} active={}",
            root.as_deref().map_or("?".into(), crate::trace::abbrev),
            identity.as_deref().unwrap_or("none"),
            coverage.as_deref().unwrap_or("none"),
            active_paths.len(),
        );
    }
    let current = identity
        .as_deref()
        .and_then(|id| store.repository_id(id).ok().flatten());
    let active = crate::search::ActiveFiles::new(active_paths.clone());

    // A repeated search (same query, nothing opened since) means last time missed
    // — decay this query's learned boost before ranking so a stale learned pick
    // stops dominating. Skipped under --no-record so an agent doesn't perturb it.
    if !no_record && let Some(repo) = current {
        let qn = query.to_ascii_lowercase();
        if store.is_repeat_search(repo, &qn).unwrap_or(false) {
            let _ = store.decay_selections(repo, &qn);
        }
    }

    // Warm the index on a background thread (its own connection — WAL lets it
    // write while we read), for the whole budget, whenever there's work: a
    // not-yet-complete repo, or a complete one changed since it was indexed. The
    // search below reads whatever it has committed so far, and we block on it
    // before exiting so the shell waits the same total time.
    let warm_budget = answer_warm_budget() + deferred_warm_budget();
    let was_warming = coverage.as_deref() != Some("complete");
    let want_warm = warming_ok
        && match &root {
            Some(c) => {
                was_warming || !repo_unchanged_since_index(&store, c, current, coverage.as_deref())
            }
            None => false,
        };
    // `warm_done` lets the poll stop the instant the indexer finishes — so a miss
    // on a small repo returns as soon as it's indexed, not at the deadline.
    let warm_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let indexer = (want_warm && root.is_some()).then(|| {
        crate::trace!(
            "background warm ({warm_budget:?}, {} jobs)",
            crate::index::parse_jobs()
        );
        let root = root.clone().expect("checked");
        let active = active_paths.clone();
        let q = query.to_string();
        let warm_done = std::sync::Arc::clone(&warm_done);
        std::thread::spawn(move || {
            if let Ok(mut idx) = open_store() {
                // path-prioritize toward the query so the relevant file indexes first
                let _ =
                    crate::index::index_budgeted(&mut idx, &root, &active, warm_budget, Some(&q));
            }
            warm_done.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    });

    // Poll while a cold/partial repo warms. Don't print the first hit off a
    // sparse index — a fuzzy or path match can be wrong once more is indexed.
    // Hold until a *high-confidence* (exact or prefix name) match appears, which
    // means the index has built enough to rank it; otherwise keep building until
    // warming finishes or the answer deadline passes, then rank the fuller index.
    crate::trace!(
        "setup (open + repo detect + warm decision): {} ms",
        t_setup.elapsed().as_millis()
    );
    let answer_deadline = std::time::Instant::now() + answer_warm_budget();
    let polling = indexer.is_some() && was_warming;
    let mut hits = loop {
        match crate::search::search(&store, query, current, &active, limit) {
            Ok(h) => {
                let confident = h.first().is_some_and(|hit| {
                    hit.features
                        .iter()
                        .any(|f| matches!(f.name, "exact" | "prefix"))
                });
                if !polling
                    || confident
                    || warm_done.load(std::sync::atomic::Ordering::Relaxed)
                    || std::time::Instant::now() >= answer_deadline
                {
                    break h;
                }
            }
            Err(e) => {
                if let Some(h) = indexer {
                    let _ = h.join();
                }
                return fail(format_args!("rq: {e}"));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    // Staleness: revalidate the files behind the top hits; re-rank once if changed.
    if !hits.is_empty() && revalidate_top(&mut store, &hits) {
        hits = crate::search::search(&store, query, current, &active, limit).unwrap_or_default();
    }

    // Untracked non-git dir — nothing persisted, no warmer running — so scan it
    // live in-memory to answer at all (substring, then fuzzy). The only
    // non-persisting scan left.
    if hits.is_empty()
        && indexer.is_none()
        && coverage.is_none()
        && let Some(root) = &root
    {
        crate::trace!("empty → live (in-memory) scan of an untracked dir");
        let deadline = std::time::Instant::now() + live_fallback_budget();
        let mut h =
            crate::search::live_search(root, query, limit, &HashSet::new(), Some(deadline), true);
        if h.is_empty() {
            h = crate::search::live_search(
                root,
                query,
                limit,
                &HashSet::new(),
                Some(deadline),
                false,
            );
        }
        hits = h;
    }

    // Relevance gate: when the query lands a real name match (exact or prefix),
    // drop the scattered fuzzy / path-only near-matches — they're noise next to a
    // solid hit, and rq favors fewer, better results. A purely-fuzzy query (no
    // exact/prefix anywhere) keeps its matches.
    let strong = |h: &crate::search::Hit| {
        h.features
            .iter()
            .any(|f| matches!(f.name, "exact" | "prefix"))
    };
    if hits.iter().any(strong) {
        hits.retain(strong);
    }

    // post-filters: keep only results under a --path dir, of a --kind, and/or in
    // a --lang, then trim to the requested count.
    if !paths.is_empty() {
        hits.retain(|h| under_any(&h.file, paths));
    }
    if !kinds.is_empty() {
        hits.retain(|h| kinds.iter().any(|k| k == &h.kind));
    }
    if !langs.is_empty() {
        hits.retain(|h| langs.iter().any(|l| l == &h.language));
    }
    if !paths.is_empty() || !kinds.is_empty() || !langs.is_empty() {
        hits.truncate(want);
    }

    if hits.is_empty() {
        match out {
            Output::Json => println!("[]"),
            Output::Ndjson => {}
            Output::Text => eprintln!("no matches for {query:?}"),
        }
        // a miss still warms for next time — block on the background pass
        if let Some(h) = indexer {
            let _ = h.join();
        }
        return ExitCode::FAILURE;
    }

    // Attach each result's definition line (e.g. `def perform(refund)`) — shown
    // in text output and carried in JSON. Cheap: only the displayed results.
    for hit in &mut hits {
        hit.signature = read_signature(&store, hit, cwd.as_deref());
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
            let c = color.as_deref();
            for hit in &hits {
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
                    hit.kind,
                    qualified
                );
                if let Some(sig) = &hit.signature {
                    println!("    {}", hl(sig, query, c));
                }
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

    // Results are out; block until the background warm finishes its budget. It
    // persists as it goes (incremental commits), so even a pass cut short by the
    // budget keeps everything it parsed — building coverage across queries and,
    // on a changed repo, picking up edits and reconciling deletions on a full
    // sweep, all without a daemon.
    if let Some(h) = indexer {
        let _ = h.join();
    }

    ExitCode::SUCCESS
}

/// Whether a complete repo is provably unchanged since its last index — same
/// HEAD and a clean work tree — so the deferred re-walk can be skipped. The git
/// HEAD + dirty check is cheap (~tens of ms) and authoritative at any size, so
/// it gates warming for small and large repos alike: a clean, fully-indexed repo
/// has nothing to warm, and re-walking it on every query just to discover that
/// wasted a full sweep (~hundreds of ms) per search. Conservative: any
/// uncertainty (not complete, non-git / no recorded head, git hiccup) returns
/// false, so we warm.
fn repo_unchanged_since_index(
    store: &Store,
    cwd: &std::path::Path,
    current: Option<i64>,
    coverage: Option<&str>,
) -> bool {
    if coverage != Some("complete") {
        return false;
    }
    let Some(id) = current else { return false };
    let indexed_head = store.indexed_head(id).ok().flatten();
    indexed_head.is_some()
        && crate::index::git_head(cwd) == indexed_head
        && !crate::index::is_dirty(cwd)
}

/// Inline warm budget on the search path. A *cap*, not a fixed delay:
/// `index_budgeted` returns the moment a full sweep finishes, so small/medium
/// repos index completely and pay only their real cost. The cap only bites a
/// genuinely huge, never-indexed repo — where a bigger budget buys a much better
/// first answer (a tiny budget can return nothing, since a git repo has no
/// live-scan fallback). 500 ms is a one-time cold-cache cost, trivial next to
/// scanning a large tree from scratch; the deferred pass and later queries fill
/// in the rest.
fn answer_warm_budget() -> Duration {
    env_budget("RQ_ANSWER_BUDGET_MS", 500)
}

/// Deferred warm budget, spent after results are printed: larger, to make real
/// progress on coverage per query while keeping each invocation snappy.
fn deferred_warm_budget() -> Duration {
    env_budget("RQ_DEFERRED_BUDGET_MS", 250)
}

/// Bound for the git-repo live-scan fallback (index empty, still warming): enough
/// to surface a result the warm hasn't reached, without an unbounded walk.
fn live_fallback_budget() -> Duration {
    env_budget("RQ_FALLBACK_BUDGET_MS", 250)
}

/// Read a budget (milliseconds) from an env var, else the default. The env knobs
/// exist mainly for testing — a tiny budget reproduces large-repo warming
/// behavior on a small repo.
fn env_budget(var: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
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
        "s" | "struct" => "struct",
        "e" | "enum" => "enum",
        "t" | "trait" => "trait",
        other => return other.to_string(),
    }
    .to_string()
}

/// Expand a `--lang` value to the language tag(s) it selects: a **prefix** of any
/// known language name (so `r` → ruby+rust, `p`/`py` → python, `g` → go), plus a
/// few non-prefix aliases (`rb`→ruby, `rs`→rust, `golang`→go). An unknown value
/// passes through lowercased so it simply matches nothing.
fn canonical_langs(s: &str) -> Vec<String> {
    let t = s.to_ascii_lowercase();
    let alias = match t.as_str() {
        "rb" => Some("ruby"),
        "rs" => Some("rust"),
        "golang" => Some("go"),
        _ => None,
    };
    let matched: Vec<String> = crate::lang::languages()
        .into_iter()
        .filter(|lang| alias == Some(*lang) || lang.starts_with(&t))
        .map(str::to_string)
        .collect();
    if matched.is_empty() { vec![t] } else { matched }
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
fn hl_path(path: &str, query: &str, color: Option<&str>) -> String {
    let Some(c) = color else {
        return path.to_string();
    };
    let base_byte = path.rfind('/').map(|b| b + 1).unwrap_or(0);
    let base_start = path[..base_byte].chars().count();
    // align on the filename *stem* (drop the extension), the same string the
    // scorer matched — so the query can't straggle into `.rb` instead of lighting
    // up the logical name (`employees_controller`)
    let base = &path[base_byte..];
    let stem = match base.rfind('.') {
        Some(i) if i > 0 => &base[..i],
        _ => base,
    };
    let positions: Vec<usize> = crate::search::match_positions(query, stem)
        .into_iter()
        .map(|p| p + base_start)
        .collect();
    highlight(path, &positions, c)
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
        if let Ok(crate::index::Refresh::Updated) =
            crate::index::refresh_file(store, repo_id, std::path::Path::new(&root), &hit.file)
        {
            changed = true;
        }
    }
    changed
}

/// The repository's normalized identity for `cwd`, cache-first: look it up by
/// the canonical cwd (the checkout root indexing records), so a known repo (git
/// or explicitly `--index`ed) costs no `git` fork. On a cache miss, a non-git
/// dir resolves to its `local:` path directly (still no fork); only a git work
/// tree we haven't seen yet pays a `git remote` call.
fn resolve_identity(store: &Store, cwd: &std::path::Path) -> String {
    if let Ok(canon) = cwd.canonicalize() {
        if let Ok(Some(identity)) = store.identity_for_root(&canon.to_string_lossy()) {
            return identity;
        }
        if crate::index::repo_root(cwd).is_none() {
            return crate::core::RepoIdentity::local(&canon.to_string_lossy()).to_string();
        }
    }
    crate::index::detect_identity(cwd).to_string()
}

fn cmd_index(path: Option<PathBuf>, subdirs: &[String]) -> ExitCode {
    let explicit = path.is_some();
    let target = path.unwrap_or_else(|| PathBuf::from("."));
    // Normalize to the repo root: the index is repo-root-relative, so indexing
    // from a subdirectory must still key off the root (a subdir-relative index
    // would mismatch a later search and get reconciled away). `--path` scopes a
    // subset; outside git the target is used as-is.
    let root = crate::index::repo_root(&target).unwrap_or_else(|| target.clone());
    // An explicit TARGET *inside* the repo scopes the index to that subtree — the
    // user pointed at a subdir, not the whole repo, and shouldn't pay to walk
    // everything. Folded in alongside any `--path` subdirs. (A bare `rq --index`
    // with no target still walks the whole repo.)
    let mut subdirs = subdirs.to_vec();
    if explicit
        && let (Ok(t), Ok(r)) = (target.canonicalize(), root.canonicalize())
        && t != r
        && let Ok(rel) = t.strip_prefix(&r)
        && !rel.as_os_str().is_empty()
    {
        subdirs.push(rel.to_string_lossy().into_owned());
    }
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    match crate::index::index_under(&mut store, &root, &subdirs) {
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
        Err(e) => fail(format_args!("rq --index: {e}")),
    }
}

fn cmd_status() -> ExitCode {
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    match store.coverage_overview() {
        Ok(rows) if rows.is_empty() => {
            println!("no repositories indexed yet (try `rq --index`)");
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
        Err(e) => fail(format_args!("rq --status: {e}")),
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

    #[test]
    fn hl_path_highlights_the_stem_not_the_extension() {
        // matching `employeescontroller`, the highlight covers the logical name in
        // the stem and never straggles into `.rb`
        let out = hl_path(
            "app/employees_controller.rb",
            "employeescontroller",
            Some("1;31"),
        );
        assert!(
            out.starts_with("app/\u{1b}[1;31memployees"),
            "stem highlighted: {out:?}"
        );
        assert!(
            out.ends_with("controller\u{1b}[0m.rb"),
            "`.rb` left un-highlighted: {out:?}"
        );
    }
}
