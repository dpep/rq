//! Phase timing for `--profile`.
//!
//! [`trace`](crate::trace) answers "what did this run decide?" — the resolved
//! root, coverage, what got warmed. This answers "where did the time go?", and
//! keeps the two apart: trace lines are prose meant to be read as they happen,
//! phases are a table meant to be compared against another run.
//!
//! Off by default and free when off, on the same terms as trace: a span reads
//! no clock, takes no lock and allocates nothing unless profiling is on, so the
//! only cost left on the search path is a relaxed atomic load per phase.
//!
//! Streaming makes one measurement matter more than the total: `first result`
//! is the number the sub-50 ms budget is about, and a change that improves the
//! total while delaying the first answer is a regression here.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static PHASES: Mutex<Vec<Phase>> = Mutex::new(Vec::new());
static COUNTERS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());
static SLOWEST: Mutex<Vec<(Duration, String)>> = Mutex::new(Vec::new());

/// How many entries the "slowest units of work" list keeps. Enough to spot a
/// pathological input, few enough to read at a glance.
const SLOWEST_KEEP: usize = 5;

/// Microseconds of the current worst-of-the-kept entry — the bar a candidate
/// must clear to be worth the lock. Read as a plain atomic so the common case
/// (a fast file, on one of many parse workers) costs a load rather than
/// contending every worker on one mutex.
static SLOW_BAR: AtomicU64 = AtomicU64::new(0);

/// One measured phase.
pub(crate) struct Phase {
    pub name: &'static str,
    pub elapsed: Duration,
    /// What the phase did — candidate counts, symbols scored, a cache verdict.
    pub note: Option<String>,
}

/// Enable profiling from the `--profile` flag; `RQ_PROFILE` in the environment
/// also enables it, so a shipped binary can be measured in place — the same
/// affordance `RQ_LOG` gives trace.
pub(crate) fn enable_from(flag: bool) {
    let on = flag || std::env::var_os("RQ_PROFILE").is_some();
    ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Start timing a phase. The returned span records it when dropped; with
/// profiling off it is inert.
pub(crate) fn span(name: &'static str) -> Span {
    Span {
        name,
        start: enabled().then(Instant::now),
        note: None,
    }
}

/// Record a phase whose duration was measured elsewhere — for the timings the
/// search path already takes for its own trace lines.
pub(crate) fn record(name: &'static str, elapsed: Duration, note: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    if let Ok(mut phases) = PHASES.lock() {
        phases.push(Phase {
            name,
            elapsed,
            note: Some(note()),
        });
    }
}

/// Add `n` to a named counter — files seen, batches committed. Counts describe
/// *how much work there was*, which a duration alone can't distinguish from
/// work that was merely slow. Additive: repeated calls with one name sum.
pub(crate) fn count(name: &'static str, n: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut counters) = COUNTERS.lock() {
        match counters.iter_mut().find(|(k, _)| *k == name) {
            Some((_, v)) => *v += n,
            None => counters.push((name, n)),
        }
    }
}

/// Offer one unit of work to the "slowest" list, which keeps the worst
/// [`SLOWEST_KEEP`]. These locate pathological *inputs* — one generated file
/// can dominate a phase without moving any average. `label` runs only for a
/// candidate that clears the bar, so naming a fast file is never paid for.
pub(crate) fn slow(elapsed: Duration, label: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    let us = elapsed.as_micros() as u64;
    if us <= SLOW_BAR.load(Ordering::Relaxed) {
        return; // can't displace anything already kept
    }
    if let Ok(mut slowest) = SLOWEST.lock() {
        slowest.push((elapsed, label()));
        slowest.sort_by_key(|a| std::cmp::Reverse(a.0));
        slowest.truncate(SLOWEST_KEEP);
        // raise the bar only once the list is full — until then everything is
        // worth keeping
        if slowest.len() == SLOWEST_KEEP {
            let bar = slowest[SLOWEST_KEEP - 1].0.as_micros() as u64;
            SLOW_BAR.store(bar, Ordering::Relaxed);
        }
    }
}

/// Counters recorded so far, highest first. Drains.
pub(crate) fn counters() -> Vec<(&'static str, u64)> {
    COUNTERS
        .lock()
        .map(|mut c| std::mem::take(&mut *c))
        .unwrap_or_default()
}

/// The slowest units of work, slowest first. Drains.
pub(crate) fn slowest() -> Vec<(Duration, String)> {
    SLOW_BAR.store(0, Ordering::Relaxed);
    SLOWEST
        .lock()
        .map(|mut s| std::mem::take(&mut *s))
        .unwrap_or_default()
}

pub(crate) struct Span {
    name: &'static str,
    start: Option<Instant>,
    note: Option<String>,
}

impl Span {
    /// Attach detail to this phase. The closure runs only when profiling is on,
    /// so formatting a count is never paid for in a normal run.
    pub(crate) fn note(&mut self, f: impl FnOnce() -> String) {
        if self.start.is_some() {
            self.note = Some(f());
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(start) = self.start else { return };
        if let Ok(mut phases) = PHASES.lock() {
            phases.push(Phase {
                name: self.name,
                elapsed: start.elapsed(),
                note: self.note.take(),
            });
        }
    }
}

/// Every phase recorded so far, in the order they finished. Drains.
pub(crate) fn phases() -> Vec<Phase> {
    PHASES
        .lock()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

/// The report as stderr-ready lines. Empty when nothing was measured.
pub(crate) fn report(total: Duration) -> Vec<String> {
    let phases = phases();
    let counters = counters();
    let slowest = slowest();
    if phases.is_empty() && counters.is_empty() && slowest.is_empty() {
        return Vec::new();
    }
    let w = phases
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let mut out: Vec<String> = phases
        .iter()
        .map(|p| {
            let note = p.note.as_deref().unwrap_or_default();
            format!("  {:<w$}  {:>8}  {note}", p.name, ms(p.elapsed), w = w)
                .trim_end()
                .to_string()
        })
        .collect();
    out.push(format!("  {:<w$}  {:>8}", "─".repeat(w.min(20)), "", w = w));
    out.push(format!("  {:<w$}  {:>8}", "total", ms(total), w = w));
    for (name, v) in &counters {
        out.push(format!("  {name:<w$}  {v:>8}", w = w));
    }
    // one "slowest" heading, then the ranked rows under it
    for (i, (elapsed, label)) in slowest.iter().enumerate() {
        let head = if i == 0 { "slowest" } else { "" };
        out.push(format!("  {head:<w$}  {:>8}  {label}", ms(*elapsed), w = w));
    }
    out
}

/// Phases as JSON, for storing a baseline and diffing runs.
pub(crate) fn json(total: Duration) -> String {
    let phases = phases();
    let counters = counters();
    let slowest = slowest();
    let body: Vec<String> = phases
        .iter()
        .map(|p| {
            let note = match &p.note {
                Some(n) => format!("\"{}\"", n.replace('"', "'")),
                None => "null".to_string(),
            };
            format!(
                "{{\"name\":\"{}\",\"ms\":{:.3},\"note\":{note}}}",
                p.name,
                p.elapsed.as_secs_f64() * 1000.0
            )
        })
        .collect();
    // `counters` and `slowest` are always present (empty when a run recorded
    // none), so a consumer can read them without probing for the key.
    let counts: Vec<String> = counters
        .iter()
        .map(|(k, v)| format!("{}:{v}", quote(k)))
        .collect();
    let slow: Vec<String> = slowest
        .iter()
        .map(|(elapsed, label)| {
            format!(
                "{{\"file\":{},\"ms\":{:.3}}}",
                quote(label),
                elapsed.as_secs_f64() * 1000.0
            )
        })
        .collect();
    format!(
        "{{\"total_ms\":{:.3},\"phases\":[{}],\"counters\":{{{}}},\"slowest\":[{}]}}",
        total.as_secs_f64() * 1000.0,
        body.join(","),
        counts.join(","),
        slow.join(",")
    )
}

/// A JSON string literal. Paths and counter names are arbitrary text, so they
/// go through serde rather than hand-rolled quoting.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ENABLED` and `PHASES` are process-global and cargo runs tests as
    /// threads in one process, so these two would otherwise interleave — the
    /// "off" test seeing the "on" test's flag and running a closure that
    /// panics on purpose.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// Poison-tolerant: one failing test shouldn't cascade into the other.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_span_is_inert_when_profiling_is_off() {
        let _guard = serial();
        ENABLED.store(false, Ordering::Relaxed);
        let _ = phases();
        let mut s = span("off");
        s.note(|| panic!("the note closure must not run when disabled"));
        drop(s);
        record("also off", Duration::from_millis(1), || {
            panic!("nor this one")
        });
        count("off", 1);
        slow(Duration::from_secs(9), || panic!("nor the slow label"));
        assert!(phases().is_empty());
        assert!(counters().is_empty());
        assert!(slowest().is_empty());
    }

    #[test]
    fn counters_sum_and_the_slowest_list_keeps_the_worst() {
        let _guard = serial();
        enable_from(true);
        let _ = (phases(), counters(), slowest());

        count("files seen", 40);
        count("files seen", 2);
        assert_eq!(counters(), vec![("files seen", 42)]);

        // offered fastest-first, so every entry has to displace the bar
        for i in 1..=(SLOWEST_KEEP as u64 + 3) {
            slow(Duration::from_millis(i), || format!("f{i}"));
        }
        let kept = slowest();
        assert_eq!(kept.len(), SLOWEST_KEEP);
        assert_eq!(kept[0].1, format!("f{}", SLOWEST_KEEP as u64 + 3));
        assert!(kept.windows(2).all(|w| w[0].0 >= w[1].0), "slowest first");
        // draining resets the bar, so the next run isn't gated by the last one's
        slow(Duration::from_micros(1), || "tiny".to_string());
        assert_eq!(slowest().len(), 1);

        ENABLED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn an_enabled_span_records_its_name_and_note() {
        let _guard = serial();
        enable_from(true);
        {
            let mut s = span("on");
            s.note(|| "9 candidates".to_string());
        }
        let recorded = phases();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].name, "on");
        assert_eq!(recorded[0].note.as_deref(), Some("9 candidates"));
        assert!(phases().is_empty(), "phases() drains");
        ENABLED.store(false, Ordering::Relaxed);
    }
}
