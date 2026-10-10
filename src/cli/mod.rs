//! Command-line surface. Search is the default action: `rq <query>`.

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::core::{Kind, now_unix};
use crate::search::Verdict;
use crate::store::{Checkout, Coverage, Store};

mod admin;
mod args;
mod open;
mod output;
mod query;
mod render;
mod session;
mod symbols;
mod warm;

use admin::*;
use args::*;
use open::*;
use output::*;
use query::*;
use render::*;
use session::*;
use symbols::*;
use warm::*;

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

    /// Prefer definitions near this line: its scopes, its file, code it can reach.
    ///
    /// Pass an editor's cursor, or the file an agent is reading. Definitions in
    /// the scopes around that line come first, then ones in the same file and
    /// nearby directories, and ones in a language that file can refer to (TS
    /// and JS reach each other) rank above ones it can't. In TS and JS, the
    /// definition the file's imports resolve the name to ranks first (relative
    /// imports and the checkout's own workspace packages). Asked from inside a
    /// test or example tree, that tree's own definitions aren't held back as
    /// test or example code (a private one only where the anchor can reach it:
    /// its own file in TS and JS, its directory elsewhere). It's context, not a
    /// filter. FILE is relative to the current directory; COL is accepted and
    /// ignored.
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
    // A reader that stops early (`rq --status | head -1`) ends rq the way it
    // ends `cat`, rather than `println!` panicking over the broken pipe. Rust
    // starts every program ignoring SIGPIPE, so a write would fail instead.
    // SAFETY: called first, before any thread exists; SIG_DFL is a plain value.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
            ExitCode::from(cmd_search(
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
            ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::Scratch;

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
        let (hit, miss, retry) = (Outcome::Hit, Outcome::Miss, Outcome::Warming);
        let error = Outcome::from(Failure::Database);
        let cases = [
            (vec![miss, hit, retry], hit),
            (vec![retry, miss], retry),
            (vec![miss, retry], retry),
            (vec![miss, miss], miss),
            (vec![retry, error, miss], error),
        ];
        for (outcomes, want) in cases {
            assert_eq!(outcomes.iter().max(), Some(&want), "{outcomes:?}");
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
        let base = Scratch::new("here");
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
    }

    #[test]
    fn every_mode_is_run_with_json() {
        // The modes `every_mode_answers_json` and `json_shapes` run with
        // --json. `--warm` is the detached child (no output); `--completions`
        // prints a shell script. A new mode fails here until it's added there.
        const RUN_WITH_JSON: [&str; 5] = ["drop", "index", "status", "symbols", "usage"];
        const NO_OUTPUT: [&str; 2] = ["completions", "warm"];
        let cmd = Cli::command();
        let group = cmd
            .get_groups()
            .find(|g| g.get_id() == "mode")
            .expect("the exclusive mode group");
        let mut modes: Vec<&str> = group.get_args().map(|a| a.as_str()).collect();
        modes.sort_unstable();
        let mut covered: Vec<&str> = RUN_WITH_JSON.into_iter().chain(NO_OUTPUT).collect();
        covered.sort_unstable();
        assert_eq!(
            covered, modes,
            "a mode without a --json run in the e2e tests"
        );
    }
}
