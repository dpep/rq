//! The index's own commands: `--index`, `--drop`, `--status`, `--storage` and `--usage`.

use super::*;

pub(super) fn cmd_index(path: Option<PathBuf>, subdirs: &[String], out: Output) -> ExitCode {
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
                    // keys sorted, as they always went out; totals absent when
                    // the checkout can't be counted
                    #[derive(serde::Serialize)]
                    struct Indexed {
                        #[serde(skip_serializing_if = "Option::is_none")]
                        files: Option<i64>,
                        files_added: usize,
                        repo: String,
                        root: String,
                        scope: &'static str,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        symbols: Option<i64>,
                        symbols_added: usize,
                    }
                    let row = Indexed {
                        files: totals.map(|(files, _)| files),
                        files_added: stats.files_parsed,
                        repo: identity,
                        root: root_key(&root),
                        scope: if subtree { "subtree" } else { "full" },
                        symbols: totals.map(|(_, symbols)| symbols),
                        symbols_added: stats.symbols,
                    };
                    let emitted = emit_json(out, &row);
                    after_pass(&mut store);
                    return exit_code(emitted);
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
            after_pass(&mut store);
            ExitCode::SUCCESS
        }
        Err(e) if e.is::<crate::index::Dropped>() => fail(
            out,
            Failure::Busy,
            format_args!(
                "rq --index: {} was dropped (rq --drop) while this index ran; \
                 run rq --index again to rebuild it",
                root_key(&root)
            ),
        ),
        Err(e) => fail(out, Failure::Index, format_args!("rq --index: {e}")),
    }
}

pub(super) fn cmd_drop(target: Option<String>, out: Output) -> ExitCode {
    let mut store = match open_store_or_fail(out, "rq --drop") {
        Ok(s) => s,
        Err(code) => return code,
    };
    // as `--index` waits: a cleanup's one full rewrite (D65), or a cold
    // pass's name-index rebuild, holds the lock past a search's wait
    let on_long_wait = show_progress(out, stderr_interactive()).then_some(say_waiting as fn());
    let _ = store.wait_out_writers(writer_wait(), on_long_wait);

    // Resolve what to drop: TARGET as a path (→ repo root, like --index) drops
    // that checkout; failing that, TARGET as a repo identity drops the repo
    // with every checkout of it — so cruft shown by --status can be dropped by
    // name even if the checkout is gone.
    let path = PathBuf::from(target.clone().unwrap_or_else(|| ".".to_string()));
    let here = Here::at(&store, &path);
    let root = here.root;
    let key = root_key(&root);
    let dropped = |repo: String, root: Option<String>, (files, symbols): (i64, i64)| Dropped {
        dropped: true,
        files,
        repo,
        root,
        symbols,
    };

    if let Some(checkout) = here.checkout {
        if let Some(busy) = indexing(&store, [key.clone()], out) {
            return busy;
        }
        let identity = store
            .identity_for_root(&key)
            .ok()
            .flatten()
            .unwrap_or_default();
        let totals = store.checkout_totals(checkout.id).unwrap_or((0, 0));
        return match store.forget_checkout(&key) {
            Ok(()) => dropped(identity, Some(key), totals).emit(out),
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
        let nothing = Dropped {
            dropped: false,
            ..dropped(identity, at, (0, 0))
        };
        return nothing.emit(out);
    };
    let roots = store.checkout_roots(repo_id).unwrap_or_default();
    if let Some(busy) = indexing(&store, roots, out) {
        return busy;
    }
    let totals = store.repo_totals(repo_id).unwrap_or((0, 0));
    match store.drop_repository(repo_id) {
        Ok(()) => dropped(identity, None, totals).emit(out),
        Err(e) => fail(out, Failure::Database, format_args!("rq --drop: {e}")),
    }
}

/// A drop's refusal when a live pass or warm holds one of `roots`: dropping
/// under it would pull the checkout out from under its writes. A dead pid's
/// leftover mark doesn't count.
fn indexing(
    store: &Store,
    roots: impl IntoIterator<Item = String>,
    out: Output,
) -> Option<ExitCode> {
    roots.into_iter().find_map(|root| {
        let pids = store.other_indexers(&root);
        let first = *pids.first()?;
        let pids = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Some(fail(
            out,
            Failure::Busy,
            format_args!(
                "rq --drop: {root} is being indexed by another rq (pid {pids}); \
                 wait for it to finish, or stop it (kill {first}), then drop again"
            ),
        ))
    })
}

/// What `--drop` did. Keys sorted, as they always went out.
#[derive(serde::Serialize)]
struct Dropped {
    /// `false` when there was nothing to drop: idempotent, and a script can tell.
    dropped: bool,
    files: i64,
    repo: String,
    /// The checkout's root; absent for a whole repo dropped by identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    symbols: i64,
}

impl Dropped {
    fn emit(&self, out: Output) -> ExitCode {
        let Dropped {
            dropped,
            files,
            repo,
            root,
            symbols,
        } = self;
        match out {
            Output::Text if *dropped => {
                let what = root
                    .as_ref()
                    .map_or_else(|| repo.clone(), |r| format!("{repo} at {r}"));
                println!("dropped {what} ({files} file(s), {symbols} symbol(s))");
                ExitCode::SUCCESS
            }
            Output::Text => {
                println!("not indexed: {}", root.as_deref().unwrap_or(repo));
                ExitCode::SUCCESS
            }
            Output::Json | Output::Ndjson => exit_code(emit_json(out, self)),
        }
    }
}

pub(super) fn cmd_status(out: Output) -> ExitCode {
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
    if let Err(failed) = emit_rows(out, &rows) {
        return failed.into();
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
    if out == Output::Text {
        if let Ok(storage) = store.storage() {
            println!("{}", storage_line(&storage, now_unix()));
        }
        if let Ok(db) = db_location() {
            for line in leftovers(&db, store.file()) {
                println!("{line}");
            }
        }
    }
    ExitCode::SUCCESS
}

/// `--storage`: the index file's size, the free pages inside it, and what the
/// background cleanup last did (D65). One object: it is one file.
pub(super) fn cmd_storage(out: Output) -> ExitCode {
    let store = match open_store_or_fail(out, "rq --storage") {
        Ok(s) => s,
        Err(code) => return code,
    };
    let storage = match store.storage() {
        Ok(storage) => storage,
        Err(e) => return fail(out, Failure::Database, format_args!("rq --storage: {e}")),
    };
    #[derive(serde::Serialize)]
    struct Report<'a> {
        /// The file in use: beside a newer rq's, this rq's side store.
        #[serde(skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        #[serde(flatten)]
        storage: &'a crate::store::Storage,
    }
    match out {
        Output::Json | Output::Ndjson => exit_code(emit_json(
            out,
            &Report {
                file: store.file().map(|f| f.display().to_string()),
                storage: &storage,
            },
        )),
        Output::Text => {
            if let Some(file) = store.file() {
                println!("{}", file.display());
            }
            println!("{}", storage_line(&storage, now_unix()));
            ExitCode::SUCCESS
        }
    }
}

/// The store in a line: its size, what's free inside it, and the last
/// cleanup — or why it left the space.
fn storage_line(storage: &crate::store::Storage, now: i64) -> String {
    let mb = |bytes: i64| bytes as f64 / 1e6;
    let mut line = format!("index: {:.1} MB", mb(storage.bytes));
    if storage.free_bytes > 0 {
        line.push_str(&format!(", {:.1} MB of it free", mb(storage.free_bytes)));
    }
    let cleanup = &storage.cleanup;
    match cleanup.skipped {
        Some(crate::store::Held::NoRoom) => line.push_str(
            "; the last cleanup left it: no room on disk for the one rewrite it needs \
             (twice the live size, free)",
        ),
        Some(crate::store::Held::Busy) => {
            line.push_str("; the last cleanup left it: another rq was writing, so it retries")
        }
        None => {}
    }
    if let (Some(at), Some(bytes)) = (cleanup.ran_at, cleanup.reclaimed_bytes) {
        line.push_str(&format!(
            "; cleaned up {} ago, {:.1} MB back to the disk",
            ago(now - at),
            mb(bytes)
        ));
    }
    line
}

/// A duration in its largest whole unit: `40 s`, `3 min`, `2 h`, `5 d`.
fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
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
pub(super) fn cmd_usage(out: Output) -> ExitCode {
    let store = match open_store_or_fail(out, "rq --usage") {
        Ok(s) => s,
        Err(code) => return code,
    };
    let rows = match store.usage_overview() {
        Ok(rows) => rows,
        Err(e) => return fail(out, Failure::Database, format_args!("rq --usage: {e}")),
    };
    if let Err(failed) = emit_rows(out, &rows) {
        return failed.into();
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
        return Outcome::Miss.into();
    }
    ExitCode::SUCCESS
}
