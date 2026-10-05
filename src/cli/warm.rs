//! Keeping the index fresh: the detached `rq --warm` child, the worktree-moved check, and the wait budgets.

use super::*;

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
pub(super) fn spawn_detached_warm(root: &std::path::Path) {
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
pub(super) fn cmd_warm(path: Option<&str>) -> ExitCode {
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
        Err(_) => return Failure::Database.into(),
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
pub(super) fn worktree_moved(store: &Store, root: &std::path::Path, head: Option<&str>) -> bool {
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
pub(super) struct Staleness(pub(super) PathBuf, pub(super) Option<String>);

/// Settle warming once the answer is out: resolve the staleness check,
/// reindex if the worktree moved, and hand any remainder to a detached child.
///
/// Called from *both* exits. The miss path matters as much as the render one —
/// a symbol added a moment ago is precisely a miss, and reindexing before we
/// exit is what makes the immediate retry hit. `hit` says which exit this is;
/// `moved` is the session's memo of the answer.
pub(super) fn settle_warm(
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
pub(super) fn answer_warm_budget() -> Duration {
    env_budget("RQ_ANSWER_BUDGET_MS", 500)
}

/// What `--symbols` adds to the answer budget for its synchronous warm: an
/// outline has no answer to get out of the way of.
pub(super) fn deferred_warm_budget() -> Duration {
    env_budget("RQ_DEFERRED_BUDGET_MS", 250)
}

/// Bound for the git-repo live-scan fallback (index empty, still warming): enough
/// to surface a result the warm hasn't reached, without an unbounded walk.
pub(super) fn live_fallback_budget() -> Duration {
    env_budget("RQ_FALLBACK_BUDGET_MS", 250)
}

/// How long a pass nobody is waiting on — `rq --index`, a warm child — waits
/// out another writer, each time: a cold pass's end rebuilds the name index
/// in one transaction, which on a large repo outlasts a search's busy timeout.
pub(super) fn writer_wait() -> Duration {
    env_budget("RQ_WRITER_WAIT_MS", 30_000)
}

/// Tell the human at the terminal, once, why `rq --index` isn't moving.
pub(super) fn say_waiting() {
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
pub(super) fn warm_detach_enabled() -> bool {
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
pub(super) fn wait_budget() -> Duration {
    env_budget("RQ_WAIT_BUDGET_MS", 60_000)
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
