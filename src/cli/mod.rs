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
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = std::env::current_dir().ok();
    let cwd_is_git = cwd.as_deref().is_some_and(crate::index::is_git_repo);

    // Files you're changing on this feature branch (and their directory
    // neighbors): the branch ranking boost, and the warm pass's priority set.
    let active_paths: Vec<String> = match &cwd {
        Some(c) if cwd_is_git => crate::index::branch_changed_files(c),
        _ => Vec::new(),
    };

    // Resolve identity for any cwd, cache-first: looked up by checkout root (no
    // `git remote` fork), falling back to git only the first time we see a repo.
    // Computed even for non-git dirs so an explicitly `--index`ed one is still
    // recognized as the current repo below.
    let identity = cwd.as_deref().map(|c| resolve_identity(&store, c));
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
    if warming_ok
        && coverage.as_deref() != Some("complete")
        && let Some(c) = &cwd
    {
        let _ = crate::index::index_budgeted(
            &mut store,
            c,
            &active_paths,
            ANSWER_WARM_BUDGET,
            Some(query),
        );
    }

    let current = identity
        .as_deref()
        .and_then(|id| store.repository_id(id).ok().flatten());
    let active = crate::search::ActiveFiles::new(active_paths.clone());

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

    // Layer 4 fallback when the index came up empty, keyed on what we know about
    // the dir:
    //   - untracked (no coverage — typically a non-git dir we won't persist):
    //     scan it live, in full, so we answer at all.
    //   - warming (a git repo mid-warm): a bounded live scan that skips already-
    //     indexed files, so the budget reaches ground the warm hasn't covered.
    //   - complete / partial: empty is a genuine miss (or outside the subset).
    if hits.is_empty()
        && let Some(cwd) = &cwd
    {
        // a pre-filtered scan parses only files containing the query — fast, but
        // blind to fuzzy abbreviations, so retry unfiltered if it finds nothing.
        let scan = |skip: &HashSet<String>, deadline| {
            let mut h = crate::search::live_search(cwd, query, limit, skip, deadline, true);
            if h.is_empty() {
                h = crate::search::live_search(cwd, query, limit, skip, deadline, false);
            }
            h
        };
        match coverage.as_deref() {
            None => {
                hits = scan(&HashSet::new(), None);
            }
            Some("warming") => {
                let indexed: HashSet<String> = current
                    .and_then(|id| store.file_mtimes(id).ok())
                    .map(|m| m.into_keys().collect())
                    .unwrap_or_default();
                let deadline = Some(std::time::Instant::now() + LIVE_FALLBACK_BUDGET);
                hits = scan(&indexed, deadline);
            }
            _ => {}
        }
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

    // Now that results are out, warm the index a little more (bounded). This
    // builds coverage on a large repo across queries and, on an already-complete
    // repo, picks up new/changed files (active ones first) and drops deleted ones
    // once a full sweep finishes — so the index tracks edits without a daemon.
    //
    // On a *large, complete* repo the per-search re-walk is the dominant cost;
    // skip it when git confirms nothing changed since the index (HEAD identical
    // and a clean work tree). Small repos always warm — the walk is cheaper than
    // forking git for them — so they pay nothing for this check.
    if warming_ok
        && let Some(c) = &cwd
        && !repo_unchanged_since_index(&store, c, current, coverage.as_deref())
    {
        let _ = crate::index::index_budgeted(
            &mut store,
            c,
            &active_paths,
            DEFERRED_WARM_BUDGET,
            Some(query),
        );
    }

    ExitCode::SUCCESS
}

/// Above this many indexed files, a complete repo checks git for changes before
/// re-walking; below it the walk is cheap enough to just run.
const WARM_SKIP_MIN_FILES: i64 = 2000;

/// Whether a large, complete repo is provably unchanged since its last index —
/// same HEAD and a clean work tree — so the deferred re-walk can be skipped.
/// Conservative: any uncertainty (small repo, not complete, git hiccup) returns
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
    let files = store.repo_totals(id).map(|(f, _)| f).unwrap_or(0);
    if files <= WARM_SKIP_MIN_FILES {
        return false;
    }
    let indexed_head = store.indexed_head(id).ok().flatten();
    crate::index::git_head(cwd) == indexed_head
        && indexed_head.is_some()
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
const ANSWER_WARM_BUDGET: Duration = Duration::from_millis(500);

/// Deferred warm budget, spent after results are printed: larger, to make real
/// progress on coverage per query while keeping each invocation snappy.
const DEFERRED_WARM_BUDGET: Duration = Duration::from_millis(250);

/// Bound for the git-repo live-scan fallback (index empty, still warming): enough
/// to surface a result the warm hasn't reached, without an unbounded walk.
const LIVE_FALLBACK_BUDGET: Duration = Duration::from_millis(250);

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
    let positions: Vec<usize> = crate::search::match_positions(query, &path[base_byte..])
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
        if let Ok(crate::index::Refresh::Updated | crate::index::Refresh::Deleted) =
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
