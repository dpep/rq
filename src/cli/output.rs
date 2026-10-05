//! Structured output and failure: JSON emitters, exit outcomes, and errors.

use super::*;

/// Print a single value as JSON: `--json` pretty, `--ndjson` compact one-liner.
/// Used by the single-object operations (`--index`, `--drop`) and the
/// no-match status objects; [`emit_rows`] is the multi-row twin.
pub(super) fn emit_json<T: serde::Serialize>(out: Output, value: &T) -> Result<(), Failure> {
    let rendered = if out == Output::Json {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match rendered {
        Ok(s) => {
            println!("{s}");
            Ok(())
        }
        Err(e) => Err(report(out, Failure::Internal, format_args!("rq: {e}"))),
    }
}

/// A command's exit once its output is out: success, or the failure it reported.
pub(super) fn exit_code(emitted: Result<(), Failure>) -> ExitCode {
    emitted.map_or_else(ExitCode::from, |()| ExitCode::SUCCESS)
}

/// Print a row set as structured output: `--json` one pretty array, `--ndjson`
/// one compact object per line (Text output is the caller's business).
pub(super) fn emit_rows<T: serde::Serialize>(out: Output, rows: &[T]) -> Result<(), Failure> {
    let failed = |e: serde_json::Error| report(out, Failure::Internal, format_args!("rq: {e}"));
    match out {
        Output::Json => println!("{}", serde_json::to_string_pretty(rows).map_err(failed)?),
        Output::Ndjson => {
            for r in rows {
                println!("{}", serde_json::to_string(r).map_err(failed)?);
            }
        }
        Output::Text => {}
    }
    Ok(())
}

/// How a search ended. Ordered by what a batch reports when its lines
/// disagree: any hit; else an error (the higher code, if several); else
/// "retry"; and a miss only when a retry would change nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Outcome {
    Miss,
    Warming,
    /// Reported already; its exit code.
    Failed(u8),
    Hit,
}

impl From<Verdict> for Outcome {
    fn from(verdict: Verdict) -> Outcome {
        match verdict {
            Verdict::Hit => Outcome::Hit,
            Verdict::Miss => Outcome::Miss,
            Verdict::Warming => Outcome::Warming,
        }
    }
}

impl From<Failure> for Outcome {
    fn from(kind: Failure) -> Outcome {
        Outcome::Failed(kind.exit_code())
    }
}

impl From<Outcome> for ExitCode {
    fn from(outcome: Outcome) -> ExitCode {
        match outcome {
            Outcome::Hit => ExitCode::SUCCESS,
            Outcome::Miss => ExitCode::FAILURE,
            Outcome::Warming => ExitCode::from(2),
            Outcome::Failed(code) => ExitCode::from(code),
        }
    }
}

/// What kind of thing went wrong: the stable `kind` of a structured error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
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

impl From<Failure> for ExitCode {
    fn from(kind: Failure) -> ExitCode {
        ExitCode::from(kind.exit_code())
    }
}

/// Report an error and return its exit code. The message always goes to
/// stderr; a structured caller also gets it as one JSON object on stdout.
pub(super) fn fail(out: Output, kind: Failure, args: std::fmt::Arguments) -> ExitCode {
    report(out, kind, args).into()
}

/// [`fail`] for a caller that carries the failure on rather than exiting.
pub(super) fn report(out: Output, kind: Failure, args: std::fmt::Arguments) -> Failure {
    let message = args.to_string();
    eprintln!("{message}");
    emit_error(out, kind, &message);
    kind
}

/// The structured half of an error: `{"error", "kind", "code"}` on stdout,
/// nothing for text. `code` is the exit code the process leaves with.
fn emit_error(out: Output, kind: Failure, message: &str) {
    // keys sorted, as they always went out
    #[derive(serde::Serialize)]
    struct Error<'a> {
        code: u8,
        error: &'a str,
        kind: &'static str,
    }
    let obj = Error {
        code: kind.exit_code(),
        error: message,
        kind: kind.as_str(),
    };
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
pub(super) fn clap_failure(err: clap::Error) -> ExitCode {
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
