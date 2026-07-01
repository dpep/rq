//! Command-line surface. Search is the default action: `rq <query>`.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
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
    about = "Ranked definition lookup — the one place a symbol is defined, first.",
    long_about = "rq finds where a symbol is defined and ranks the one you most \
likely meant to the top — not every match.\n\n\
Search is the default action; operations are flags, not subcommands, so every \
word (including \"index\", \"status\", \"record\") stays searchable. Ranking favors \
your current repo and recently-active files, and learns from the results you open \
(see RECORDING below). Run `rq <query> --explain` to see the score behind each result.",
    after_help = "EXAMPLES:\n  \
rq thing                  search for a definition named or like \"thing\"\n  \
rq wibble --explain       same, plus the score behind each result\n  \
rq thing --json           machine-readable results (for editors/agents)\n  \
rq thing --no-record      search without recording it (speculative/agent queries)\n  \
rq thing app/web          restrict to a directory (rg-style)\n  \
rq perform -k method      restrict to a symbol kind (c/mod/m/f/s/e/t)\n  \
rq class Widget           a leading kind keyword is shorthand for -k\n  \
rq --symbols FILE         outline a file's definitions, in line order\n  \
rq thing -x rust          restrict to a language (ruby/rust/go/python)\n  \
rq -o thing               open the best match in your editor (and record it)\n  \
rq --index                index the current repository\n  \
rq --status               show indexing coverage\n  \
rq --drop                 remove this repo's index (opposite of --index)\n\n\
SHORT FLAGS (easy to misread):\n  \
-j = --json (not jobs; --jobs is long-only)   -l = --limit (not lang)   -x = --lang\n\n\
RECORDING (editor/shell hook):\n  \
rq --record --file <path> --line <n> <query>\n  \
Tells rq which result you opened for a query, so ranking learns. Pass --no-record \
to a search to skip this. Editors and the script/rq-open wrapper call --record for you.\n\n\
The index is a SQLite file at $RQ_DB (default ~/.local/share/rq/rq.db); it warms \
automatically on the first search in a git repo. On a large, cold repo a search \
keeps indexing until it can answer rather than reporting a premature \"no \
matches\" (an interactive run shows progress and stops on Ctrl-C). Exit codes: 0 \
= matched, 1 = no match, 2 = no match yet (index still warming — try again)."
)]
struct Cli {
    /// Search query. With --drop, the repo path/identity to drop; with --record,
    /// the query the selection was made for.
    //
    // `Other` keeps shells from offering filenames here: a search query isn't a
    // path. The path-valued operations (--index, --symbols) carry their own
    // value with a path hint instead, so completion is scoped to them.
    #[arg(value_name = "TARGET", value_hint = clap::ValueHint::Other)]
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

    /// Open the best match in your editor and record the pick, so ranking learns.
    /// On a terminal with several matches, prompts to choose. Launcher: `RQ_OPEN`
    /// (a template with `{file}`/`{line}`/`{}` = path:line), else VS Code
    /// (`code`), else `$VISUAL`/`$EDITOR`, else prints the resolved path:line.
    #[arg(short = 'o', long, conflicts_with_all = ["index", "status", "record", "json", "ndjson"])]
    open: bool,

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

    /// Search every indexed repository, not just the current one. By default a
    /// search inside a repo returns only that repo's definitions.
    #[arg(long = "all-repos")]
    all_repos: bool,

    /// Index a repository (PATH, or the current directory).
    #[arg(long, value_name = "PATH", num_args = 0..=1, value_hint = clap::ValueHint::AnyPath, conflicts_with_all = ["status", "record"])]
    index: Option<Option<String>>,

    /// Show indexing coverage per known repository.
    #[arg(long, conflicts_with_all = ["index", "record"])]
    status: bool,

    /// List the symbols defined in FILE, in line order — a structural outline,
    /// not a ranked search. Honors -k/-x to filter by kind/language.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath, conflicts_with_all = ["index", "status", "record", "drop", "open"])]
    symbols: Option<String>,

    /// Drop a repository's index — the opposite of --index. Removes its symbols,
    /// files, coverage, and learned ranking. TARGET is the repo's path (or the
    /// current repo); a known identity string (as shown by --status) also works.
    #[arg(long, conflicts_with_all = ["index", "status", "record", "open"])]
    drop: bool,

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
    if let Some(path) = &cli.index {
        // index PATH (else cwd); with --path, only those subtrees (partial)
        let out = output_format(&cli);
        return cmd_index(path.as_deref().map(PathBuf::from), &cli.path, out);
    }
    if cli.status {
        return cmd_status(output_format(&cli));
    }
    if cli.drop {
        let out = output_format(&cli);
        return cmd_drop(cli.target, out);
    }
    if cli.record {
        // clap guarantees --file is present via `requires`
        let file = cli.file.expect("--record requires --file");
        return cmd_record(&cli.event, cli.target.as_deref(), &file, cli.line);
    }
    let out = output_format(&cli);
    let mut kinds: Vec<String> = cli.kind.iter().map(|k| canonical_kind(k)).collect();
    // a language token can expand to several tags (`r` → ruby + rust)
    let langs: Vec<String> = cli.lang.iter().flat_map(|x| canonical_langs(x)).collect();
    if let Some(file) = &cli.symbols {
        return cmd_symbols(file, &kinds, &langs, out);
    }
    // path filters: trailing positionals (rg-style) plus any --path flags
    let mut paths = cli.path.clone();
    match cli.target {
        Some(target) => {
            // A leading kind keyword (`rq class Foo`) is shorthand for `-k`; skip
            // it when the user gave an explicit `-k`, so the two never conflict.
            let query = if cli.kind.is_empty() {
                let (kw, query, dirs) = split_kind_keyword(target, cli.dirs.clone());
                if let Some(k) = kw {
                    kinds.push(k.to_string());
                }
                paths.extend(dirs);
                query
            } else {
                paths.extend(cli.dirs.clone());
                target
            };
            cmd_search(
                &query,
                cli.explain,
                out,
                &paths,
                &kinds,
                &langs,
                cli.limit,
                cli.no_record,
                cli.open,
                cli.all_repos,
            )
        }
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

/// How long a cold-repo query may wait silently before we tell the user we're
/// indexing — short enough to explain the pause, long enough that a repo which
/// indexes quickly never flashes a message.
const HEADS_UP_DELAY: Duration = Duration::from_millis(500);

/// Minimum gap between progress-line redraws once the heads-up is showing — keeps
/// the line from flickering (and the count query off the hot path) while still
/// feeling live.
const PROGRESS_REDRAW: Duration = Duration::from_millis(120);

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
    open: bool,
    all_repos: bool,
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
    // Default: scope results to the current repo (when it's indexed) so a search
    // never leaks another repo's definitions. `--all-repos` searches everything.
    let only_repo = if all_repos { None } else { current };
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

    // Block-until-answered on a cold/partial repo. A bounded warm exists so a
    // query never hangs, but on a *huge, cold* repo it can expire before the
    // symbol is indexed — turning a real hit into a false "no matches". Since
    // correctness beats the first query's latency (and once warm the repo answers
    // fast), we keep indexing until the answer appears or the repo is fully
    // indexed — for humans *and* programs alike. Small/medium repos finish inside
    // the normal budget and are unaffected; only a genuinely large cold repo
    // waits, and only once.
    let block = want_warm && was_warming;
    // A human at a plain-text terminal also gets a live progress heads-up and a
    // graceful Ctrl-C; piped/`--json` callers (agents, scripts) block silently and
    // are bounded by a wait budget instead, since there's nothing to draw to and
    // no one to interrupt.
    let progress_ui = block && show_progress(out, stderr_interactive());
    let indexer_budget = if block { wait_budget() } else { warm_budget };
    if progress_ui {
        install_interrupt_handler();
    }

    // `warm_done` lets the poll stop the instant the indexer finishes — so a miss
    // on a small repo returns as soon as it's indexed, not at the deadline.
    let warm_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let indexer = (want_warm && root.is_some()).then(|| {
        crate::trace!(
            "background warm ({indexer_budget:?}, block={block}, progress_ui={progress_ui}, {} jobs)",
            crate::index::parse_jobs()
        );
        let root = root.clone().expect("checked");
        let active = active_paths.clone();
        let q = query.to_string();
        let warm_done = std::sync::Arc::clone(&warm_done);
        std::thread::spawn(move || {
            if let Ok(mut idx) = open_store() {
                // path-prioritize toward the query so the relevant file indexes first
                let _ = if block {
                    // the abort flag (`INTERRUPTED`) lets a Ctrl-C, a wait timeout,
                    // or an early answer stop the pass without losing committed work
                    crate::index::index_budgeted_cancellable(
                        &mut idx,
                        &root,
                        &active,
                        indexer_budget,
                        Some(&q),
                        &INTERRUPTED,
                    )
                } else {
                    crate::index::index_budgeted(&mut idx, &root, &active, indexer_budget, Some(&q))
                };
            }
            warm_done.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    });

    // Poll while a cold/partial repo warms. Don't print the first hit off a sparse
    // index — a fuzzy or path match can be wrong once more is indexed. Hold until a
    // *high-confidence* (exact or prefix name) match appears; otherwise keep
    // building until the index is complete (a "no matches" is then trustworthy), a
    // wait deadline passes, or — interactively — Ctrl-C. A human sees a progress
    // line once the pause is noticeable.
    crate::trace!(
        "setup (open + repo detect + warm decision): {} ms",
        t_setup.elapsed().as_millis()
    );
    let poll_start = std::time::Instant::now();
    // Deadline: an interactive block waits unbounded (Ctrl-C escapes); a
    // programmatic block waits out the wait budget; a non-block (complete repo)
    // keeps the original fast answer budget.
    let deadline = if progress_ui {
        None
    } else if block {
        Some(poll_start + wait_budget())
    } else {
        Some(poll_start + answer_warm_budget())
    };
    let polling = indexer.is_some() && was_warming;
    let label = repo_label(root.as_deref());
    let mut drew_progress = false;
    let mut last_draw = poll_start;
    let mut hits = loop {
        match crate::search::search(&store, query, current, only_repo, &active, limit) {
            Ok(h) => {
                let confident = h.first().is_some_and(|hit| {
                    hit.features
                        .iter()
                        .any(|f| matches!(f.name, "exact" | "prefix"))
                });
                let warm_finished = warm_done.load(std::sync::atomic::Ordering::Relaxed);
                let stopped = INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed);
                let timed_out = deadline.is_some_and(|d| std::time::Instant::now() >= d);
                if !polling || confident || warm_finished || stopped || timed_out {
                    break h;
                }
                if progress_ui
                    && poll_start.elapsed() >= HEADS_UP_DELAY
                    && last_draw.elapsed() >= PROGRESS_REDRAW
                {
                    draw_progress(&store, identity.as_deref(), &label);
                    drew_progress = true;
                    last_draw = std::time::Instant::now();
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
    if drew_progress {
        clear_progress();
    }
    // Captured before we self-cancel below, so it reflects only a *user's* Ctrl-C.
    let interrupted = INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed);

    // Staleness: revalidate the files behind the top hits; re-rank once if changed.
    if !hits.is_empty() && revalidate_top(&mut store, &hits) {
        hits = crate::search::search(&store, query, current, only_repo, &active, limit)
            .unwrap_or_default();
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

    // Scope gate: a qualified query (`Foo::Bar#baz`) that lands inside the named
    // scope keeps only the in-scope results; if none match, the others stay (the
    // definition may live elsewhere).
    crate::search::apply_scope_gate(query, &mut hits);

    // post-filters: keep only results under a --path dir, of a --kind, and/or in
    // a --lang, then trim to the requested count.
    if !paths.is_empty() {
        // --path values may be absolute or cwd-relative; stored files are
        // repo-root-relative, so normalize before prefix-matching or an
        // absolute path would silently filter everything out.
        let here = cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        let base = root.clone().unwrap_or_else(|| here.clone());
        let norm: Vec<String> = paths
            .iter()
            .map(|p| repo_relative(&base, &here, p))
            .collect();
        hits.retain(|h| under_any(&h.file, &norm));
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
        // Stop a still-running block so the join is prompt, then settle coverage.
        if block {
            INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(h) = indexer {
            let _ = h.join();
        }
        // A miss against a *complete* index is definitive (the symbol isn't
        // there); against a still-warming one it's only "not yet". Distinguish
        // them so a caller — agent or script — isn't misled into thinking the
        // symbol is absent when the index simply hasn't reached it.
        let incomplete = block
            && identity
                .as_deref()
                .and_then(|id| store.coverage_status(id).ok().flatten())
                .as_deref()
                != Some("complete");
        // Structured callers get a reason, not a bare `[]`/empty: `warming`
        // (retry — index incomplete), `interrupted` (a stopped block), or
        // `no_match` (definitive). Text keeps its human message.
        let status = if interrupted {
            "interrupted"
        } else if incomplete {
            "warming"
        } else {
            "no_match"
        };
        match out {
            Output::Json | Output::Ndjson => {
                let obj = serde_json::json!({ "status": status, "query": query });
                match out {
                    Output::Json => {
                        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default())
                    }
                    _ => println!("{obj}"),
                }
            }
            Output::Text if interrupted => {
                eprintln!("rq: indexing interrupted — run again to finish")
            }
            Output::Text if incomplete => eprintln!(
                "rq: still indexing — no match for {query:?} yet (run again, or `rq --index` to finish)"
            ),
            Output::Text => eprintln!("no matches for {query:?}"),
        }
        // Exit 2 = indeterminate (index incomplete), 1 = a definitive miss. Both
        // non-zero, so `rq … && …` still reads as "found something".
        return if incomplete {
            ExitCode::from(2)
        } else {
            ExitCode::FAILURE
        };
    }

    // Attach each result's definition line (e.g. `def perform(refund)`) — shown
    // in text output and carried in JSON. Cheap: only the displayed results.
    for hit in &mut hits {
        hit.signature = read_signature(
            &store,
            &hit.repo_identity,
            &hit.file,
            hit.line,
            cwd.as_deref(),
        );
    }

    // --open: pick the best match (prompting on a TTY with several), record the
    // pick so ranking learns, and hand off to the editor. Returns before the
    // normal print / warm-join — opening should be snappy, and a launcher `exec`s.
    if open {
        return finish_open(
            &mut store,
            &hits,
            query,
            current,
            root.as_deref(),
            no_record,
        );
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
    // sweep, all without a daemon. A blocking pass runs on a generous budget, so
    // once we have the answer we stop it rather than wait on the rest (the next
    // query resumes from the committed batches).
    if block {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(h) = indexer {
        let _ = h.join();
    }

    ExitCode::SUCCESS
}

/// Pick a hit for `--open`: the top match, unless we're on an interactive
/// terminal with several — then print a short numbered menu and read a choice
/// (empty = the top match). `None` means abort (EOF or unparseable input).
fn choose_hit(hits: &[crate::search::Hit]) -> Option<&crate::search::Hit> {
    use std::io::{IsTerminal, Write};
    if hits.len() == 1 || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return hits.first();
    }
    let mut err = std::io::stderr();
    let _ = writeln!(err, "rq: {} matches — pick one (enter = 1):", hits.len());
    for (i, h) in hits.iter().enumerate() {
        let _ = writeln!(
            err,
            "  {}. {}:{}  {} {}",
            i + 1,
            h.file,
            h.line,
            h.kind,
            h.name
        );
    }
    let _ = write!(err, "rq> ");
    let _ = err.flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return None; // Ctrl-D
    }
    parse_choice(&line, hits.len()).and_then(|i| hits.get(i))
}

/// Resolve a menu reply to a 0-based index: blank → 0 (the top match), `N` → N-1
/// when in range, anything else → `None` (abort). Pure, so it's unit-tested.
fn parse_choice(input: &str, n: usize) -> Option<usize> {
    let s = input.trim();
    if s.is_empty() {
        return Some(0);
    }
    let i = s.parse::<usize>().ok()?.checked_sub(1)?;
    (i < n).then_some(i)
}

/// `--open`: choose a hit, record it as a selection so ranking learns, then hand
/// off to the editor. The launcher `exec`s (replacing this process), so the shell
/// waits on the editor — not on rq's background warm.
fn finish_open(
    store: &mut Store,
    hits: &[crate::search::Hit],
    query: &str,
    current: Option<i64>,
    root: Option<&std::path::Path>,
    no_record: bool,
) -> ExitCode {
    let Some(hit) = choose_hit(hits) else {
        return ExitCode::SUCCESS; // aborted at the prompt
    };

    // Record the pick — same signal as `rq --record`. The hit's path is already
    // repo-relative, which is what the selection rollup keys off.
    if !no_record {
        let _ = store.record_event(
            "select",
            Some(&query.to_ascii_lowercase()),
            current,
            Some(&hit.file),
            Some(hit.line),
            None,
        );
        deferred_maintenance(store);
    }

    // Results are repo-root-relative, so resolve against the root — the bare path
    // wouldn't open from a subdirectory.
    let target = match root {
        Some(r) => r.join(&hit.file),
        None => PathBuf::from(&hit.file),
    };
    launch_editor(&target, hit.line)
}

/// Launch the editor on `file:line`, resolving the command in order: `RQ_OPEN`
/// template → VS Code (`code`) → `$VISUAL`/`$EDITOR` → print the location. The
/// chosen command replaces this process via `exec`.
fn launch_editor(file: &std::path::Path, line: i64) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let loc = format!("{}:{}", file.display(), line);
    match open_command(file, line, &loc) {
        Some((prog, args)) => {
            // exec returns only on failure
            let err = std::process::Command::new(&prog).args(&args).exec();
            fail(format_args!("rq --open: cannot run {prog}: {err}"))
        }
        None => {
            println!("{loc}");
            ExitCode::SUCCESS
        }
    }
}

/// Resolve the editor command + args. `None` → no launcher configured (the
/// caller prints the location). `RQ_OPEN` is split on whitespace (no shell) with
/// `{file}` / `{line}` / `{}` (= `path:line`) substituted per token.
fn open_command(file: &std::path::Path, line: i64, loc: &str) -> Option<(String, Vec<String>)> {
    let fstr = file.to_string_lossy().into_owned();

    if let Some(t) = std::env::var_os("RQ_OPEN") {
        let t = t.to_string_lossy();
        let mut parts = t.split_whitespace().map(|p| {
            p.replace("{file}", &fstr)
                .replace("{line}", &line.to_string())
                .replace("{}", loc)
        });
        if let Some(prog) = parts.next() {
            return Some((prog, parts.collect()));
        }
    }

    if on_path("code") {
        return Some(("code".into(), vec!["--goto".into(), loc.into()]));
    }

    if let Some(ed) = std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) {
        let ed = ed.to_string_lossy().into_owned();
        let l = ed.to_ascii_lowercase();
        // line-aware launch for the common terminal editors; others just get the file
        if ["vim", "nvim", "vi", "nano", "emacs", "kak", "micro"]
            .iter()
            .any(|e| l.contains(e))
        {
            return Some((ed, vec![format!("+{line}"), fstr]));
        }
        return Some((ed, vec![fstr]));
    }

    None
}

/// Whether `prog` resolves on `PATH` (a regular file; symlinks followed).
fn on_path(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
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

/// How long a query may block indexing a cold repo before giving up with an
/// honest "still indexing" rather than a false miss. A generous backstop, not the
/// real cost: `index_budgeted` returns the moment the sweep completes, so any
/// normal repo finishes well under it, and an interactive run isn't bounded by it
/// at all (Ctrl-C escapes). It mainly bounds a programmatic caller on a
/// pathologically huge repo — where the partial index still persists for the next
/// query. `RQ_WAIT_BUDGET_MS=0` makes a programmatic caller non-blocking again —
/// it answers immediately from whatever's already indexed.
fn wait_budget() -> Duration {
    env_budget("RQ_WAIT_BUDGET_MS", 60_000)
}

/// Set by the SIGINT handler during an interactive cold-start escalation. The
/// poll loop and the running index pass watch it, so Ctrl-C stops the wait
/// promptly and prints the best partial results instead of killing the process.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    // Async-signal-safe: a lone relaxed atomic store — no allocation, no locks.
    INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Install the SIGINT handler once. Scoped to the escalation path: a normal fast
/// query keeps the default behavior (Ctrl-C kills it outright).
fn install_interrupt_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    });
}

/// Is a human watching stderr? True for a real terminal; `RQ_ASSUME_INTERACTIVE`
/// forces it on so the progress/Ctrl-C path is exercisable under test (where
/// stderr is a pipe), mirroring the `RQ_*_BUDGET_MS` testing knobs.
fn stderr_interactive() -> bool {
    std::io::stderr().is_terminal() || std::env::var_os("RQ_ASSUME_INTERACTIVE").is_some()
}

/// Whether to show the live "indexing…" progress heads-up and handle Ctrl-C
/// gracefully while a cold repo blocks — a human watching a plain-text terminal.
/// Piped / `--json` / `--ndjson` callers block silently instead (no line to draw,
/// no one to interrupt); the *decision to block* is the same for both.
fn show_progress(out: Output, interactive: bool) -> bool {
    interactive && matches!(out, Output::Text)
}

/// A short, friendly name for the repo being indexed — its directory name, for
/// the progress line.
fn repo_label(root: Option<&std::path::Path>) -> String {
    root.and_then(|r| r.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// Redraw the in-place "indexing…" progress line on stderr (kept off stdout so
/// piped/`--json` output stays clean). The file count comes from the index the
/// background pass is filling, so it climbs as warming proceeds.
fn draw_progress(store: &Store, identity: Option<&str>, label: &str) {
    let files = identity
        .and_then(|id| store.repository_id(id).ok().flatten())
        .and_then(|rid| store.repo_totals(rid).ok())
        .map_or(0, |(f, _)| f);
    eprint!("\r\x1b[Krq: indexing {label}… {files} files");
    let _ = std::io::stderr().flush();
}

/// Erase the progress line so results print to a clean terminal.
fn clear_progress() {
    eprint!("\r\x1b[K");
    let _ = std::io::stderr().flush();
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
    repo_identity: &str,
    file: &str,
    line: i64,
    cwd: Option<&std::path::Path>,
) -> Option<String> {
    let root = store
        .repository_id(repo_identity)
        .ok()
        .flatten()
        .and_then(|id| store.checkout_root(id).ok().flatten())
        .map(PathBuf::from)
        .or_else(|| cwd.map(std::path::Path::to_path_buf))?;
    let content = std::fs::read_to_string(root.join(file)).ok()?;
    signature_in(&content, line)
}

/// The trimmed source line `line` (1-based) of already-read `content`, if
/// non-empty — a symbol's definition line. Splitting this out lets `--symbols`
/// read one file once instead of re-reading it per symbol.
fn signature_in(content: &str, line: i64) -> Option<String> {
    let idx = usize::try_from(line).ok()?.checked_sub(1)?;
    let l = content.lines().nth(idx)?.trim();
    (!l.is_empty()).then(|| l.to_string())
}

/// One symbol in `rq --symbols` output. Same field names as a search hit
/// (`repo`, `signature`) for agent consistency, but no score/features — an
/// outline is structural, not ranked.
#[derive(serde::Serialize)]
struct SymbolOut {
    name: String,
    kind: String,
    language: String,
    file: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

/// `rq --symbols <file>`: list a file's symbols in line order — a structural
/// outline, not a ranked search. Warms the file's repo if it's cold/partial or
/// changed (same gate as search), then reads straight from the index. Honors
/// --kind/--lang filters and --json/--ndjson.
fn cmd_symbols(file_arg: &str, kinds: &[String], langs: &[String], out: Output) -> ExitCode {
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = crate::index::repo_root(&cwd).unwrap_or_else(|| cwd.clone());
    let rel = repo_relative(&root, &cwd, file_arg);

    let identity = resolve_identity(&store, &root);
    let coverage = store.coverage_status(&identity).ok().flatten();
    let warming_ok = (crate::index::is_git_repo(&root) || coverage.is_some())
        && coverage.as_deref() != Some("partial");
    let current = store.repository_id(&identity).ok().flatten();
    let needs_warm = warming_ok
        && (coverage.as_deref() != Some("complete")
            || !repo_unchanged_since_index(&store, &root, current, coverage.as_deref()));
    if needs_warm {
        // Path-prioritize the warm toward the requested file so it indexes first.
        let budget = answer_warm_budget() + deferred_warm_budget();
        let _ = crate::index::index_budgeted(&mut store, &root, &[], budget, Some(&rel));
    }

    let Some(repo_id) = store.repository_id(&identity).ok().flatten() else {
        return emit_symbols(out, &[]); // unknown / un-indexed repo → nothing
    };
    let mut rows = match store.symbols_in_file(repo_id, &rel) {
        Ok(r) => r,
        Err(e) => return fail(format_args!("rq: {e}")),
    };
    if !kinds.is_empty() {
        rows.retain(|r| kinds.iter().any(|k| k == &r.kind));
    }
    if !langs.is_empty() {
        rows.retain(|r| langs.iter().any(|l| l == &r.language));
    }

    // Read the source once for signatures (every row is the same file).
    let file_root = store
        .checkout_root(repo_id)
        .ok()
        .flatten()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let content = std::fs::read_to_string(file_root.join(&rel)).ok();
    let syms: Vec<SymbolOut> = rows
        .into_iter()
        .map(|r| SymbolOut {
            signature: content.as_deref().and_then(|c| signature_in(c, r.line)),
            name: r.name,
            kind: r.kind,
            language: r.language,
            file: r.file,
            line: r.line,
            end_line: r.end_line,
            parent: r.parent,
            repo: r.repo_identity,
        })
        .collect();
    emit_symbols(out, &syms)
}

/// Render the outline. Exit 0 if any symbols, non-zero if none — rq's exit-code
/// convention, matching how search reports an empty result per format.
fn emit_symbols(out: Output, syms: &[SymbolOut]) -> ExitCode {
    if syms.is_empty() {
        match out {
            Output::Json | Output::Ndjson => {
                let obj = serde_json::json!({ "status": "no_match" });
                match out {
                    Output::Json => {
                        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default())
                    }
                    _ => println!("{obj}"),
                }
            }
            Output::Text => eprintln!("no symbols"),
        }
        return ExitCode::FAILURE;
    }
    match out {
        Output::Ndjson => {
            for s in syms {
                match serde_json::to_string(s) {
                    Ok(line) => println!("{line}"),
                    Err(e) => return fail(format_args!("rq: {e}")),
                }
            }
        }
        Output::Json => match serde_json::to_string_pretty(&syms) {
            Ok(s) => println!("{s}"),
            Err(e) => return fail(format_args!("rq: {e}")),
        },
        Output::Text => {
            for s in syms {
                let qualified = match &s.parent {
                    Some(p) => format!("{} · {p}", s.name),
                    None => s.name.clone(),
                };
                println!("{}:{}  {} {}", s.file, s.line, s.kind, qualified);
                if let Some(sig) = &s.signature {
                    println!("    {sig}");
                }
            }
        }
    }
    ExitCode::SUCCESS
}

/// Normalize a `--kind` value (name or shortcut) to a canonical symbol kind.
/// Unknown values pass through lowercased (so they simply match nothing).
/// A leading positional that names a symbol kind — the shorthand behind
/// `rq class Foo` and `rq method zoom`. Only the full, unambiguous keyword forms
/// count (never the single-letter `-k` shortcuts, which are far likelier to be a
/// real query). Returns the canonical kind, so it filters exactly like `--kind`.
fn keyword_kind(token: &str) -> Option<&'static str> {
    match token.to_ascii_lowercase().as_str() {
        "class" => Some("class"),
        "module" => Some("module"),
        "method" => Some("method"),
        "function" | "fn" => Some("function"),
        "struct" => Some("struct"),
        "enum" => Some("enum"),
        "trait" => Some("trait"),
        _ => None,
    }
}

/// Peel a leading kind keyword off the query, so `rq class Foo` (or the quoted
/// `rq 'class Foo'`) means `-k class` + query `Foo`. The keyword must be followed
/// by a real query token — a bare `rq class` stays a search for a symbol literally
/// named `class`. Returns `(kind, query, trailing_path_dirs)`; the trailing dirs
/// are the rg-style positionals left after the query is consumed.
fn split_kind_keyword(
    target: String,
    dirs: Vec<String>,
) -> (Option<&'static str>, String, Vec<String>) {
    // Quoted form: the whole thing is one arg (`"class Foo"`), so peel the first
    // whitespace-separated word and keep the remainder as the query.
    if let Some((head, rest)) = target.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if let Some(k) = keyword_kind(head)
            && !rest.is_empty()
        {
            return (Some(k), rest.to_string(), dirs);
        }
    } else if let Some(k) = keyword_kind(&target)
        && let Some((query, extra)) = dirs.split_first()
    {
        // Unquoted form: `rq class Foo` — the next positional is the query.
        return (Some(k), query.clone(), extra.to_vec());
    }
    (None, target, dirs)
}

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

fn cmd_index(path: Option<PathBuf>, subdirs: &[String], out: Output) -> ExitCode {
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
    let identity = crate::index::detect_identity(&root).to_string();
    match crate::index::index_under(&mut store, &root, &subdirs) {
        Ok(stats) => {
            let partial = !subdirs.is_empty();
            // distinguish this run's incremental work from the index totals
            let totals = store
                .repository_id(&identity)
                .ok()
                .flatten()
                .and_then(|id| store.repo_totals(id).ok());
            match out {
                Output::Json | Output::Ndjson => {
                    let (files, symbols) = match totals {
                        Some((f, s)) => (Some(f), Some(s)),
                        None => (None, None),
                    };
                    return emit_json(
                        out,
                        &serde_json::json!({
                            "repo": identity,
                            "scope": if partial { "partial" } else { "full" },
                            "files_added": stats.files_indexed,
                            "symbols_added": stats.symbols,
                            "files": files,
                            "symbols": symbols,
                        }),
                    );
                }
                Output::Text => {
                    let scope = if partial { " (partial)" } else { "" };
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
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(format_args!("rq --index: {e}")),
    }
}

fn cmd_drop(target: Option<String>, out: Output) -> ExitCode {
    let mut store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };

    // Resolve the repo to drop: TARGET as a path (→ repo root → identity, like
    // --index), falling back to TARGET as a literal identity string — so cruft
    // shown by --status can be dropped by name even if the checkout is gone.
    let path = PathBuf::from(target.clone().unwrap_or_else(|| ".".to_string()));
    let root = crate::index::repo_root(&path).unwrap_or(path);
    let from_path = crate::index::detect_identity(&root).to_string();
    let resolved = match store.repository_id(&from_path) {
        Ok(Some(id)) => Some((from_path.clone(), id)),
        Ok(None) => target.as_deref().and_then(|s| {
            store
                .repository_id(s)
                .ok()
                .flatten()
                .map(|id| (s.to_string(), id))
        }),
        Err(e) => return fail(format_args!("rq --drop: {e}")),
    };

    let Some((identity, repo_id)) = resolved else {
        // nothing to drop — idempotent. `dropped: false` lets a script tell.
        return match out {
            Output::Text => {
                println!("not indexed: {from_path}");
                ExitCode::SUCCESS
            }
            _ => emit_json(
                out,
                &serde_json::json!({"repo": from_path, "files": 0, "symbols": 0, "dropped": false}),
            ),
        };
    };

    let (files, symbols) = store.repo_totals(repo_id).unwrap_or((0, 0));
    match store.drop_repository(repo_id) {
        Ok(()) => match out {
            Output::Text => {
                println!("dropped {identity} ({files} file(s), {symbols} symbol(s))");
                ExitCode::SUCCESS
            }
            _ => emit_json(
                out,
                &serde_json::json!({"repo": identity, "files": files, "symbols": symbols, "dropped": true}),
            ),
        },
        Err(e) => fail(format_args!("rq --drop: {e}")),
    }
}

/// Print a single value as JSON: `--json` pretty, `--ndjson` compact one-liner.
/// Used by the single-object operations (`--index`, `--drop`); `--status` builds
/// an array / one-row-per-line itself.
fn emit_json<T: serde::Serialize>(out: Output, value: &T) -> ExitCode {
    let rendered = if out == Output::Json {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match rendered {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(format_args!("rq: {e}")),
    }
}

fn cmd_status(out: Output) -> ExitCode {
    let store = match open_store() {
        Ok(s) => s,
        Err(e) => return fail(format_args!("rq: cannot open database: {e}")),
    };
    let rows = match store.coverage_overview() {
        Ok(rows) => rows,
        Err(e) => return fail(format_args!("rq --status: {e}")),
    };
    match out {
        Output::Json => match serde_json::to_string_pretty(&rows) {
            Ok(s) => println!("{s}"),
            Err(e) => return fail(format_args!("rq: {e}")),
        },
        Output::Ndjson => {
            for r in &rows {
                match serde_json::to_string(r) {
                    Ok(line) => println!("{line}"),
                    Err(e) => return fail(format_args!("rq: {e}")),
                }
            }
        }
        Output::Text if rows.is_empty() => {
            println!("no repositories indexed yet (try `rq --index`)");
        }
        Output::Text => {
            for r in &rows {
                println!(
                    "{:<10} {:>6} files  {:>7} symbols  {}",
                    r.status, r.files, r.symbols, r.identity
                );
            }
        }
    }
    ExitCode::SUCCESS
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
    fn open_menu_choice_parsing() {
        // blank reply takes the top match; a valid number maps to its index
        assert_eq!(parse_choice("\n", 5), Some(0));
        assert_eq!(parse_choice("  ", 5), Some(0));
        assert_eq!(parse_choice("3", 5), Some(2));
        assert_eq!(parse_choice("5", 5), Some(4));
        // out of range, zero, or non-numeric aborts
        assert_eq!(parse_choice("6", 5), None);
        assert_eq!(parse_choice("0", 5), None);
        assert_eq!(parse_choice("q", 5), None);
    }

    #[test]
    fn leading_kind_keyword_becomes_a_kind_filter() {
        let d = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // unquoted: `rq class Widget` — keyword + next positional is the query
        assert_eq!(
            split_kind_keyword("class".into(), d(&["Widget"])),
            (Some("class"), "Widget".into(), vec![])
        );
        // quoted: `rq 'method zoom'` — one arg, peel the first word
        assert_eq!(
            split_kind_keyword("method zoom".into(), vec![]),
            (Some("method"), "zoom".into(), vec![])
        );
        // `fn` is an alias for function; composes with a qualifier tail
        assert_eq!(
            split_kind_keyword("fn".into(), d(&["Foo::run"])),
            (Some("function"), "Foo::run".into(), vec![])
        );
        // extra positionals after the query stay as rg-style path dirs
        assert_eq!(
            split_kind_keyword("struct".into(), d(&["Gadget", "src"])),
            (Some("struct"), "Gadget".into(), d(&["src"]))
        );
    }

    #[test]
    fn a_bare_or_non_keyword_query_is_left_alone() {
        let d = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // a keyword with no following query token is a search for that literal name
        assert_eq!(
            split_kind_keyword("class".into(), vec![]),
            (None, "class".into(), vec![])
        );
        // an ordinary query is untouched, trailing dirs preserved
        assert_eq!(
            split_kind_keyword("Widget".into(), d(&["app"])),
            (None, "Widget".into(), d(&["app"]))
        );
        // single-letter `-k` shortcuts are NOT keywords here (too query-like)
        assert_eq!(
            split_kind_keyword("c".into(), d(&["Foo"])),
            (None, "c".into(), d(&["Foo"]))
        );
    }

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
    fn progress_ui_only_for_an_interactive_text_terminal() {
        // a person at a terminal, plain text → live progress + graceful Ctrl-C
        assert!(show_progress(Output::Text, true));

        // machine-readable output blocks silently (no progress line to corrupt it)
        assert!(!show_progress(Output::Json, true));
        assert!(!show_progress(Output::Ndjson, true));

        // not a terminal (a script/agent/pipe) — block, but without the UI
        assert!(!show_progress(Output::Text, false));
    }

    #[test]
    fn repo_label_uses_the_directory_name() {
        assert_eq!(
            repo_label(Some(std::path::Path::new("/src/widgets"))),
            "widgets"
        );
        assert_eq!(repo_label(None), "repo");
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
