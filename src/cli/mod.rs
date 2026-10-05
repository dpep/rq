//! Command-line surface. Search is the default action: `rq <query>`.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::core::{Kind, now_unix};
use crate::store::{Checkout, Coverage, Store};

/// Search is the default action (`rq <query>`). Operations are flags rather
/// than subcommands so no word is reserved — `rq index`, `rq status`, and
/// `rq record` all search for those symbols. This also matches the rg/fd feel.
#[derive(Parser)]
// modes are exclusive: `dispatch` would run whichever it checks first
#[command(group(
    clap::ArgGroup::new("mode")
        .args(["symbols", "index", "status", "drop", "usage", "warm", "completions"])
        .multiple(false)
))]
#[command(
    name = "rq",
    version,
    about = "rq finds the code you're looking for: type a name, get its definition.",
    long_about = "rq finds the code you're looking for. Name a class, method, function, \
struct or constant in Ruby, Rust, Go, Python, TypeScript or JavaScript, and rq shows \
where it's defined.",
    after_help = "EXAMPLES:\n  \
rq HashWithIndifferentAccess   the class's definition\n  \
rq hwia                        the same class: fuzzy and abbreviation-aware\n  \
rq 'Hash*Access'               wildcards: * any run, ? one char (quote them)\n  \
rq ActiveRecord::Base          the Base inside ActiveRecord\n  \
rq Persistence#save            the save inside Persistence\n  \
rq hugolib.HugoSites           a package, module or directory scopes too\n  \
rq Migration.new               its constructor (initialize, __init__, constructor)\n  \
rq class Base                  classes named Base (same as -k class)\n  \
rq perform activejob           the perform under activejob/\n  \
rq save -o                     open the best match in your editor\n  \
rq save --explain              the score behind each result\n  \
rq save --json                 JSON, for editors, scripts and agents\n  \
rq --symbols app/w.rb          outline a file, in line order\n\n\
SHORT FLAGS (easy to misread):\n  \
-j is --json, not --jobs (long-only)   -l is --limit, not --lang (that's -x)\n\n\
THE INDEX:\n  \
One SQLite file for all repos, at $RQ_DB (an absolute path; default\n  \
~/.local/share/rq/rq.db). On a cold repo, a search keeps indexing until it can\n  \
answer instead of reporting a false \"no matches\"; --no-wait answers right away.\n  \
A result from an index still being built says so (`warming`: files read of\n  \
the tree) and scales its confidence to the share read.\n\n\
EXIT CODES:\n  \
0   matched; for --status, --index, --drop: ran, even with nothing to show or drop\n  \
1   no match; for --usage: nothing recorded yet\n  \
2   no answer yet: the index is still warming or indexing was interrupted; ask\n      \
again. Matches found so far come back as `provisional`\n  \
64  usage error: a bad flag, value or query\n  \
66  a file named in the command doesn't exist\n  \
69  no editor, browser or git remote to hand off to\n  \
70  internal error (a bug)\n  \
74  can't open, read or write the index"
)]
struct Cli {
    /// A name, abbreviation, `Scope::name`, or `'Glob*'`. With --drop, a repo.
    //
    // `Other` keeps shells from offering filenames here: a search query isn't a
    // path. The path-valued operations (--index, --symbols) carry their own
    // value with a path hint instead, so completion is scoped to them.
    #[arg(value_name = "TARGET", value_hint = clap::ValueHint::Other)]
    target: Option<String>,

    /// Search only under these directories (like rg; same as --path).
    #[arg(value_name = "PATH")]
    dirs: Vec<String>,

    // Narrow the search
    /// Search only under this repo-relative directory (repeatable).
    #[arg(
        help_heading = "Narrow the search",
        short = 'p',
        long,
        value_name = "DIR"
    )]
    path: Vec<String>,

    /// Limit to these kinds: class, method, function, struct, …
    ///
    /// All kinds: class, module, method, function, struct, enum, trait, constant,
    /// type, variant, macro, field. Shortcuts: c, mod, m, f, s, e, t, const, v.
    /// `interface` = trait, `alias` = type, `member` = variant, `property` =
    /// field; `type` also takes structs. Repeatable or comma-separated.
    #[arg(
        help_heading = "Narrow the search",
        short = 'k',
        long,
        value_name = "KIND",
        value_delimiter = ','
    )]
    kind: Vec<String>,

    /// Limit to these languages: ruby, rust, go, python, ts, js
    ///
    /// Prefix-matched, so `r` means ruby and rust, and `p` means python. Also
    /// takes rb, rs, golang, typescript, tsx, javascript, jsx. Repeatable or
    /// comma-separated.
    #[arg(
        help_heading = "Narrow the search",
        short = 'x',
        long = "lang",
        value_name = "LANG",
        value_delimiter = ','
    )]
    lang: Vec<String>,

    /// Prefer definitions near this line: its scopes, then its file.
    ///
    /// Pass an editor's cursor, or the file an agent is reading. Definitions in
    /// the scopes around that line come first, then ones in the same file and
    /// nearby directories. It's context, not a filter. FILE is relative to the
    /// current directory; COL is accepted and ignored.
    #[arg(help_heading = "Narrow the search", long, value_name = "FILE:LINE[:COL]", value_parser = parse_anchor, conflicts_with = "mode")]
    anchor: Option<AnchorSpec>,

    /// Search every indexed checkout, not just this one.
    #[arg(help_heading = "Narrow the search", short = 'a', long = "all-repos")]
    all_repos: bool,

    /// Show at most N results; `0` shows them all.
    #[arg(help_heading = "Narrow the search", short = 'l', long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    limit: usize,

    // Output
    /// Show the score behind each result.
    #[arg(help_heading = "Output", short = 'e', long)]
    explain: bool,

    /// Print results as JSON, for editors, scripts and agents.
    #[arg(help_heading = "Output", short = 'j', long)]
    json: bool,

    /// Print results as NDJSON, one object per line.
    #[arg(help_heading = "Output", short = 'J', long, conflicts_with = "json")]
    ndjson: bool,

    /// Print the definition's source, when the top match is clear.
    ///
    /// Otherwise prints the list. Pipe it to a pager: `rq --show foo | less`.
    /// JSON adds a `body` field.
    #[arg(help_heading = "Output", long, conflicts_with_all = ["open", "web", "mode"])]
    show: bool,

    /// Open the best match in your editor.
    ///
    /// On a terminal with several matches, asks which one. Launcher: `RQ_OPEN` (a
    /// template with `{file}`, `{line}`, or `{}` for path:line; with none of them,
    /// path:line is appended), else VS Code (`code`), else `$VISUAL`/`$EDITOR`,
    /// else prints path:line.
    #[arg(help_heading = "Output", short = 'o', long, conflicts_with_all = ["mode", "json", "ndjson"])]
    open: bool,

    /// Open the best match on its git host, in the browser.
    ///
    /// A GitHub-style `blob/<sha>/<file>#L<line>` link, pinned to the newest
    /// pushed commit in HEAD's history so it resolves and stays accurate.
    /// Launcher: `$BROWSER`, else `open`/`xdg-open`, else prints the URL.
    #[arg(help_heading = "Output", short = 'w', long, conflicts_with_all = ["open", "mode", "json", "ndjson"])]
    web: bool,

    // Waiting on the index
    /// Answer now from what's indexed; never wait on a rebuild.
    ///
    /// For agents and scripts. Mid-rebuild, a miss reports `warming` (exit 2) so
    /// the caller can retry, and so does a best match a file not read yet could
    /// beat, with what was found as `provisional`. An exact match in the
    /// capitals typed answers, marked `warming`. A repo with nothing indexed yet
    /// is scanned live instead. Same as `--wait 0`; indexing carries on in the
    /// background.
    #[arg(help_heading = "Waiting on the index", long = "no-wait")]
    no_wait: bool,

    /// Wait at most this long for the index to warm (default 1m; at a
    /// terminal, until answered or Ctrl-C).
    ///
    /// `50ms`, `2s`, `1m`, or a bare number of seconds; `0` is --no-wait.
    /// Overrides `RQ_WAIT_BUDGET_MS` for this call.
    #[arg(help_heading = "Waiting on the index", long, value_name = "DUR", value_parser = parse_wait, conflicts_with = "no_wait")]
    wait: Option<Duration>,

    // The index
    /// List a file's definitions, in line order.
    ///
    /// Honors -k and -x.
    #[arg(help_heading = "The index", long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    symbols: Option<String>,

    /// Index a checkout now (PATH, or this one).
    ///
    /// Searches index on their own; this just does it up front.
    #[arg(help_heading = "The index", long, value_name = "PATH", num_args = 0..=1, value_hint = clap::ValueHint::AnyPath)]
    index: Option<Option<String>>,

    /// Show what's indexed, per checkout.
    #[arg(help_heading = "The index", long)]
    status: bool,

    /// Forget a checkout's index (the opposite of --index).
    ///
    /// TARGET is the checkout's path (default: this one), or a repo identity as
    /// --status shows it, which forgets every checkout of the repo.
    #[arg(help_heading = "The index", long)]
    drop: bool,

    /// Show searches per day, by caller and flags.
    #[arg(help_heading = "The index", long)]
    usage: bool,

    /// Finish warming a repository's index in the background — the target a
    /// search re-execs after printing results, detached, so the shell never
    /// waits on it. Single-flighted per checkout; safe to run by hand.
    #[arg(long, hide = true, value_name = "PATH", num_args = 0..=1, value_hint = clap::ValueHint::AnyPath)]
    warm: Option<Option<String>>,

    // Debugging
    /// Trace rq's decisions to stderr (`RQ_LOG=1` when installed).
    ///
    /// Root, coverage, warming and reconcile decisions.
    #[arg(help_heading = "Debugging", short = 'v', long)]
    verbose: bool,

    /// Time each search phase to stderr (`RQ_PROFILE=1` when installed).
    ///
    /// Prints JSON when used with --json, so you can store a baseline and diff
    /// against it.
    #[arg(help_heading = "Debugging", long)]
    profile: bool,

    /// Set parse threads for indexing; 0 = auto (or `RQ_JOBS`).
    ///
    /// Long-only, since `-j` is --json.
    #[arg(
        help_heading = "Debugging",
        long,
        value_name = "N",
        default_value_t = 0
    )]
    jobs: usize,

    /// Print a shell completion script.
    #[arg(help_heading = "Debugging", long, value_name = "SHELL")]
    completions: Option<Shell>,
}

/// Parse arguments and dispatch. Returns the process exit code.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return clap_failure(err),
    };
    crate::trace::enable_from(cli.verbose);
    crate::profile::enable_from(cli.profile);
    crate::index::set_parse_jobs(cli.jobs);
    let json_out = output_format(&cli) != Output::Text;
    let code = dispatch(cli);
    crate::profile::emit(json_out);
    code
}

fn dispatch(cli: Cli) -> ExitCode {
    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "rq", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }
    if let Err(message) = db_location() {
        return fail(
            output_format(&cli),
            Failure::Usage,
            format_args!("{message}"),
        );
    }
    if let Some(path) = &cli.index {
        // index PATH (else cwd); with --path, seed only those subtrees
        let out = output_format(&cli);
        return cmd_index(path.as_deref().map(PathBuf::from), &cli.path, out);
    }
    if let Some(path) = &cli.warm {
        return cmd_warm(path.as_deref());
    }
    if cli.status {
        return cmd_status(output_format(&cli));
    }
    if cli.usage {
        return cmd_usage(output_format(&cli));
    }
    if cli.drop {
        let out = output_format(&cli);
        return cmd_drop(cli.target, out);
    }
    let out = output_format(&cli);
    // Kind-only browsing (`rq '' -k trait`) is listing, not navigation, and a
    // partial index would list partially; a file's outline is the listing rq has.
    if cli.target.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return fail(
            out,
            Failure::Usage,
            format_args!(
                "rq: empty query — name the definition to find; `rq --symbols FILE` lists a file's (-k to filter)"
            ),
        );
    }
    // Reject an unknown --kind/--lang rather than filtering everything away: a
    // typo used to come back as `no_match`, exit 1 — the one code a script is
    // meant to trust as "this symbol does not exist".
    let mut kinds: Vec<String> = Vec::new();
    for k in &cli.kind {
        match canonical_kind(k) {
            Some(c) => kinds.extend(c.iter().map(|k| k.to_string())),
            None => {
                let known: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
                return fail(
                    out,
                    Failure::Usage,
                    format_args!("rq: unknown --kind {k:?} ({})", known.join(", ")),
                );
            }
        }
    }
    // a language token can expand to several tags (`r` → ruby + rust)
    let mut langs: Vec<String> = Vec::new();
    for x in &cli.lang {
        let matched = canonical_langs(x);
        if matched.is_empty() {
            return fail(
                out,
                Failure::Usage,
                format_args!(
                    "rq: unknown --lang {x:?} ({})",
                    crate::lang::languages().join(", ")
                ),
            );
        }
        langs.extend(matched);
    }
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
                    kinds.extend(k.iter().map(|k| k.to_string()));
                }
                paths.extend(dirs);
                query
            } else {
                paths.extend(cli.dirs.clone());
                target
            };
            let mut session = match Session::open(out) {
                Ok(s) => s,
                Err(code) => return code,
            };
            session.anchor = cli.anchor.as_ref().map(|a| session.anchor_at(a));
            cmd_search(
                &mut session,
                &SearchArgs {
                    query: &query,
                    explain: cli.explain,
                    out,
                    paths: &paths,
                    kinds: &kinds,
                    langs: &langs,
                    want: requested_limit(cli.limit),
                    no_wait: cli.no_wait,
                    wait: cli.wait,
                    open: cli.open,
                    web: cli.web,
                    all_repos: cli.all_repos,
                    show: cli.show,
                    batch: false,
                    anchored: cli.anchor.is_some(),
                },
            )
        }
        // No query, but a pipe on stdin: each line is one, all sharing this
        // run's store, repo resolution and warm.
        None if !std::io::stdin().is_terminal() => cmd_batch(&cli, out, &paths, &kinds, &langs),
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

/// Results shown when `--limit` isn't given.
const DEFAULT_LIMIT: usize = 10;

/// Minimum headroom to rank before a `--path` filter (so filtered-in results
/// aren't lost to the cutoff).
const PATH_HEADROOM: usize = 200;

/// `--limit 0` means unlimited: every ranked hit, bounded only by how many
/// candidates recall returned.
fn requested_limit(limit: usize) -> usize {
    if limit == 0 { usize::MAX } else { limit }
}

/// Count one search for `--usage`. Observability only: nothing reads it back
/// into ranking.
fn record_usage(
    store: &Store,
    args: &SearchArgs,
    verdict: Verdict,
    coverage: Option<Coverage>,
    live: bool,
) {
    let _ = store.record_search(&crate::store::SearchRecord {
        source: &crate::origin::detect(),
        flags: &flag_summary(args),
        status: verdict.as_str(),
        coverage,
        live,
    });
}

/// The call's flags as a canonical, comma-joined string, for usage counts.
/// A fixed vocabulary in a fixed order, so the same call always produces the
/// same string and the counter table stays small — values are never included,
/// only which knobs were reached for.
fn flag_summary(args: &SearchArgs) -> String {
    let mut on: Vec<&str> = Vec::new();
    match args.out {
        Output::Json => on.push("json"),
        Output::Ndjson => on.push("ndjson"),
        Output::Text => {}
    }
    for (present, name) in [
        (args.explain, "explain"),
        (args.show, "show"),
        (args.open, "open"),
        (args.web, "web"),
        (args.all_repos, "all-repos"),
        (args.no_wait, "no-wait"),
        (args.batch, "batch"),
        (args.anchored, "anchor"),
        (!args.paths.is_empty(), "path"),
        (!args.kinds.is_empty(), "kind"),
        (!args.langs.is_empty(), "lang"),
        (args.want != DEFAULT_LIMIT, "limit"),
    ] {
        if present {
            on.push(name);
        }
    }
    on.join(",")
}

/// How often the search re-checks the index while a cold repo warms on the
/// background thread. Each poll runs a full read query against the DB the
/// indexer is actively writing, so polling too fast steals CPU and read-lock
/// churn from the warm; 100 ms keeps that pressure low while staying
/// imperceptible (an early answer or completion appears within a frame, and the
/// progress line only redraws every `PROGRESS_REDRAW` anyway).
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a cold-repo query may wait silently before we tell the user we're
/// indexing — short enough to explain the pause, long enough that a repo which
/// indexes quickly never flashes a message.
const HEADS_UP_DELAY: Duration = Duration::from_millis(500);

/// Minimum gap between progress-line redraws once the heads-up is showing — keeps
/// the line from flickering (and the count query off the hot path) while still
/// feeling live.
const PROGRESS_REDRAW: Duration = Duration::from_millis(120);

/// How long a search follows another process's indexing while nobody commits
/// to the index. A live pass commits many times a second, and pauses under a
/// second between batches (a 97k-file rebuild, name index included); a
/// stopped one, or a crashed pass's mark on a reused pid, never commits again.
const INDEXER_STALL: Duration = Duration::from_secs(5);

/// Everything `rq <query>` needs, bundled from the parsed CLI flags.
struct SearchArgs<'a> {
    query: &'a str,
    explain: bool,
    out: Output,
    paths: &'a [String],
    kinds: &'a [String],
    langs: &'a [String],
    /// Number of results to show (`--limit`).
    want: usize,
    /// Answer from the committed index without blocking on a (re)index (`--no-wait`).
    no_wait: bool,
    /// Cap on how long to wait for the index to warm (`--wait`); `None` = the
    /// default/`RQ_WAIT_BUDGET_MS` budget.
    wait: Option<Duration>,
    open: bool,
    web: bool,
    all_repos: bool,
    /// One of several queries sharing a run, so each row says which query it
    /// answers — a single stream serving many questions is otherwise
    /// unattributable.
    batch: bool,
    show: bool,
    /// Asked from a position (`--anchor`); the anchor itself lives on the session.
    anchored: bool,
}

/// Where a command is asked from, and what the index knows of it. Every
/// command that acts on "this checkout" builds one, so they agree on which
/// checkout that is and whether rq may warm it.
struct Here {
    /// The git work tree's root, else the directory itself — never a subdir
    /// of a repo, which would re-key it under subdir-relative paths that the
    /// deletion reconcile would then forget. Canonical, as indexing keys it.
    root: PathBuf,
    is_git: bool,
    coverage: Option<Coverage>,
    checkout: Option<Checkout>,
}

impl Here {
    fn at(store: &Store, start: &std::path::Path) -> Here {
        let (root, is_git) = checkout_root(start);
        let mut here = Here {
            root,
            is_git,
            coverage: None,
            checkout: None,
        };
        here.refresh(store);
        here
    }

    /// Re-read what the index knows, after a pass here.
    fn refresh(&mut self, store: &Store) {
        self.coverage = store.coverage_status(&root_key(&self.root)).ok().flatten();
        self.checkout = checkout_at(store, &self.root);
    }

    /// Whether rq may index here: a git work tree (safe to auto-discover), or
    /// any dir it already tracks — one earns tracking by being explicitly
    /// `--index`ed. Never an unknown non-git dir: don't walk a random directory.
    fn warms(&self) -> bool {
        self.is_git || self.coverage.is_some()
    }
}

/// [`Here::root`] for a command at `start`, and whether it's a git work tree.
fn checkout_root(start: &std::path::Path) -> (PathBuf, bool) {
    match crate::index::repo_root(start) {
        Some(root) => (root, true),
        None => (
            start.canonicalize().unwrap_or_else(|_| start.to_path_buf()),
            false,
        ),
    }
}

/// Default action: search the index and print ranked results.
/// Everything a search needs that doesn't depend on the query: the open store,
/// the repo it's rooted in, the branch's changed files, and who that repo is.
///
/// Split out because it's the expensive half — opening the store, resolving the
/// root, reading branch files, resolving identity — and none of it varies per
/// query. One search builds one and drops it; a caller answering many can build
/// it once. Deliberately *not* holding the warm decision: that one is entangled
/// with the query (the indexer path-prioritises toward it) and belongs to a
/// single search.
struct Session {
    store: Store,
    cwd: Option<PathBuf>,
    /// The checkout the cwd is in; `None` when there's no cwd to ask from.
    here: Option<Here>,
    active_paths: Vec<String>,
    branch_refresh: Option<BranchRefresh>,
    identity: Option<String>,
    /// Where the queries are asked from (`--anchor`), resolved once.
    anchor: Option<crate::search::Anchor>,
    /// Whether the worktree has moved since its index ([`worktree_moved`]),
    /// once someone in this process asked — and dispatched the reindex that
    /// answer called for. A batch asks up front; a single search on a miss.
    moved: Option<bool>,
}

impl Session {
    /// Resolve the search context, or the exit code to fail with.
    fn open(out: Output) -> std::result::Result<Session, ExitCode> {
        let open_span = crate::profile::span("store open");
        let store = open_store_or_fail(out, "rq")?;
        drop(open_span);
        let git_span = crate::profile::span("setup: git root");
        let cwd = std::env::current_dir().ok();
        let here = cwd.as_deref().map(|c| Here::at(&store, c));
        drop(git_span);

        // Files you're changing on this feature branch (and their directory
        // neighbors): the branch ranking boost, and the warm pass's priority set.
        let mut branch_span = crate::profile::span("setup: branch files");
        let (active_paths, branch_refresh, cached_cost) = match &here {
            Some(h) if h.is_git => cached_branch_files(&store, &h.root),
            _ => (Vec::new(), None, None),
        };
        branch_span.note(|| {
            let how = if branch_refresh.is_some() {
                "cached, refreshing alongside"
            } else {
                "cached"
            };
            // The window is derived, not constant — say which one is in force,
            // or a slow repo's backoff looks like rq ignoring stale state.
            let ttl = branch_files_ttl(cached_cost);
            format!("{} changed, {how} ({ttl}s window)", active_paths.len())
        });
        drop(branch_span);

        // Resolve identity from the repo root, cache-first: looked up by checkout root
        // (no `git remote` fork), falling back to git only the first time we see a
        // repo. Computed even for non-git dirs so an explicitly `--index`ed one is
        // still recognized as the current repo below.
        let mut identity_span = crate::profile::span("setup: identity");
        let identity = here.as_ref().map(|h| resolve_identity(&store, &h.root));
        identity_span.note(|| {
            let coverage = here.as_ref().and_then(|h| h.coverage);
            coverage.map_or("unknown", Coverage::as_str).to_string()
        });
        drop(identity_span);
        Ok(Session {
            store,
            cwd,
            here,
            active_paths,
            branch_refresh,
            identity,
            anchor: None,
            moved: None,
        })
    }

    /// Resolve `--anchor` against this session: the repo its file sits in (the
    /// one we're in, when it's under our root), its path there, and what
    /// encloses its line — read live if the index doesn't hold this version.
    fn anchor_at(&self, spec: &AnchorSpec) -> crate::search::Anchor {
        let _span = crate::profile::span("setup: anchor");
        let here = self.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        let abs = here.join(&spec.file);
        let abs = abs.canonicalize().unwrap_or(abs);
        let root = self
            .here
            .as_ref()
            .map(|h| h.root.clone())
            .filter(|r| abs.starts_with(r))
            .or_else(|| crate::index::repo_root(&abs))
            .or_else(|| abs.parent().map(PathBuf::from))
            .unwrap_or(here);
        let identity = resolve_identity(&self.store, &root);
        let rel = crate::index::rel_key(&root, &abs)
            .unwrap_or_else(|| spec.file.to_string_lossy().into_owned());
        let checkout = checkout_at(&self.store, &root);
        let defs = crate::index::current_definitions(&self.store, checkout, &identity, &root, &rel);
        crate::search::Anchor::new(root_key(&root), rel, spec.line, &defs)
    }
}

/// A parsed `--anchor FILE:LINE[:COL]`.
#[derive(Debug, Clone, PartialEq)]
struct AnchorSpec {
    file: PathBuf,
    line: i64,
}

/// Parse `FILE:LINE` or `FILE:LINE:COL` (1-based), splitting from the right so
/// a path holding a colon still parses.
fn parse_anchor(s: &str) -> Result<AnchorSpec, String> {
    let num = |t: &str| t.parse::<i64>().ok().filter(|n| *n > 0);
    let bad = || format!("expected FILE:LINE[:COL], got {s:?}");
    let (rest, last) = s.rsplit_once(':').ok_or_else(bad)?;
    let last = num(last).ok_or_else(bad)?;
    // `FILE:LINE:COL` when what precedes the last number is itself a number
    let (file, line) = rest
        .rsplit_once(':')
        .and_then(|(file, line)| num(line).map(|line| (file, line)))
        .unwrap_or((rest, last));
    if file.is_empty() {
        return Err(bad());
    }
    Ok(AnchorSpec {
        file: PathBuf::from(file),
        line,
    })
}

/// Answer a stream of queries, one per line on stdin, in a single run.
///
/// Everything a query doesn't vary — the store, the repo, its identity, the
/// branch's changed files — is resolved once and reused, which on a large repo
/// is most of what a single lookup costs. Agents and scripts do runs of
/// lookups; this is that shape.
///
/// A cold repo warms **once, up front, until complete** rather than answering
/// each line from whatever happens to be indexed. Block-until-*answered*
/// doesn't generalise to queries we haven't read yet — you can't prioritise
/// toward them — so block-until-*complete* is the batch-shaped equivalent, and
/// it keeps this file's own rule that correctness beats the first query's
/// latency. `--no-wait` opts out, exactly as it does for one query, and any
/// line the index can't yet answer says so with `status: "warming"`.
fn cmd_batch(
    cli: &Cli,
    out: Output,
    paths: &[String],
    kinds: &[String],
    langs: &[String],
) -> ExitCode {
    if out == Output::Json {
        return fail(
            out,
            Failure::Usage,
            format_args!(
                "rq: --json can't frame a stream of queries — use --ndjson (-J), \
             where each line carries the query it answers"
            ),
        );
    }
    if cli.open || cli.web || cli.show {
        return fail(
            out,
            Failure::Usage,
            format_args!(
                "rq: --open, --web and --show act on a single result, not a stream of queries"
            ),
        );
    }

    use std::io::BufRead;
    let queries: Vec<String> = std::io::stdin()
        .lock()
        .lines()
        .map_while(std::result::Result::ok)
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // Nothing on stdin isn't a batch — it's a bare invocation that happens to
    // run without a terminal (a script, a test harness, stdin from /dev/null).
    // Treat it the way `rq` with no arguments is always treated.
    if queries.is_empty() {
        let _ = Cli::command().print_long_help();
        return ExitCode::SUCCESS;
    }

    let mut session = match Session::open(out) {
        Ok(s) => s,
        Err(code) => return code,
    };
    session.anchor = cli.anchor.as_ref().map(|a| session.anchor_at(a));

    // Whether the worktree moved is the repo's question, not a query's: a
    // batch asks it once, here, where a single search asks after answering.
    let warming_ok = session.here.as_ref().is_some_and(Here::warms);
    let mut moved = false;
    if warming_ok
        && let Some(here) = &session.here
        && here.coverage == Some(Coverage::Complete)
    {
        let store = &session.store;
        let head = here
            .checkout
            .and_then(|c| store.indexed_head(c.id).ok().flatten());
        moved = worktree_moved(store, &here.root, head.as_deref());
        session.moved = Some(moved);
        if moved && cli.no_wait {
            spawn_detached_warm(&here.root);
        }
    }

    // Warm to completion before answering anything, so a cold or edited repo
    // doesn't return a page of misses that only mean "not indexed yet".
    if !cli.no_wait
        && warming_ok
        && let Some(here) = &mut session.here
        && (here.coverage != Some(Coverage::Complete) || moved)
    {
        let budget = cli.wait.unwrap_or_else(wait_budget);
        crate::trace!(
            "batch: warming {} queries' worth of index first",
            queries.len()
        );
        let active = &session.active_paths;
        let _ = crate::index::index_budgeted(&mut session.store, &here.root, active, budget, None);
        here.refresh(&session.store);
        // the index has just caught up with the worktree
        session.moved = Some(false);
    }

    let codes: Vec<ExitCode> = queries
        .iter()
        .map(|query| {
            cmd_search(
                &mut session,
                &SearchArgs {
                    query,
                    explain: cli.explain,
                    out,
                    paths,
                    kinds,
                    langs,
                    want: requested_limit(cli.limit),
                    // The warm happened above, once. Per-query warming would undo
                    // the point of batching, and block-until-answered is meaningless
                    // when the queries were all read up front.
                    no_wait: true,
                    wait: cli.wait,
                    open: false,
                    web: false,
                    all_repos: cli.all_repos,
                    show: false,
                    batch: true,
                    anchored: cli.anchor.is_some(),
                },
            )
        })
        .collect();
    batch_exit(&codes)
}

/// A batch's exit code. The batch ran, and per-line `status` carries each
/// query's outcome, so only a wholly fruitless batch reports failure,
/// mirroring one query's contract — then with the code the caller can act on
/// most: an error, then "retry" (2), and a miss (1) only when a retry would
/// change nothing.
fn batch_exit(codes: &[ExitCode]) -> ExitCode {
    let [hit, miss, retry] =
        [Verdict::Hit, Verdict::Miss, Verdict::Warming].map(Verdict::exit_code);
    if codes.contains(&hit) {
        hit
    } else if let Some(&error) = codes.iter().find(|&&c| c != retry && c != miss) {
        error
    } else if codes.contains(&retry) {
        retry
    } else {
        miss
    }
}

fn cmd_search(session: &mut Session, args: &SearchArgs) -> ExitCode {
    let &SearchArgs {
        query,
        out,
        want,
        no_wait,
        wait,
        open,
        web,
        all_repos,
        show,
        ..
    } = args;
    // `--wait DUR` overrides the wait budget for this call; `--wait 0` (or
    // `--no-wait`) means don't block or warm in-process at all.
    let wait_budget = wait.unwrap_or_else(wait_budget);
    let no_wait = no_wait || wait_budget.is_zero();
    // post-filters (--path, --kind, --lang) need headroom before the cutoff so a
    // filtered-in result isn't lost to the top-N truncation
    let limit = if args.paths.is_empty() && args.kinds.is_empty() && args.langs.is_empty() {
        want
    } else {
        want.saturating_mul(20).max(PATH_HEADROOM)
    };
    let _timer = crate::trace::Timer::start("search done");
    let t_setup = std::time::Instant::now();
    // Brackets the warm decision as well as the session, so it outlives both.
    let setup_span = crate::profile::span("setup");
    // Borrowed field-by-field so the body reads the same as when it owned them,
    // while the session itself outlives this call and can answer again.
    let Session {
        store,
        cwd,
        here: place,
        active_paths,
        branch_refresh,
        identity,
        anchor,
        moved,
    } = session;
    let root = place.as_ref().map(|h| h.root.as_path());
    let coverage = place.as_ref().and_then(|h| h.coverage);
    // `-a` reads every checkout, so none may be one deleted from disk
    if all_repos {
        crate::index::prune_missing_checkouts(store);
    }

    // Opportunistic indexing (Layer 5), time-bounded so the first query in a
    // large repo never blocks on a full walk, wherever `Here::warms`. A subtree
    // index (`--index --path …`) is a seed, not a fence: coverage stays
    // `warming`, so warming continues over the rest of the repo from here.
    let warming_ok = place.as_ref().is_some_and(Here::warms);
    if crate::trace::enabled() {
        crate::trace!(
            "query {query:?}: root={} identity={} coverage={} warming_ok={warming_ok} active={}",
            root.map_or("?".into(), crate::trace::abbrev),
            identity.as_deref().unwrap_or("none"),
            coverage.map_or("none", Coverage::as_str),
            active_paths.len(),
        );
    }
    let repo_span = crate::profile::span("setup: repo state");
    let checkout_here = |store: &Store| root.and_then(|r| checkout_at(store, r));
    let mut current = checkout_here(store);
    // Default: scope results to the current checkout so a search never leaks
    // another tree's definitions. `--all-repos` searches everything.
    let scope = |current| Scope::new(all_repos, warming_ok, current);
    let ctx = crate::search::Context {
        active: crate::search::ActiveFiles::new(active_paths.clone()),
        anchor: anchor.clone(),
    };

    drop(repo_span);
    let warm_span = crate::profile::span("setup: warm decision");

    // Warm the index on a background thread (its own connection — WAL lets it
    // write while we read) whenever there's work: a not-yet-complete repo, or a
    // complete one changed since it was indexed. The search below reads whatever
    // it has committed so far. This in-process warm only serves *this*
    // answer — leftover work goes to a detached child after results print, so
    // the shell never waits on it.
    let was_warming = coverage != Some(Coverage::Complete);

    // On a complete repo the only question left is whether the worktree moved
    // since it was indexed — and answering it forks `git status`, which on a
    // large worktree is most of a query's cost. It decides nothing this answer
    // depends on: with `was_warming` false, `block` and `polling` below are
    // false too, the search reads the committed index, and `revalidate_top`
    // guarantees the freshness of what we print. So start it alongside the
    // search and collect it in `settle_warm` once results are out.
    //
    // A still-warming repo never ran this check at all — the `||` short-circuit
    // saw to that — so its path here is unchanged.
    let indexed_head = (!was_warming)
        .then(|| current.and_then(|c| store.indexed_head(c.id).ok().flatten()))
        .flatten();
    let staleness = (!was_warming && warming_ok)
        .then_some(root)
        .flatten()
        .map(|c| Staleness(c.to_path_buf(), indexed_head));
    // Only a repo that's still warming warms *before* the answer now; a
    // complete-but-edited one is reindexed by `settle_warm` afterwards.
    let want_warm = warming_ok && was_warming && root.is_some();

    // Block-until-answered on a cold/partial repo. A bounded warm exists so a
    // query never hangs, but on a *huge, cold* repo it can expire before the
    // symbol is indexed — turning a real hit into a false "no matches". Since
    // correctness beats the first query's latency (and once warm the repo answers
    // fast), we keep indexing until the answer appears or the repo is fully
    // indexed — for humans *and* programs alike. Small/medium repos finish inside
    // the normal budget and are unaffected; only a genuinely large cold repo
    // waits, and only once.
    // `--no-wait`: a scripted/agent caller that would rather answer from the
    // committed index right now than block up to the wait budget while a
    // background rebuild rewrites the index. It suppresses the block-until-answered
    // escalation *and* the in-process warm (no lock contention, no join) — leftover
    // warming still detaches below, so the index keeps improving for next time.
    let block = want_warm && !no_wait;
    let here = root.map(root_key);
    // `-a` reads other checkouts too: one another process is still filling
    // can hold a better match, so the search waits on it as on its own warm
    let follow_others =
        all_repos && warming_ok && !no_wait && others_open(store, here.as_deref()).is_some();
    // A human at a plain-text terminal also gets a live progress heads-up and a
    // graceful Ctrl-C; piped/`--json` callers (agents, scripts) block silently and
    // are bounded by a wait budget instead, since there's nothing to draw to and
    // no one to interrupt.
    let progress_ui = (block || follow_others) && show_progress(out, stderr_interactive());
    if progress_ui {
        install_interrupt_handler();
    }

    // `warm_done` lets the poll stop the instant the indexer finishes — so a miss
    // on a small repo returns as soon as it's indexed, not at the deadline.
    let warm_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Set once the warm has read every file containing the query's name: from
    // then on no unread file can hold an exact or prefix match for it.
    let demanded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let indexer = block.then(|| {
        crate::trace!(
            "background warm ({wait_budget:?}, progress_ui={progress_ui}, {} jobs)",
            crate::index::parse_jobs()
        );
        let root = root.expect("checked").to_path_buf();
        let active = active_paths.clone();
        let q = query.to_string();
        let warm_done = std::sync::Arc::clone(&warm_done);
        let demanded = std::sync::Arc::clone(&demanded);
        std::thread::spawn(move || {
            if let Ok(mut idx) = open_store() {
                // path-prioritize toward the query so the relevant file indexes
                // first; the abort flag (`INTERRUPTED`) lets a Ctrl-C, a wait
                // timeout, or an early answer stop the pass without losing
                // committed work
                let pass = crate::index::index_budgeted_cancellable(
                    &mut idx,
                    &root,
                    &active,
                    wait_budget,
                    Some(&q),
                    &INTERRUPTED,
                    &demanded,
                );
                if let Err(e) = pass {
                    crate::trace!("warm {}: pass failed: {e}", crate::trace::abbrev(&root));
                }
            }
            warm_done.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    });

    // Poll while a cold/partial repo warms. Don't print the first hit off a sparse
    // index — a prefix, fuzzy or path match can be wrong once more is indexed.
    // Hold until the top match is `settled`; otherwise keep waiting while anyone
    // — this warm or another process's — is still indexing, until the index is
    // complete (a "no matches" is then trustworthy), a wait deadline passes, or —
    // interactively — Ctrl-C. A human sees a progress line once the pause is
    // noticeable.
    crate::trace!(
        "setup (open + repo detect + warm decision): {} ms",
        t_setup.elapsed().as_millis()
    );
    let poll_start = std::time::Instant::now();
    // Deadline: an interactive block waits unbounded (Ctrl-C escapes) unless
    // `--wait` names a bound; a programmatic block, or one following another
    // checkout's indexer, waits out the wait budget; a non-block (complete
    // repo) keeps the original fast answer budget.
    let deadline = if progress_ui && wait.is_none() {
        None
    } else if block || follow_others {
        Some(poll_start + wait_budget)
    } else {
        Some(poll_start + answer_warm_budget())
    };
    drop(warm_span);
    let polling = block || follow_others;
    // Everything before the first search: resolving the repo root, checking
    // coverage, deciding whether to warm. It runs on every query, so it counts
    // toward the first-answer budget even though no searching happened yet.
    drop(setup_span);
    let mut query_span = crate::profile::span("query");
    let label = repo_label(root);
    // What a retry would read that this search hasn't, which is what a
    // partial index's top match must not be beatable by to answer (D52).
    let unread = |store: &Store, name_read: bool| Unread {
        here: here
            .clone()
            .filter(|r| want_warm && !is_complete_key(store, r)),
        name_read,
        elsewhere: all_repos
            .then(|| others_open(store, here.as_deref()))
            .flatten(),
    };
    let mut drew_progress = false;
    let mut last_draw = poll_start;
    // when anyone last committed to the index, so a search stops following
    // an indexer that writes nothing (stopped, or a crashed pass's mark)
    let mut last_version = store.data_version().ok();
    let mut last_write = poll_start;
    // Rank one deeper than asked: confidence is a comparison against the
    // runner-up, so normalizing over the returned window made `-l 1` read 1.0
    // every time — and that reading is what gates `--show`.
    let rank_limit = limit.max(2);
    let mut total;
    let mut hits = loop {
        // the warm registers a repo it's indexing for the first time
        if current.is_none() {
            current = checkout_here(store);
        }
        let searched_at = std::time::Instant::now();
        match scope(current).search(store, query, current, &ctx, rank_limit) {
            Ok(m) => {
                total = m.total;
                let h = m.hits;
                if !polling {
                    break h;
                }
                let unread = unread(store, demanded.load(std::sync::atomic::Ordering::Acquire));
                let answered = shown(args, cwd.as_deref(), root, &h)
                    .first()
                    .is_some_and(|top| held_back_by(top, here.as_deref(), &unread).is_none());
                let version = store.data_version().ok();
                if version != last_version {
                    (last_version, last_write) = (version, std::time::Instant::now());
                }
                let own =
                    indexer.is_some() && !warm_done.load(std::sync::atomic::Ordering::Relaxed);
                let stalled = !own && last_write.elapsed() >= INDEXER_STALL;
                // Our own warm ending says nothing about the index while
                // another process is still filling it.
                let indexing = own
                    || !stalled
                        && (root.is_some_and(|r| others_indexing(store, r))
                            || unread.elsewhere.is_some());
                let complete =
                    root.is_some_and(|r| is_complete(store, r)) && unread.elsewhere.is_none();
                let stopped = INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed);
                // A poll costs what the last one did (about a second while a
                // cold pass holds the name index), so stop rather than start
                // one that would end past the deadline.
                let timed_out = deadline.is_some_and(|d| {
                    !poll_fits(std::time::Instant::now(), d, searched_at.elapsed())
                });
                if answered || complete || !indexing || stopped || timed_out {
                    break h;
                }
                if progress_ui
                    && poll_start.elapsed() >= HEADS_UP_DELAY
                    && last_draw.elapsed() >= PROGRESS_REDRAW
                {
                    // name the checkout being waited on: ours, or another's
                    match unread.elsewhere.as_deref().filter(|_| !own) {
                        Some(r) => {
                            let r = std::path::Path::new(r);
                            draw_progress(store, checkout_at(store, r), &repo_label(Some(r)));
                        }
                        None => draw_progress(store, current, &label),
                    }
                    drew_progress = true;
                    last_draw = std::time::Instant::now();
                }
            }
            Err(e) => {
                if let Some(h) = indexer {
                    let _ = h.join();
                }
                return fail(out, Failure::Database, format_args!("rq: {e}"));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    query_span.note(|| {
        if polling {
            "polled a warming index".to_string()
        } else {
            String::new()
        }
    });
    drop(query_span);
    if drew_progress {
        clear_progress();
    }
    // Captured before we self-cancel below, so it reflects only a *user's* Ctrl-C.
    let interrupted = INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed);

    // Staleness: revalidate the files behind the top hits; re-rank once if changed.
    let mut gone = Vec::new();
    if !hits.is_empty()
        && revalidate_top(store, &hits, &mut gone)
        && let Ok(m) = scope(current).search(store, query, current, &ctx, rank_limit)
    {
        total = m.total;
        hits = m.hits;
    }

    // A file gone from disk since its index (a branch switch deleted it) is
    // no answer. Only dropped from this one: revalidation never forgets on a
    // failed read, and the warm the switch sets off reconciles the index.
    let before = hits.len();
    hits.retain(|h| {
        !gone
            .iter()
            .any(|(root, file)| h.root.as_deref() == Some(root) && h.file == *file)
    });
    total = total.saturating_sub(before - hits.len());

    // Untracked non-git dir — nothing persisted, no warmer running — so scan it
    // live in-memory (substring, then fuzzy) and blend with whatever the index
    // gave. The only non-persisting scan left.
    let mut live_scan = None;
    if !hits.iter().any(strong)
        && indexer.is_none()
        && coverage.is_none()
        && let (Some(root), Some(identity)) = (root, &identity)
    {
        let tree = crate::index::LiveTree::new(root, identity.clone());
        let (tail, cost) = live_fallback(&tree, query, rank_limit, &ctx);
        hits = crate::search::merge(hits, tail, rank_limit);
        total = total.max(hits.len());
        live_scan = Some(cost);
    }

    apply_gates(query, &mut hits);
    apply_post_filters(args, cwd.as_deref(), root, &mut hits);
    // A filtered search reports what survived the filter — that's the set the
    // caller asked about — counted before any cut. The runner-up stays for
    // confidence; `--limit` applies once that's assigned.
    if !args.paths.is_empty() || !args.kinds.is_empty() || !args.langs.is_empty() {
        total = hits.len();
        hits.truncate(want.max(2));
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
        // symbol is absent when the index simply hasn't reached it. `--no-wait`
        // returns without blocking, so its miss is judged the same way — an
        // incomplete index yields `warming` (exit 2, "retry"), not a false absence.
        // Where nothing warms (a dir rq doesn't track) the live scan was whole.
        let mut incomplete = (block || no_wait)
            && warming_ok
            && root.and_then(|r| store.coverage_status(&root_key(r)).ok().flatten())
                != Some(Coverage::Complete);
        // a "not yet" miss leaves work behind — reindex an edited worktree and
        // let a detached child keep warming, so the retry lands on a better
        // index. This is the path a just-added symbol takes, so it has to do
        // the same settling the render path does.
        // A worktree that has moved since we indexed it makes this miss
        // provisional, not definitive: the symbol may be in an edit the
        // detached warm hasn't caught up with. Say "warming" (exit 2, retry)
        // rather than "no match" (exit 1, absent) — a just-added symbol is
        // exactly this case, and a confident no is the wrong answer to it.
        incomplete |= settle_warm(
            store,
            staleness,
            moved,
            false,
            was_warming,
            warming_ok,
            root,
        );
        // A named scope that matched nothing is a different miss from a name
        // that doesn't exist: re-run on the bare leaf to tell them apart, and
        // say where the name actually lives. Only on the miss path, so a normal
        // search never pays for it.
        let here_id = current.map(|c| c.id);
        let elsewhere = match scope(current) {
            Scope::Nothing => None,
            Scope::All => crate::search::scope_miss_owner(store, query, here_id, None, &ctx),
            Scope::Checkout(c) => {
                crate::search::scope_miss_owner(store, query, here_id, Some(c), &ctx)
            }
        };
        if let (Some(cost), Some(root)) = (&live_scan, root) {
            note_live_scan(root, cost, false);
        }
        // `-a` can't vouch for a checkout nothing has indexed, and warms only
        // the one it runs in: name the others rather than calling it `warming`
        let incomplete_roots = match scope(current) {
            Scope::All if !incomplete => store.incomplete_roots().unwrap_or_default(),
            _ => Vec::new(),
        };
        // how far this checkout's index got, as a provisional answer says
        let warming = incomplete
            .then_some(here.as_deref())
            .flatten()
            .and_then(|r| {
                let continuing = warm_detach_enabled() && warming_ok;
                warming_state(store, r, true, continuing)
            });
        let verdict = if incomplete {
            Verdict::Warming
        } else {
            Verdict::Miss
        };
        let code = no_match_code(
            out,
            query,
            interrupted,
            verdict,
            warming.as_ref(),
            elsewhere.as_deref(),
            &incomplete_roots,
        );
        // Counted after the answer, and only here: whether this was a
        // definitive miss or a not-ready one is only known on this path, and
        // counting them as one number overstates how often rq truly finds nothing.
        let _span = crate::profile::span("after: record usage");
        record_usage(store, args, verdict, coverage, false);
        return code;
    }

    // Live if any result the caller sees came from the scan; `hits` is
    // already in its final order, so the first `want` are the ones shown.
    let live = hits
        .iter()
        .take(want)
        .any(|h| h.source == crate::search::Source::Live);
    if let (Some(cost), Some(root)) = (&live_scan, root) {
        note_live_scan(root, cost, live);
    }

    // A hit from a checkout still being indexed says how far the index got,
    // and a top match that isn't `settled` is no answer yet: it goes out as
    // `warming` (exit 2), its hits provisional (D52).
    let continuing = (warm_detach_enabled() && warming_ok)
        .then_some(here.as_deref())
        .flatten();
    // a live scan stands in for the index of a checkout rq is building
    let scanned = here.as_deref().filter(|_| warming_ok);
    disclose_warming(store, &mut hits, here.as_deref(), scanned, continuing);
    let unread = unread(store, demanded.load(std::sync::atomic::Ordering::Acquire));
    // the checkout whose unread files could still beat the top match
    let held_back = hits
        .first()
        .and_then(|top| held_back_by(top, here.as_deref(), &unread))
        .map(str::to_string);
    let provisional = held_back.is_some();
    let verdict = if provisional {
        Verdict::Warming
    } else {
        Verdict::Hit
    };

    // A process's first write can stall for milliseconds on a busy machine
    // (DECISIONS D13), so the ranked list counts itself once it has printed.
    // --show/--open/--web leave by their own exits (--open `exec`s), so they
    // count here; a --show that falls through to the list was already counted.
    // None of them acts on a provisional match.
    let counted_early = !provisional && (show || open || web);
    if counted_early {
        let _span = crate::profile::span("record usage");
        record_usage(store, args, verdict, coverage, live);
    }

    // Confidence first, while the runner-up is still in hand, then cut to the
    // window the caller asked for — `--show`'s gate reads this, so measuring it
    // over an already-truncated list made `-l 1` unconditionally confident.
    attach_confidence(&mut hits);
    // `--show` asks which definition was meant, a question of ranking, as
    // `--open` does; how much of the tree is read is disclosed, not gated on
    let ranked = hits.first().map_or(0.0, |h| h.confidence);
    for hit in &mut hits {
        if let Some(w) = &hit.warming {
            hit.confidence = read_share_confidence(hit.confidence, w);
        }
    }
    hits.truncate(want);
    let total = total.max(hits.len());
    for hit in &mut hits {
        hit.total = total;
        if args.explain {
            hit.explain = Some(
                hit.features
                    .iter()
                    .map(|f| (f.name.to_string(), f.reported()))
                    .collect(),
            );
        }
    }

    // Attach each result's definition line (e.g. `def perform(refund)`) — shown
    // in text output and carried in JSON. Cheap: only the displayed results.
    let _signatures_span = crate::profile::span("signatures");
    for hit in &mut hits {
        hit.signature = hit
            .root
            .as_deref()
            .and_then(|r| read_signature(&std::path::Path::new(r).join(&hit.file), hit.line));
    }
    drop(_signatures_span);

    // --show: print the top hit's full source when confident; otherwise fall
    // through to the normal ranked list (rq won't dump a body it isn't sure of).
    if counted_early
        && show
        && let Some(code) = show_top_definition(&mut hits, query, out, ranked)
    {
        if out == Output::Text
            && let Some(note) = hits.first().and_then(|h| warming_note(h, here.as_deref()))
        {
            eprintln!("{note}");
        }
        return code;
    }

    // --open/--web: pick the best match (prompting on a TTY with several) and
    // hand off to the editor or browser.
    // Returns before the normal print / warm-join — opening should be snappy,
    // and a launcher `exec`s.
    if counted_early && (open || web) {
        if out == Output::Text
            && let Some(note) = hits.first().and_then(|h| warming_note(h, here.as_deref()))
        {
            eprintln!("{note}");
        }
        return finish_open(&hits, root, web);
    }

    if let Some(by) = held_back.as_deref() {
        let warming = warming_state(
            store,
            by,
            Some(by) == here.as_deref(),
            Some(by) == continuing,
        );
        if let Some(code) = emit_provisional(args, &hits, warming.as_ref(), by, here.as_deref()) {
            return code;
        }
    } else if let Some(code) = render_hits(args, &hits, show) {
        return code;
    } else if out == Output::Text
        && let Some(note) = hits.first().and_then(|h| warming_note(h, here.as_deref()))
    {
        eprintln!("{note}");
    }

    // The budget's number. `total` adds the bookkeeping below, which runs
    // after results are out but still before the process exits.
    crate::profile::mark("first answer");

    // Before the warm child is spawned below, whose own writes it would
    // otherwise queue behind.
    if !counted_early {
        let _span = crate::profile::span("after: record usage");
        record_usage(store, args, verdict, coverage, live);
    }

    // Collect the refresh started back at setup. It ran alongside the search
    // rather than after it, so by now it has usually finished — and it only
    // ever feeds the *next* query, never this one's ranking, so waiting on it
    // can't reorder what was just printed.
    // Taken, not borrowed: the refresh is one-shot, and a session answering
    // several queries must not re-store a result it already consumed.
    if let Some(refresh) = branch_refresh.take() {
        let _span = crate::profile::span("after: branch refresh");
        refresh.store(store);
    }

    // Results are out; stop the in-process warm (it persists as it goes, so a
    // cut pass keeps everything parsed) and join it — then hand whatever's left
    // to a detached child, which finishes coverage with a budget no foreground
    // query could afford. The shell only ever waits on the answer.
    if block {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(h) = indexer {
        let _ = h.join();
    }
    let _ = settle_warm(store, staleness, moved, true, was_warming, warming_ok, root);

    // "no answer yet" (2), whether nothing matched or nothing settled
    verdict.exit_code()
}

/// The hits as the caller would see them: gated and filtered, uncut. What the
/// poll judges a warming answer by, since a filter can drop the top match.
fn shown(
    args: &SearchArgs,
    cwd: Option<&std::path::Path>,
    root: Option<&std::path::Path>,
    hits: &[crate::search::Hit],
) -> Vec<crate::search::Hit> {
    let mut hits = hits.to_vec();
    apply_gates(args.query, &mut hits);
    apply_post_filters(args, cwd, root, &mut hits);
    hits
}

/// What a retry of this search would read that this one hasn't.
struct Unread {
    /// More of this checkout, at this root: it isn't complete, and a search
    /// here warms it.
    here: Option<String>,
    /// ...though not a file of it containing the name: this search's warm
    /// read them all.
    name_read: bool,
    /// More of another checkout `-a` reads, at this root, which another
    /// process is indexing.
    elsewhere: Option<String>,
}

/// Which checkout keeps the top match on an index still being filled from
/// answering the query, if any: it answers once nothing a retry would read
/// can beat it. An exact match in the capitals
/// typed answers regardless, though another definition of the name may be
/// unread (the hit says so); a literal match once no file a retry would read
/// can hold the name; a fuzzy or path match once a retry would read nothing.
/// A checkout nothing is indexing, other than this one, is as read as it will
/// get, so its hit answers rather than asking for a retry that can't help (D52).
fn held_back_by<'a>(
    top: &crate::search::Hit,
    here: Option<&str>,
    unread: &'a Unread,
) -> Option<&'a str> {
    let has = |name| top.features.iter().any(|f| f.name == name);
    if has("exact") && has("case") {
        return None;
    }
    let literal = crate::search::is_literal(&top.features);
    // the demand walk read this checkout's files, not another's
    let from_here = top.root.as_deref().is_none_or(|r| Some(r) == here);
    let here_read = unread.name_read && literal && from_here;
    unread
        .here
        .as_deref()
        .filter(|_| !here_read)
        .or(unread.elsewhere.as_deref())
}

/// Whether another poll, after the sleep, costing what the last one did,
/// ends by `deadline`.
fn poll_fits(now: std::time::Instant, deadline: std::time::Instant, cost: Duration) -> bool {
    now + POLL_INTERVAL + cost <= deadline
}

fn is_complete(store: &Store, root: &std::path::Path) -> bool {
    is_complete_key(store, &root_key(root))
}

fn is_complete_key(store: &Store, key: &str) -> bool {
    store.coverage_status(key).ok().flatten() == Some(Coverage::Complete)
}

/// A checkout other than `here`, not complete yet, that another process is
/// indexing.
fn others_open(store: &Store, here: Option<&str>) -> Option<String> {
    store
        .incomplete_roots()
        .unwrap_or_default()
        .into_iter()
        .find(|r| Some(r.as_str()) != here && store.indexed_by_others(r))
}

/// Whether a process other than this one is indexing the checkout at `root`.
fn others_indexing(store: &Store, root: &std::path::Path) -> bool {
    store.indexed_by_others(&root_key(root))
}

/// Mark each hit from a checkout that isn't fully indexed with how far its
/// index got, which scales every candidate from it alike: a live hit too, from
/// `scanned`, the checkout a live scan stood in for (D52). `continuing`: the
/// checkout this rq leaves a warm behind for.
fn disclose_warming(
    store: &Store,
    hits: &mut [crate::search::Hit],
    here: Option<&str>,
    scanned: Option<&str>,
    continuing: Option<&str>,
) {
    let mut seen: HashMap<String, Option<crate::search::Warming>> = HashMap::new();
    for hit in hits.iter_mut() {
        let root = match hit.source {
            crate::search::Source::Index => hit.root.as_deref(),
            crate::search::Source::Live => scanned,
        };
        let Some(root) = root else {
            continue;
        };
        hit.warming = seen
            .entry(root.to_string())
            .or_insert_with(|| {
                warming_state(store, root, Some(root) == here, Some(root) == continuing)
            })
            .clone();
    }
}

/// How far the index of the checkout at `root` has got, or `None` once it is
/// complete. `here`: the checkout this search runs in, which `rq --index`
/// alone names.
fn warming_state(
    store: &Store,
    root: &str,
    here: bool,
    continuing: bool,
) -> Option<crate::search::Warming> {
    if store.coverage_status(root).ok().flatten() == Some(Coverage::Complete) {
        return None;
    }
    // none yet: dropped, or never read, with no pass registered to fill it
    let checkout = store.checkout(root).ok().flatten();
    let read = checkout.map_or(0, |c| store.checkout_file_count(c.id).unwrap_or(0));
    let span = match checkout {
        Some(c) => tree_span(store, root, c.id),
        None => crate::index::count_span_unheld(std::path::Path::new(root)).map(|s| s as i64),
    };
    let interrupted = !continuing && !store.indexed_by_others(root);
    let index = index_command(root, here);
    let hint = if interrupted {
        format!("indexing stopped part-way: `{index}` finishes it")
    } else {
        format!(
            "rq is still indexing {}: ask again for a fuller answer, or run `{index}` to finish it now",
            which_checkout(root, here)
        )
    };
    let phase = store.pass_phase(root);
    Some(crate::search::Warming {
        read,
        of: span.map(|s| s.max(read)),
        interrupted,
        phase: phase.map(|(p, _)| p),
        phase_secs: phase.map(|(_, since)| crate::store::phase_secs(since)),
        hint,
    })
}

/// Files the tree at `root` spans: as the last pass recorded it, or counted
/// now — and kept — for a partial index an older rq left without one.
fn tree_span(store: &Store, root: &str, checkout: i64) -> Option<i64> {
    if let Some(span) = store.passes(root).ok().and_then(|(_, span)| span) {
        return Some(span);
    }
    let _span = crate::profile::span("warming: count the tree");
    let span = crate::index::count_span(store, std::path::Path::new(root), checkout)?;
    store.keep_span(root, span);
    Some(span as i64)
}

/// Confidence on a partial index, scaled by the share of the tree read — the
/// files that could hold a better match. Floored in whole hundredths, so it
/// never rounds up to a whole the index can't back. A tree nothing could count
/// backs none of it.
fn read_share_confidence(confidence: f64, w: &crate::search::Warming) -> f64 {
    match w.of {
        Some(of) if of > 0 => {
            let hundredths = (confidence * 100.0).round() as i64;
            (hundredths * w.read.min(of) / of) as f64 / 100.0
        }
        _ => 0.0,
    }
}

/// What finishes indexing the checkout at `root`: `rq --index` in the one
/// this search runs in, naming the root anywhere else.
fn index_command(root: &str, here: bool) -> String {
    if here {
        "rq --index".to_string()
    } else {
        format!("rq --index {root}")
    }
}

/// The stderr line under a hit from a partial index: how far it got, and
/// whether anything is still filling it.
fn warming_note(hit: &crate::search::Hit, here: Option<&str>) -> Option<String> {
    let w = hit.warming.as_ref()?;
    let read = read_so_far(w);
    Some(if w.interrupted {
        let root = hit.root.as_deref().unwrap_or_default();
        format!(
            "rq: indexing stopped part-way ({read}) — another definition may not be indexed yet; `{}` finishes it",
            index_command(root, Some(root) == here)
        )
    } else {
        let root = hit.root.as_deref().unwrap_or_default();
        format!(
            "rq: still indexing {} ({read}) — another definition may not be indexed yet",
            which_checkout(root, Some(root) == here)
        )
    })
}

/// "this checkout", or the one at `root` when the search runs elsewhere.
fn which_checkout(root: &str, here: bool) -> String {
    if here {
        "this checkout".to_string()
    } else {
        format!("the checkout at {root}")
    }
}

/// "N of M files read", or "N files read" when nothing counted the tree,
/// and why `read` stands still when a pass is finishing.
fn read_so_far(w: &crate::search::Warming) -> String {
    let files = |n| if n == 1 { "file" } else { "files" };
    let read = match w.of {
        Some(of) => format!("{} of {of} {} read", w.read, files(of)),
        None => format!("{} {} read", w.read, files(w.read)),
    };
    format!("{read}{}", finishing_note(w.phase, w.phase_secs))
}

/// ", finishing a pass (N s)" while a pass is past its reads.
fn finishing_note(phase: Option<&str>, secs: Option<i64>) -> String {
    match (phase, secs) {
        (Some(crate::store::FINISHING), Some(secs)) => format!(", finishing a pass ({secs} s)"),
        _ => String::new(),
    }
}

/// Report a top match that isn't settled on an index still being filled:
/// `warming`, as a miss would be, with what was found so far as `provisional`.
/// Text lists them as a hit would, under a note saying they may change.
/// `warming` is how far `by`, the checkout holding the answer back, has got.
fn emit_provisional(
    args: &SearchArgs,
    hits: &[crate::search::Hit],
    warming: Option<&crate::search::Warming>,
    by: &str,
    here: Option<&str>,
) -> Option<ExitCode> {
    // A miss's status object, keys in its order, around hits in a result's.
    #[derive(serde::Serialize)]
    struct Provisional<'a> {
        provisional: &'a [crate::search::Hit],
        query: &'a str,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        warming: Option<&'a crate::search::Warming>,
    }
    match args.out {
        Output::Json | Output::Ndjson => {
            let obj = Provisional {
                provisional: hits,
                query: args.query,
                status: "warming",
                warming,
            };
            let code = emit_json(args.out, &obj);
            (code != ExitCode::SUCCESS).then_some(code)
        }
        Output::Text => {
            if let Some(code) = render_hits(args, hits, false) {
                return Some(code);
            }
            let state = if warming.is_some_and(|w| w.interrupted) {
                "indexing stopped part-way"
            } else {
                "still indexing"
            };
            eprintln!(
                "rq: {state} ({}) — no settled match for {:?} yet, so these may change (run again, or `{}` to finish)",
                warming.map_or_else(String::new, read_so_far),
                args.query,
                index_command(by, Some(by) == here)
            );
            None
        }
    }
}

/// Re-exec a detached warm child when this query's warming didn't finish the
/// job. No-op when detach is off, nothing was warming, or coverage completed.
fn maybe_detach_warm(
    store: &Store,
    want_warm: bool,
    changed: bool,
    root: Option<&std::path::Path>,
) {
    if !want_warm {
        return;
    }
    let Some(root) = root else {
        return;
    };
    // Coverage measures breadth, not freshness — an edit never demotes it. So
    // "complete" alone isn't done; it's done only if the worktree also hasn't
    // moved since we indexed it.
    let status = store.coverage_status(&root_key(root)).ok().flatten();
    if !changed && status == Some(Coverage::Complete) {
        return; // the in-process pass finished the job
    }
    spawn_detached_warm(root);
}

/// Spawn `rq --warm <root>` fully detached: null stdio and its own process
/// group, so it survives this process and a later Ctrl-C in the terminal
/// can't reach it. The child nices itself and is single-flighted per checkout.
fn spawn_detached_warm(root: &std::path::Path) {
    use std::os::unix::process::CommandExt;
    if !warm_detach_enabled() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--warm")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    match cmd.spawn() {
        Ok(mut child) => {
            crate::trace!(
                "background warm (detached): pid {} for {}",
                child.id(),
                crate::trace::abbrev(root)
            );
            if warm_detach_waits() {
                let _ = child.wait();
            }
        }
        Err(e) => crate::trace!("detached warm failed to spawn: {e}"),
    }
}

/// How long a warm child's "nothing moved" verdict spares later hits the spawn,
/// while git's own state still matches it. Only an unstaged edit to a tracked
/// file can hide inside this window (it touches nothing in `.git`); a miss still
/// checks inline and a top hit's file is revalidated on read, so what's left is
/// a changed file that isn't among the hits, picked up by the first hit after.
fn warm_recheck_window() -> Duration {
    env_budget("RQ_WARM_RECHECK_MS", 10_000)
}

/// Whether a warm child found this worktree unchanged recently enough, with
/// git's state untouched since, that spawning another would find nothing.
fn recently_verified(store: &Store, root: &std::path::Path, indexed_head: Option<&str>) -> bool {
    let _span = crate::profile::span("after: warm recently verified?");
    let Some(stamp) = verified_stamp(root, indexed_head) else {
        return false;
    };
    let Ok(Some((seen, at))) = store.warm_verified(&root_key(root)) else {
        return false;
    };
    let age = now_unix().saturating_sub(at);
    seen == stamp && (0..warm_recheck_window().as_secs() as i64).contains(&age)
}

/// What a "nothing moved" verdict is recorded against: the git state, or
/// outside git (where only the files could say) the root alone, so any change
/// there waits out the window.
fn verified_stamp(root: &std::path::Path, indexed_head: Option<&str>) -> Option<String> {
    match indexed_head {
        Some(head) => crate::index::git_state_stamp(root, head),
        None => (!crate::index::is_git_repo(root)).then(|| root.display().to_string()),
    }
}

/// Record that a check begun at `checked_at` found the worktree unchanged.
fn record_verified(store: &Store, root: &std::path::Path, head: Option<&str>, checked_at: i64) {
    if let Some(stamp) = verified_stamp(root, head) {
        let _ = store.set_warm_verified(&root_key(root), &stamp, checked_at);
    }
}

/// `rq --warm [PATH]`: the detached child a search re-execs after printing —
/// finishes warming the repo's index in the background. Niced so it stays out
/// of the foreground's way; single-flighted per checkout so a burst of queries
/// runs at most one warmer. Safe (and boring) to run by hand.
fn cmd_warm(path: Option<&str>) -> ExitCode {
    // Stay out of the way: drop scheduling priority, and throttle disk I/O on
    // macOS. Best-effort — a failure just means a less-polite warm.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        // <sys/resource.h>; not in the libc crate. Args below:
        // IOPOL_TYPE_DISK=0, IOPOL_SCOPE_PROCESS=0, IOPOL_THROTTLE=3.
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }
    // SAFETY: plain syscalls on this process with constant arguments; neither
    // touches memory we own.
    unsafe {
        libc::nice(10);
        #[cfg(target_os = "macos")]
        setiopolicy_np(0, 0, 3);
    }
    let mut store = match open_store() {
        Ok(s) => s,
        Err(_) => return ExitCode::from(Failure::Database.exit_code()),
    };
    let _ = store.wait_out_writers(writer_wait(), None);
    let start = path
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let here = Here::at(&store, &start);
    let root = here.root;
    let key = root_key(&root);

    // Single-flight: if another live rq is already warming this checkout, bow out.
    // A dead pid or a stale stamp is a crashed warmer — take over. A claim that
    // can't be made at all means another writer is busy; the next search retries.
    match store.claim_warm_lock(&key, std::process::id(), crate::store::warm_lock_held) {
        Ok(true) => {}
        Ok(false) => {
            crate::trace!(
                "warm {}: another warm holds it",
                crate::trace::abbrev(&root)
            );
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            crate::trace!("warm {}: can't claim it: {e}", crate::trace::abbrev(&root));
            return ExitCode::SUCCESS;
        }
    }

    // A search on a complete repo hands us the staleness check rather than
    // wait on `git status` itself, so most runs end here: nothing moved.
    if here.coverage == Some(Coverage::Complete) {
        let checkout = here.checkout;
        let head = checkout.and_then(|c| store.indexed_head(c.id).ok().flatten());
        if !worktree_moved(&store, &root, head.as_deref()) {
            crate::trace!("warm: unchanged since indexed, nothing to do");
            // but a database from before the name index has none yet
            if let Some(c) = checkout {
                let _ = store.maintain_name_index(c.repo);
            }
            let _ = store.clear_warm_lock(&key);
            return ExitCode::SUCCESS;
        }
    }

    // Sweep until coverage completes or a pass stops making progress (each
    // pass converges — mtime-skips what's done). A search that found this
    // child holding the lock bowed out and told its caller rq is still
    // indexing, so the budget bounds a pass, not the sweep: stopping on it
    // would leave the checkout part-read with nobody indexing it (D52).
    let deadline = std::time::Instant::now() + warm_sweep_cap();
    let active = crate::index::branch_changed_files(&root);
    loop {
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .min(warm_bg_budget());
        if remaining.is_zero() {
            break;
        }
        let stats = match crate::index::index_budgeted(&mut store, &root, &active, remaining, None)
        {
            Ok(s) => s,
            Err(e) => {
                crate::trace!(
                    "warm {}: pass failed, stopping: {e}",
                    crate::trace::abbrev(&root)
                );
                break;
            }
        };
        if store.coverage_status(&key).ok().flatten() == Some(Coverage::Complete)
            || stats.files_indexed == 0
        {
            break;
        }
    }
    let _ = store.clear_warm_lock(&key);
    ExitCode::SUCCESS
}

/// Live in-memory scan of an untracked (non-git, never-indexed) dir: substring
/// pre-filtered first, then the unfiltered fuzzy retry. Persists nothing.
fn live_fallback(
    tree: &crate::index::LiveTree,
    query: &str,
    limit: usize,
    ctx: &crate::search::Context,
) -> (Vec<crate::search::Hit>, LiveCost) {
    let start = std::time::Instant::now();
    let deadline = start + live_fallback_budget();
    let mut files = 0;
    let mut scan = |prefilter| {
        let mut span = crate::profile::span(if prefilter {
            "live scan: prefiltered"
        } else {
            "live scan: unfiltered"
        });
        let found = crate::search::live_search(
            tree,
            query,
            limit,
            &HashSet::new(),
            Some(deadline),
            prefilter,
            ctx,
        );
        span.note(|| format!("{} files", found.files));
        files += found.files;
        found.hits
    };
    let mut hits = scan(true);
    if hits.is_empty() {
        hits = scan(false);
    }
    let cost = LiveCost {
        files,
        elapsed: start.elapsed(),
    };
    (hits, cost)
}

/// What a live scan cost, for its `-v` note.
struct LiveCost {
    files: usize,
    elapsed: Duration,
}

/// Under `-v`, say that a live scan ran and whether the answer came from it —
/// otherwise invisible, since a live result prints like an indexed one.
fn note_live_scan(root: &std::path::Path, cost: &LiveCost, answered: bool) {
    if !crate::trace::enabled() {
        return;
    }
    let budget = live_fallback_budget();
    let files = match cost.files {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    let what = if answered {
        "answered from a live scan of"
    } else {
        "no answer from a live scan of"
    };
    let cut = if cost.elapsed >= budget {
        " (stopped at the budget)"
    } else {
        ""
    };
    crate::trace!(
        "{what} {}: {files} in {} ms, budget {} ms{cut}",
        crate::trace::abbrev(root),
        cost.elapsed.as_millis(),
        budget.as_millis(),
    );
}

/// A high-confidence name match: exact or prefix (not fuzzy/path-only).
fn strong(h: &crate::search::Hit) -> bool {
    h.features
        .iter()
        .any(|f| matches!(f.name, "exact" | "prefix"))
}

/// The result-quality gates, in order:
/// - relevance: when the query lands a real name match (exact or prefix), drop
///   the scattered fuzzy / path-only near-matches — they're noise next to a
///   solid hit, and rq favors fewer, better results. A purely-fuzzy query (no
///   exact/prefix anywhere) keeps its matches.
/// - scope: a qualified query (`Foo::Bar#baz`) that lands inside the named
///   scope keeps only the in-scope results; if none match, the others stay
///   (the definition may live elsewhere).
fn apply_gates(query: &str, hits: &mut Vec<crate::search::Hit>) {
    if hits.iter().any(strong) {
        hits.retain(strong);
    }
    crate::search::apply_scope_gate(query, hits);
}

/// Post-filters: keep only results under a `--path` dir, of a `--kind`, and/or
/// in a `--lang`.
fn apply_post_filters(
    args: &SearchArgs,
    cwd: Option<&std::path::Path>,
    root: Option<&std::path::Path>,
    hits: &mut Vec<crate::search::Hit>,
) {
    if !args.paths.is_empty() {
        // --path values may be absolute or cwd-relative; stored files are
        // repo-root-relative, so normalize before prefix-matching or an
        // absolute path would silently filter everything out.
        let here = cwd.map_or_else(|| PathBuf::from("."), PathBuf::from);
        let base = root.map_or_else(|| here.clone(), PathBuf::from);
        let norm: Vec<String> = args
            .paths
            .iter()
            .map(|p| repo_relative(&base, &here, p))
            .collect();
        hits.retain(|h| under_any(&h.file, &norm));
    }
    if !args.kinds.is_empty() {
        hits.retain(|h| args.kinds.iter().any(|k| k == &h.kind));
    }
    if !args.langs.is_empty() {
        hits.retain(|h| args.langs.iter().any(|l| l == &h.language));
    }
}

/// Report a miss and pick its exit code. Structured callers get a reason, not
/// a bare `[]`/empty: `warming` (retry — index incomplete), `interrupted` (a
/// stopped block), or `no_match` (definitive). Text keeps its human message.
/// Exit 2 = indeterminate (index incomplete), 1 = a definitive miss — both
/// non-zero, so `rq … && …` still reads as "found something".
fn no_match_code(
    out: Output,
    query: &str,
    interrupted: bool,
    // `Warming` when the index couldn't vouch for the miss
    verdict: Verdict,
    warming: Option<&crate::search::Warming>,
    // Where the unqualified name *does* live, when a scope was named and
    // nothing in it matched. "Not in that scope" and "no such name" are
    // different answers and the second is the less useful one.
    elsewhere: Option<&str>,
    // checkouts `-a` read that aren't fully indexed, so the miss isn't theirs
    incomplete_roots: &[String],
) -> ExitCode {
    let incomplete = verdict == Verdict::Warming;
    let status = if interrupted {
        "interrupted"
    } else if incomplete {
        "warming"
    } else if elsewhere.is_some() {
        "scope_not_found"
    } else {
        "no_match"
    };
    match out {
        Output::Json | Output::Ndjson => {
            // keys sorted, as a status object's are; `warming` in a result's order
            #[derive(serde::Serialize)]
            struct Miss<'a> {
                #[serde(skip_serializing_if = "Option::is_none")]
                found_in: Option<&'a str>,
                #[serde(skip_serializing_if = "<[String]>::is_empty")]
                incomplete: &'a [String],
                query: &'a str,
                status: &'a str,
                #[serde(skip_serializing_if = "Option::is_none")]
                warming: Option<&'a crate::search::Warming>,
            }
            let obj = Miss {
                found_in: elsewhere,
                incomplete: incomplete_roots,
                query,
                status,
                warming,
            };
            let _ = emit_json(out, &obj); // the exit code below carries the miss
        }
        Output::Text if interrupted => {
            eprintln!("rq: indexing interrupted — run again to finish")
        }
        Output::Text if incomplete => {
            let (state, read) = match warming {
                Some(w) if w.interrupted => ("indexing stopped part-way", read_so_far(w)),
                Some(w) => ("still indexing", read_so_far(w)),
                None => ("still indexing", String::new()),
            };
            let read = if read.is_empty() {
                read
            } else {
                format!(" ({read})")
            };
            eprintln!(
                "rq: {state}{read} — no match for {query:?} yet (run again, or `rq --index` to finish)"
            )
        }
        Output::Text if elsewhere.is_some() => eprintln!(
            "rq: nothing matching {query:?} in that scope — the name is defined elsewhere: {}",
            elsewhere.unwrap_or_default()
        ),
        Output::Text if !incomplete_roots.is_empty() => eprintln!(
            "no matches for {query:?} in what's indexed; these checkouts aren't fully indexed (a search in one indexes it):\n  {}",
            incomplete_roots.join("\n  ")
        ),
        Output::Text => eprintln!("no matches for {query:?}"),
    }
    verdict.exit_code()
}

/// Normalized confidence per hit: match quality scaled by dominance over the
/// other results (needs the whole ranked set). "Best other" is the top score —
/// or the runner-up, for the top hit itself.
fn attach_confidence(hits: &mut [crate::search::Hit]) {
    let (top, second) = hits.iter().fold((None::<f64>, None::<f64>), |(t, s), h| {
        if t.is_none_or(|t| h.score > t) {
            (Some(h.score), t)
        } else if s.is_none_or(|s| h.score > s) {
            (t, Some(h.score))
        } else {
            (t, s)
        }
    });
    for hit in hits.iter_mut() {
        let best_other = if Some(hit.score) == top { second } else { top };
        hit.confidence = crate::search::confidence(
            hit.score,
            crate::search::match_quality(&hit.features),
            best_other,
        );
    }
}

/// Print the ranked results (JSON array, NDJSON lines, or highlighted text).
/// `Some(exit)` on a serialization failure, `None` on success. `unshown`: a
/// `--show` found no single confident match to print instead.
fn render_hits(args: &SearchArgs, hits: &[crate::search::Hit], unshown: bool) -> Option<ExitCode> {
    // Time to the first printed result, not to the last: rq streams, and the
    // sub-50 ms budget is about the first answer. A change that speeds the
    // total while delaying this one is a regression.
    let render_span = crate::profile::span("render");
    if args.batch {
        // One stream, many questions: tag each row with the query it answers,
        // the same way `no_match_code` already tags a miss.
        #[derive(serde::Serialize)]
        struct Tagged<'a> {
            query: &'a str,
            #[serde(flatten)]
            hit: &'a crate::search::Hit,
        }
        let rows: Vec<Tagged> = hits
            .iter()
            .map(|hit| Tagged {
                query: args.query,
                hit,
            })
            .collect();
        if let Some(code) = emit_rows(args.out, &rows) {
            return Some(code);
        }
    } else if let Some(code) = emit_rows(args.out, hits) {
        return Some(code);
    }
    if args.out != Output::Text {
        return None;
    }
    drop(render_span);
    let color = match_color();
    let c = color.as_deref();
    let query = args.query;
    if unshown {
        // fell through from --show: no single confident match to print
        let total = hits.first().map_or(hits.len(), |h| h.total);
        eprintln!(
            "rq: no single confident match for {query:?} — {} of {total} candidates below; narrow the query to --show one",
            hits.len()
        );
    }
    for hit in hits {
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
            kind_label(&hit.kind, hit.singleton),
            qualified
        );
        if let Some(sig) = &hit.signature {
            println!("    {}", hl(sig, query, c));
        }
        if args.explain {
            let parts: Vec<String> = hit
                .features
                .iter()
                .map(|f| format!("{} {}", f.name, f.reported()))
                .collect();
            println!(
                "    confidence {:.2} · score {:.0} = {}",
                hit.confidence,
                hit.score,
                parts.join(" + ")
            );
        }
    }
    None
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

/// `--open`/`--web`: choose a hit, then hand off to the editor or browser. The launcher `exec`s (replacing this
/// process), so the shell waits on it — not on rq's background warm.
fn finish_open(hits: &[crate::search::Hit], root: Option<&std::path::Path>, web: bool) -> ExitCode {
    let Some(hit) = choose_hit(hits) else {
        return ExitCode::SUCCESS; // aborted at the prompt
    };

    if web {
        return open_web(hit, root);
    }

    // Results are relative to their own checkout, which under `--all-repos`
    // needn't be the one we're in; the bare path wouldn't open from a subdir.
    let target = match hit.root.as_deref().map(std::path::Path::new).or(root) {
        Some(r) => r.join(&hit.file),
        None => PathBuf::from(&hit.file),
    };
    launch_editor(&target, hit.line)
}

/// The checkouts a search may answer from.
#[derive(Clone, Copy)]
enum Scope {
    /// `--all-repos`, or a directory that isn't a repo rq tracks.
    All,
    Checkout(Checkout),
    /// A checkout the index hasn't registered yet — its first query, before
    /// the warm has written anything. Nothing is in scope, not every repo.
    Nothing,
}

impl Scope {
    fn new(all_repos: bool, in_repo: bool, current: Option<Checkout>) -> Scope {
        match current {
            _ if all_repos => Scope::All,
            Some(c) => Scope::Checkout(c),
            None if in_repo => Scope::Nothing,
            None => Scope::All,
        }
    }

    fn search(
        self,
        store: &Store,
        query: &str,
        current: Option<Checkout>,
        ctx: &crate::search::Context,
        limit: usize,
    ) -> crate::store::Result<crate::search::Matches> {
        let only = match self {
            Scope::All => None,
            Scope::Checkout(c) => Some(c),
            Scope::Nothing => {
                return Ok(crate::search::Matches {
                    hits: Vec::new(),
                    total: 0,
                });
            }
        };
        crate::search::search(store, query, current.map(|c| c.id), only, ctx, limit)
    }
}

/// Launch the editor on `file:line`, resolving the command in order: `RQ_OPEN`
/// template → VS Code (`code`) → `$VISUAL`/`$EDITOR` → print the location. The
/// chosen command replaces this process via `exec`.
fn launch_editor(file: &std::path::Path, line: i64) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let loc = format!("{}:{}", file.display(), line);
    match open_command(file, line, &loc) {
        Some((prog, args)) => {
            // exec replaces this process, so the run's profile goes out now
            crate::profile::emit(false);
            // exec returns only on failure
            let err = std::process::Command::new(&prog).args(&args).exec();
            fail(
                Output::Text,
                Failure::Launch,
                format_args!("rq --open: cannot run {prog}: {err}"),
            )
        }
        None => {
            println!("{loc}");
            ExitCode::SUCCESS
        }
    }
}

/// Resolve the editor command + args. `None` → no launcher configured (the
/// caller prints the location). `RQ_OPEN` is split on whitespace (no shell) with
/// `{file}` / `{line}` / `{}` (= `path:line`) substituted per token; a template
/// with none of them gets `path:line` as its last argument, so `RQ_OPEN=subl`
/// opens the match rather than a bare editor.
fn open_command(file: &std::path::Path, line: i64, loc: &str) -> Option<(String, Vec<String>)> {
    let fstr = file.to_string_lossy().into_owned();

    if let Some(t) = std::env::var_os("RQ_OPEN") {
        let t = t.to_string_lossy();
        let placeholder = ["{file}", "{line}", "{}"].iter().any(|p| t.contains(p));
        let mut parts = t.split_whitespace().map(|p| {
            p.replace("{file}", &fstr)
                .replace("{line}", &line.to_string())
                .replace("{}", loc)
        });
        if let Some(prog) = parts.next() {
            let mut args: Vec<String> = parts.collect();
            if !placeholder {
                args.push(loc.to_string());
            }
            return Some((prog, args));
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

/// `--web`: open `hit` on its git host. Pinned to the newest pushed sha in HEAD's
/// history when the hit is in the repo we're standing in — an unpushed sha would
/// 404. Another repo's checkout state is unknown, so its link follows the host's
/// default branch instead.
fn open_web(hit: &crate::search::Hit, root: Option<&std::path::Path>) -> ExitCode {
    if hit.repo_identity.starts_with("local:") {
        return fail(
            Output::Text,
            Failure::NoRemote,
            format_args!(
                "rq --web: {}:{} has no git remote to link to ({}) — open it \
                 locally with -o, or add one with `git remote add origin <url>`",
                hit.file, hit.line, hit.repo_identity
            ),
        );
    }
    let here = root.is_some_and(|r| hit.root.as_deref() == Some(root_key(r).as_str()));
    let rev = root
        .filter(|_| here)
        .and_then(crate::index::pushed_head)
        .unwrap_or_else(|| "HEAD".into());
    let url = web_url(&hit.repo_identity, &rev, &hit.file, hit.line);

    use std::os::unix::process::CommandExt;
    let browser = std::env::var("BROWSER")
        .ok()
        .filter(|b| !b.is_empty())
        .or_else(|| {
            ["open", "xdg-open"]
                .into_iter()
                .find(|p| on_path(p))
                .map(str::to_string)
        });
    match browser {
        Some(prog) => {
            // exec replaces this process, so the run's profile goes out now
            crate::profile::emit(false);
            // exec returns only on failure
            let err = std::process::Command::new(&prog).arg(&url).exec();
            fail(
                Output::Text,
                Failure::Launch,
                format_args!("rq --web: cannot run {prog}: {err}"),
            )
        }
        None => {
            println!("{url}");
            ExitCode::SUCCESS
        }
    }
}

/// A GitHub-style permalink: `https://<host/org/repo>/blob/<rev>/<file>#L<line>`.
/// GitLab redirects the same shape, so it isn't GitHub-only.
fn web_url(identity: &str, rev: &str, file: &str, line: i64) -> String {
    let path: String = file
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://{identity}/blob/{rev}/{path}#L{line}")
}

/// Whether `prog` resolves on `PATH` (a regular file; symlinks followed).
fn on_path(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
}

/// How long a branch-file list is served before it's refreshed. A commit or a
/// checkout is caught by the stamp; a bare working-tree edit touches neither
/// `.git/HEAD` nor `.git/index`, so only elapsed time catches that — short
/// enough that a burst of searches shares one computation and the edit you just
/// made is reflected on the next search.
const BRANCH_FILES_TTL_SECS: i64 = 15;

/// Longest a branch-file list may be served for, however slow it is to rebuild.
/// The window only governs noticing an *unstaged* edit — every git operation
/// invalidates by stamp regardless — so five minutes is already generous.
const BRANCH_FILES_TTL_MAX_SECS: i64 = 300;

/// How many times its own rebuild cost a list may be served for — so the
/// refresh never eats more than about 1% of the time between searches. The
/// floor binds below ~150 ms, which is where every small repo sits.
///
/// The refresh forks two `git diff`s over the whole worktree, which is cheap on
/// a small repo and very much not on a large one — it was measured at 700 ms on
/// a 90k-file monorepo, against a 40-115 ms query it runs *alongside* and
/// competes with for disk. A fixed 15-second window then re-paid that every
/// fifteen seconds of active searching. Scaling the window by the measured cost
/// leaves small repos exactly where they were and backs off only where the
/// evidence says it's needed.
const BRANCH_FILES_WINDOW_MULTIPLE: i64 = 100;

/// How long a cached branch-file list stays good, given what it cost to build.
fn branch_files_ttl(cost_ms: Option<u64>) -> i64 {
    // `as i64` would wrap a large cost to a negative and quietly hand it the
    // floor — the opposite of what an expensive rebuild has earned.
    let earned = cost_ms.map_or(0, |ms| {
        i64::try_from(ms)
            .unwrap_or(i64::MAX)
            .saturating_mul(BRANCH_FILES_WINDOW_MULTIPLE)
            / 1000
    });
    earned.clamp(BRANCH_FILES_TTL_SECS, BRANCH_FILES_TTL_MAX_SECS)
}

/// A branch-file recomputation running alongside the search. The git work
/// happens on the thread; the store write waits for the main thread, since a
/// SQLite connection isn't shared.
struct BranchRefresh {
    /// Yields the file list and what it cost to build, which sets how long
    /// the result stays good.
    handle: std::thread::JoinHandle<(Vec<String>, u64)>,
    /// The checkout's key ([`root_key`]).
    root: String,
    stamp: String,
}

impl BranchRefresh {
    /// Wait for the recomputation and store it for the next query.
    fn store(self, store: &Store) {
        let Ok((files, cost_ms)) = self.handle.join() else {
            return;
        };
        let _ = store.branch_files_set(&self.root, &self.stamp, now_unix(), cost_ms, &files);
    }
}

/// The branch-changed file list, served from the store when it's still good.
/// Returns the list, plus a recomputation to collect after results print when
/// the stored one has aged out.
///
/// The list feeds a *ranking boost*, so serving a slightly old one costs a
/// little ranking quality, while recomputing it first would cost every search
/// the git diff behind it — which is O(tracked files). So the stored list is
/// served immediately and the refresh runs concurrently with the search rather
/// than after it, which usually hides its cost entirely. It feeds only the next
/// query, so nothing about this run's ranking depends on how the race lands.
///
/// The first search in a repo has nothing to serve and computes inline; that's
/// once per repo, like the first index.
fn cached_branch_files(
    store: &Store,
    root: &std::path::Path,
) -> (Vec<String>, Option<BranchRefresh>, Option<u64>) {
    let key = root_key(root);
    let stamp = crate::index::branch_files_stamp(root);
    let cached = store.branch_files_get(&key).ok().flatten();
    let now = now_unix();

    if let (Some(hit), Some(stamp)) = (&cached, &stamp) {
        if &hit.stamp == stamp && now.saturating_sub(hit.written_at) < branch_files_ttl(hit.cost_ms)
        {
            return (hit.files.clone(), None, hit.cost_ms);
        }
        let owned_root = root.to_path_buf();
        let refresh = BranchRefresh {
            handle: std::thread::spawn(move || {
                let t = std::time::Instant::now();
                let files = crate::index::branch_changed_files(&owned_root);
                (files, t.elapsed().as_millis() as u64)
            }),
            root: key,
            stamp: stamp.clone(),
        };
        return (hit.files.clone(), Some(refresh), hit.cost_ms);
    }

    // Nothing cached (or no git state to stamp it with): compute inline.
    let t = std::time::Instant::now();
    let files = crate::index::branch_changed_files(root);
    let cost_ms = t.elapsed().as_millis() as u64;
    if let Some(stamp) = stamp {
        let _ = store.branch_files_set(&key, &stamp, now, cost_ms, &files);
    }
    (files, None, Some(cost_ms))
}

/// What git says of the worktree since it was indexed.
enum Worktree {
    /// HEAD moved, or no HEAD was recorded to compare against: everything
    /// counts as changed.
    Moved,
    /// The files `git status` calls dirty, to check against the index.
    Dirty(Vec<String>),
    /// Git can't say (no git, or nothing it tracks): compare the tree itself.
    Untold,
}

/// Whether the worktree has moved since it was indexed — a different HEAD, or
/// uncommitted edits. Split out from the store read so this half can run on its
/// own thread: `dirty_files` forks `git status`, which on a large worktree costs
/// more than the search it was gating (measured: 12.6ms of a 16.8ms query on a
/// 6k-file repo, against 0.1ms on a 54-file one).
fn worktree_edits(cwd: &std::path::Path, indexed_head: Option<&str>) -> Worktree {
    let Some(head) = indexed_head else {
        return if crate::index::is_git_repo(cwd) {
            Worktree::Moved
        } else {
            Worktree::Untold
        };
    };
    let _span = crate::profile::span("git: worktree changed?");
    if crate::index::head_state(cwd).as_deref() != Some(head) {
        Worktree::Moved
    } else if !crate::index::git_speaks_for(cwd, head) {
        Worktree::Untold
    } else {
        Worktree::Dirty(crate::index::dirty_files(cwd))
    }
}

/// Whether the worktree holds anything the index doesn't yet reflect, given
/// what [`worktree_edits`] found.
fn changed_since_index(store: &Store, root: &std::path::Path, edits: Worktree) -> bool {
    let checkout = checkout_at(store, root);
    match edits {
        Worktree::Moved => true,
        Worktree::Untold => {
            checkout.is_none_or(|c| crate::index::untracked_tree_moved(store, c.id, root))
        }
        Worktree::Dirty(dirty) => match checkout {
            Some(c) => crate::index::has_unindexed_changes(store, c.id, root, &dirty),
            None => !dirty.is_empty(),
        },
    }
}

/// Whether the worktree at `root` has moved since it was indexed at `head`,
/// recording a "nothing moved" verdict that spares warm children the same
/// question within the recheck window.
fn worktree_moved(store: &Store, root: &std::path::Path, head: Option<&str>) -> bool {
    // The window runs from before `git status`, so an edit made during it
    // still falls inside. The stamp is read after: status may rewrite
    // `.git/index` itself, and a HEAD that moved meanwhile yields none.
    let checked_at = now_unix();
    let moved = changed_since_index(store, root, worktree_edits(root, head));
    if !moved {
        record_verified(store, root, head, checked_at);
    }
    moved
}

/// The "has the worktree moved since it was indexed?" check on a complete
/// repo. It forks `git status`, which grows with the worktree (~12 ms on
/// rails, ~27 ms on a 14k-file repo) and decides nothing a hit depends on: a
/// hit hands it to the detached warm child, so the process exits without
/// waiting on git; a miss, whose exit code depends on it, runs it inline.
/// Holds the root and the HEAD the index reflects.
struct Staleness(PathBuf, Option<String>);

/// Settle warming once the answer is out: resolve the staleness check,
/// reindex if the worktree moved, and hand any remainder to a detached child.
///
/// Called from *both* exits. The miss path matters as much as the render one —
/// a symbol added a moment ago is precisely a miss, and reindexing before we
/// exit is what makes the immediate retry hit. `hit` says which exit this is;
/// `moved` is the session's memo of the answer.
fn settle_warm(
    store: &Store,
    staleness: Option<Staleness>,
    moved: &mut Option<bool>,
    hit: bool,
    was_warming: bool,
    warming_ok: bool,
    root: Option<&std::path::Path>,
) -> bool {
    let changed = match staleness {
        None => false,
        // asked already, and its reindex dispatched then (a batch, up front)
        Some(_) if let Some(m) = *moved => return m,
        // The answer is out and didn't depend on this: the warm child asks git
        // and reindexes only if something moved (see `cmd_warm`).
        Some(Staleness(r, head)) if hit => {
            if recently_verified(store, &r, head.as_deref()) {
                crate::trace!("warm: verified unchanged within the recheck window, not spawning");
            } else {
                spawn_detached_warm(&r);
            }
            return false;
        }
        Some(Staleness(r, head)) => {
            let _span = crate::profile::span("after: staleness check");
            *moved.insert(worktree_moved(store, &r, head.as_deref()))
        }
    };
    // Reindexing an edited worktree means sweeping every file to find the few
    // that moved — ~32ms on a 3000-file repo, and it was paid on *every* query
    // for as long as anything stayed uncommitted, which is exactly while you're
    // working. The shell shouldn't wait for that: hand it to the detached
    // child, which is what "the shell never waits on it" already promises
    // everywhere else.
    maybe_detach_warm(store, warming_ok && (was_warming || changed), changed, root);
    // Whether the worktree moved: the reindex is the child's, so a miss is
    // provisional.
    changed
}

/// Inline warm budget on the search path. A *cap*, not a fixed delay:
/// `index_budgeted` returns the moment a full sweep finishes, so small/medium
/// repos index completely and pay only their real cost. The cap only bites a
/// genuinely huge, never-indexed repo — where a bigger budget buys a much better
/// first answer (a tiny budget can return nothing, since a git repo has no
/// live-scan fallback). 500 ms is a one-time cold-cache cost, trivial next to
/// scanning a large tree from scratch; the detached child and later queries
/// fill in the rest.
fn answer_warm_budget() -> Duration {
    env_budget("RQ_ANSWER_BUDGET_MS", 500)
}

/// What `--symbols` adds to the answer budget for its synchronous warm: an
/// outline has no answer to get out of the way of.
fn deferred_warm_budget() -> Duration {
    env_budget("RQ_DEFERRED_BUDGET_MS", 250)
}

/// Bound for the git-repo live-scan fallback (index empty, still warming): enough
/// to surface a result the warm hasn't reached, without an unbounded walk.
fn live_fallback_budget() -> Duration {
    env_budget("RQ_FALLBACK_BUDGET_MS", 250)
}

/// How long a pass nobody is waiting on — `rq --index`, a warm child — waits
/// out another writer, each time: a cold pass's end rebuilds the name index
/// in one transaction, which on a large repo outlasts a search's busy timeout.
fn writer_wait() -> Duration {
    env_budget("RQ_WRITER_WAIT_MS", 30_000)
}

/// Tell the human at the terminal, once, why `rq --index` isn't moving.
fn say_waiting() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| eprintln!("rq: waiting for another rq writing the index…"));
}

/// Budget for each pass of the *detached* warm child — generous, because
/// nothing waits on it: the shell got its results and the child runs niced in
/// the background.
fn warm_bg_budget() -> Duration {
    env_budget("RQ_WARM_BUDGET_MS", 20_000)
}

/// How long a warm child sweeps in all. Inside the lock's TTL, so a live
/// child's lock never reads as a crashed one's and lets a second warmer in.
fn warm_sweep_cap() -> Duration {
    Duration::from_secs(crate::store::WARM_LOCK_TTL_SECS as u64 / 2)
}

/// Whether a search hands leftover warming to a detached child (default), or
/// never spawns one (`RQ_WARM_DETACH=0`): the index is then only what a
/// search's own in-process warm reads and `rq --index` writes. Recall runs and
/// tests that stage a half-built index use it.
fn warm_detach_enabled() -> bool {
    std::env::var("RQ_WARM_DETACH").map_or(true, |v| v != "0")
}

/// `RQ_WARM_DETACH=wait`: the detached child is spawned as in production,
/// and the search waits for it, so a test sees what the child did without
/// racing it.
fn warm_detach_waits() -> bool {
    std::env::var("RQ_WARM_DETACH").is_ok_and(|v| v == "wait")
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

/// Parse a `--wait` value into a duration: `<n>ms`, `<n>s`, `<n>m`, or a bare
/// `<n>` (seconds). Fractions are allowed (`1.5s`); `0` (any unit) means "don't
/// wait". A `clap` value parser, so an invalid duration is rejected at parse
/// time with a usage error.
fn parse_wait(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let bad = || format!("invalid duration {s:?} — use e.g. 50ms, 2s, 1m, or 0");
    // check "ms" before "s" so the "s" arm doesn't swallow it
    let (num, unit_ms) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1_000.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000.0)
    } else {
        // a bare number is seconds
        (s, 1_000.0)
    };
    let val: f64 = num.trim().parse().map_err(|_| bad())?;
    if !val.is_finite() || val < 0.0 {
        return Err(bad());
    }
    Ok(Duration::from_millis((val * unit_ms).round() as u64))
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
    // SAFETY: a zeroed sigaction is valid (empty mask, no flags), its handler
    // is async-signal-safe, and `Once` keeps the install single-threaded.
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
fn draw_progress(store: &Store, checkout: Option<Checkout>, label: &str) {
    let files = checkout
        .and_then(|c| store.checkout_totals(c.id).ok())
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

/// The key a checkout is stored under: its canonical root, as indexing
/// records it.
fn root_key(root: &std::path::Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The checkout rooted at `root`, once a pass has registered it.
fn checkout_at(store: &Store, root: &std::path::Path) -> Option<Checkout> {
    store.checkout(&root_key(root)).ok().flatten()
}

/// The definition's source line (trimmed) at `line` of `path`. Best-effort.
fn read_signature(path: &std::path::Path, line: i64) -> Option<String> {
    let src = crate::index::read_source(path).ok()?;
    signature_in(&src.lines().collect::<Vec<_>>(), line)
}

/// Confidence at or above which `--show` prints a body instead of a list. Exact
/// (1.0) and a unique prefix (0.9) clear it; a fuzzy or tied match does not — so
/// `--show` never prints a definition it isn't sure about.
const SHOW_CONFIDENCE: f64 = 0.85;

/// `--show`: if the top hit is confident, read and print its full source span
/// and return the exit code; otherwise return `None` to fall through to the
/// ranked list. Emits a single object in JSON/NDJSON (with a `body` field).
fn show_top_definition(
    hits: &mut [crate::search::Hit],
    query: &str,
    out: Output,
    ranked: f64,
) -> Option<ExitCode> {
    let top = hits.first()?;
    if ranked < SHOW_CONFIDENCE {
        return None; // ambiguous / weak — let the caller list candidates
    }
    let end = top.end_line.unwrap_or(top.line);
    let body = top.root.as_deref().and_then(|root| {
        let src = crate::index::read_source(&std::path::Path::new(root).join(&top.file)).ok()?;
        span_in(&src, top.line, end)
    });
    hits[0].body = body;
    let top = &hits[0];
    let code = match out {
        Output::Json | Output::Ndjson => {
            // fail loudly on a serialize error, like every other JSON path
            emit_json(out, top)
        }
        Output::Text => {
            let color = match_color();
            let c = color.as_deref();
            let name = hl(&top.name, query, c);
            let qualified = match &top.parent {
                Some(p) => format!("{name} · {p}"),
                None => name,
            };
            println!(
                "{}:{}  {} {}",
                hl_path(&top.file, query, c),
                top.line,
                kind_label(&top.kind, top.singleton),
                qualified
            );
            match (&top.body, &top.signature) {
                (Some(body), _) => println!("{body}"),
                // end_line unknown (pre-v4 row) → at least the definition line
                (None, Some(sig)) => println!("{sig}"),
                (None, None) => {}
            }
            ExitCode::SUCCESS
        }
    };

    Some(code)
}

/// Lines `start..=end` (1-based, inclusive) of already-read `content`, joined —
/// clamped to the file's bounds. `None` if `start` is past the end.
fn span_in(content: &str, start: i64, end: i64) -> Option<String> {
    let s = usize::try_from(start).ok()?.checked_sub(1)?;
    let lines: Vec<&str> = content.lines().collect();
    if s >= lines.len() {
        return None;
    }
    let e = usize::try_from(end).ok()?.clamp(s + 1, lines.len());
    Some(lines[s..e].join("\n"))
}

/// The trimmed source line `line` (1-based) of a file already split into
/// `lines`, if non-empty — a symbol's definition line. Takes the split rather
/// than the text so `--symbols` splits once: re-scanning from the top per
/// symbol is quadratic in a large file.
fn signature_in(lines: &[&str], line: i64) -> Option<String> {
    let idx = usize::try_from(line).ok()?.checked_sub(1)?;
    let l = lines.get(idx)?.trim();
    (!l.is_empty()).then(|| l.to_string())
}

/// A result's kind as text prints it: a type's own member (a class method, a
/// `static` one) says so, as JSON's `singleton` does.
fn kind_label(kind: &str, singleton: bool) -> std::borrow::Cow<'_, str> {
    if singleton {
        format!("singleton {kind}").into()
    } else {
        kind.into()
    }
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
fn cmd_symbols(file_arg: &str, kinds: &[String], langs: &[String], out: Output) -> ExitCode {
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
                let obj = serde_json::json!({ "status": "no_match" });
                let _ = emit_json(out, &obj); // exit code below carries the miss
            }
            Output::Text => eprintln!("no symbols"),
        }
        return Verdict::Miss.exit_code();
    }
    if let Some(code) = emit_rows(out, syms) {
        return code;
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

/// A leading positional that names a symbol kind — the shorthand behind
/// `rq class Foo` and `rq method zoom`. Only the full, unambiguous keyword forms
/// count (never the single-letter `-k` shortcuts, which are far likelier to be a
/// real query). Returns the canonical kind, so it filters exactly like `--kind`.
fn keyword_kind(token: &str) -> Option<&'static [&'static str]> {
    let word = token.to_ascii_lowercase();
    // a kind's own name, or the keyword a language declares it with
    let keyword =
        Kind::from_tag(&word).is_some() || matches!(word.as_str(), "fn" | "interface" | "const");
    keyword.then(|| canonical_kind(&word)).flatten()
}

/// Peel a leading kind keyword off the query, so `rq class Foo` (or the quoted
/// `rq 'class Foo'`) means `-k class` + query `Foo`. The keyword must be followed
/// by a real query token — a bare `rq class` stays a search for a symbol literally
/// named `class`. Returns `(kind, query, trailing_path_dirs)`; the trailing dirs
/// are the rg-style positionals left after the query is consumed.
fn split_kind_keyword(
    target: String,
    dirs: Vec<String>,
) -> (Option<&'static [&'static str]>, String, Vec<String>) {
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

/// Normalize a `--kind` value (name or shortcut) to the canonical symbol kinds
/// it selects; `None` for an unknown one, which is a usage error.
fn canonical_kind(s: &str) -> Option<&'static [&'static str]> {
    Some(match s.to_ascii_lowercase().as_str() {
        "c" | "class" => &["class"],
        "m" | "method" => &["method"],
        "f" | "fn" | "func" | "function" => &["function"],
        "mod" | "module" => &["module"],
        "s" | "struct" => &["struct"],
        // `type` is any named type: an alias, and the structs that languages
        // declare with the same keyword (Go's `type Foo struct`)
        "type" => &["type", "struct"],
        "alias" | "type_alias" => &["type"],
        "e" | "enum" => &["enum"],
        "t" | "trait" | "interface" => &["trait"],
        "const" | "constant" => &["constant"],
        "macro" | "macro_rules" => &["macro"],
        "v" | "variant" | "member" | "enum_member" => &["variant"],
        "field" | "property" | "prop" => &["field"],
        _ => return None,
    })
}

/// Expand a `--lang` value to the language tag(s) it selects: a **prefix** of any
/// known language name (so `r` → ruby+rust, `p`/`py` → python, `g` → go,
/// `t` → typescript, `j` → javascript), or one of a plugin's extensions
/// (`rb`, `tsx`) or aliases (`golang`). An unknown value selects none, which
/// is a usage error — the empty one too (`-x ruby,`), though it prefixes all.
fn canonical_langs(s: &str) -> Vec<String> {
    let t = s.to_ascii_lowercase();
    if t.is_empty() {
        return Vec::new();
    }
    crate::lang::registry()
        .iter()
        .filter(|p| {
            p.language().starts_with(&t)
                || p.extensions().contains(&t.as_str())
                || p.aliases().contains(&t.as_str())
        })
        .map(|p| p.language().to_string())
        .collect()
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
    let stem = crate::search::path_stem(path);
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
    crate::index::rel_key(root, &abs).unwrap_or_else(|| file.to_string())
}

/// Revalidate the files behind the top hits against disk — each in the
/// checkout it was read from — refreshing any that changed, and noting in
/// `gone` any deleted since. Returns true if anything changed (so the caller
/// re-runs the search).
fn revalidate_top(
    store: &mut Store,
    hits: &[crate::search::Hit],
    gone: &mut Vec<(String, String)>,
) -> bool {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut changed = false;
    for hit in hits {
        let Some(root) = hit.root.as_deref() else {
            continue;
        };
        if !seen.insert((root, hit.file.as_str())) {
            continue;
        }
        let Some(checkout) = store.checkout(root).ok().flatten() else {
            continue; // a live row: nothing stored to refresh
        };
        match crate::index::refresh_file(store, checkout, std::path::Path::new(root), &hit.file) {
            Ok(crate::index::Refresh::Updated) => changed = true,
            Ok(crate::index::Refresh::Missing) => gone.push((root.to_string(), hit.file.clone())),
            _ => {}
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
    // The repo root even from a subdirectory: `--path` scopes a subset.
    let (root, _) = checkout_root(&target);
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
    let open_span = crate::profile::span("store open");
    let mut store = match open_store_or_fail(out, "rq --index") {
        Ok(s) => s,
        Err(code) => return code,
    };
    drop(open_span);
    let on_long_wait = show_progress(out, stderr_interactive()).then_some(say_waiting as fn());
    let _ = store.wait_out_writers(writer_wait(), on_long_wait);
    let indexed = crate::index::index_under(&mut store, &root, &subdirs);
    match indexed {
        Ok(stats) => {
            // After the index, which has just recorded this checkout's
            // identity — so this is a cache hit rather than a second `git
            // remote` fork. Not on failure: a busy writer would cost a second wait.
            let identity = resolve_identity(&store, &root);
            let subtree = !subdirs.is_empty();
            // distinguish this run's incremental work from the checkout's totals
            let totals = checkout_at(&store, &root).and_then(|c| store.checkout_totals(c.id).ok());
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
                            "root": root_key(&root),
                            "scope": if subtree { "subtree" } else { "full" },
                            "files_added": stats.files_parsed,
                            "symbols_added": stats.symbols,
                            "files": files,
                            "symbols": symbols,
                        }),
                    );
                }
                Output::Text => {
                    let scope = if subtree { " (subtree seed)" } else { "" };
                    match totals {
                        Some((files, symbols)) => println!(
                            "{} file(s)/{} symbol(s) added this run; index{scope} now {files} files, {symbols} symbols",
                            stats.files_parsed, stats.symbols
                        ),
                        None => println!(
                            "{} file(s)/{} symbol(s) added this run{scope}",
                            stats.files_parsed, stats.symbols
                        ),
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(out, Failure::Index, format_args!("rq --index: {e}")),
    }
}

fn cmd_drop(target: Option<String>, out: Output) -> ExitCode {
    let mut store = match open_store_or_fail(out, "rq --drop") {
        Ok(s) => s,
        Err(code) => return code,
    };

    // Resolve what to drop: TARGET as a path (→ repo root, like --index) drops
    // that checkout; failing that, TARGET as a repo identity drops the repo
    // with every checkout of it — so cruft shown by --status can be dropped by
    // name even if the checkout is gone.
    let path = PathBuf::from(target.clone().unwrap_or_else(|| ".".to_string()));
    let here = Here::at(&store, &path);
    let root = here.root;
    let key = root_key(&root);
    let dropped = |identity: &str, root: Option<&str>, (files, symbols): (i64, i64)| match out {
        Output::Text => {
            let what = root.map_or_else(|| identity.to_string(), |r| format!("{identity} at {r}"));
            println!("dropped {what} ({files} file(s), {symbols} symbol(s))");
            ExitCode::SUCCESS
        }
        _ => emit_json(
            out,
            &serde_json::json!({"repo": identity, "root": root, "files": files, "symbols": symbols, "dropped": true}),
        ),
    };

    if let Some(checkout) = here.checkout {
        let identity = store
            .identity_for_root(&key)
            .ok()
            .flatten()
            .unwrap_or_default();
        let totals = store.checkout_totals(checkout.id).unwrap_or((0, 0));
        return match store.forget_checkout(&key) {
            Ok(()) => dropped(&identity, Some(&key), totals),
            Err(e) => fail(out, Failure::Database, format_args!("rq --drop: {e}")),
        };
    }
    let repo = match target.as_deref().map(|t| store.repository_id(t)) {
        Some(Ok(Some(id))) => Some((target.clone().unwrap_or_default(), id)),
        Some(Err(e)) => return fail(out, Failure::Database, format_args!("rq --drop: {e}")),
        _ => None,
    };
    let Some((identity, repo_id)) = repo else {
        // nothing to drop — idempotent. `dropped: false` lets a script tell.
        // `repo` stays an identity: a path's is what `--index` would record.
        let (identity, at) = if root.try_exists().unwrap_or(false) {
            (crate::index::detect_identity(&root).to_string(), Some(key))
        } else {
            (target.unwrap_or_default(), None)
        };
        return match out {
            Output::Text => {
                println!("not indexed: {}", at.as_deref().unwrap_or(&identity));
                ExitCode::SUCCESS
            }
            _ => emit_json(
                out,
                &serde_json::json!({"repo": identity, "root": at, "files": 0, "symbols": 0, "dropped": false}),
            ),
        };
    };
    let totals = store.repo_totals(repo_id).unwrap_or((0, 0));
    match store.drop_repository(repo_id) {
        Ok(()) => dropped(&identity, None, totals),
        Err(e) => fail(out, Failure::Database, format_args!("rq --drop: {e}")),
    }
}

/// Print a single value as JSON: `--json` pretty, `--ndjson` compact one-liner.
/// Used by the single-object operations (`--index`, `--drop`) and the
/// no-match status objects; [`emit_rows`] is the multi-row twin.
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
        Err(e) => fail(out, Failure::Internal, format_args!("rq: {e}")),
    }
}

/// Print a row set as structured output: `--json` one pretty array, `--ndjson`
/// one compact object per line. Returns `Some(exit)` on a serialization
/// failure, `None` on success (Text output is the caller's business).
fn emit_rows<T: serde::Serialize>(out: Output, rows: &[T]) -> Option<ExitCode> {
    match out {
        Output::Json => match serde_json::to_string_pretty(rows) {
            Ok(s) => println!("{s}"),
            Err(e) => return Some(fail(out, Failure::Internal, format_args!("rq: {e}"))),
        },
        Output::Ndjson => {
            for r in rows {
                match serde_json::to_string(r) {
                    Ok(line) => println!("{line}"),
                    Err(e) => return Some(fail(out, Failure::Internal, format_args!("rq: {e}"))),
                }
            }
        }
        Output::Text => {}
    }
    None
}

fn cmd_status(out: Output) -> ExitCode {
    let store = match open_store_or_fail(out, "rq --status") {
        Ok(s) => s,
        Err(code) => return code,
    };
    crate::index::prune_missing_checkouts(&store);
    let mut rows = match store.coverage_overview() {
        Ok(rows) => rows,
        Err(e) => return fail(out, Failure::Database, format_args!("rq --status: {e}")),
    };
    // the same `of` a hit from the checkout reports
    for row in rows
        .iter_mut()
        .filter(|r| r.status != Coverage::Complete && r.of.is_none())
    {
        if let Some(checkout) = store.checkout(&row.root).ok().flatten() {
            row.of = tree_span(&store, &row.root, checkout.id).map(|s| s.max(row.files));
        }
    }
    if let Some(code) = emit_rows(out, &rows) {
        return code;
    }
    match out {
        Output::Json | Output::Ndjson => {}
        Output::Text if rows.is_empty() => {
            println!("no repositories indexed yet (try `rq --index`)");
        }
        Output::Text => {
            for r in &rows {
                // a local identity is its root already
                let at = if r.identity == format!("local:{}", r.root) {
                    String::new()
                } else {
                    format!("  {}", r.root)
                };
                let files = match r.of {
                    Some(of) => format!("{} of {of}", r.files),
                    None => r.files.to_string(),
                };
                let finishing = finishing_note(r.phase, r.phase_secs);
                println!(
                    "{:<10} {files:>6} files{finishing}  {:>7} symbols  {}{at}",
                    r.status, r.symbols, r.identity
                );
            }
        }
    }
    if out == Output::Text
        && let Ok(db) = db_location()
    {
        for line in leftovers(&db, store.file()) {
            println!("{line}");
        }
    }
    ExitCode::SUCCESS
}

/// What `--status` says about the files beside the index (DECISIONS D51): the
/// index this rq uses when a newer rq owns the default one, other versions'
/// indexes, and a damaged index kept after a rebuild.
fn leftovers(db: &std::path::Path, using: Option<&std::path::Path>) -> Vec<String> {
    let name = |p: &std::path::Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    let size = |p: &std::path::Path| {
        let bytes: u64 = ["", "-wal"]
            .iter()
            .filter_map(|s| std::fs::metadata(format!("{}{s}", p.display())).ok())
            .map(|m| m.len())
            .sum();
        format!("{} MB", bytes.div_ceil(1 << 20))
    };
    let mut lines = Vec::new();
    let own = crate::store::side_path(db, crate::store::VERSION);
    // SQLite reports the file with symlinks resolved, so compare names
    if using.is_some_and(|u| u.file_name() == own.file_name()) {
        lines.push(format!(
            "this rq uses {}: {} belongs to a newer rq",
            name(&own),
            name(db)
        ));
    }
    for (side, v) in crate::store::side_stores(db, 1..=crate::store::VERSION + 64) {
        if side != own {
            lines.push(format!(
                "{}  schema v{v}'s own index, {}",
                name(&side),
                size(&side)
            ));
        }
    }
    if let Some(copy) = crate::store::broken_copy(db) {
        lines.push(format!(
            "{}  set aside when it couldn't be used, {} (safe to delete)",
            name(&copy),
            size(&copy)
        ));
    }
    lines
}

/// `--usage`: how rq has actually been called, by day, caller, and flag set.
/// Reads `usage_daily`, which outlives the pruned raw event log.
fn cmd_usage(out: Output) -> ExitCode {
    let store = match open_store_or_fail(out, "rq --usage") {
        Ok(s) => s,
        Err(code) => return code,
    };
    let rows = match store.usage_overview() {
        Ok(rows) => rows,
        Err(e) => return fail(out, Failure::Database, format_args!("rq --usage: {e}")),
    };
    if let Some(code) = emit_rows(out, &rows) {
        return code;
    }
    match out {
        Output::Json | Output::Ndjson => {}
        Output::Text if rows.is_empty() => {
            eprintln!("no usage recorded yet");
        }
        Output::Text => {
            // Columns of bare numbers need naming; `--status` gets away without
            // a header because its columns carry their own units.
            println!(
                "{:<10}  {:<16} {:>6} {:>7} {:>8}  flags",
                "day", "caller", "found", "missed", "warming"
            );
            for r in &rows {
                let flags = if r.flags.is_empty() { "-" } else { &r.flags };
                println!(
                    "{:<10}  {:<16} {:>6} {:>7} {:>8}  {}",
                    r.day,
                    r.source,
                    r.searches - r.misses - r.warming,
                    r.misses,
                    r.warming,
                    flags
                );
            }
            let searches: i64 = rows.iter().map(|r| r.searches).sum();
            let misses: i64 = rows.iter().map(|r| r.misses).sum();
            let warming: i64 = rows.iter().map(|r| r.warming).sum();
            let complete: i64 = rows.iter().map(|r| r.on_complete).sum();
            let live: i64 = rows.iter().map(|r| r.live).sum();
            let plural = if searches == 1 { "search" } else { "searches" };
            // rare, and only outside a repo: named when it happened at all
            let live = if live > 0 {
                format!(" · {live} from a live scan")
            } else {
                String::new()
            };
            // Counts, not a percentage: these totals are often small enough
            // that a percentage would read as more evidence than there is.
            println!(
                "{searches} {plural} · {misses} missed · {warming} asked too early · {complete} on a complete index{live}"
            );
        }
    }
    // Nothing recorded exits 1, like a search that finds nothing. (An empty
    // --status exits 0: it ran, and "no repos" is its answer.)
    if rows.is_empty() {
        return Verdict::Miss.exit_code();
    }
    ExitCode::SUCCESS
}

/// Open the rq database, honoring `RQ_DB` and creating parent dirs.
fn open_store() -> Result<Store, Box<dyn std::error::Error>> {
    let path = db_location()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Store::open(&path)?)
}

/// [`open_store`] for a command, or the exit code of the error it reported
/// as `{mode}: …` (a structured caller gets it as JSON).
fn open_store_or_fail(out: Output, mode: &str) -> std::result::Result<Store, ExitCode> {
    open_store().map_err(|e| {
        fail(
            out,
            Failure::Database,
            format_args!("{mode}: cannot open database: {e}"),
        )
    })
}

/// The database path from the environment, or the usage error that refuses it.
/// A relative path would resolve against each caller's cwd, silently splitting
/// the one shared index into a database per directory.
fn db_location() -> Result<PathBuf, String> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
    db_location_from(var("RQ_DB"), var("HOME"))
}

fn db_location_from(
    rq_db: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(db) = rq_db {
        let shown = db.to_string_lossy();
        if std::path::Path::new(&db).is_relative() {
            // the shell doesn't expand a quoted or mid-word `~`
            let hint = match shown.strip_prefix("~/") {
                Some(rest) => format!("$HOME/{rest}"),
                None => format!("$PWD/{shown}"),
            };
            return Err(format!(
                "rq: RQ_DB must be an absolute path, not {shown:?} (e.g. RQ_DB=\"{hint}\")"
            ));
        }
        if shown.ends_with('/') || std::path::Path::new(&db).is_dir() {
            return Err(format!(
                "rq: RQ_DB names a directory, not a database file: {shown:?} (e.g. RQ_DB=\"{}/rq.db\")",
                shown.trim_end_matches('/')
            ));
        }
        return Ok(PathBuf::from(db));
    }
    let Some(home) = home else {
        return Err(
            "rq: HOME is not set, so there's no default index path; set HOME, or RQ_DB to an absolute path".into(),
        );
    };
    if std::path::Path::new(&home).is_relative() {
        return Err(format!(
            "rq: HOME is a relative path ({:?}), so the default index would move with the working directory; set RQ_DB to an absolute path",
            home.to_string_lossy()
        ));
    }
    Ok(PathBuf::from(home).join(".local/share/rq/rq.db"))
}

/// What a search found, as its exit code and the usage counters tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// An answer (0).
    Hit,
    /// The index is complete, and nothing matched (1): definitive.
    Miss,
    /// The index couldn't yet say (2): ask again.
    Warming,
}

impl Verdict {
    /// The label `--usage` counts it under.
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Hit => "hit",
            Verdict::Miss => "miss",
            Verdict::Warming => "warming",
        }
    }

    fn exit_code(self) -> ExitCode {
        match self {
            Verdict::Hit => ExitCode::SUCCESS,
            Verdict::Miss => ExitCode::FAILURE,
            Verdict::Warming => ExitCode::from(2),
        }
    }
}

/// What kind of thing went wrong: the stable `kind` of a structured error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    /// The command line asks for something rq can't do.
    Usage,
    /// The index can't be opened or read.
    Database,
    /// A file the command names doesn't exist.
    NotFound,
    /// `--web` on a repo with no git host to link to.
    NoRemote,
    /// The editor or browser couldn't be started.
    Launch,
    /// `--index` failed part-way.
    Index,
    /// rq couldn't render its own output.
    Internal,
}

impl Failure {
    fn as_str(self) -> &'static str {
        match self {
            Failure::Usage => "usage",
            Failure::Database => "database",
            Failure::NotFound => "not_found",
            Failure::NoRemote => "no_remote",
            Failure::Launch => "launch",
            Failure::Index => "index",
            Failure::Internal => "internal",
        }
    }

    /// The process exit code, from sysexits(3) so no error shares a code with
    /// a verdict (0 hit, 1 miss, 2 warming). Coarser than `kind`: one code per
    /// thing the caller does about it.
    fn exit_code(self) -> u8 {
        match self {
            Failure::Usage => 64,                      // EX_USAGE: fix the command
            Failure::NotFound => 66,                   // EX_NOINPUT: fix the path
            Failure::NoRemote | Failure::Launch => 69, // EX_UNAVAILABLE
            Failure::Internal => 70,                   // EX_SOFTWARE: a bug
            Failure::Database | Failure::Index => 74,  // EX_IOERR
        }
    }
}

/// Report an error and return its exit code. The message always goes to
/// stderr; a structured caller also gets it as one JSON object on stdout.
fn fail(out: Output, kind: Failure, args: std::fmt::Arguments) -> ExitCode {
    let message = args.to_string();
    eprintln!("{message}");
    emit_error(out, kind, &message);
    ExitCode::from(kind.exit_code())
}

/// The structured half of an error: `{"error", "kind", "code"}` on stdout,
/// nothing for text. `code` is the exit code the process leaves with.
fn emit_error(out: Output, kind: Failure, message: &str) {
    let obj = serde_json::json!({
        "error": message,
        "kind": kind.as_str(),
        "code": kind.exit_code(),
    });
    // Printed directly: `emit_json` reports its own failures through here.
    let rendered = match out {
        Output::Text => return,
        Output::Json => serde_json::to_string_pretty(&obj),
        Output::Ndjson => serde_json::to_string(&obj),
    };
    if let Ok(s) = rendered {
        println!("{s}");
    }
}

/// A command line clap rejected. It fails before rq knows its output mode, so
/// the structured flags are read off argv directly: a caller that asked for
/// JSON gets its usage error as JSON too. Not `err.exit()`: clap exits 2,
/// which rq reserves for warming.
fn clap_failure(err: clap::Error) -> ExitCode {
    // help and --version aren't errors
    if !err.use_stderr() {
        err.exit();
    }
    let _ = err.print();
    let out = requested_output(std::env::args_os().skip(1));
    let text = err.to_string();
    emit_error(out, Failure::Usage, text.lines().next().unwrap_or(""));
    ExitCode::from(Failure::Usage.exit_code())
}

/// The output mode argv asks for, without a full parse: `--json`/`--ndjson`,
/// or `-j`/`-J` alone or in a cluster of short flags (`-ej`). A cluster ends at
/// the first flag that takes a value, since the rest is that value (`-xj` is
/// `--lang j`). Nothing after `--` is a flag.
fn requested_output(args: impl IntoIterator<Item = std::ffi::OsString>) -> Output {
    let cmd = Cli::command();
    let takes_value = |c: char| {
        cmd.get_arguments()
            .any(|a| a.get_short() == Some(c) && a.get_action().takes_values())
    };
    let (mut json, mut ndjson) = (false, false);
    for arg in args {
        let arg = arg.to_string_lossy();
        match arg.as_ref() {
            "--" => break,
            "--json" => json = true,
            "--ndjson" => ndjson = true,
            a if a.starts_with('-') && !a.starts_with("--") => {
                for c in a.chars().skip(1) {
                    match c {
                        'j' => json = true,
                        'J' => ndjson = true,
                        c if takes_value(c) => break,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // the same precedence `output_format` gives a parsed command line
    if ndjson {
        Output::Ndjson
    } else if json {
        Output::Json
    } else {
        Output::Text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_help_example_parses() {
        let help = Cli::command().render_long_help().to_string();
        let examples = help
            .split("EXAMPLES:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .expect("--help has an EXAMPLES section");
        let mut seen = 0;
        for line in examples.lines() {
            // the command, up to the gap before its description
            let command = line.trim().split("   ").next().unwrap().trim();
            let args: Vec<&str> = command
                .split_whitespace()
                .map(|w| w.trim_matches('\''))
                .collect();
            assert_eq!(args[0], "rq", "an example starts with rq: {line}");
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("example doesn't parse: {line}\n{e}");
            }
            seen += 1;
        }
        assert!(seen > 5, "found the examples: {examples}");
    }

    #[test]
    fn the_database_path_is_refused_unless_it_is_an_absolute_file() {
        let at = |db: Option<&str>, home: Option<&str>| {
            db_location_from(db.map(Into::into), home.map(Into::into))
        };
        assert_eq!(
            at(None, Some("/home/a")).unwrap(),
            PathBuf::from("/home/a/.local/share/rq/rq.db")
        );
        assert_eq!(
            at(Some("/x/rq.db"), None).unwrap(),
            PathBuf::from("/x/rq.db")
        );

        let refused = |db, home| at(db, home).unwrap_err();
        assert!(refused(Some("~/x.db"), None).contains("RQ_DB=\"$HOME/x.db\""));
        assert!(refused(Some("x.db"), None).contains("RQ_DB=\"$PWD/x.db\""));
        assert!(refused(Some("/x/"), None).contains("RQ_DB=\"/x/rq.db\""));
        assert!(refused(None, None).contains("HOME is not set"));
        assert!(refused(None, Some("home/a")).contains("HOME is a relative path"));
    }

    #[test]
    fn parses_an_anchor_from_the_right() {
        let ok = |file: &str, line| {
            Ok(AnchorSpec {
                file: PathBuf::from(file),
                line,
            })
        };
        assert_eq!(parse_anchor("app/w.rb:42"), ok("app/w.rb", 42));
        assert_eq!(parse_anchor("app/w.rb:42:7"), ok("app/w.rb", 42));
        assert_eq!(parse_anchor("C:odd/w.rb:3"), ok("C:odd/w.rb", 3));
        for bad in ["app/w.rb", "app/w.rb:", "app/w.rb:0", ":12"] {
            assert!(parse_anchor(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_output_mode_is_read_off_argv_before_clap_parses_it() {
        let mode = |args: &[&str]| requested_output(args.iter().map(std::ffi::OsString::from));
        for (args, want) in [
            (&["x", "--json"][..], Output::Json),
            (&["x", "--ndjson"], Output::Ndjson),
            (&["x", "-ej"], Output::Json),
            (&["-Je", "x"], Output::Ndjson),
            (&["x", "--json", "-J"], Output::Ndjson),
            (&["x"], Output::Text),
            // the rest of a cluster after a value-taking flag is its value
            (&["x", "-xj"], Output::Text),
            (&["x", "-l5j"], Output::Text),
            // and after `--` nothing is a flag
            (&["--", "-j"], Output::Text),
            (&["x", "--jobs", "2"], Output::Text),
        ] {
            assert!(mode(args) == want, "{args:?}");
        }
    }

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
    fn web_url_shape() {
        assert_eq!(
            web_url("github.com/org/repo", "abc123", "src/a b#.rs", 42),
            "https://github.com/org/repo/blob/abc123/src/a%20b%23.rs#L42"
        );
    }

    #[test]
    fn wait_duration_parsing() {
        use std::time::Duration;
        // units: ms / s / m, and a bare number is seconds
        assert_eq!(parse_wait("50ms"), Ok(Duration::from_millis(50)));
        assert_eq!(parse_wait("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse_wait("1m"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_wait("250"), Ok(Duration::from_secs(250)));
        // fractions and zero
        assert_eq!(parse_wait("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_wait("0"), Ok(Duration::ZERO));
        assert!(parse_wait("0s").unwrap().is_zero());
        // surrounding whitespace is tolerated
        assert_eq!(parse_wait(" 2s "), Ok(Duration::from_secs(2)));
        // garbage, empty, and negatives are rejected (a usage error at parse time)
        assert!(parse_wait("2x").is_err());
        assert!(parse_wait("").is_err());
        assert!(parse_wait("s").is_err());
        assert!(parse_wait("-1s").is_err());
    }

    #[test]
    fn confidence_scales_by_the_share_read_in_whole_hundredths() {
        let w = |read, of| crate::search::Warming {
            read,
            of,
            interrupted: false,
            phase: None,
            phase_secs: None,
            hint: String::new(),
        };
        for (confidence, read, of, scaled) in [
            (1.0, 29, Some(100), 0.29), // not 0.28 from 28.999…
            (0.87, 1, Some(2), 0.43),
            (1.0, 7, Some(7), 1.0),
            (1.0, 0, Some(0), 0.0),
            (1.0, 5, None, 0.0), // a tree nothing counted backs none of it
        ] {
            assert_eq!(read_share_confidence(confidence, &w(read, of)), scaled);
        }
    }

    #[test]
    fn files_read_counts_in_words() {
        let w = |read, of| crate::search::Warming {
            read,
            of,
            interrupted: false,
            phase: None,
            phase_secs: None,
            hint: String::new(),
        };
        assert_eq!(read_so_far(&w(1, None)), "1 file read");
        assert_eq!(read_so_far(&w(2, None)), "2 files read");
        assert_eq!(read_so_far(&w(1, Some(2))), "1 of 2 files read");
        assert_eq!(read_so_far(&w(0, Some(1))), "0 of 1 file read");
        let finishing = crate::search::Warming {
            phase: Some(crate::store::FINISHING),
            phase_secs: Some(9),
            ..w(5, Some(8))
        };
        assert_eq!(
            read_so_far(&finishing),
            "5 of 8 files read, finishing a pass (9 s)"
        );
    }

    #[test]
    fn a_poll_that_would_end_past_the_deadline_is_not_started() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let deadline = now + Duration::from_secs(3);
        assert!(poll_fits(now, deadline, Duration::from_millis(5)));
        // a slow poll with two seconds left would overshoot by most of one
        let late = now + Duration::from_secs(1);
        assert!(!poll_fits(late, deadline, Duration::from_millis(1950)));
        assert!(!poll_fits(deadline, deadline, Duration::ZERO));
    }

    #[test]
    fn leading_kind_keyword_becomes_a_kind_filter() {
        let d = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // unquoted: `rq class Widget` — keyword + next positional is the query
        assert_eq!(
            split_kind_keyword("class".into(), d(&["Widget"])),
            (Some(&["class"][..]), "Widget".into(), vec![])
        );
        // quoted: `rq 'method zoom'` — one arg, peel the first word
        assert_eq!(
            split_kind_keyword("method zoom".into(), vec![]),
            (Some(&["method"][..]), "zoom".into(), vec![])
        );
        // `fn` is an alias for function; composes with a qualifier tail
        assert_eq!(
            split_kind_keyword("fn".into(), d(&["Foo::run"])),
            (Some(&["function"][..]), "Foo::run".into(), vec![])
        );
        // extra positionals after the query stay as rg-style path dirs
        assert_eq!(
            split_kind_keyword("struct".into(), d(&["Gadget", "src"])),
            (Some(&["struct"][..]), "Gadget".into(), d(&["src"]))
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
    fn the_branch_window_scales_with_what_the_refresh_costs() {
        // a cheap refresh keeps the default window exactly — small repos see no
        // change in behaviour at all
        assert_eq!(branch_files_ttl(Some(5)), BRANCH_FILES_TTL_SECS);
        assert_eq!(branch_files_ttl(Some(150)), BRANCH_FILES_TTL_SECS);
        // an expensive one earns a proportionally longer window: the refresh
        // runs alongside the query and competes with it for disk, so a 700ms
        // rebuild every 15s costs more than the searches it decorates
        assert_eq!(branch_files_ttl(Some(700)), 70);
        assert_eq!(branch_files_ttl(Some(2_000)), 200);
        // never indefinite — the window is the only thing that notices an
        // unstaged edit, since every git operation invalidates by stamp
        assert_eq!(branch_files_ttl(Some(60_000)), BRANCH_FILES_TTL_MAX_SECS);
        assert_eq!(branch_files_ttl(Some(u64::MAX)), BRANCH_FILES_TTL_MAX_SECS);
        // an entry written before the cost was recorded falls back to default
        assert_eq!(branch_files_ttl(None), BRANCH_FILES_TTL_SECS);
    }

    #[test]
    fn a_fruitless_batch_exits_with_what_the_caller_can_act_on() {
        let [hit, miss, retry] =
            [Verdict::Hit, Verdict::Miss, Verdict::Warming].map(Verdict::exit_code);
        let error = ExitCode::from(Failure::Database.exit_code());
        let cases = [
            (vec![miss, hit, retry], hit),
            (vec![retry, miss], retry),
            (vec![miss, retry], retry),
            (vec![miss, miss], miss),
            (vec![retry, error, miss], error),
        ];
        for (codes, want) in cases {
            assert_eq!(batch_exit(&codes), want, "{codes:?}");
        }
    }

    #[test]
    fn a_language_selects_by_prefix_or_alias() {
        // a prefix can name more than one language
        assert_eq!(canonical_langs("r"), ["ruby", "rust"]);
        assert_eq!(canonical_langs("t"), ["typescript"]);
        // the names people actually type aren't prefixes of the tag
        assert_eq!(canonical_langs("ts"), ["typescript"]);
        assert_eq!(canonical_langs("jsx"), ["javascript"]);
        assert_eq!(canonical_langs("rb"), ["ruby"]);
        assert_eq!(canonical_langs("mjs"), ["javascript"]);
        assert_eq!(canonical_langs("golang"), ["go"]);
        // an unknown value matches nothing, so the caller can reject it rather
        // than silently filtering every result away
        assert!(canonical_langs("COBOL").is_empty());
        assert!(canonical_langs("").is_empty(), "not a prefix of everything");
    }

    #[test]
    fn every_language_is_named_wherever_the_languages_are_listed() {
        let words = |text: &str| -> HashSet<String> {
            text.split(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .collect()
        };
        let help = Cli::command().get_long_about().unwrap().to_string();
        let places = [
            ("--help", help.as_str()),
            ("README.md", include_str!("../../README.md")),
            (
                "claude/rq-skill.md",
                include_str!("../../claude/rq-skill.md"),
            ),
        ];
        for (place, text) in places {
            let words = words(text);
            for language in crate::lang::languages() {
                assert!(words.contains(language), "{place} doesn't name {language}");
            }
        }
    }

    #[test]
    fn a_kind_normalizes_language_specific_spellings() {
        assert_eq!(canonical_kind("f"), Some(&["function"][..]));
        // TypeScript's spellings land on the shared model's kinds
        assert_eq!(canonical_kind("interface"), Some(&["trait"][..]));
        // `type` is any named type; `alias` only the alias
        assert_eq!(canonical_kind("type"), Some(&["type", "struct"][..]));
        assert_eq!(canonical_kind("alias"), Some(&["type"][..]));
        assert_eq!(canonical_kind("member"), Some(&["variant"][..]));
        assert_eq!(canonical_kind("property"), Some(&["field"][..]));
        assert_eq!(canonical_kind("macro"), Some(&["macro"][..]));
        assert_eq!(canonical_kind("const"), Some(&["constant"][..]));
        assert_eq!(canonical_kind("banana"), None);
    }

    #[test]
    fn every_kind_is_reachable_by_its_name() {
        for kind in Kind::ALL {
            let tag = kind.as_str();
            assert_eq!(Kind::from_tag(tag), Some(kind));
            for selects in [canonical_kind(tag), keyword_kind(tag)] {
                assert!(
                    selects.is_some_and(|s| s.contains(&tag)),
                    "{tag}: {selects:?}"
                );
            }
        }
        // …and work as the leading-keyword shorthand too
        let d = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            split_kind_keyword("interface".into(), d(&["Renderer"])),
            (Some(&["trait"][..]), "Renderer".into(), vec![])
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
    fn signature_in_reads_one_based_trimmed_nonblank_lines() {
        let lines = ["class Widget", "", "  def go"];
        assert_eq!(signature_in(&lines, 1).as_deref(), Some("class Widget"));
        assert_eq!(signature_in(&lines, 3).as_deref(), Some("def go"));
        assert_eq!(signature_in(&lines, 2), None, "blank line");
        assert_eq!(signature_in(&lines, 0), None, "lines are 1-based");
        assert_eq!(signature_in(&lines, 4), None, "past the end");
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

    #[test]
    fn here_is_the_checkout_root_and_warms_only_git_or_tracked() {
        let base = std::env::temp_dir().join(format!("rq-here-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (repo, plain) = (base.join("repo"), base.join("plain"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("a.rb"), "class Widget; end\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();

        let here = Here::at(&store, &repo.join("sub"));
        assert_eq!(
            here.root,
            repo.canonicalize().unwrap(),
            "a subdir keys its repo"
        );
        assert!(here.is_git && here.warms());

        let here = Here::at(&store, &plain);
        assert_eq!(here.root, plain.canonicalize().unwrap());
        assert!(!here.warms(), "an unknown non-git dir is never walked");

        crate::index::index_path(&mut store, &plain).unwrap();
        let here = Here::at(&store, &plain);
        assert!(here.warms(), "an indexed one is tracked");
        assert!(here.checkout.is_some());
        let _ = std::fs::remove_dir_all(&base);
    }
}
