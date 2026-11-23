//! Holder marks: who is working on a checkout, as `meta` rows (D52, D54, D64).
//!
//! - `pass:<root>:<pid>` = `<stamp>`: a pass is filling the checkout, renewed
//!   as it writes.
//! - `phase:<root>:<pid>` = `<phase>:<since>`: what that pass is doing.
//! - `warm_lock:<root>` = `<pid>:<stamp>`: a warm child, held across its
//!   passes.
//! - `span:<root>` = `<files>`: how many files the tree spans. Not a holder.
//!
//! Older rq binaries share the store and parse these strings (D54), so the
//! encoding is fixed; this module is its one reader and writer.
//!
//! A pass mark or warm lock is held while its pid runs and its stamp is inside
//! [`WARM_LOCK_TTL_SECS`]: past it, the mark is a crashed process's and its pid
//! may be reused. A phase counts only beside a held mark. The name index's
//! suspended build (`names.rs`) is the one holder kept elsewhere, judged by its
//! pid alone: it has no stamp.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Result, Store, WARM_LOCK_TTL_SECS, pid_alive, prefix_upper_bound};
use crate::core::now_unix;

/// What a pass is doing (D54): [`Reading`](Phase::Reading) from its start,
/// [`Finishing`](Phase::Finishing) once past its reads, when what remains (the
/// name index, commit times) holds `read` still.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Reading,
    Finishing,
}

impl Phase {
    /// As a `phase:` row stores it and `warming.phase` names it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Phase::Reading => "reading",
            Phase::Finishing => "finishing",
        }
    }

    fn parse(s: &str) -> Option<Phase> {
        [Phase::Reading, Phase::Finishing]
            .into_iter()
            .find(|p| p.as_str() == s)
    }
}

const PASS: &str = "pass:";
const PHASE: &str = "phase:";
const WARM_LOCK: &str = "warm_lock:";

fn pass_key(root: &str, pid: u32) -> String {
    format!("{PASS}{root}:{pid}")
}

fn phase_key(root: &str, pid: u32) -> String {
    format!("{PHASE}{root}:{pid}")
}

pub(super) fn warm_lock_key(root: &str) -> String {
    format!("{WARM_LOCK}{root}")
}

pub(super) fn span_key(root: &str) -> String {
    format!("span:{root}")
}

/// The start every `kind` mark on `root` shares; a longer root may share it
/// too (`/a` and `/a:b`), so a match is checked by [`root_and_pid`].
fn marks_on(kind: &str, root: &str) -> String {
    format!("{kind}{root}:")
}

/// The root and pid a `pass:` or `phase:` key names. The pid is last, so a
/// root holding a colon parses whole.
fn root_and_pid<'k>(kind: &str, key: &'k str) -> Option<(&'k str, u32)> {
    let (root, pid) = key.strip_prefix(kind)?.rsplit_once(':')?;
    Some((root, pid.parse().ok()?))
}

/// A process holding a checkout: a pass mark or the warm lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Holder {
    pub pid: u32,
    /// When it last stamped its mark (unix seconds).
    pub stamp: i64,
}

impl Holder {
    /// The holder a `pass:` or `warm_lock:` row records, with the root it
    /// holds; `None` for any other row or a garbled one.
    fn parse<'k>(key: &'k str, value: &str) -> Option<(&'k str, Holder)> {
        let (root, pid, stamp) = match root_and_pid(PASS, key) {
            Some((root, pid)) => (root, pid, value),
            None => {
                let root = key.strip_prefix(WARM_LOCK)?;
                let (pid, stamp) = value.split_once(':')?;
                (root, pid.parse().ok()?, stamp)
            }
        };
        let stamp = stamp.parse().ok()?;
        Some((root, Holder { pid, stamp }))
    }

    /// Whether it still holds: its pid runs and its stamp is inside the TTL.
    pub(super) fn live(self) -> bool {
        pid_alive(i64::from(self.pid)) && now_unix() - self.stamp < WARM_LOCK_TTL_SECS
    }
}

/// Whoever the warm lock on `root` records, held or not; `None` for no lock
/// or a garbled one.
fn warm_lock_holder(conn: &Connection, root: &str) -> Result<Option<Holder>> {
    let key = warm_lock_key(root);
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(value.and_then(|v| Holder::parse(&key, &v).map(|(_, h)| h)))
}

fn put(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .map(drop)
}

/// Every `meta` row whose key starts with `prefix`: a range on the key, which
/// seeks the primary key and, unlike `substr`, measures nothing in characters.
fn rows_under(conn: &Connection, prefix: &str) -> Result<Vec<(String, String)>> {
    conn.prepare_cached("SELECT key, value FROM meta WHERE key >= ?1 AND key < ?2")?
        .query_map(params![prefix, prefix_upper_bound(prefix)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect()
}

/// Forget every pass and phase mark on `root`, live or not: its checkout is
/// going.
pub(super) fn forget_marks(conn: &Connection, root: &str) -> Result<()> {
    for kind in [PASS, PHASE] {
        for (key, _) in rows_under(conn, &marks_on(kind, root))? {
            if root_and_pid(kind, &key).is_some_and(|(r, _)| r == root) {
                conn.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
            }
        }
    }
    Ok(())
}

/// Clear the pass marks on `root` no process holds — crashed passes' — and
/// every phase not beside a held one, sparing `pid`'s own: a holder arriving
/// sweeps, so the marks never outgrow the live writers.
fn sweep_dead_marks(conn: &Connection, root: &str, pid: u32) -> Result<()> {
    let mut held = Vec::new();
    let mut dead = Vec::new();
    for (key, value) in rows_under(conn, &marks_on(PASS, root))? {
        if let Some((r, h)) = Holder::parse(&key, &value)
            && r == root
            && h.pid != pid
        {
            if h.live() { &mut held } else { &mut dead }.push(h.pid);
        }
    }
    dead.extend(
        rows_under(conn, &marks_on(PHASE, root))?
            .iter()
            .filter_map(|(k, _)| root_and_pid(PHASE, k))
            .filter(|&(r, p)| r == root && p != pid && !held.contains(&p))
            .map(|(_, p)| p),
    );
    for p in dead {
        conn.execute(
            "DELETE FROM meta WHERE key IN (?1, ?2)",
            params![pass_key(root, p), phase_key(root, p)],
        )?;
    }
    Ok(())
}

/// This process's pass mark on a checkout, held while the pass runs, and the
/// store it marks, borrowed for the pass to write through. [`end`](Self::end)
/// records how the pass left the tree; dropped without it — a pass that
/// failed — the mark goes anyway, so a long-lived process doesn't hold the
/// checkout for the TTL after its pass is gone. The span stays: it is still
/// the tree's count, read only while the checkout is incomplete, and without
/// it the next search there would count the tree again.
#[must_use = "dropping it ends the mark"]
pub(crate) struct PassMark<'s> {
    store: &'s mut Store,
    root: String,
    ended: bool,
}

impl PassMark<'_> {
    /// End the pass, recording the tree's span as it leaves it (see
    /// [`Store::end_pass`]).
    pub(crate) fn end(mut self, span: Option<usize>) -> Result<()> {
        self.store.end_pass(&self.root, std::process::id(), span)?;
        self.ended = true;
        Ok(())
    }
}

impl std::ops::Deref for PassMark<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.store
    }
}

impl std::ops::DerefMut for PassMark<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        self.store
    }
}

impl Drop for PassMark<'_> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        // best-effort and without waiting: a writer holding the lock past this
        // leaves the mark to the pid's next pass, its exit, or the TTL
        let pid = std::process::id();
        let store = &*self.store;
        let _ = store.waiting_at_most(std::time::Duration::ZERO, || {
            store.conn.execute(
                "DELETE FROM meta WHERE key IN (?1, ?2)",
                params![pass_key(&self.root, pid), phase_key(&self.root, pid)],
            )
        });
    }
}

impl Store {
    /// Mark a pass of this process's over the checkout at `root`, as
    /// [`begin_pass`](Self::begin_pass) does, until the mark is ended or
    /// dropped.
    pub(crate) fn mark_pass(&mut self, root: &str, span: Option<usize>) -> Result<PassMark<'_>> {
        self.begin_pass(root, std::process::id(), span)?;
        Ok(PassMark {
            store: self,
            root: root.to_string(),
            ended: false,
        })
    }

    /// Mark a pass over the checkout at `root` as running in `pid`, and record
    /// how many source files the tree spans when the pass enumerated them.
    /// Marks no process holds are swept first.
    pub(super) fn begin_pass(&mut self, root: &str, pid: u32, span: Option<usize>) -> Result<()> {
        // it reads before it writes: deferred, a busy writer fails it at once
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        sweep_dead_marks(&tx, root, pid)?;
        let now = now_unix();
        put(&tx, &pass_key(root, pid), &now.to_string())?;
        let reading = Phase::Reading.as_str();
        put(&tx, &phase_key(root, pid), &format!("{reading}:{now}"))?;
        if let Some(span) = span {
            put(&tx, &span_key(root), &span.to_string())?;
        }
        tx.commit()
    }

    /// Record how many source files the tree at `root` spans, as a pass's
    /// enumeration counted them.
    pub(crate) fn set_span(&self, root: &str, span: usize) -> Result<()> {
        put(&self.conn, &span_key(root), &span.to_string())
    }

    /// Restamp `pid`'s mark from [`begin_pass`](Self::begin_pass), so a pass
    /// that runs past the TTL isn't read as a crashed one.
    pub(crate) fn renew_pass(&self, root: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = ?1",
            params![pass_key(root, pid), now_unix().to_string()],
        )?;
        Ok(())
    }

    /// Record what `pid`'s pass over the checkout at `root` is doing.
    pub(crate) fn set_pass_phase(&self, root: &str, pid: u32, phase: Phase) -> Result<()> {
        let value = format!("{}:{}", phase.as_str(), now_unix());
        put(&self.conn, &phase_key(root, pid), &value)
    }

    /// What the passes indexing the checkout at `root` are doing, and since
    /// when (unix seconds): `reading` while any is, as `read` then moves.
    /// `None` when no live pass records one — an older rq's records none.
    pub(crate) fn pass_phase(&self, root: &str) -> Option<(&'static str, i64)> {
        let (pids, _) = self.passes(root).ok()?;
        pids.into_iter()
            .filter_map(|p| self.meta_get(&phase_key(root, p)).ok().flatten())
            .filter_map(|v| {
                let (phase, since) = v.split_once(':')?;
                Some((Phase::parse(phase)?, since.parse().ok()?))
            })
            .max_by_key(|&(phase, since)| (phase == Phase::Reading, since))
            .map(|(phase, since)| (phase.as_str(), since))
    }

    /// Clear `pid`'s mark from [`begin_pass`](Self::begin_pass), and record
    /// the tree's span as the pass leaves it, or forget it (a complete
    /// checkout discloses none).
    pub(super) fn end_pass(&self, root: &str, pid: u32, span: Option<usize>) -> Result<()> {
        self.conn.execute(
            "DELETE FROM meta WHERE key IN (?1, ?2)",
            params![pass_key(root, pid), phase_key(root, pid)],
        )?;
        match span {
            Some(span) => put(&self.conn, &span_key(root), &span.to_string()),
            None => {
                self.conn
                    .execute("DELETE FROM meta WHERE key = ?1", params![span_key(root)])?;
                Ok(())
            }
        }
    }

    /// The live processes holding the checkout at `root` — this one too —
    /// and the files its tree spanned when last enumerated. A warm child
    /// holds it by its lock, across the passes it runs, not only during one.
    pub(crate) fn passes(&self, root: &str) -> Result<(Vec<u32>, Option<i64>)> {
        let lock = warm_lock_key(root);
        let lock = self.meta_get(&lock)?.map(|v| (lock, v));
        let mut pids: Vec<u32> = rows_under(&self.conn, &marks_on(PASS, root))?
            .into_iter()
            .chain(lock)
            .filter_map(|(k, v)| {
                let (r, h) = Holder::parse(&k, &v)?;
                (r == root && h.live()).then_some(h.pid)
            })
            .collect();
        pids.sort_unstable();
        pids.dedup();
        let span = self.meta_get(&span_key(root))?.and_then(|v| v.parse().ok());
        Ok((pids, span))
    }

    /// Whether a live process other than this one holds the checkout at
    /// `root`.
    pub(crate) fn indexed_by_others(&self, root: &str) -> bool {
        !self.other_indexers(root).is_empty()
    }

    /// The live processes other than this one holding the checkout at `root`.
    pub(crate) fn other_indexers(&self, root: &str) -> Vec<u32> {
        let me = std::process::id();
        let (mut pids, _) = self.passes(root).unwrap_or_default();
        pids.retain(|&p| p != me);
        pids
    }

    /// Whether a live rq other than this one holds any checkout, as
    /// [`other_indexers`](Self::other_indexers) judges one. Its writes would
    /// reuse the free pages a cleanup would hand back.
    pub(crate) fn indexing_anywhere(&self) -> Result<bool> {
        let me = std::process::id();
        let mut rows = rows_under(&self.conn, PASS)?;
        rows.extend(rows_under(&self.conn, WARM_LOCK)?);
        Ok(rows
            .iter()
            .filter_map(|(k, v)| Holder::parse(k, v))
            .any(|(_, h)| h.pid != me && h.live()))
    }

    /// Claim the detached-warm lock for `pid` unless another live process
    /// holds it, sweeping the marks no process holds as a pass does: a pass
    /// on a complete checkout may take none. Read and write share one write
    /// transaction, so of two warmers racing for it only one wins.
    pub(crate) fn claim_warm_lock(&mut self, root: &str, pid: u32) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if warm_lock_holder(&tx, root)?.is_some_and(|h| h.pid != pid && h.live()) {
            return Ok(false);
        }
        sweep_dead_marks(&tx, root, pid)?;
        put(&tx, &warm_lock_key(root), &format!("{pid}:{}", now_unix()))?;
        tx.commit()?;
        Ok(true)
    }

    /// Whether `pid` still holds the detached-warm lock: a holder suspended
    /// past the TTL (a laptop asleep) may wake to find it taken over.
    pub(crate) fn holds_warm_lock(&self, root: &str, pid: u32) -> Result<bool> {
        Ok(warm_lock_holder(&self.conn, root)?.is_some_and(|h| h.pid == pid))
    }

    /// Release `pid`'s detached-warm lock, and only its: one taken over while
    /// `pid` was suspended is its successor's.
    pub(crate) fn clear_warm_lock(&mut self, root: &str, pid: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if warm_lock_holder(&tx, root)?.is_some_and(|h| h.pid == pid) {
            tx.execute(
                "DELETE FROM meta WHERE key = ?1",
                params![warm_lock_key(root)],
            )?;
        }
        tx.commit()
    }

    /// Leave the mark a pass in `pid` that crashed would.
    #[cfg(test)]
    pub(crate) fn crashed_pass(&mut self, root: &str, pid: u32) {
        self.begin_pass(root, pid, None).unwrap();
    }

    /// Every pid with a pass mark on `root`, held or not.
    #[cfg(test)]
    pub(crate) fn marked_pids(&self, root: &str) -> Vec<u32> {
        let mut pids: Vec<u32> = rows_under(&self.conn, &marks_on(PASS, root))
            .unwrap()
            .iter()
            .filter_map(|(k, _)| root_and_pid(PASS, k))
            .filter_map(|(r, p)| (r == root).then_some(p))
            .collect();
        pids.sort_unstable();
        pids
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{Holder, phase_key};
    use crate::tests::support::Scratch;

    /// Above any pid the OS hands out.
    const DEAD: u32 = 4_000_000;

    /// Another live process: the test runner's parent.
    fn other() -> u32 {
        std::os::unix::process::parent_id()
    }

    fn stale() -> i64 {
        now_unix() - WARM_LOCK_TTL_SECS - 1
    }

    /// Live processes of the test's own, gone with it.
    struct Sleepers(Vec<std::process::Child>);

    impl Sleepers {
        fn new(n: usize) -> Self {
            let spawn = || {
                std::process::Command::new("sleep")
                    .arg("60")
                    .spawn()
                    .unwrap()
            };
            Sleepers((0..n).map(|_| spawn()).collect())
        }

        fn pids(&self) -> Vec<u32> {
            self.0.iter().map(std::process::Child::id).collect()
        }
    }

    impl Drop for Sleepers {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn a_holder_holds_while_its_pid_runs_inside_the_ttl() {
        let me = std::process::id();
        let fresh = now_unix();
        let stale = fresh - WARM_LOCK_TTL_SECS;
        // pid 1 is another user's: kill() says EPERM, which is still alive
        let cases = [
            (me, fresh, true),
            (1, fresh, true),
            (me, stale, false),
            (0, fresh, false),
            (u32::MAX, fresh, false),
        ];
        for (pid, stamp, held) in cases {
            assert_eq!(Holder { pid, stamp }.live(), held, "pid {pid} at {stamp}");
        }
    }

    #[test]
    fn one_of_several_racing_warmers_claims_the_lock() {
        let dir = Scratch::new("warm-lock");
        let path = dir.join("rq.db");
        drop(Store::open(&path).unwrap());
        // every racer is live, so only an empty lock is claimable
        let racers = Sleepers::new(8);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let claims: Vec<_> = racers
            .pids()
            .into_iter()
            .map(|pid| {
                let (path, start) = (path.clone(), std::sync::Arc::clone(&start));
                std::thread::spawn(move || {
                    let mut store = Store::open(&path).unwrap();
                    start.wait();
                    store.claim_warm_lock("repo", pid).unwrap()
                })
            })
            .collect();
        let won = claims
            .into_iter()
            .filter_map(|h| h.join().unwrap().then_some(()))
            .count();
        assert_eq!(won, 1);

        // a holder gone (dead pid, stale stamp) is taken over; a live one isn't
        let mut store = Store::open(&path).unwrap();
        let (me, p) = (std::process::id(), other());
        let lock = |store: &Store, v: String| store.meta_set("warm_lock:repo", &v).unwrap();
        lock(&store, format!("{DEAD}:{}", now_unix()));
        assert!(store.claim_warm_lock("repo", p).unwrap());
        lock(&store, format!("{p}:{}", stale()));
        assert!(store.claim_warm_lock("repo", me).unwrap());
        assert!(!store.claim_warm_lock("repo", p).unwrap());
        assert!(store.claim_warm_lock("repo", me).unwrap(), "its own");
    }

    #[test]
    fn a_warm_child_clears_only_its_own_lock() {
        // woken past the TTL, a warm child finds its lock a successor's
        let me = std::process::id();
        let now = now_unix();
        for root in ["/r", "/x/café", "/a:b"] {
            let mut store = Store::open_in_memory().unwrap();
            let key = format!("warm_lock:{root}");
            assert!(store.claim_warm_lock(root, me).unwrap());
            assert!(store.holds_warm_lock(root, me).unwrap());
            // a pid this one's digits start is another process
            for successor in [format!("{}:{now}", other()), format!("{me}0:{now}")] {
                store.meta_set(&key, &successor).unwrap();
                assert!(!store.holds_warm_lock(root, me).unwrap(), "{root}");
                store.clear_warm_lock(root, me).unwrap();
                assert_eq!(store.meta_get(&key).unwrap(), Some(successor), "{root}");
            }
            store.meta_set(&key, "garbled").unwrap();
            store.clear_warm_lock(root, me).unwrap();
            assert!(store.meta_get(&key).unwrap().is_some(), "not its to clear");
            // its own, even past the TTL
            store.meta_set(&key, &format!("{me}:{}", stale())).unwrap();
            assert!(store.holds_warm_lock(root, me).unwrap());
            store.clear_warm_lock(root, me).unwrap();
            assert!(store.meta_get(&key).unwrap().is_none(), "{root}");
        }
    }

    #[test]
    fn a_warm_child_sweeps_the_marks_no_process_holds() {
        // a pass on a complete checkout may take no mark, so the warm child
        // is what arrives after a crashed refresh
        let mut store = Store::open_in_memory().unwrap();
        let (me, p) = (std::process::id(), other());
        store.crashed_pass("/r", DEAD);
        store.begin_pass("/r", p, None).unwrap();
        store.begin_pass("/r:1", DEAD, None).unwrap();
        // a phase whose pass mark is gone, its pid reused
        let orphan = phase_key("/r", 1);
        store.meta_set(&orphan, "reading:0").unwrap();
        assert!(store.claim_warm_lock("/r", me).unwrap());
        assert_eq!(store.marked_pids("/r"), vec![p], "the live pass stays");
        assert!(store.meta_get(&phase_key("/r", DEAD)).unwrap().is_none());
        assert!(store.meta_get(&phase_key("/r", p)).unwrap().is_some());
        assert!(store.meta_get(&orphan).unwrap().is_none());
        assert_eq!(store.marked_pids("/r:1"), vec![DEAD], "another root's");

        // a live lock is still respected
        store.clear_warm_lock("/r", me).unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", now_unix()))
            .unwrap();
        assert!(!store.claim_warm_lock("/r", me).unwrap());
    }

    #[test]
    fn a_dropped_mark_leaves_the_span() {
        // still the tree's count, which a search would otherwise redo
        let mut store = Store::open_in_memory().unwrap();
        drop(store.mark_pass("/r", Some(3)).unwrap());
        assert_eq!(store.passes("/r").unwrap(), (vec![], Some(3)));
    }

    #[test]
    fn a_pass_mark_waits_out_a_concurrent_writer() {
        // read-then-write in a deferred transaction can't wait for the lock:
        // SQLite fails the upgrade with SQLITE_BUSY, busy_timeout or not
        let dir = Scratch::new("busy-pass");
        let path = dir.join("rq.db");
        let mut store = Store::open(&path).unwrap();
        let other_conn = Store::open(&path).unwrap();
        other_conn
            .conn
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO meta VALUES ('k', 'v');")
            .unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            other_conn.conn.execute_batch("COMMIT").unwrap();
        });
        let marked = store.begin_pass("/repo", other(), Some(3));
        holder.join().unwrap();
        marked.unwrap();
        assert_eq!(store.passes("/repo").unwrap(), (vec![other()], Some(3)));
    }

    #[test]
    fn a_pass_clears_the_marks_of_passes_that_died() {
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/repo", DEAD, Some(10)).unwrap();
        store.begin_pass("/repo", other(), None).unwrap();
        assert_eq!(
            store.marked_pids("/repo"),
            vec![other()],
            "the dead pass's mark is gone"
        );
        assert_eq!(
            store.passes("/repo").unwrap(),
            (vec![other()], Some(10)),
            "a pass that didn't count keeps the last span"
        );

        let me = std::process::id();
        store.end_pass("/repo", other(), Some(12)).unwrap();
        store.claim_warm_lock("/repo", me).unwrap();
        assert_eq!(
            store.passes("/repo").unwrap(),
            (vec![me], Some(12)),
            "a warm child counts, and the pass's recount stands"
        );

        store.end_pass("/repo", me, None).unwrap();
        assert_eq!(store.passes("/repo").unwrap().1, None, "complete: no span");

        // a live pid on a lock past its TTL is a reused pid, not a warmer
        store
            .meta_set("warm_lock:/old", &format!("{me}:0"))
            .unwrap();
        assert!(store.passes("/old").unwrap().0.is_empty());
    }

    #[test]
    fn a_pass_says_whether_it_is_reading_or_finishing() {
        let mut store = Store::open_in_memory().unwrap();
        let me = std::process::id();
        store.begin_pass("/repo", DEAD, None).unwrap();
        store
            .set_pass_phase("/repo", DEAD, Phase::Finishing)
            .unwrap();
        assert_eq!(store.pass_phase("/repo"), None, "a dead pass says nothing");

        store.begin_pass("/repo", me, None).unwrap();
        assert!(
            store
                .meta_get(&format!("phase:/repo:{DEAD}"))
                .unwrap()
                .is_none(),
            "cleared with the dead pass's mark"
        );
        assert_eq!(store.pass_phase("/repo").map(|p| p.0), Some("reading"));
        store.set_pass_phase("/repo", me, Phase::Finishing).unwrap();
        assert_eq!(store.pass_phase("/repo").map(|p| p.0), Some("finishing"));
        let stored = store.meta_get(&format!("phase:/repo:{me}")).unwrap();
        let (phase, since) = stored.as_deref().unwrap().split_once(':').unwrap();
        assert_eq!(phase, "finishing", "stored as an older rq reads it");
        assert!(since.parse::<i64>().is_ok());
        store.end_pass("/repo", me, None).unwrap();
        assert_eq!(store.pass_phase("/repo"), None);
        assert!(
            store
                .meta_get(&format!("phase:/repo:{me}"))
                .unwrap()
                .is_none(),
            "a finished pass leaves no phase behind"
        );
    }

    #[test]
    fn a_pass_mark_unrenewed_past_its_ttl_is_a_crashed_passs() {
        // a crashed pass whose pid was reused by a live process
        let mut store = Store::open_in_memory().unwrap();
        let me = std::process::id();
        let stale = stale().to_string();
        store.meta_set(&format!("pass:/repo:{me}"), &stale).unwrap();
        assert!(store.passes("/repo").unwrap().0.is_empty());
        assert!(!store.indexed_by_others("/repo"));

        // a running pass renews its mark, and a stale one is cleared
        store.renew_pass("/repo", me).unwrap();
        assert_eq!(store.passes("/repo").unwrap().0, vec![me]);
        store.meta_set(&format!("pass:/repo:{me}"), &stale).unwrap();
        store.begin_pass("/repo", other(), None).unwrap();
        assert_eq!(store.marked_pids("/repo"), vec![other()]);
    }

    #[test]
    fn a_pass_that_fails_leaves_no_mark() {
        // a long-lived process (a `--batch`) outlives its failed pass, so its
        // pid can't be what retires the mark
        let dir = Scratch::new("failed-pass");
        std::fs::write(dir.join("w.rb"), "class Widget\nend\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER fail BEFORE INSERT ON files \
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END",
            )
            .unwrap();
        // a warm child's pass: its lock outlives the pass, and stays
        let root = dir.canonicalize().unwrap().to_string_lossy().into_owned();
        let me = std::process::id();
        assert!(store.claim_warm_lock(&root, me).unwrap());
        assert!(crate::index::index_under(&mut store, &dir, &[]).is_err());
        assert!(store.marked_pids(&root).is_empty(), "no pass mark");
        let phase = phase_key(&root, me);
        assert!(store.meta_get(&phase).unwrap().is_none(), "no phase");
        assert_eq!(store.passes(&root).unwrap().0, vec![me], "the lock holds");
    }

    #[test]
    fn a_mark_dropped_behind_a_writer_is_left_rather_than_waited_for() {
        let dir = Scratch::new("mark-busy");
        let path = dir.join("rq.db");
        let mut store = Store::open(&path).unwrap();
        let writer = Store::open(&path).unwrap();
        let mark = store.mark_pass("/r", None).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let t = std::time::Instant::now();
        drop(mark);
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "no wait");
        writer.conn.execute_batch("COMMIT").unwrap();
        assert_eq!(store.marked_pids("/r"), vec![std::process::id()]);
    }

    #[test]
    fn a_drop_forgets_only_its_own_roots_marks() {
        let mut store = Store::open_in_memory().unwrap();
        let repo = store
            .upsert_repository(&crate::core::RepoIdentity::local("/a"), None)
            .unwrap();
        store.upsert_checkout(repo, "/a", None).unwrap();
        store.begin_pass("/a", other(), None).unwrap();
        store.begin_pass("/a:b", other(), None).unwrap();
        store.forget_checkout("/a").unwrap();
        assert!(store.marked_pids("/a").is_empty());
        assert_eq!(store.marked_pids("/a:b"), vec![other()]);
        let phase = phase_key("/a:b", other());
        assert!(store.meta_get(&phase).unwrap().is_some());
    }

    #[test]
    fn a_pass_leaves_another_roots_marks_alone() {
        // `pass:/a:` starts `/a:b`'s keys too; its dead pass isn't `/a`'s
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/a:b", DEAD, None).unwrap();
        store.begin_pass("/a", other(), None).unwrap();
        assert_eq!(store.marked_pids("/a:b"), vec![DEAD]);
        assert_eq!(store.marked_pids("/a"), vec![other()]);
    }

    #[test]
    fn a_dead_pid_holds_nothing() {
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/r", DEAD, None).unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{DEAD}:{}", now_unix()))
            .unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_stale_stamp_holds_nothing() {
        let store = Store::open_in_memory().unwrap();
        let p = other();
        store
            .meta_set(&format!("pass:/r:{p}"), &stale().to_string())
            .unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", stale()))
            .unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn this_process_is_not_another_indexer() {
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass("/r", std::process::id(), None).unwrap();
        assert!(store.other_indexers("/r").is_empty());
        assert!(!store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_warm_lock_counts_as_a_holder() {
        let store = Store::open_in_memory().unwrap();
        let p = other();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", now_unix()))
            .unwrap();
        assert_eq!(store.other_indexers("/r"), vec![p]);
        assert!(store.indexing_anywhere().unwrap());
    }

    #[test]
    fn a_warm_lock_and_a_pass_by_one_process_name_it_once() {
        let mut store = Store::open_in_memory().unwrap();
        let p = other();
        store.begin_pass("/r", p, None).unwrap();
        store
            .meta_set("warm_lock:/r", &format!("{p}:{}", now_unix()))
            .unwrap();
        assert_eq!(store.other_indexers("/r"), vec![p]);
    }

    #[test]
    fn a_mark_holds_only_its_own_root() {
        let mut store = Store::open_in_memory().unwrap();
        let p = other();
        store.begin_pass("/a/b", p, None).unwrap();
        for root in ["/a/bc", "/a", "/a/b/c", "/a/b:1"] {
            assert!(store.other_indexers(root).is_empty(), "{root}");
        }
        assert_eq!(store.other_indexers("/a/b"), vec![p]);
    }

    /// The cleanup's "anyone indexing anything" and a drop's "anyone indexing
    /// this" are one judgement, over the stored encodings.
    #[test]
    fn indexing_anywhere_agrees_with_other_indexers() {
        let (me, p, now) = (std::process::id(), other(), now_unix());
        let stale = stale();
        // each mark as `key=value`, the strings an rq of any version stores
        let cases: Vec<(&str, Vec<String>, bool)> = vec![
            ("empty", vec![], false),
            ("live pass", vec![format!("pass:/a:{p}={now}")], true),
            ("dead pass", vec![format!("pass:/a:{DEAD}={now}")], false),
            ("stale pass", vec![format!("pass:/a:{p}={stale}")], false),
            ("own pass", vec![format!("pass:/a:{me}={now}")], false),
            ("live lock", vec![format!("warm_lock:/a={p}:{now}")], true),
            (
                "dead lock",
                vec![format!("warm_lock:/a={DEAD}:{now}")],
                false,
            ),
            (
                "stale lock",
                vec![format!("warm_lock:/a={p}:{stale}")],
                false,
            ),
            ("own lock", vec![format!("warm_lock:/a={me}:{now}")], false),
            ("garbled lock", vec!["warm_lock:/a=x".into()], false),
            (
                "phase alone",
                vec![format!("phase:/a:{p}=reading:{now}")],
                false,
            ),
            ("span alone", vec!["span:/a=3".into()], false),
            ("colon in root", vec![format!("pass:/a:b:{p}={now}")], true),
            (
                "mixed, one live",
                vec![
                    format!("pass:/a:{DEAD}={now}"),
                    format!("pass:/b:{me}={now}"),
                    format!("warm_lock:/c={p}:{now}"),
                ],
                true,
            ),
        ];
        for (name, rows, expected) in cases {
            let store = Store::open_in_memory().unwrap();
            for row in &rows {
                let (k, v) = row.split_once('=').unwrap();
                store.meta_set(k, v).unwrap();
            }
            let roots = ["/a", "/a:b", "/b", "/c"];
            let others = roots.iter().any(|r| !store.other_indexers(r).is_empty());
            let anywhere = store.indexing_anywhere().unwrap();
            assert_eq!(others, expected, "{name}: other_indexers");
            assert_eq!(anywhere, expected, "{name}: indexing_anywhere");
        }
    }

    /// A multi-byte root's marks are found, cleared and forgotten like any
    /// other's: matched as a key range, not a length.
    #[test]
    fn a_non_ascii_root() {
        let root = "/x/café";
        let mut store = Store::open_in_memory().unwrap();
        store.begin_pass(root, DEAD, None).unwrap();
        store.begin_pass(root, other(), None).unwrap();
        assert_eq!(store.other_indexers(root), vec![other()]);
        assert!(store.indexing_anywhere().unwrap());
        assert_eq!(
            store.marked_pids(root),
            vec![other()],
            "the dead pass's mark is cleared"
        );
        assert!(store.other_indexers("/x/caf").is_empty());

        let repo = store
            .upsert_repository(&crate::core::RepoIdentity::local(root), None)
            .unwrap();
        store.upsert_checkout(repo, root, None).unwrap();
        store.forget_checkout(root).unwrap();
        assert!(store.marked_pids(root).is_empty(), "a drop forgets them");
        assert!(
            store
                .meta_get(&format!("phase:{root}:{}", other()))
                .unwrap()
                .is_none()
        );
    }
}
