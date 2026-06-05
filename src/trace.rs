//! Opt-in diagnostics. `rq -v` (or `RQ_LOG` set in the environment, so an
//! already-installed binary can be debugged without a rebuild) turns on stderr
//! trace lines describing what each search and index pass decided — the resolved
//! root and coverage, what got warmed, written, or reconciled away, and how long
//! the search took. Off by default; when off it's a single relaxed atomic load,
//! so it costs nothing on the hot path. Trace goes to stderr, never stdout, so
//! results stay machine-parseable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable tracing from the `-v` flag; `RQ_LOG` in the environment also enables
/// it, so a shipped binary can be debugged in place.
pub fn enable_from(flag: bool) {
    let on = flag || std::env::var_os("RQ_LOG").is_some();
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether trace output is on.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A path for display in trace lines, with the home directory shown as `~`.
pub fn abbrev(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => match s.strip_prefix(&*home.to_string_lossy()) {
            Some(rest) => format!("~{rest}"),
            None => s,
        },
        _ => s,
    }
}

/// Emit a trace line to stderr (prefixed `rq:`), only when enabled.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            eprintln!("rq: {}", format_args!($($arg)*));
        }
    };
}

/// Times a phase and logs `<label> (N ms)` when dropped — but only if tracing
/// was on at construction, so it's free otherwise.
pub struct Timer {
    label: &'static str,
    start: Option<Instant>,
}

impl Timer {
    pub fn start(label: &'static str) -> Self {
        Self {
            label,
            start: enabled().then(Instant::now),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            eprintln!("rq: {} ({} ms)", self.label, start.elapsed().as_millis());
        }
    }
}
