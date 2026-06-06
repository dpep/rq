//! Indexing — walk a checkout, extract symbols, persist incrementally.
//!
//! Decoupled from search: it only writes. Unchanged files (same content hash)
//! are skipped, and coverage is recorded so search can judge its own confidence.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ignore::WalkBuilder;

use crate::core::RepoIdentity;
use crate::lang;
use crate::store::Store;

/// Outcome of an indexing run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Files matching a known language that were walked.
    pub files_seen: usize,
    /// Files (re)parsed this run (unchanged files are skipped).
    pub files_indexed: usize,
    /// Symbols written this run.
    pub symbols: usize,
}

/// Index the whole repository rooted at `root`.
pub fn index_path(store: &mut Store, root: &Path) -> Result<Stats, Box<dyn std::error::Error>> {
    index_under(store, root, &[])
}

/// Index `root`, or — when `subdirs` is non-empty — only those repo-relative
/// subtrees of it (a partial index of the repo). Unbounded: an explicit index
/// is thorough. A whole-repo index also reconciles deletions; a deliberate
/// subset is marked `partial` so a later search won't auto-warm over it.
pub fn index_under(
    store: &mut Store,
    root: &Path,
    subdirs: &[String],
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(store, root, &[], subdirs, None, None)
}

/// Lowercase the alphanumeric chars of `s` — the normal form for loose,
/// separator-insensitive path matching.
fn alnum_lower(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Move the candidate paths whose *filename* looks relevant to the query to the
/// front (preserving order within each group), so a warming pass parses likely
/// files first. Deliberately generous: a stem qualifies if it shares any ~4-char
/// run with the query — parsing is cheap, so over-including a near-match beats
/// missing the target. `employeescontroller` flags employee / employers /
/// EmpController, tosses companies. Matched on the filename stem (not the whole
/// path), so a common directory like `controllers/` doesn't flag the whole tree.
/// String-only over the in-memory list — no file reads. No-op for an empty query.
fn prioritize_by_path(
    paths: Vec<std::path::PathBuf>,
    _root: &Path,
    query: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let needle = alnum_lower(query.unwrap_or(""));
    let k = needle.len().min(4);
    if k == 0 {
        return paths;
    }
    let kgrams: std::collections::HashSet<&[u8]> = needle.as_bytes().windows(k).collect();
    // one pass, reusing a scratch buffer for the normalized stem and an O(1)
    // k-gram lookup — string-only, no per-file allocation
    let mut prio = Vec::new();
    let mut rest = Vec::new();
    let mut stem = String::new();
    for p in paths {
        stem.clear();
        if let Some(s) = p.file_stem() {
            stem.extend(
                s.to_string_lossy()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .map(|c| c.to_ascii_lowercase()),
            );
        }
        // shares a k-char run with the query (a common substring of length ≥ k)
        if stem.as_bytes().windows(k).any(|w| kgrams.contains(w)) {
            prio.push(p);
        } else {
            rest.push(p);
        }
    }
    prio.extend(rest);
    prio
}

/// Opportunistic, time-bounded indexing — warm the index a little per call so no
/// single query blocks on a full walk of a large repo. `active` (branch) files
/// are parsed first and ignore the budget (the working set stays fresh); then the
/// walk streams the rest, honoring `budget`. When `query` is set, files whose
/// *path* matches it are parsed first (a cheap, in-memory reorder of the
/// candidate list — no file reads), so a relevant symbol indexes fast. A sweep
/// that finishes within budget marks coverage `complete`, else `warming`.
pub fn index_budgeted(
    store: &mut Store,
    root: &Path,
    active: &[String],
    budget: Duration,
    query: Option<&str>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(store, root, active, &[], Some(budget), query)
}

/// Max files a single *bounded* (warming) pass walks before it stops. The walk
/// is cheap (stat-only), but on a huge repo it must not run the whole tree
/// (memory + latency); the deadline cuts it short sooner. An explicit `--index`
/// (unbounded) ignores this and walks everything. Overridable via
/// `RQ_COLLECT_CAP` (tuning / deterministic tests).
const COLLECT_CAP: usize = 50_000;

fn collect_cap() -> usize {
    std::env::var("RQ_COLLECT_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(COLLECT_CAP)
}

/// Parse workers the background warmer uses (`--jobs`); 0 = auto.
static PARSE_JOBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set the parse-worker count (from `--jobs`/`RQ_JOBS`); 0 restores auto.
pub fn set_parse_jobs(n: usize) {
    PARSE_JOBS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Parse workers for one indexer pass — the configured value, else `RQ_JOBS`,
/// else an auto default. Parsing is CPU-bound but writes serialize through one
/// SQLite writer, so flooding every core rarely pays; the default caps at 8.
pub fn parse_jobs() -> usize {
    let configured = PARSE_JOBS.load(std::sync::atomic::Ordering::Relaxed);
    if configured > 0 {
        return configured;
    }
    if let Some(n) = std::env::var("RQ_JOBS").ok().and_then(|v| v.parse().ok())
        && n > 0
    {
        return n;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores.clamp(1, 8)
}

/// Files buffered before a streaming write commits them — bounds per-transaction
/// size and how much parsed-but-unwritten work a cut-short pass can lose.
const WRITE_BATCH: usize = 512;

/// Accumulates parsed files and commits them to the store in `WRITE_BATCH`
/// chunks, so a long or cut-short index persists incrementally rather than in one
/// final write. The `stream_walk` sink for `run_index`.
struct BatchWriter<'a> {
    store: &'a mut Store,
    repo_id: i64,
    buf: Vec<crate::store::FileSymbols>,
    files: usize,
    symbols: usize,
    /// Cumulative time spent in `replace_files` (the single-writer store path) —
    /// surfaced under `-v` so we can see write vs. walk/parse contention.
    write_time: Duration,
}

impl<'a> BatchWriter<'a> {
    fn new(store: &'a mut Store, repo_id: i64) -> Self {
        Self {
            store,
            repo_id,
            buf: Vec::new(),
            files: 0,
            symbols: 0,
            write_time: Duration::ZERO,
        }
    }

    fn push(&mut self, fs: crate::store::FileSymbols) -> Result<(), Box<dyn std::error::Error>> {
        self.buf.push(fs);
        if self.buf.len() >= WRITE_BATCH {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.buf.is_empty() {
            let t = Instant::now();
            let (f, sy) = self.store.replace_files(self.repo_id, &self.buf)?;
            self.write_time += t.elapsed();
            self.files += f;
            self.symbols += sy;
            self.buf.clear();
        }
        Ok(())
    }
}

/// Source-file candidates from `git ls-files` — read out of git's index, not by
/// walking the filesystem. On a huge repo this is the difference between
/// answering and timing out: enumeration is O(index read), and source-extension
/// pathspecs make git hand back only files we can parse, so warming never burns
/// its budget re-traversing non-source trees. Tracked files only (untracked are
/// caught by an explicit `rq --index`'s filesystem walk). `None` outside a git
/// work tree, so the caller falls back to walking the filesystem.
fn git_source_candidates(root: &Path) -> Option<Vec<std::path::PathBuf>> {
    if !is_git_repo(root) {
        return None;
    }
    let globs: Vec<String> = lang::registry()
        .iter()
        .flat_map(|p| p.extensions().iter().map(|e| format!("*.{e}")))
        .collect();
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--"])
        .args(&globs);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| root.join(String::from_utf8_lossy(s).as_ref()))
            .collect(),
    )
}

/// A lazy, streaming filesystem walk of `roots` yielding file paths — the
/// fallback when git can't enumerate (an explicit unbounded index, or a non-git
/// dir). Honors `.gitignore`/hidden rules via the `ignore` crate.
fn fs_walk_candidates(roots: Vec<std::path::PathBuf>) -> impl Iterator<Item = std::path::PathBuf> {
    roots.into_iter().flat_map(|root| {
        WalkBuilder::new(&root)
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(ignore::DirEntry::into_path)
    })
}

/// The one fused walk→parse→consume engine. A walk thread streams the source
/// paths that `keep` selects (in walk order, the instant each is found) through a
/// bounded channel to a pool of parse workers; the workers parse in parallel
/// (skipping files that lack `needle`, when set) and stream each result to `sink`
/// on the calling thread. Bounded channels back-pressure the walk and workers so
/// neither runs ahead into unbounded memory; `deadline`/`cap` bound the pass.
/// `seen` is seeded by the caller and returned holding every source file walked
/// (for deletion reconcile). The bool is whether walk *and* parse finished within
/// budget. Streaming — never collect-then-parse — is what keeps a pass too big to
/// finish from making zero progress.
///
/// `run_index` sinks to the store (writing in batches via [`BatchWriter`]); the
/// live [`scan`] sinks into a `Vec` it returns — same engine, different consumer.
#[allow(clippy::too_many_arguments)]
fn stream_walk(
    root: &Path,
    candidates: impl Iterator<Item = std::path::PathBuf> + Send,
    deadline: Option<Instant>,
    cap: Option<usize>,
    needle: Option<&[u8]>,
    seen: HashSet<String>,
    keep: impl Fn(&str, &Path) -> bool + Send,
    mut sink: impl FnMut(crate::store::FileSymbols) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(HashSet<String>, bool), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let workers = parse_jobs();
    let parse_incomplete = AtomicBool::new(false);
    let (path_tx, path_rx) = std::sync::mpsc::sync_channel::<std::path::PathBuf>(1024);
    let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<crate::store::FileSymbols>(1024);
    let path_rx = Arc::new(Mutex::new(path_rx));

    let (seen, walk_finished) = std::thread::scope(|s| -> Result<_, Box<dyn std::error::Error>> {
        // walk thread: stream every kept source path to the workers, in order, the
        // instant it's found. No buffering or deferral — on a repo too big to
        // finish in budget, anything held back would never be sent.
        let walk = s.spawn(move || {
            let mut seen = seen;
            let mut finished = true;
            let mut processed = 0usize;
            for path in candidates {
                if past(deadline) {
                    finished = false;
                    break;
                }
                let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                if lang::plugin_for_extension(ext).is_none() {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if !seen.insert(rel.clone()) {
                    continue; // already handled (active file), or a duplicate
                }
                if !keep(&rel, &path) {
                    continue; // caller skipped it (unchanged / already indexed)
                }
                if path_tx.send(path).is_err() {
                    finished = false; // workers gone (deadline) — walk didn't complete
                    break;
                }
                processed += 1;
                if cap.is_some_and(|c| processed >= c) {
                    finished = false;
                    break;
                }
            }
            drop(path_tx); // close → workers drain and exit
            (seen, finished)
        });

        // parse workers: pull paths, parse (with the content pre-filter) in
        // parallel, stream results out
        let parse_incomplete = &parse_incomplete;
        for _ in 0..workers {
            let path_rx = Arc::clone(&path_rx);
            let res_tx = res_tx.clone();
            s.spawn(move || {
                loop {
                    let got = { path_rx.lock().unwrap().recv() };
                    let Ok(path) = got else { break }; // channel closed
                    if past(deadline) {
                        parse_incomplete.store(true, Ordering::Relaxed); // backlog abandoned
                        break;
                    }
                    if let Some(fs) = parse_file(root, &path, needle)
                        && res_tx.send(fs).is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(res_tx); // the workers hold the live clones

        // consumer (this thread): hand each parsed file to the sink as it arrives
        while let Ok(fs) = res_rx.recv() {
            sink(fs)?;
        }
        Ok(walk.join().unwrap())
    })?;

    Ok((
        seen,
        walk_finished && !parse_incomplete.load(Ordering::Relaxed),
    ))
}

/// The shared indexing core behind both the explicit (`index_under`) and
/// opportunistic (`index_budgeted`) paths, run as a single fused pipeline: one
/// walk thread streams candidate paths (cheap, stat-only, mtime-skipping
/// unchanged files), a pool of parse workers turns them into symbols in parallel,
/// and this thread writes the results in batches **as they arrive** — so a pass
/// cut short by its budget still persists everything parsed up to that point, and
/// indexing starts the instant the first file is found (walk and parse overlap).
///
/// `active` files are parsed first and ignore `budget` (the working set stays
/// fresh); then the walk streams the rest in walk order. `subdirs` (empty = whole
/// repo) scope the walk; `budget` bounds it (`None` = unbounded). A whole-repo
/// sweep that finishes within budget reconciles deletions and is `complete`; a
/// sweep cut short is `warming`; a deliberate subtree is `partial`.
fn run_index(
    store: &mut Store,
    root: &Path,
    active: &[String],
    subdirs: &[String],
    budget: Option<Duration>,
    query: Option<&str>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let identity = detect_identity(root);
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo_id = store.upsert_repository(&identity, branch.as_deref())?;
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.upsert_checkout(repo_id, &root_display.to_string_lossy(), branch.as_deref())?;

    let stored = store.file_mtimes(repo_id)?;
    let mut seen: HashSet<String> = HashSet::new();

    // A cold, unbounded index (no prior coverage, the explicit `rq --index`)
    // suspends per-row FTS maintenance and rebuilds the trigram index in one bulk
    // pass at the end — the per-row trigger is ~70% of the write cost. Scoped to
    // the cold full path so incremental re-index and budgeted warming (which may
    // run concurrently and only touch a few files) keep the per-row trigger.
    let bulk_fts = budget.is_none() && stored.is_empty();
    if bulk_fts {
        store.defer_fts_insert()?;
    }

    // Active (branch) files first: always parsed and written, so the working set
    // stays fresh even when a tight budget cuts the walk short.
    let mut active_to_parse: Vec<std::path::PathBuf> = Vec::new();
    for rel in active {
        note_candidate(
            root,
            &root.join(rel),
            &stored,
            &mut seen,
            &mut active_to_parse,
        );
    }
    let (active_parsed, _) = parse_files(root, &active_to_parse, None, None);
    let (mut files_indexed, mut symbols) = store.replace_files(repo_id, &active_parsed)?;

    // walk the whole repo, or just the requested subtrees — paths stay relative
    // to `root` so they're repo-relative either way
    let walk_roots: Vec<std::path::PathBuf> = if subdirs.is_empty() {
        vec![root.to_path_buf()]
    } else {
        subdirs.iter().map(|s| root.join(s)).collect()
    };

    // Enumerate candidates. A budgeted (warming) pass on a git repo reads git's
    // index — O(index read), no filesystem traversal — so a huge repo isn't stuck
    // re-walking non-source trees every pass and never reaching source. An
    // explicit unbounded index, or a non-git dir, walks the filesystem (thorough;
    // catches untracked files). `git ls-files` runs *before* the deadline so its
    // (cheap) work never eats the parse budget.
    // An empty result means nothing is tracked yet (a fresh/uncommitted repo), so
    // fall back to the filesystem walk, which sees untracked files.
    let git_candidates = budget
        .and_then(|_| git_source_candidates(root))
        .filter(|paths| !paths.is_empty());
    let candidates: Box<dyn Iterator<Item = std::path::PathBuf> + Send> = match git_candidates {
        // parse query-relevant files (by path) first — a cheap in-memory reorder
        Some(paths) => Box::new(prioritize_by_path(paths, root, query).into_iter()),
        None => Box::new(fs_walk_candidates(walk_roots)),
    };

    let deadline = budget.map(|b| Instant::now() + b);
    let cap = budget.map(|_| collect_cap());

    // Fused walk → parse → write: stream candidates through the shared pipeline,
    // committing parsed files in batches as they arrive (so a budget-cut or killed
    // pass keeps what it parsed). Only new or changed files are parsed; every
    // source file seen lands in `seen` for deletion reconcile.
    let stored_ref = &stored;
    let keep = |rel: &str, path: &Path| match stored_ref.get(rel) {
        Some(&Some(m)) => Some(m) != file_mtime(path),
        _ => true, // new file, or one stored without an mtime
    };
    let stream_start = Instant::now();
    let (seen, completed, walk_files, walk_symbols, write_time) = {
        let mut writer = BatchWriter::new(&mut *store, repo_id);
        let (seen, completed) =
            stream_walk(root, candidates, deadline, cap, None, seen, keep, |fs| {
                writer.push(fs)
            })?;
        writer.flush()?;
        (
            seen,
            completed,
            writer.files,
            writer.symbols,
            writer.write_time,
        )
    };
    if crate::trace::enabled() {
        let elapsed = stream_start.elapsed();
        crate::trace!(
            "walk+parse+write {} file(s)/{} symbol(s) in {} ms ({} ms in store writes, {} parse jobs)",
            walk_files,
            walk_symbols,
            elapsed.as_millis(),
            write_time.as_millis(),
            parse_jobs(),
        );
    }
    if bulk_fts {
        let t = crate::trace::Timer::start("fts bulk rebuild");
        store.rebuild_fts()?;
        drop(t);
    }
    files_indexed += walk_files;
    symbols += walk_symbols;
    let stats = Stats {
        files_seen: seen.len(),
        files_indexed,
        symbols,
    };

    let whole_repo = subdirs.is_empty();
    // a finished whole-repo sweep saw every live file → anything still indexed
    // (but not seen) was deleted on disk
    if completed && whole_repo {
        let mut forgotten = 0;
        for path in stored.keys() {
            if !seen.contains(path) {
                store.forget_file(repo_id, path)?;
                forgotten += 1;
            }
        }
        if forgotten > 0 {
            crate::trace!(
                "reconcile {}: forgot {forgotten} file(s) not seen on disk",
                crate::trace::abbrev(&root_display)
            );
        }
        // record the commit the index now reflects, so a later search can detect
        // an unchanged committed tree and skip re-walking a large repo
        if let Some(head) = git_head(root) {
            let _ = store.set_indexed_head(repo_id, &head);
        }
    }
    // commit times feed the recency signal, but `git log -n1000 --name-only` is
    // pricey on a big repo. Run it only when this run indexed something AND
    // `root` is the work-tree root: a subdir index's `git log` walks the whole
    // repo's history yet emits repo-relative paths that wouldn't match our
    // subdir-relative ones — pure waste. (A subdir index leans on mtime recency.)
    if stats.files_indexed > 0 && repo_root(root).is_some_and(|r| r == root_display) {
        let times = git_commit_times(root, 1000);
        if !times.is_empty() {
            let _ = store.set_file_git_ts(repo_id, &times);
        }
    }

    let status = if !whole_repo {
        "partial"
    } else if completed {
        "complete"
    } else {
        "warming"
    };
    store.set_coverage(
        repo_id,
        stats.files_seen as i64,
        stats.files_indexed as i64,
        status,
    )?;
    crate::trace!(
        "index {} (budget {budget:?}): {} seen, {} indexed, {} symbols → {status}",
        crate::trace::abbrev(&root_display),
        stats.files_seen,
        stats.files_indexed,
        stats.symbols,
    );
    Ok(stats)
}

/// Note a walked file: record every source file in `seen` (for deletion
/// reconcile), and queue it for parsing only when it's new or its mtime moved —
/// a cheap `stat` skips unchanged files before any read. Non-source files are
/// ignored entirely.
fn note_candidate(
    root: &Path,
    file: &Path,
    stored: &HashMap<String, Option<i64>>,
    seen: &mut HashSet<String>,
    to_parse: &mut Vec<std::path::PathBuf>,
) {
    let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
        return;
    };
    if lang::plugin_for_extension(ext).is_none() {
        return;
    }
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    if !seen.insert(rel.clone()) {
        return; // already noted (e.g. an active file re-seen by the walk)
    }
    // unchanged by mtime → already indexed, no need to re-parse
    if let Some(&Some(m)) = stored.get(&rel)
        && Some(m) == file_mtime(file)
    {
        return;
    }
    to_parse.push(file.to_path_buf());
}

/// Read + parse one source file into a [`FileSymbols`], or `None` if it isn't a
/// known language, can't be read, or (when `needle` is set) doesn't contain the
/// query — the ripgrep-style content pre-filter, applied here so it runs on the
/// worker thread. Touches no store — safe to run in parallel (each call builds
/// its own Tree-sitter parser).
fn parse_file(
    root: &Path,
    file: &Path,
    needle: Option<&[u8]>,
) -> Option<crate::store::FileSymbols> {
    let ext = file.extension().and_then(|e| e.to_str())?;
    let plugin = lang::plugin_for_extension(ext)?;
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    let source = std::fs::read_to_string(file).ok()?;
    // pre-filter: skip the expensive parse on files that can't hold the match
    if let Some(n) = needle
        && !contains_ascii_ci(source.as_bytes(), n)
    {
        return None;
    }
    let content_hash = content_hash(&source);
    let symbols = plugin.extract(&rel, &source);
    let language = symbols
        .first()
        .map(|s| s.language.clone())
        .unwrap_or_else(|| "unknown".to_string());
    Some(crate::store::FileSymbols {
        path: rel,
        language,
        mtime: file_mtime(file),
        content_hash,
        symbols,
    })
}

/// Whether an optional deadline has passed (always false when unbounded).
fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Parse many files across the available CPUs, stopping early once `deadline`
/// passes; when `needle` is set, each worker skips files that don't contain it
/// (the content pre-filter). Returns the parsed files and whether *all* of them
/// were parsed (false if the deadline cut it short). Parsing is the expensive,
/// CPU-bound step; writing stays serialized in one batched transaction by the
/// caller.
fn parse_files(
    root: &Path,
    paths: &[std::path::PathBuf],
    deadline: Option<Instant>,
    needle: Option<&[u8]>,
) -> (Vec<crate::store::FileSymbols>, bool) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let workers = parse_jobs().min(paths.len());

    if workers <= 1 {
        let mut out = Vec::new();
        for p in paths {
            if past(deadline) {
                return (out, false);
            }
            if let Some(parsed) = parse_file(root, p, needle) {
                out.push(parsed);
            }
        }
        return (out, true);
    }

    let bailed = AtomicBool::new(false);
    let chunk_size = paths.len().div_ceil(workers);
    let mut out = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .map(|chunk| {
                let bailed = &bailed;
                s.spawn(move || {
                    let mut local = Vec::new();
                    for p in chunk {
                        if past(deadline) {
                            bailed.store(true, Ordering::Relaxed);
                            break;
                        }
                        if let Some(parsed) = parse_file(root, p, needle) {
                            local.push(parsed);
                        }
                    }
                    local
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().unwrap_or_default());
        }
    });
    (out, !bailed.load(Ordering::Relaxed))
}

/// Map of repo-relative path → most-recent commit time (unix seconds), from the
/// last `limit` commits. Paths are repo-root-relative, matching the indexed
/// paths when `root` is the repository root.
fn git_commit_times(root: &Path, limit: usize) -> HashMap<String, i64> {
    match git_output(
        root,
        &[
            "log",
            &format!("-n{limit}"),
            "--name-only",
            "--pretty=format:%ct",
        ],
    ) {
        Some(text) => parse_git_log(&text),
        None => HashMap::new(),
    }
}

/// Parse `git log --name-only --pretty=format:%ct` output into path → latest
/// commit time. Newest-first, so the first time a path appears is its most
/// recent commit.
fn parse_git_log(text: &str) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    let mut current_ts = 0i64;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(ts) = line.parse::<i64>() {
            // a commit-timestamp header (filenames that are pure integers don't
            // occur in practice)
            current_ts = ts;
        } else {
            map.entry(line.to_string()).or_insert(current_ts);
        }
    }
    map
}

/// Live, budgeted scan (search Layer 4): stream-walk `root` on the same fused
/// [`stream_walk`] engine as the indexer, parsing source files and returning the
/// parsed `FileSymbols` *without* touching the store — so `rq` answers at zero
/// coverage. Bounded and filtered:
/// - stop once `deadline` passes;
/// - skip any file whose repo-relative path is in `skip` (already indexed);
/// - when `needle` is set, parse only files containing it (case-insensitive
///   substring) — the ripgrep-style pre-filter that skips the tree-sitter parse
///   on files that can't hold an exact/prefix/substring match. `needle` is `None`
///   for the *fuzzy* fallback: an abbreviation (`usr` → `user`) isn't a substring
///   of its match, so it can't be content-filtered; callers retry unfiltered when
///   a filtered scan comes up empty.
///
/// The caller decides the fate of the result, which is exactly where the
/// persist-or-not policy lives: a warming git repo **persists** them via
/// `replace_files` (folds the scan into the index — demand-first coverage); a
/// non-git dir ranks them in-memory and discards them (there's no index to fold
/// into). Streaming — never collect-then-parse — keeps a scan too big to finish
/// from coming up empty.
pub fn scan(
    root: &Path,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
    needle: Option<&[u8]>,
) -> Vec<crate::store::FileSymbols> {
    let needle = needle.filter(|n| !n.is_empty());
    // git's index for a git repo (content-scan a huge repo without traversing it),
    // else a filesystem walk (the live scan of a non-git dir)
    let candidates: Box<dyn Iterator<Item = std::path::PathBuf> + Send> =
        match git_source_candidates(root).filter(|paths| !paths.is_empty()) {
            Some(paths) => Box::new(paths.into_iter()),
            None => Box::new(fs_walk_candidates(vec![root.to_path_buf()])),
        };
    let mut out: Vec<crate::store::FileSymbols> = Vec::new();
    let keep = |rel: &str, _: &Path| !skip.contains(rel); // skip already-indexed
    let _ = stream_walk(
        root,
        candidates,
        deadline,
        None,
        needle,
        HashSet::new(),
        keep,
        |fs| {
            out.push(fs);
            Ok(())
        },
    );
    out
}

/// Case-insensitive (ASCII) substring test — `haystack` contains `needle`.
/// Allocation-free; used to pre-filter live-scan files before parsing.
fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Result of revalidating a single file against what's on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Nothing to do — content hash still matches, or the file couldn't be read
    /// right now (left in place rather than forgotten — see [`refresh_file`]).
    Unchanged,
    /// File changed; its symbols were re-extracted.
    Updated,
}

/// Whether `root` is inside a git work tree. Implicit (opportunistic) indexing
/// is gated on this so a stray query never walks a non-repo directory. Native
/// (no `git` fork) — it runs on every search.
pub fn is_git_repo(root: &Path) -> bool {
    repo_root(root).is_some()
}

/// The git work-tree root at or above `path` — the nearest ancestor holding a
/// `.git` entry — found without shelling out. `.git` may be a directory or a
/// file (worktrees, submodules), so we test existence either way. `None` when
/// `path` is not inside a work tree.
pub fn repo_root(path: &Path) -> Option<std::path::PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The current HEAD commit sha, or `None` outside a git work tree.
pub fn git_head(root: &Path) -> Option<String> {
    git_output(root, &["rev-parse", "HEAD"])
}

/// Whether the work tree has uncommitted changes (tracked or untracked).
/// `git status --porcelain` prints nothing when clean, so empty stdout (which
/// `git_output` reports as `None`) means clean.
pub fn is_dirty(root: &Path) -> bool {
    git_output(root, &["status", "--porcelain"]).is_some()
}

/// Repo-relative files you're working on this branch: committed changes since
/// the branch diverged from the trunk, plus uncommitted edits. Empty on the
/// trunk itself (where it isn't a useful signal) or outside git. Feeds the
/// branch ranking boost — necessarily a few git calls, but gated to feature
/// branches.
pub fn branch_changed_files(root: &Path) -> Vec<String> {
    let Some(branch) = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return Vec::new();
    };
    if is_trunk(&branch) {
        return Vec::new();
    }
    let Some(trunk) = trunk_ref(root) else {
        return Vec::new();
    };

    let mut files: HashMap<String, ()> = HashMap::new();
    // committed branch changes since divergence from the trunk (three-dot)
    if let Some(out) = git_output(root, &["diff", "--name-only", &format!("{trunk}...HEAD")]) {
        files.extend(
            out.lines()
                .filter(|l| !l.is_empty())
                .map(|l| (l.to_string(), ())),
        );
    }
    // uncommitted edits to tracked files
    if let Some(out) = git_output(root, &["diff", "--name-only", "HEAD"]) {
        files.extend(
            out.lines()
                .filter(|l| !l.is_empty())
                .map(|l| (l.to_string(), ())),
        );
    }
    files.into_keys().collect()
}

/// Branch names treated as the trunk — the "active files" signal doesn't apply
/// there (you're not on a feature branch).
fn is_trunk(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "trunk")
}

/// The trunk ref to diff against: `main` if it exists, else `master`.
fn trunk_ref(root: &Path) -> Option<String> {
    ["main", "master"]
        .into_iter()
        .find(|name| git_output(root, &["rev-parse", "--verify", "--quiet", name]).is_some())
        .map(str::to_string)
}

/// Lazily revalidate one indexed file against disk: re-extract it if its content
/// changed. This is the staleness check search runs over its top results.
///
/// It deliberately **never forgets** a file: a failed read isn't proof of
/// deletion (a wrong checkout root, a transient FS error, or a race all look the
/// same), and a search must never destroy index data over it — that bug dropped
/// whole indexes when a stale checkout root made every read fail. Genuine
/// deletions are reconciled by an indexing pass ([`run_index`]), which sees the
/// whole tree at once and can tell "gone" from "couldn't read one file".
pub fn refresh_file(
    store: &mut Store,
    repository_id: i64,
    root: &Path,
    rel: &str,
) -> Result<Refresh, Box<dyn std::error::Error>> {
    let path = root.join(rel);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(Refresh::Unchanged), // unreadable now — leave it, don't forget
    };
    let hash = content_hash(&source);
    if store.file_unchanged(repository_id, rel, &hash)? {
        return Ok(Refresh::Unchanged);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let symbols = match lang::plugin_for_extension(ext) {
        Some(plugin) => plugin.extract(rel, &source),
        None => Vec::new(),
    };
    let language = symbols
        .first()
        .map(|s| s.language.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let mtime = file_mtime(&path);
    store.replace_file_symbols(repository_id, rel, &language, mtime, &hash, &symbols)?;
    Ok(Refresh::Updated)
}

/// Best-effort repository identity: upstream git remote, else the local path.
pub fn detect_identity(root: &Path) -> RepoIdentity {
    for remote in ["origin", "upstream"] {
        if let Some(url) = git_output(root, &["remote", "get-url", remote])
            && let Some(id) = RepoIdentity::from_remote_url(&url)
        {
            return id;
        }
    }
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    RepoIdentity::local(&abs.to_string_lossy())
}

/// Run a git command in `root`, returning trimmed stdout on success.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn content_hash(source: &str) -> String {
    // DefaultHasher uses fixed keys, so this is stable across runs — enough for
    // change detection (not cryptographic).
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn file_mtime(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(secs as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_stable_and_distinguishes() {
        assert_eq!(
            content_hash("class Foo\nend"),
            content_hash("class Foo\nend")
        );
        assert_ne!(
            content_hash("class Foo\nend"),
            content_hash("class Bar\nend")
        );
    }

    #[test]
    fn trunk_names_are_recognized() {
        assert!(is_trunk("main"));
        assert!(is_trunk("master"));
        assert!(!is_trunk("feature/x"));
        assert!(!is_trunk("dpep/fix"));
    }

    #[test]
    fn prioritize_by_path_is_loose_but_targeted() {
        let root = Path::new("/repo");
        let paths: Vec<std::path::PathBuf> = [
            "companies.rb",         // unrelated → tail
            "app/employee.rb",      // near-match → front
            "lib/EmpController.rb", // near-match (shares "cont…") → front
            "employers.rb",         // near-match (shares "employe") → front
            "app/controllers/x.rb", // dir matches but stem doesn't → tail
        ]
        .iter()
        .map(|p| root.join(p))
        .collect();
        let out = prioritize_by_path(paths.clone(), root, Some("employeescontroller"));
        let name = |p: &std::path::PathBuf| p.file_name().unwrap().to_str().unwrap().to_string();
        let front: Vec<String> = out[..3].iter().map(name).collect();
        assert!(front.contains(&"employee.rb".to_string()), "{front:?}");
        assert!(front.contains(&"EmpController.rb".to_string()), "{front:?}");
        assert!(front.contains(&"employers.rb".to_string()), "{front:?}");
        let tail: Vec<String> = out[3..].iter().map(name).collect();
        assert!(tail.contains(&"companies.rb".to_string()), "{tail:?}");
        assert!(tail.contains(&"x.rb".to_string()), "{tail:?}"); // dir match isn't enough
        // no query → unchanged
        assert_eq!(prioritize_by_path(paths.clone(), root, None), paths);
    }

    #[test]
    fn detects_git_work_tree_natively() {
        let dir = std::env::temp_dir().join(format!("rq-reporoot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        assert!(!is_git_repo(&dir), "no .git yet");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(is_git_repo(&dir), "a .git entry marks a work tree");
        // from a subdirectory, repo_root walks up to the work-tree root
        assert_eq!(
            repo_root(&dir.join("sub")).unwrap(),
            dir.canonicalize().unwrap()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_git_log_keeping_most_recent_commit_per_file() {
        // newest-first: a.rb appears in both commits; the newer ts wins
        let log = "1700000000\n\na.rb\nb.rb\n1699990000\n\na.rb\nc.rb\n";
        let map = parse_git_log(log);
        assert_eq!(map.get("a.rb"), Some(&1700000000));
        assert_eq!(map.get("b.rb"), Some(&1700000000));
        assert_eq!(map.get("c.rb"), Some(&1699990000));
        assert_eq!(map.len(), 3);
    }
}
