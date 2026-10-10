//! `rq <query>` and batch mode: warm, poll, rank, and settle on an answer.

use super::*;

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
        status: verdict,
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
pub(super) struct SearchArgs<'a> {
    pub(super) query: &'a str,
    pub(super) explain: bool,
    pub(super) out: Output,
    pub(super) paths: &'a [String],
    pub(super) kinds: &'a [String],
    pub(super) langs: &'a [String],
    /// Number of results to show (`--limit`).
    pub(super) want: usize,
    /// Answer from the committed index without blocking on a (re)index (`--no-wait`).
    pub(super) no_wait: bool,
    /// Cap on how long to wait for the index to warm (`--wait`); `None` = the
    /// default/`RQ_WAIT_BUDGET_MS` budget.
    pub(super) wait: Option<Duration>,
    pub(super) open: bool,
    pub(super) web: bool,
    pub(super) all_repos: bool,
    /// One of several queries sharing a run, so each row says which query it
    /// answers — a single stream serving many questions is otherwise
    /// unattributable.
    pub(super) batch: bool,
    pub(super) show: bool,
    /// Asked from a position (`--anchor`); the anchor itself lives on the session.
    pub(super) anchored: bool,
}

/// One non-blank line of a batch's stdin.
#[derive(Debug, PartialEq)]
enum BatchLine {
    Query(String),
    /// Its 1-based line number: there's no query to echo back.
    NotUtf8(usize),
}

/// The batch's lines as they arrive, trimmed, blanks dropped. A line that
/// isn't UTF-8 is its own item rather than the end of the stream; a read
/// error ends it.
fn batch_lines(input: impl std::io::BufRead) -> impl Iterator<Item = BatchLine> {
    input
        .split(b'\n')
        .map_while(std::result::Result::ok)
        .enumerate()
        .filter_map(|(i, bytes)| match String::from_utf8(bytes) {
            Ok(line) => {
                let line = line.trim();
                (!line.is_empty()).then(|| BatchLine::Query(line.to_string()))
            }
            Err(_) => Some(BatchLine::NotUtf8(i + 1)),
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
pub(super) fn cmd_batch(
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

    // Read as each line arrives, not to EOF: a caller holding the pipe open
    // for its next question gets each answer before it asks.
    let mut lines = batch_lines(std::io::stdin().lock()).peekable();
    // Nothing on stdin isn't a batch — it's a bare invocation that happens to
    // run without a terminal (a script, a test harness, stdin from /dev/null).
    // Treat it the way `rq` with no arguments is always treated.
    if lines.peek().is_none() {
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
        crate::trace!("batch: warming the index before the first answer");
        let active = &session.active_paths;
        let _ = crate::index::index_budgeted(&mut session.store, &here.root, active, budget, None);
        here.refresh(&session.store);
        // the index has just caught up with the worktree
        session.moved = Some(false);
    }

    // The batch ran, and each line's `status` carries its query's outcome, so
    // only a wholly fruitless batch reports failure — with the outcome the
    // caller can act on most (see `Outcome`'s order).
    let outcome = lines
        .map(|line| {
            let query = match line {
                BatchLine::Query(query) => query,
                BatchLine::NotUtf8(n) => {
                    return report(
                        out,
                        Failure::Usage,
                        format_args!("rq: stdin line {n} isn't UTF-8; skipped"),
                    )
                    .into();
                }
            };
            cmd_search(
                &mut session,
                &SearchArgs {
                    query: &query,
                    explain: cli.explain,
                    out,
                    paths,
                    kinds,
                    langs,
                    want: requested_limit(cli.limit),
                    // The warm happened above, once, before the first answer.
                    // Per-query warming would undo the point of batching.
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
        .max()
        .expect("a batch has a query");
    ExitCode::from(outcome)
}

pub(super) fn cmd_search(session: &mut Session, args: &SearchArgs) -> Outcome {
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
        anchor: anchor.as_ref().map(|a| a.for_query(query)),
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
                return report(out, Failure::Database, format_args!("rq: {e}")).into();
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
        hits = crate::search::merge(hits, tail, rank_limit, &ctx);
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
        && let Some(shown) = show_top_definition(&mut hits, query, out, ranked)
    {
        if out == Output::Text
            && let Some(note) = hits.first().and_then(|h| warming_note(h, here.as_deref()))
        {
            eprintln!("{note}");
        }
        return shown.map_or_else(Outcome::from, |()| Outcome::Hit);
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
        return finish_open(&hits, root, web).map_or_else(Outcome::from, |()| Outcome::Hit);
    }

    if let Some(by) = held_back.as_deref() {
        let warming = warming_state(
            store,
            by,
            Some(by) == here.as_deref(),
            Some(by) == continuing,
        );
        if let Err(failed) = emit_provisional(args, &hits, warming.as_ref(), by, here.as_deref()) {
            return failed.into();
        }
    } else if let Err(failed) = render_hits(args, &hits, show) {
        return failed.into();
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
    verdict.into()
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
pub(super) fn poll_fits(
    now: std::time::Instant,
    deadline: std::time::Instant,
    cost: Duration,
) -> bool {
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
pub(super) fn tree_span(store: &Store, root: &str, checkout: i64) -> Option<i64> {
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
pub(super) fn read_share_confidence(confidence: f64, w: &crate::search::Warming) -> f64 {
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
pub(super) fn read_so_far(w: &crate::search::Warming) -> String {
    let files = |n| if n == 1 { "file" } else { "files" };
    let read = match w.of {
        Some(of) => format!("{} of {of} {} read", w.read, files(of)),
        None => format!("{} {} read", w.read, files(w.read)),
    };
    format!("{read}{}", finishing_note(w.phase, w.phase_secs))
}

/// ", finishing a pass (N s)" while a pass is past its reads.
pub(super) fn finishing_note(phase: Option<&str>, secs: Option<i64>) -> String {
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
) -> Result<(), Failure> {
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
                status: Verdict::Warming.status(),
                warming,
            };
            emit_json(args.out, &obj)
        }
        Output::Text => {
            render_hits(args, hits, false)?;
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
            Ok(())
        }
    }
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
) -> Outcome {
    let incomplete = verdict == Verdict::Warming;
    // a miss's reason, where structured callers can act on more than the verdict
    let status = if interrupted {
        "interrupted"
    } else if !incomplete && elsewhere.is_some() {
        "scope_not_found"
    } else {
        verdict.status()
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
    verdict.into()
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

/// Whether a repo-relative `file` sits under one of the `--path` directories
/// (prefix match on a path boundary). `app/services` matches
/// `app/services/refund.rb` but not `app/services_old/x.rb`.
fn under_any(file: &str, paths: &[String]) -> bool {
    paths.iter().any(|p| {
        let p = p.trim_start_matches("./").trim_end_matches('/');
        p.is_empty() || file == p || file.starts_with(&format!("{p}/"))
    })
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

#[cfg(test)]
mod tests {
    use super::{BatchLine, batch_lines};

    #[test]
    fn batch_lines_keep_going_past_a_line_that_is_not_utf8() {
        use BatchLine::{NotUtf8, Query};
        let q = |s: &str| Query(s.to_string());
        for (input, want) in [
            (&b"a\nb\n"[..], vec![q("a"), q("b")]),
            (b"a\r\n  \n\tb  ", vec![q("a"), q("b")]),
            (b"a\n\xff\nb\n", vec![q("a"), NotUtf8(2), q("b")]),
            (b"\n\n\xc3\n", vec![NotUtf8(3)]),
            (b"caf\xc3\xa9\n", vec![q("café")]),
            (b"", vec![]),
        ] {
            assert_eq!(batch_lines(input).collect::<Vec<_>>(), want, "{input:?}");
        }
    }
}
