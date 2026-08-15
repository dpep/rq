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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static PHASES: Mutex<Vec<Phase>> = Mutex::new(Vec::new());

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
    if phases.is_empty() {
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
    out
}

/// Phases as JSON, for storing a baseline and diffing runs.
pub(crate) fn json(total: Duration) -> String {
    let phases = phases();
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
    format!(
        "{{\"total_ms\":{:.3},\"phases\":[{}]}}",
        total.as_secs_f64() * 1000.0,
        body.join(",")
    )
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_is_inert_when_profiling_is_off() {
        let mut s = span("off");
        s.note(|| panic!("the note closure must not run when disabled"));
        drop(s);
        record("also off", Duration::from_millis(1), || {
            panic!("nor this one")
        });
        assert!(phases().is_empty());
    }

    #[test]
    fn an_enabled_span_records_its_name_and_note() {
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
