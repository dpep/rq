//! `--open` and `--web`: handing a result to an editor or browser.

use super::*;

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
pub(super) fn parse_choice(input: &str, n: usize) -> Option<usize> {
    let s = input.trim();
    if s.is_empty() {
        return Some(0);
    }
    let i = s.parse::<usize>().ok()?.checked_sub(1)?;
    (i < n).then_some(i)
}

/// `--open`/`--web`: choose a hit, then hand off to the editor or browser. The launcher `exec`s (replacing this
/// process), so the shell waits on it — not on rq's background warm.
pub(super) fn finish_open(
    hits: &[crate::search::Hit],
    root: Option<&std::path::Path>,
    web: bool,
) -> Result<(), Failure> {
    let Some(hit) = choose_hit(hits) else {
        return Ok(()); // aborted at the prompt
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

/// Launch the editor on `file:line`, resolving the command in order: `RQ_OPEN`
/// template → VS Code (`code`) → `$VISUAL`/`$EDITOR` → print the location. The
/// chosen command replaces this process via `exec`.
fn launch_editor(file: &std::path::Path, line: i64) -> Result<(), Failure> {
    use std::os::unix::process::CommandExt;
    let loc = format!("{}:{}", file.display(), line);
    match open_command(file, line, &loc) {
        Some((prog, args)) => {
            // exec replaces this process, so the run's profile goes out now
            crate::profile::emit(false);
            // exec returns only on failure
            let err = std::process::Command::new(&prog).args(&args).exec();
            Err(report(
                Output::Text,
                Failure::Launch,
                format_args!("rq --open: cannot run {prog}: {err}"),
            ))
        }
        None => {
            println!("{loc}");
            Ok(())
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
fn open_web(hit: &crate::search::Hit, root: Option<&std::path::Path>) -> Result<(), Failure> {
    if hit.repo_identity.starts_with("local:") {
        return Err(report(
            Output::Text,
            Failure::NoRemote,
            format_args!(
                "rq --web: {}:{} has no git remote to link to ({}) — open it \
                 locally with -o, or add one with `git remote add origin <url>`",
                hit.file, hit.line, hit.repo_identity
            ),
        ));
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
            Err(report(
                Output::Text,
                Failure::Launch,
                format_args!("rq --web: cannot run {prog}: {err}"),
            ))
        }
        None => {
            println!("{url}");
            Ok(())
        }
    }
}

/// A GitHub-style permalink: `https://<host/org/repo>/blob/<rev>/<file>#L<line>`.
/// GitLab redirects the same shape, so it isn't GitHub-only.
pub(super) fn web_url(identity: &str, rev: &str, file: &str, line: i64) -> String {
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
