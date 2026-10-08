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
use crate::store::{Checkout, Coverage, Store};

/// Path → the content hashes of the versions a repo holds there
/// ([`Store::versions`]): a file hashing to one of them needs no parse.
type Versions = HashMap<String, Vec<String>>;

/// Outcome of an indexing run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Files matching a known language that were walked.
    pub files_seen: usize,
    /// Files (re)indexed this run: parsed, or mapped to a version the repo
    /// already held (unchanged files are skipped).
    pub files_indexed: usize,
    /// Of those, the ones parsed: versions the repo didn't hold.
    pub files_parsed: usize,
    /// Symbols written this run.
    pub symbols: usize,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, o: Stats) {
        self.files_seen += o.files_seen;
        self.files_indexed += o.files_indexed;
        self.files_parsed += o.files_parsed;
        self.symbols += o.symbols;
    }
}

/// Index the whole repository rooted at `root`. The CLI calls [`index_under`]
/// directly (it has subdirs to pass); this spelling exists for tests.
#[cfg(test)]
pub(crate) fn index_path(
    store: &mut Store,
    root: &Path,
) -> Result<Stats, Box<dyn std::error::Error>> {
    index_under(store, root, &[])
}

/// Index `root`, or — when `subdirs` is non-empty — only those repo-relative
/// subtrees of it. Unbounded: an explicit index is thorough. A whole-repo index
/// also reconciles deletions; a subtree index is a *seed* (it gets those files
/// in first) that leaves coverage `warming`, so normal warming continues over
/// the rest of the repo through use.
pub(crate) fn index_under(
    store: &mut Store,
    root: &Path,
    subdirs: &[String],
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(store, root, &[], subdirs, None, None, None, None)
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
/// walk streams the rest, honoring `budget`. When `query` (a search query) is
/// set, files containing its leaf name are parsed ahead of the walk, and files
/// whose *path* resembles it lead the walk, so a relevant symbol indexes fast. A
/// sweep that finishes within budget marks coverage `complete`, else `warming`.
pub(crate) fn index_budgeted(
    store: &mut Store,
    root: &Path,
    active: &[String],
    budget: Duration,
    query: Option<&str>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(store, root, active, &[], Some(budget), query, None, None)
}

/// Like [`index_budgeted`], but the pass stops promptly when `cancel` is set —
/// the interactive cold-start escalation (see the CLI's search path) runs a long,
/// generous-budget warm and lets the user abort it with Ctrl-C without losing the
/// batches already committed. `demanded` is set once every file containing the
/// query's leaf name is indexed: from then on no unread file can hold an exact
/// or prefix match for it.
pub(crate) fn index_budgeted_cancellable(
    store: &mut Store,
    root: &Path,
    active: &[String],
    budget: Duration,
    query: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
    demanded: &std::sync::atomic::AtomicBool,
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(
        store,
        root,
        active,
        &[],
        Some(budget),
        query,
        Some(cancel),
        Some(demanded),
    )
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
pub(crate) fn set_parse_jobs(n: usize) {
    PARSE_JOBS.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Parse workers for one indexer pass — the configured value, else `RQ_JOBS`,
/// else one per available core.
///
/// Parsing keeps scaling to the core count: writes overlap it on the consumer
/// thread rather than serializing behind it, so the single SQLite writer is not
/// the ceiling a former cap of 8 assumed (see docs/DECISIONS.md D5). Going
/// *past* the core count buys nothing, and `available_parallelism` has the
/// useful property of reporting the cgroup/affinity budget in a container
/// rather than the host's core count.
pub(crate) fn parse_jobs() -> usize {
    let configured = PARSE_JOBS.load(std::sync::atomic::Ordering::Relaxed);
    if configured > 0 {
        return configured;
    }
    if let Some(n) = std::env::var("RQ_JOBS").ok().and_then(|v| v.parse().ok())
        && n > 0
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Summed per-file parse time across every worker, in microseconds. Profiling
/// only, and CPU time rather than wall time — the workers overlap each other and
/// the writer, so this exceeds the phase it sits inside.
static PARSE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Files buffered before a streaming write commits them — bounds per-transaction
/// size and how much parsed-but-unwritten work a cut-short pass can lose.
const WRITE_BATCH: usize = 512;

/// Longest a parsed file waits for a commit once more arrive — a warming search
/// can only see committed files, and a demand scan finds matches too sparsely to
/// fill a batch quickly.
const WRITE_INTERVAL: Duration = Duration::from_millis(50);

/// Accumulates parsed files and commits them to the store in `WRITE_BATCH`
/// chunks, so a long or cut-short index persists incrementally rather than in one
/// final write. The `stream_walk` sink for `run_index`.
struct BatchWriter<'a> {
    store: &'a mut Store,
    checkout: Checkout,
    root: &'a Path,
    buf: Vec<crate::store::FileSymbols>,
    written: Stats,
    /// Cumulative time spent in `replace_files` (the single-writer store path) —
    /// surfaced under `-v` so we can see write vs. walk/parse contention.
    write_time: Duration,
    /// Transactions committed — `write_time` per batch is the number that says
    /// whether batching is sized right.
    batches: usize,
    last_flush: Option<Instant>,
    /// This pass's mark, when it holds one, and when it last renewed it.
    mark: Option<(&'a str, Instant)>,
}

impl<'a> BatchWriter<'a> {
    fn new(store: &'a mut Store, checkout: Checkout, root: &'a Path) -> Self {
        Self {
            store,
            checkout,
            root,
            buf: Vec::new(),
            written: Stats::default(),
            write_time: Duration::ZERO,
            batches: 0,
            last_flush: None,
            mark: None,
        }
    }

    fn push(&mut self, fs: crate::store::FileSymbols) -> Result<(), Box<dyn std::error::Error>> {
        self.buf.push(fs);
        if self.buf.len() >= WRITE_BATCH
            || self
                .last_flush
                .is_none_or(|t| t.elapsed() >= WRITE_INTERVAL)
        {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.buf.is_empty() {
            let t = Instant::now();
            self.written += write_files(self.store, self.checkout, self.root, &self.buf)?;
            self.write_time += t.elapsed();
            self.batches += 1;
            self.buf.clear();
            self.last_flush = Some(Instant::now());
            if let Some((root, renewed)) = &mut self.mark
                && renewed.elapsed() >= crate::store::PASS_RENEWAL
            {
                // best-effort: a missed renewal costs nothing until the TTL
                let _ = self.store.renew_pass(root, std::process::id());
                *renewed = Instant::now();
            }
        }
        Ok(())
    }
}

/// Write parsed files into the checkout. A file sent unparsed, for a version
/// another checkout let go of since this pass loaded the repo's versions, is
/// parsed and written again, so every file lands. Returns what it wrote.
fn write_files(
    store: &mut Store,
    checkout: Checkout,
    root: &Path,
    files: &[crate::store::FileSymbols],
) -> crate::store::Result<Stats> {
    let stats = |w: &crate::store::Written| Stats {
        files_seen: 0,
        files_indexed: w.files,
        files_parsed: w.versions,
        symbols: w.symbols,
    };
    let written = store.replace_files(checkout, files)?;
    let mut out = stats(&written);
    if !written.unmapped.is_empty() {
        let paths: Vec<_> = written.unmapped.iter().map(|p| root.join(p)).collect();
        let (parsed, _) = parse_files(root, &paths, None, None, &Versions::new());
        out += stats(&store.replace_files(checkout, &parsed)?);
    }
    Ok(out)
}

/// Source-file candidates from `git ls-files`, as index keys in git's order —
/// read out of git's index, not by walking the filesystem. On a huge repo
/// this is the difference between answering and timing out: enumeration is
/// O(index read), and source-extension pathspecs make git hand back only
/// files we can parse. Tracked files only: an explicit `rq --index`'s
/// filesystem walk finds untracked ones, and a warm keeps those the index
/// holds (D53). What git lists isn't yet what a pass reads: see
/// [`walk_reaches`]. `None` outside a git work tree, so the caller falls back
/// to walking the filesystem.
fn git_listed(root: &Path) -> Option<Vec<String>> {
    if !is_git_repo(root) {
        return None;
    }
    let globs: Vec<String> = lang::registry()
        .iter()
        .flat_map(|p| p.extensions().iter().map(|e| format!("*.{e}")))
        .collect();
    let out = git(root)
        // a submodule's files are in the checkout, as the walk sees them
        .args(["ls-files", "-z", "--cached", "--recurse-submodules", "--"])
        .args(&globs)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&b| b == 0)
            // a name that isn't UTF-8 has no key (see `rel_key`)
            .filter_map(|p| std::str::from_utf8(p).ok())
            .filter(|key| is_source(key))
            .map(str::to_owned)
            .collect(),
    )
}

/// The filesystem walk every pass answers to: `.gitignore`, `.ignore` and
/// git's excludes, hidden entries skipped, symlinks not followed. An explicit
/// index enumerates with it; a warm, which lists with git, keeps only what it
/// reaches ([`walk_reaches`]) — so both read the same files.
fn walk(root: &Path) -> WalkBuilder {
    WalkBuilder::new(root)
}

/// What [`walk_reaches`] reached of the keys it was given, each spelled as the
/// walk spells it: an explicit index holds that spelling, so a warm must too.
#[derive(Debug, Default)]
struct Reached(HashMap<String, String>);

impl Reached {
    /// The walk's spelling of `key`, if the walk reaches it.
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The walk's spellings of `keys`, in their order, each once: two keys a
    /// case-insensitive filesystem holds as one file are one file.
    fn spell<'a>(&'a self, keys: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut seen = HashSet::new();
        keys.into_iter()
            .filter_map(|k| self.get(k))
            .filter(|k| seen.insert(*k))
            .map(str::to_owned)
            .collect()
    }
}

/// Which of `keys` (index keys below `root`) [`walk`] reaches as a regular
/// file: exactly what an explicit index's walk would read of them. Descends
/// only into directories holding one of them, so it costs a directory read
/// per such directory, not a walk of the tree. The directories it couldn't
/// read come back too: a key under one is neither reached nor gone.
///
/// A key names a file by git's spelling, which can differ from the disk's
/// where the filesystem folds case or Unicode normalization (a directory
/// renamed `Src` → `src` behind git's back, an NFD name git lists as NFC).
/// A key the walk doesn't spell is matched again by the file it names, so the
/// filesystem's own rule decides what is the same name.
fn walk_reaches<'k>(
    root: &Path,
    keys: impl IntoIterator<Item = &'k str>,
) -> (Reached, Vec<std::path::PathBuf>) {
    let want: HashSet<&str> = keys.into_iter().collect();
    if want.is_empty() {
        return (Reached::default(), Vec::new());
    }
    let dirs = ancestors(want.iter().copied());
    let (exact, mut unwalked) = walk_matching(
        root,
        |rel, _| dirs.contains(rel),
        |rel, _| want.contains(rel).then(|| rel.to_owned()),
    );
    let mut reached: HashMap<String, String> =
        exact.into_iter().map(|(k, _)| (k.clone(), k)).collect();
    let missed: Vec<&str> = want
        .into_iter()
        .filter(|k| !reached.contains_key(*k))
        .collect();
    let (respelled, unread) = respelled(root, &missed);
    unwalked.extend(unread);
    unwalked.sort();
    unwalked.dedup();
    for (key, spelled) in respelled {
        // the walk's spelling reaches itself, so spelling twice changes nothing
        reached
            .entry(spelled.clone())
            .or_insert_with(|| spelled.clone());
        reached.entry(key).or_insert(spelled);
    }
    (Reached(reached), unwalked)
}

/// Every directory above `keys`, as keys.
fn ancestors<'k>(keys: impl IntoIterator<Item = &'k str>) -> HashSet<&'k str> {
    let mut dirs = HashSet::new();
    for key in keys {
        let mut dir = key;
        while let Some(i) = dir.rfind('/') {
            dir = &dir[..i];
            if !dirs.insert(dir) {
                break; // and so are its ancestors
            }
        }
    }
    dirs
}

/// Walk `root` with [`walk`], descending into the directories `descend`
/// accepts, and collecting `(key, walk's spelling)` for each regular file
/// `take` names a key for.
fn walk_matching(
    root: &Path,
    descend: impl Fn(&str, &ignore::DirEntry) -> bool + Sync,
    take: impl Fn(&str, &ignore::DirEntry) -> Option<String> + Sync,
) -> (Vec<(String, String)>, Vec<std::path::PathBuf>) {
    use ignore::WalkState;
    use std::sync::Mutex;
    let (found, unwalked) = (Mutex::new(Vec::new()), Mutex::new(Vec::new()));
    walk(root).threads(parse_jobs()).build_parallel().run(|| {
        let (descend, take, found, unwalked) = (&descend, &take, &found, &unwalked);
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let at = unwalked_at(&e, root);
                    unwalked.lock().expect("unwalked lock").extend(at);
                    return WalkState::Continue;
                }
            };
            let Some(rel) = entry.path().strip_prefix(root).ok().and_then(Path::to_str) else {
                return WalkState::Continue;
            };
            match entry.file_type() {
                _ if entry.depth() == 0 => WalkState::Continue,
                Some(t) if t.is_dir() && !descend(rel, &entry) => WalkState::Skip,
                Some(t) if t.is_file() => {
                    if let Some(key) = take(rel, &entry) {
                        found
                            .lock()
                            .expect("found lock")
                            .push((key, rel.to_owned()));
                    }
                    WalkState::Continue
                }
                _ => WalkState::Continue,
            }
        })
    });
    (
        found.into_inner().expect("found lock"),
        unwalked.into_inner().expect("unwalked lock"),
    )
}

/// Which of `missed` the walk reaches under another spelling, as `(key, walk's
/// spelling)`: the regular file it yields that is the same file (device and
/// inode) the key names. It descends only into the directories above such a
/// file, so a key that is absent or ignored costs an `lstat`.
fn respelled(root: &Path, missed: &[&str]) -> (Vec<(String, String)>, Vec<std::path::PathBuf>) {
    use std::os::unix::fs::MetadataExt;
    let meta = |key: &str| std::fs::symlink_metadata(root.join(key)).ok();
    let files: HashMap<(u64, u64), &str> = missed
        .iter()
        .filter_map(|&key| {
            let m = meta(key).filter(std::fs::Metadata::is_file)?;
            Some(((m.dev(), m.ino()), key))
        })
        .collect();
    if files.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let inos: HashSet<u64> = files.keys().map(|&(_, ino)| ino).collect();
    let dirs: HashSet<u64> = ancestors(files.values().copied())
        .into_iter()
        .filter_map(|dir| meta(dir).filter(std::fs::Metadata::is_dir))
        .map(|m| m.ino())
        .collect();
    walk_matching(
        root,
        |_, entry| entry.ino().is_some_and(|ino| dirs.contains(&ino)),
        |_, entry| {
            // the inode alone is a hint; the device settles it
            let ino = entry.ino().filter(|ino| inos.contains(ino))?;
            let dev = entry.metadata().ok()?.dev();
            files.get(&(dev, ino)).map(|&key| key.to_owned())
        },
    )
}

/// The source files git tracks under `root` that a pass reads, in git's
/// order: [`git_listed`] as far as [`walk_reaches`]. `None` outside git.
fn git_source_candidates(root: &Path) -> Option<Vec<String>> {
    let listed = git_listed(root)?;
    let (reached, _) = walk_reaches(root, listed.iter().map(String::as_str));
    Some(reached.spell(listed.iter().map(String::as_str)))
}

/// The source files git tracks under `root` that a pass reads; `None` outside
/// git, or before a first commit, where only a whole walk could count the
/// tree.
fn tracked_files(root: &Path) -> Option<HashSet<String>> {
    git_source_candidates(root)
        .filter(|p| !p.is_empty())
        .map(|p| p.into_iter().collect())
}

/// How many files the tree spans, counted as D52's `read` counts the files
/// the index holds: what git tracks, plus what the index holds that git
/// doesn't (untracked files an explicit index read, deletions not yet
/// reconciled), so `read` never covers files missing from `of`.
fn span_of<'a>(tracked: &HashSet<String>, held: impl IntoIterator<Item = &'a str>) -> usize {
    tracked.len() + held.into_iter().filter(|f| !tracked.contains(*f)).count()
}

/// [`span_of`] for a tree whose checkout holds nothing.
pub(crate) fn count_span_unheld(root: &Path) -> Option<usize> {
    tracked_files(root).map(|t| t.len())
}

/// [`span_of`] for a checkout no pass has counted (an older rq built it).
pub(crate) fn count_span(store: &Store, root: &Path, checkout: i64) -> Option<usize> {
    let tracked = tracked_files(root)?;
    let held = store.file_mtimes(checkout).ok()?;
    Some(span_of(&tracked, held.keys().map(String::as_str)))
}

/// A lazy, streaming filesystem walk of `roots` yielding file paths — the
/// fallback when git can't enumerate (an explicit unbounded index, or a non-git
/// dir). Honors `.gitignore`/hidden rules via the `ignore` crate. Stops
/// descending at `deadline`: a caller can only check it between files, and a
/// long run of directories without one (`/`, a temp dir) would outlast it.
///
/// A walk error is collected into `unwalked`, not dropped: what lies under a
/// directory the walk couldn't read wasn't seen, which is not the same as gone.
fn fs_walk_candidates(
    roots: Vec<std::path::PathBuf>,
    deadline: Option<Instant>,
    unwalked: Unwalked,
) -> impl Iterator<Item = std::path::PathBuf> {
    roots.into_iter().flat_map(move |root| {
        let unwalked = unwalked.clone();
        walk(&root)
            .filter_entry(move |_| !past(deadline))
            .build()
            .filter_map(move |entry| {
                entry
                    .map_err(|e| {
                        let at = unwalked_at(&e, &root);
                        unwalked.lock().expect("unwalked lock").extend(at);
                    })
                    .ok()
            })
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(ignore::DirEntry::into_path)
    })
}

/// Paths a filesystem walk failed to read (see [`fs_walk_candidates`]).
type Unwalked = std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>;

/// Where a walk error leaves the tree unseen; `None` when the walk learned
/// the path is gone.
fn unwalked_at(e: &ignore::Error, root: &Path) -> Option<std::path::PathBuf> {
    let gone = e
        .io_error()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
    // no path to pin it on: the whole walk is suspect
    (!gone).then(|| walk_error_path(e).unwrap_or(root).to_path_buf())
}

/// `unwalked` as index-key prefixes for [`under_any`]. One outside `root`
/// can't be narrowed, so it stands for the root.
fn unwalked_keys(unwalked: &[std::path::PathBuf], root: &Path) -> Vec<String> {
    unwalked
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .map_or_else(|_| String::new(), |r| r.to_string_lossy().into_owned())
        })
        .collect()
}

fn walk_error_path(e: &ignore::Error) -> Option<&Path> {
    match e {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        _ => None,
    }
}

/// Whether index key `key` lies at or under one of `dirs` (root-relative; an
/// empty one is the root itself).
fn under_any(dirs: &[String], key: &str) -> bool {
    dirs.iter().any(|d| {
        d.is_empty()
            || key
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.is_empty() || r.starts_with('/'))
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
    versions: &Versions,
    seen: HashSet<String>,
    keep: impl Fn(&str, Option<i64>) -> bool + Send,
    cancel: Option<&std::sync::atomic::AtomicBool>,
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
                if past(deadline) || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    finished = false;
                    break;
                }
                let Some((rel, mtime)) = admit(root, &path, &mut seen) else {
                    continue;
                };
                if !keep(&rel, mtime) {
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
            // the filesystem walk ends early, not with an error, when it stops
            // descending at the deadline — that's not a complete walk
            if past(deadline) {
                finished = false;
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
                    if past(deadline) || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        parse_incomplete.store(true, Ordering::Relaxed); // backlog abandoned
                        break;
                    }
                    if let Some(fs) = parse_file(root, &path, needle, versions)
                        && res_tx.send(fs).is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(res_tx); // the workers hold the live clones
        // Owned here so a failing sink drops both ends as it returns: a worker
        // or walk parked in `send` on a full channel then errors out instead of
        // holding the scope join forever.
        drop(path_rx);
        let res_rx = res_rx;

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

/// Decide an index sweep's outcome: whether to *finalize* (reconcile deletions +
/// record the indexed HEAD) and the coverage `status` to store.
///
/// The guard (budgeted/warm passes only): a completed whole-repo warm that saw
/// **zero** source files while the index already held some is almost certainly a
/// failed enumeration (a `git ls-files` hiccup, a wrong root), not "every file
/// was deleted". Finalizing it would forget the entire index and mark it
/// `complete` — which warm-skip then strands at zero forever (a clean, "complete"
/// repo isn't re-warmed). So it isn't finalized and stays `warming` for the next
/// query to retry. An explicit `rq --index` (unbounded, `budgeted = false`) walks
/// the filesystem and is user-initiated, so it's trusted: an empty tree really
/// does reconcile the index away. A genuinely empty repo (nothing stored before)
/// also completes.
fn sweep_outcome(
    completed: bool,
    whole_repo: bool,
    seen_empty: bool,
    had_stored: bool,
    budgeted: bool,
) -> (bool, Coverage) {
    if !whole_repo {
        // a subtree index is a *seed* — it never reconciles (it didn't see the
        // whole tree) and leaves coverage `warming` so normal warming carries
        // on over the rest of the repo
        return (false, Coverage::Warming);
    }
    if budgeted && completed && seen_empty && had_stored {
        return (false, Coverage::Warming); // suspicious empty warm — don't wipe the index
    }
    if completed {
        (true, Coverage::Complete)
    } else {
        (false, Coverage::Warming)
    }
}

/// Forget every checkout whose root is gone from disk (a removed worktree, a
/// moved repo), with the versions only it held. Runs before whatever reads
/// other checkouts than the one asked from (`-a`, `--status`, an index pass):
/// a stat per checkout, and a write only when one is gone. Best-effort: one a
/// busy writer kept is pruned next time.
pub(crate) fn prune_missing_checkouts(store: &Store) {
    for root in store.all_checkout_roots().unwrap_or_default() {
        if gone(Path::new(&root)) {
            let _ = store.forget_checkout(&root);
        }
    }
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
/// fresh); then, on a budgeted pass with a `query`, the files containing its
/// leaf name; then the walk streams the rest in walk order. `subdirs` (empty = whole
/// repo) scope the walk; `budget` bounds it (`None` = unbounded). A whole-repo
/// sweep that finishes within budget reconciles deletions and is `complete`; a
/// sweep cut short — or a subtree seed — is `warming`.
#[allow(clippy::too_many_arguments)]
fn run_index(
    store: &mut Store,
    root: &Path,
    active: &[String],
    subdirs: &[String],
    budget: Option<Duration>,
    query: Option<&str>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    demanded_flag: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let profiling = crate::profile::enabled();
    PARSE_US.store(0, std::sync::atomic::Ordering::Relaxed);
    let setup_span = crate::profile::span("index: setup");
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // A budgeted warm (the background child, run after searches) reads the
    // identity cached by checkout root, as the search path does. An explicit
    // index asks git: it's how a checkout relearns who it is after a remote
    // is added or changed.
    let identity = budget
        .and_then(|_| {
            store
                .identity_for_root(&root_display.to_string_lossy())
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| detect_identity(root).to_string());
    let branch = head_branch(root);
    let checkout = store.register_checkout(
        &identity,
        branch.as_deref(),
        &root_display.to_string_lossy(),
    )?;

    prune_missing_checkouts(store);

    let stored = store.file_mtimes(checkout.id)?;
    let coverage_mark = store.coverage_mark(checkout.id)?;
    // A checkout not yet complete says a pass is filling it, and how much
    // there is to fill, so a search answering meanwhile can tell how much of
    // the tree it read and whether anyone is still reading (D52), and what the
    // pass is doing from its start, as a warm child's next pass sets up (D54).
    let root_key = root_display.to_string_lossy().into_owned();
    let marked = coverage_mark
        .as_ref()
        .is_none_or(|&(s, _)| s != Coverage::Complete);
    if marked {
        let _ = store.set_pass_phase(&root_key, std::process::id(), crate::store::READING);
    }
    // Versions other checkouts already stored: a file hashing to one maps to
    // it unparsed, so a second worktree parses only what differs.
    let versions = store.versions(checkout.repo)?;
    drop(setup_span);
    let mut seen: HashSet<String> = HashSet::new();

    // A cold repo (nothing indexed in any checkout) suspends its name index
    // and rebuilds it from every name at the end, rather than appending batch
    // by batch. Incremental passes, and a new checkout of a repo already held,
    // touch a few names and append as they write.
    if versions.is_empty() {
        store.suspend_name_index(checkout.repo)?;
    }

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
    // catches untracked files). Enumeration runs *before* the deadline so its
    // (cheap) work never eats the parse budget.
    // An empty result means nothing is tracked yet (a fresh/uncommitted repo), so
    // fall back to the filesystem walk, which sees untracked files.
    let mut enum_span = crate::profile::span("index: enumerate");
    let listed = budget
        .and_then(|_| git_listed(root))
        .filter(|listed| !listed.is_empty());
    // Git's index lists no untracked file, but the checkout holds those an
    // explicit index's filesystem walk read: keep them, or completing would
    // reconcile them away (D53). Listing every untracked file instead
    // (`--others`) walks the tree, ~2 s a pass on 97k files. Whatever names a
    // file — git, the checkout, the branch — the explicit index's walk decides
    // whether a pass reads it, so both passes read one set (D62).
    let unwalked = Unwalked::default();
    let held_unlisted: Vec<&str> = match &listed {
        Some(listed) => {
            let listed: HashSet<&str> = listed.iter().map(String::as_str).collect();
            stored
                .keys()
                .map(String::as_str)
                .filter(|f| !listed.contains(f))
                .collect()
        }
        None => Vec::new(),
    };
    let named = listed
        .iter()
        .flatten()
        .map(String::as_str)
        .chain(held_unlisted.iter().copied())
        .chain(active.iter().map(String::as_str));
    let (reached, unread) = walk_reaches(root, named);
    unwalked.lock().expect("unwalked lock").extend(unread);
    let listed: Option<Vec<String>> = listed.map(|l| reached.spell(l.iter().map(String::as_str)));
    let git_candidates: Option<Vec<std::path::PathBuf>> = listed.as_ref().map(|listed| {
        // spelled together: a held file may be one git lists under another spelling
        reached
            .spell(
                listed
                    .iter()
                    .map(String::as_str)
                    .chain(held_unlisted.iter().copied()),
            )
            .into_iter()
            .map(|f| root.join(f))
            .collect()
    });
    enum_span.note(|| match &git_candidates {
        Some(p) => format!("git ls-files, {} path(s)", p.len()),
        None => "filesystem walk (lazy — time lands in walk+parse+write)".to_string(),
    });
    drop(enum_span);
    let listed: Option<HashSet<String>> = listed.map(|l| l.into_iter().collect());
    let unlisted;
    let tracked = match &listed {
        _ if !marked => None,
        Some(listed) => Some(listed),
        None => {
            unlisted = tracked_files(root);
            unlisted.as_ref()
        }
    };
    if marked {
        let span = tracked.map(|t| span_of(t, stored.keys().map(String::as_str)));
        store.begin_pass(&root_key, std::process::id(), span)?;
    }

    // Active (branch) files first: always parsed and written, so the working set
    // stays fresh even when a tight budget cuts the walk short.
    let mut active_to_parse: Vec<std::path::PathBuf> = Vec::new();
    for rel in reached.spell(active.iter().map(String::as_str)) {
        note_candidate(
            root,
            &root.join(rel),
            &stored,
            &mut seen,
            &mut active_to_parse,
        );
    }
    let mut active_span = crate::profile::span("index: active files");
    let (active_parsed, _) = parse_files(root, &active_to_parse, None, None, &versions);
    let mut stats = write_files(store, checkout, root, &active_parsed)?;
    active_span.note(|| format!("{} file(s)", active_parsed.len()));
    drop(active_span);

    // parse query-relevant files (by path) first — a cheap in-memory reorder
    let git_candidates = git_candidates.map(|paths| prioritize_by_path(paths, root, query));

    let deadline = budget.map(|b| Instant::now() + b);
    let cap = budget.map(|_| collect_cap());

    // Fused walk → parse → write: stream candidates through the shared pipeline,
    // committing parsed files in batches as they arrive (so a budget-cut or killed
    // pass keeps what it parsed). Only new or changed files are parsed; every
    // source file seen lands in `seen` for deletion reconcile.
    let stored_ref = &stored;
    let changed = move |rel: &str, mtime: Option<i64>| match stored_ref.get(rel) {
        Some(&Some(m)) => Some(m) != mtime,
        _ => true, // new file, or one stored without an mtime
    };
    let skipped = std::sync::atomic::AtomicU64::new(0);
    let stream_start = Instant::now();
    let mut fused_span = crate::profile::span("index: walk+parse+write");
    let (seen, completed, walked, write_time, batches) = {
        let mut writer = BatchWriter::new(&mut *store, checkout, root);
        writer.mark = marked.then(|| (root_key.as_str(), Instant::now()));
        // Demand first: a query's exact or prefix match — the only kind a warming
        // search answers with — lives in a file containing its leaf name, and
        // reading for that is several times cheaper than parsing. So parse those
        // files ahead of the walk-order pass below, which skips them. Uncapped:
        // the cap bounds parsing, and on a repo bigger than one pass it's what
        // would otherwise leave the answer for a later search.
        let mut demanded: HashSet<String> = HashSet::new();
        let needle = query.and_then(crate::search::literal_leaf);
        if let (Some(paths), Some(needle)) = (&git_candidates, needle) {
            let mut demand_span = crate::profile::span("index: demand scan");
            let (_, read_all) = stream_walk(
                root,
                paths.iter().cloned(),
                deadline,
                None,
                Some(needle.as_bytes()),
                &versions,
                seen.clone(),
                changed,
                cancel,
                |fs| {
                    demanded.insert(fs.path.clone());
                    writer.push(fs)
                },
            )?;
            writer.flush()?;
            if read_all && let Some(flag) = demanded_flag {
                flag.store(true, std::sync::atomic::Ordering::Release);
            }
            demand_span.note(|| format!("{} file(s) contain {needle:?}", demanded.len()));
        }
        let candidates: Box<dyn Iterator<Item = std::path::PathBuf> + Send> = match git_candidates {
            Some(paths) => Box::new(paths.into_iter()),
            None => Box::new(fs_walk_candidates(walk_roots, deadline, unwalked.clone())),
        };
        let demanded = &demanded;
        let skipped = &skipped;
        let keep = move |rel: &str, mtime: Option<i64>| {
            if demanded.contains(rel) {
                return false;
            }
            let changed = changed(rel, mtime);
            if profiling && !changed {
                skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            changed
        };
        let (seen, completed) = stream_walk(
            root,
            candidates,
            deadline,
            cap,
            None,
            &versions,
            seen,
            keep,
            cancel,
            |fs| writer.push(fs),
        )?;
        writer.flush()?;
        (
            seen,
            completed,
            writer.written,
            writer.write_time,
            writer.batches,
        )
    };
    fused_span.note(|| {
        format!(
            "{} file(s), {} symbol(s)",
            walked.files_indexed, walked.symbols
        )
    });
    drop(fused_span);
    if marked {
        let _ = store.set_pass_phase(&root_key, std::process::id(), crate::store::FINISHING);
    }
    // Both of these overlap the phase above rather than following it — the
    // workers parse while the consumer thread writes — so they are reported as
    // components of it, not as additional time.
    crate::profile::record(
        "index: parse (worker cpu)",
        Duration::from_micros(PARSE_US.load(std::sync::atomic::Ordering::Relaxed)),
        || format!("summed across {} worker(s)", parse_jobs()),
    );
    crate::profile::record("index: store writes", write_time, || {
        format!("{batches} batch(es), serialized")
    });
    if crate::trace::enabled() {
        let elapsed = stream_start.elapsed();
        crate::trace!(
            "walk+parse+write {} file(s)/{} symbol(s) in {} ms ({} ms in store writes, {} parse jobs)",
            walked.files_indexed,
            walked.symbols,
            elapsed.as_millis(),
            write_time.as_millis(),
            parse_jobs(),
        );
    }
    // A pass cut short while another is still filling this checkout leaves
    // the tail — the name index and commit times — to that one's end, so the
    // search that cut it isn't kept past its answer paying for it twice.
    let leave_tail = marked
        && cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        && store.indexed_by_others(&root_key);
    if !leave_tail {
        let mut span = crate::profile::span("index: name index");
        let rebuilt = store.maintain_name_index(checkout.repo)?;
        span.note(|| if rebuilt { "rebuilt" } else { "current" }.to_string());
    }
    stats += walked;
    stats.files_seen = seen.len();
    if profiling {
        crate::profile::count("files seen", stats.files_seen as u64);
        crate::profile::count("files parsed", stats.files_parsed as u64);
        crate::profile::count(
            "files skipped (mtime)",
            skipped.load(std::sync::atomic::Ordering::Relaxed),
        );
        crate::profile::count("symbols", stats.symbols as u64);
        crate::profile::count("batches", batches as u64);
        crate::profile::count("parse jobs", parse_jobs() as u64);
    }

    let whole_repo = subdirs.is_empty();
    let (finalize, status) = sweep_outcome(
        completed,
        whole_repo,
        seen.is_empty(),
        !stored.is_empty(),
        budget.is_some(),
    );
    // a finalized whole-repo sweep saw every live file → anything still indexed
    // (but not seen) was deleted on disk. A sweep that saw *zero* files while the
    // index held some is treated as a failed enumeration (see `sweep_outcome`),
    // not finalized — so a transient empty walk can't wipe a populated index.
    if finalize {
        let mut reconcile_span = crate::profile::span("index: reconcile");
        let unwalked = unwalked_keys(&unwalked.lock().expect("unwalked lock"), root);
        let (mut forgotten, mut kept) = (0, 0);
        for path in stored.keys().filter(|p| !seen.contains(*p)) {
            if under_any(&unwalked, path) {
                kept += 1;
                continue;
            }
            store.forget_file(checkout.id, path)?;
            forgotten += 1;
        }
        reconcile_span.note(|| format!("{forgotten} file(s) forgotten"));
        drop(reconcile_span);
        if forgotten > 0 {
            crate::trace!(
                "reconcile {}: forgot {forgotten} file(s) not seen on disk",
                crate::trace::abbrev(&root_display)
            );
        }
        if kept > 0 {
            crate::trace!(
                "reconcile {}: kept {kept} file(s) under a dir the walk couldn't read",
                crate::trace::abbrev(&root_display)
            );
        }
        // record the commit the index now reflects, so a later search can detect
        // an unchanged committed tree and skip re-walking a large repo
        if let Some(head) = head_state(root) {
            let _ = store.set_indexed_head(checkout.id, &head);
            // A full sweep leaves the index matching the disk, so the edits it
            // holds are exactly what's dirty now. Unchanged when it parsed
            // nothing, which is most sweeps of a clean repo — skip the status.
            if stats.files_indexed > 0 {
                let edited: Vec<String> = dirty_files(root)
                    .into_iter()
                    .filter(|f| is_source(f))
                    .collect();
                let _ = store.set_edited_files(checkout.id, &edited);
            }
        }
    }
    // commit times feed the recency signal, but `git log -n1000 --name-only` is
    // pricey on a big repo. Run it only when this run indexed something AND
    // `root` is the work-tree root: a subdir index's `git log` walks the whole
    // repo's history yet emits repo-relative paths that wouldn't match our
    // subdir-relative ones — pure waste. (A subdir index leans on mtime recency.)
    if !leave_tail && stats.files_indexed > 0 && repo_root(root).is_some_and(|r| r == root_display)
    {
        let _span = crate::profile::span("index: git metadata");
        capture_commit_times(store, checkout.id, root);
    }

    // Never persist "complete" for an empty index: a zero-file complete is almost
    // by definition wrong (a failed enumeration), and warm-skip would then strand
    // the repo at zero. Keep it "warming" so the next query keeps polling for
    // files to index. Asks about the repo's *total* indexed files, not this
    // run's — a warm of an already-indexed repo re-parses nothing yet isn't empty.
    let status = if status == Coverage::Complete
        && !store.checkout_has_files(checkout.id).unwrap_or(false)
    {
        Coverage::Warming
    } else {
        status
    };
    let recorded = store.set_coverage_since(
        checkout.id,
        stats.files_seen as i64,
        stats.files_indexed as i64,
        status,
        &coverage_mark,
    )?;
    if !recorded {
        crate::trace!(
            "coverage {}: kept the `complete` another pass recorded during this one",
            crate::trace::abbrev(&root_display)
        );
    }
    if marked {
        // recount: this pass may have read files git doesn't track
        let span = (status != Coverage::Complete && recorded)
            .then_some(tracked)
            .flatten()
            .map(|t| {
                let read = seen.iter().filter(|f| !stored.contains_key(*f));
                span_of(t, stored.keys().chain(read).map(String::as_str))
            });
        store.end_pass(&root_key, std::process::id(), span)?;
    }
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
    let Some((rel, mtime)) = admit(root, file, seen) else {
        return;
    };
    // unchanged by mtime → already indexed, no need to re-parse
    if let Some(&Some(m)) = stored.get(&rel)
        && Some(m) == mtime
    {
        return;
    }
    to_parse.push(file.to_path_buf());
}

/// Read + parse one source file into a [`FileSymbols`](crate::store::FileSymbols), or `None` if it isn't a
/// known language, can't be read, or (when `needle` is set) doesn't contain the
/// query — the ripgrep-style content pre-filter, applied here so it runs on the
/// worker thread. Touches no store — safe to run in parallel (each call builds
/// its own Tree-sitter parser).
fn parse_file(
    root: &Path,
    file: &Path,
    needle: Option<&[u8]>,
    versions: &Versions,
) -> Option<crate::store::FileSymbols> {
    let timing = crate::profile::enabled().then(Instant::now);
    let ext = file.extension().and_then(|e| e.to_str())?;
    let plugin = lang::plugin_for_extension(ext)?;
    let rel = index_key(root, file)?;
    let source = read_source(file).ok()?;
    // pre-filter: skip the expensive parse on files that can't hold the match
    if let Some(n) = needle
        && !contains_ascii_ci(source.as_bytes(), n)
    {
        return None;
    }
    let content_hash = content_hash(&source);
    let held = versions
        .get(&rel)
        .is_some_and(|hashes| hashes.contains(&content_hash));
    let symbols = (!held).then(|| plugin.extract(&rel, &source));
    if let Some(started) = timing {
        // workers run concurrently, so this sums to CPU time, not wall time
        let elapsed = started.elapsed();
        PARSE_US.fetch_add(
            elapsed.as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        crate::profile::slow(elapsed, || rel.clone());
    }
    Some(crate::store::FileSymbols {
        path: rel,
        language: plugin.language().to_string(),
        mtime: file_mtime(file),
        content_hash,
        generated: is_generated(&source),
        symbols,
    })
}

/// Bytes of a file's head searched for a NUL: git's and ripgrep's binary test.
const BINARY_SNIFF: u64 = 8 * 1024;

/// A file past this size reads as empty, as a binary one does: no source
/// file is this big, and a generated one this big isn't worth the parse.
const MAX_SOURCE: u64 = 64 * 1024 * 1024;

/// Whether `path` is past the size a source file is read to: held, never
/// parsed, so it has no definitions to show whatever it holds.
pub(crate) fn oversized(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_SOURCE)
}

/// A source file's text. Bytes that aren't UTF-8 (a Latin-1 comment) become
/// U+FFFD rather than dropping the file: its names are still ASCII. A binary
/// or oversized file (an MPEG-TS video named `.ts`) reads as empty, unread
/// past its head: parsed, it costs seconds and yields junk names. Empty, not
/// an error, so an index still holds it and a tree walk doesn't read it as
/// unindexed. Anything but a regular file — a FIFO, `/dev/zero` behind a
/// symlink — is an error, never read.
pub(crate) fn read_source(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = open_regular(path)?;
    if file.metadata()?.len() > MAX_SOURCE {
        return Ok(String::new());
    }
    let mut bytes = Vec::new();
    (&mut file).take(BINARY_SNIFF).read_to_end(&mut bytes)?;
    if bytes.contains(&0) {
        return Ok(String::new());
    }
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Whether an index pass could read `path`. One it can't is no change to
/// take in: comparing it would read as "moved" on every query, forever.
fn readable(path: &Path) -> bool {
    open_regular(path).is_ok()
}

/// Open `path` for reading if it's a regular file (following symlinks).
/// Non-blocking, then checked: a plain open of a FIFO waits for a writer, and
/// a stat first would leave a window for the path to become one.
fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Lines of a file's header read for a generated-code marker: a license
/// block often comes first.
const HEADER_LINES: usize = 20;

/// Does the source declare itself generated? The markers are the tools' own,
/// read the same in every language: a comment in the header holding
/// `@generated`, or both "generated" and "do not edit" — Go's
/// `// Code generated … DO NOT EDIT.`, protoc's `# Generated by the protocol
/// buffer compiler.  DO NOT EDIT!`. A line that starts with a letter or quote is
/// code, not a comment: a generator's own source prints the marker it writes.
pub(crate) fn is_generated(source: &str) -> bool {
    source.lines().take(HEADER_LINES).any(|line| {
        let line = line.trim_start();
        let comment = line
            .chars()
            .next()
            .is_some_and(|c| !c.is_alphanumeric() && !matches!(c, '"' | '\'' | '`'));
        let lower = line.to_lowercase();
        comment
            && (lower.contains("@generated")
                || (lower.contains("generated") && lower.contains("do not edit")))
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
    versions: &Versions,
) -> (Vec<crate::store::FileSymbols>, bool) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let workers = parse_jobs().min(paths.len());

    if workers <= 1 {
        let mut out = Vec::new();
        for p in paths {
            if past(deadline) {
                return (out, false);
            }
            if let Some(parsed) = parse_file(root, p, needle, versions) {
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
                        if let Some(parsed) = parse_file(root, p, needle, versions) {
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

/// Capture per-file last-commit times for the recency signal — incrementally.
/// The full history walk is priced only once: after a capture, the HEAD it ran
/// at is recorded, so the next capture reads just the commits since
/// (`old..HEAD`) — and skips the `git log` entirely when HEAD hasn't moved
/// (the common case for a warm of uncommitted edits, which mtime already
/// covers). A vanished old sha (rebase, gc) fails the range and falls back to
/// the full bounded walk.
fn capture_commit_times(store: &mut Store, checkout: i64, root: &Path) {
    let Some(head) = git_head(root) else { return };
    let last = store.git_ts_head(checkout).ok().flatten();
    if last.as_deref() == Some(head.as_str()) {
        return; // HEAD unmoved — nothing new to capture
    }
    let first = last.is_none();
    let times = last
        .and_then(|old| git_commit_times_range(root, &old, 1000))
        .unwrap_or_else(|| git_commit_times(root, 1000));
    if !times.is_empty() {
        if store.set_file_git_ts(checkout, &times).is_err() {
            return; // don't advance the marker past an unpersisted capture
        }
    } else if first {
        return; // full walk yielded nothing — leave the marker unset to retry
    }
    let _ = store.set_git_ts_head(checkout, &head);
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

/// Like [`git_commit_times`], limited to the commits in `old..HEAD`. `None`
/// when the range can't be resolved (`old` no longer exists) *or* is empty —
/// an empty range only arises from a backwards HEAD move (reset/checkout), and
/// the full-walk fallback re-captures correct times for it.
fn git_commit_times_range(root: &Path, old: &str, limit: usize) -> Option<HashMap<String, i64>> {
    git_output(
        root,
        &[
            "log",
            &format!("-n{limit}"),
            "--name-only",
            "--pretty=format:%ct",
            &format!("{old}..HEAD"),
        ],
    )
    .map(|text| parse_git_log(&text))
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

/// A tree to scan live, resolved once: a live fallback scans it twice (filtered,
/// then not), and each scan would otherwise fork git again for the same answers.
pub(crate) struct LiveTree<'a> {
    pub(crate) root: &'a Path,
    pub(crate) identity: String,
    /// git's tracked source files; `None` walks the filesystem instead
    tracked: Option<Vec<std::path::PathBuf>>,
}

impl<'a> LiveTree<'a> {
    /// `root` under an identity the caller already resolved.
    pub(crate) fn new(root: &'a Path, identity: String) -> LiveTree<'a> {
        let tracked = git_source_candidates(root)
            .filter(|keys| !keys.is_empty())
            .map(|keys| keys.iter().map(|k| root.join(k)).collect());
        LiveTree {
            root,
            identity,
            tracked,
        }
    }

    /// `root`, asking git for its identity.
    #[cfg(test)]
    pub(crate) fn detect(root: &'a Path) -> LiveTree<'a> {
        LiveTree::new(root, detect_identity(root).to_string())
    }
}

/// Live, budgeted scan (search Layer 4): stream-walk `tree` on the same fused
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
pub(crate) fn scan(
    tree: &LiveTree,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
    needle: Option<&[u8]>,
) -> Vec<crate::store::FileSymbols> {
    let root = tree.root;
    let needle = needle.filter(|n| !n.is_empty());
    // git's index for a git repo (content-scan a huge repo without traversing it),
    // else a filesystem walk (the live scan of a non-git dir)
    let candidates: Box<dyn Iterator<Item = std::path::PathBuf> + Send + '_> = match &tree.tracked {
        Some(paths) => Box::new(paths.iter().cloned()),
        // nothing here reconciles, so what the walk couldn't read needs no note
        None => Box::new(fs_walk_candidates(
            vec![root.to_path_buf()],
            deadline,
            Unwalked::default(),
        )),
    };
    let mut out: Vec<crate::store::FileSymbols> = Vec::new();
    let keep = |rel: &str, _| !skip.contains(rel); // skip already-indexed
    let _ = stream_walk(
        root,
        candidates,
        deadline,
        None,
        needle,
        &Versions::new(),
        HashSet::new(),
        keep,
        None,
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
pub(crate) enum Refresh {
    /// Nothing to do — content hash still matches, or the file couldn't be read
    /// right now (left in place rather than forgotten — see [`refresh_file`]).
    Unchanged,
    /// File changed; its symbols were re-extracted.
    Updated,
    /// Gone from a root that's there (a branch switch deleted it): kept in the
    /// index all the same, for a pass to reconcile, but no answer now.
    Missing,
}

/// Whether `root` is inside a git work tree. Implicit (opportunistic) indexing
/// is gated on this so a stray query never walks a non-repo directory. Native
/// (no `git` fork) — it runs on every search.
pub(crate) fn is_git_repo(root: &Path) -> bool {
    repo_root(root).is_some()
}

/// The git work-tree root at or above `path` — the nearest ancestor holding a
/// `.git` entry — found without shelling out. `.git` may be a directory or a
/// file (worktrees, submodules), so we test existence either way. `None` when
/// `path` is not inside a work tree.
pub(crate) fn repo_root(path: &Path) -> Option<std::path::PathBuf> {
    let start = path.canonicalize().ok()?;
    start
        .ancestors()
        .find(|a| a.join(".git").try_exists().unwrap_or(false))
        .map(Path::to_path_buf)
}

/// Where a work tree's git state lives: its own dir (`HEAD`, `index`) and the
/// common one (refs, `packed-refs`). The same dir for a plain clone. A linked
/// worktree's `.git` is a file naming its own dir, which names the common one
/// in `commondir`; a submodule's names a dir that is both.
fn git_dirs(root: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some((dot.clone(), dot));
    }
    let pointer = read_git_file(&dot)?;
    let dir = root.join(pointer.trim().strip_prefix("gitdir:")?.trim());
    let common =
        read_git_file(&dir.join("commondir")).map_or_else(|| dir.clone(), |c| dir.join(c.trim()));
    Some((dir, common))
}

/// One of git's own small files (`HEAD`, a ref, `packed-refs`, a `.git`
/// pointer), or `None` when it can't be read. Not for checkout files: those
/// go through [`read_source`]. Regular files only, like those: `.git` sits
/// in the checkout, where anything can be.
fn read_git_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    open_regular(path).ok()?.read_to_string(&mut text).ok()?;
    Some(text)
}

/// The current HEAD commit sha, or `None` outside a git work tree.
pub(crate) fn git_head(root: &Path) -> Option<String> {
    // Resolved by reading the git dir rather than forking `git rev-parse`:
    // this runs on every search to gate warming, and the fork costs ~10 ms
    // while the lookup is one or two small file reads.
    let Some((git_dir, common)) = git_dirs(root) else {
        return git_output(root, &["rev-parse", "HEAD"]);
    };
    let head = read_git_file(&git_dir.join("HEAD"))?;
    let head = head.trim();
    let Some(git_ref) = head.strip_prefix("ref: ") else {
        // detached HEAD holds the commit itself
        return (!head.is_empty()).then(|| head.to_string());
    };
    if let Some(sha) = read_git_file(&common.join(git_ref)) {
        let sha = sha.trim();
        if !sha.is_empty() {
            return Some(sha.to_string());
        }
    }
    // Not a loose ref, so it's packed: `<sha> refs/heads/<branch>`. Matching on
    // the leading space keeps `refs/heads/main` from matching `…/mainline`.
    let packed = read_git_file(&common.join("packed-refs"))?;
    packed
        .lines()
        .find_map(|l| l.strip_suffix(&format!(" {git_ref}")))
        .map(|sha| sha.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Stands in for the commit in a repo that has none yet.
const UNBORN_HEAD: &str = "unborn";

/// The HEAD an index records itself as reflecting: the commit sha, or a marker
/// for an unborn HEAD (`git init`, nothing committed) — a real state to compare
/// against, not an absent one. Never hand this to git; use [`git_head`] for a
/// sha. `None` outside a git work tree.
pub(crate) fn head_state(root: &Path) -> Option<String> {
    git_head(root).or_else(|| is_git_repo(root).then(|| UNBORN_HEAD.to_string()))
}

/// Whether `git status` can speak for an index that reflects `head`. Not
/// before a first commit with no source staged: the index walked the tree, and
/// git sees every file in it as untracked.
pub(crate) fn git_speaks_for(root: &Path, head: &str) -> bool {
    head != UNBORN_HEAD || tracked_files(root).is_some()
}

/// Repo-relative *tracked* files with uncommitted changes (staged or unstaged),
/// both sides of a rename. `--untracked-files=no` skips the work-tree-wide
/// untracked-file scan — the expensive, cold-cache-sensitive part of `git
/// status` on a large repo (it walks to classify every path against
/// `.gitignore`). This runs on every search to gate warming, so the scan
/// dominated query-time variance.
///
/// The tradeoff: a brand-new *untracked* file isn't seen as a change here, so it
/// won't be picked up by the opportunistic warm until it's committed (HEAD moves
/// → warm) or `rq --index`ed. Tracked edits, the common case, are still caught,
/// and `git status` still refreshes the index so a touched-but-unchanged file
/// doesn't read as dirty. Empty when git can't say — no evidence of an edit.
pub(crate) fn dirty_files(root: &Path) -> Vec<String> {
    git(root)
        .args(["status", "--porcelain", "-z", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_porcelain_z(&o.stdout))
        .unwrap_or_default()
}

/// Paths from `git status --porcelain -z`: one `XY path` entry per file, with
/// a rename's or copy's source following as an entry of its own. Porcelain
/// paths are always repo-root-relative, whatever the cwd. A name that isn't
/// UTF-8 is left out, as no pass holds one (see [`rel_key`]).
fn parse_porcelain_z(out: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = out.split(|&b| b == 0).filter(|e| e.len() > 3);
    let mut push = |path: &[u8]| paths.extend(std::str::from_utf8(path).map(str::to_owned));
    while let Some(entry) = entries.next() {
        let (xy, path) = entry.split_at(3);
        push(path);
        if xy[..2].iter().any(|c| matches!(c, b'R' | b'C'))
            && let Some(source) = entries.next()
        {
            push(source);
        }
    }
    paths
}

/// Whether any of `dirty` differs from what the index holds for it: a source
/// file whose mtime moved since it was parsed, one indexed but now gone, or
/// one never indexed. An edit the index already reflects is *not* a change —
/// the worktree stays dirty until commit, and treating dirty as stale re-warmed
/// on every query and made every miss read as "still warming".
///
/// A file is a change only if a pass would read it: one the walk doesn't
/// reach (ignored, say) is no edit to the index, however dirty.
fn has_unindexed_edits(store: &Store, checkout: i64, root: &Path, dirty: &[String]) -> bool {
    let mut unread = Vec::new();
    for rel in dirty.iter().filter(|f| is_source(f)) {
        match edit_of(store, checkout, root, rel) {
            Edit::Changed => return true,
            Edit::IfRead => unread.push(rel.as_str()),
            Edit::None => {}
        }
    }
    let (reached, _) = walk_reaches(root, unread.iter().copied());
    // git names a file by its spelling, the index by the walk's (case, Unicode
    // normalization): a respelled file is an edit only if the walk's is one
    unread.iter().any(|&rel| {
        reached.get(rel).is_some_and(|spelled| {
            spelled == rel || !matches!(edit_of(store, checkout, root, spelled), Edit::None)
        })
    })
}

/// What one dirty file is to the index ([`has_unindexed_edits`]).
enum Edit {
    /// A change whatever the walk says: indexed but gone, or the store failed.
    Changed,
    /// Moved since it was read, or never read: a change if a pass reads it.
    IfRead,
    None,
}

fn edit_of(store: &Store, checkout: i64, root: &Path, rel: &str) -> Edit {
    let path = root.join(rel);
    let Ok(held) = store.file_mtime(checkout, rel) else {
        return Edit::Changed;
    };
    let moved = match (on_disk(&path), held) {
        (OnDisk::Absent, Some(_)) => return Edit::Changed,
        (OnDisk::Unknown, _) | (OnDisk::Absent, None) => false,
        (OnDisk::File(now), Some(Some(indexed))) => Some(indexed) != now && readable(&path),
        // never read, or held without an mtime
        (OnDisk::File(_), _) => readable(&path),
    };
    if moved { Edit::IfRead } else { Edit::None }
}

/// Whether a tree git can't speak for differs from its index: a source file
/// added, removed, or with an mtime the index didn't record. The same walk
/// and mtimes an index pass of the tree uses, so "unchanged" here means a
/// pass would find nothing to do.
pub(crate) fn untracked_tree_moved(store: &Store, checkout: i64, root: &Path) -> bool {
    use ignore::WalkState;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let _span = crate::profile::span("walk: tree changed?");
    let Ok(indexed) = store.file_mtimes(checkout) else {
        return true;
    };
    let (moved, held) = (AtomicBool::new(false), AtomicUsize::new(0));
    let unwalked = std::sync::Mutex::new(Vec::new());
    // parallel: on a large tree the stats are the whole cost of a miss
    walk(root).threads(parse_jobs()).build_parallel().run(|| {
        let (indexed, moved, held, unwalked) = (&indexed, &moved, &held, &unwalked);
        Box::new(move |entry| {
            if moved.load(Relaxed) {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let at = unwalked_at(&e, root);
                    unwalked.lock().expect("unwalked lock").extend(at);
                    return WalkState::Continue;
                }
            };
            let path = entry.path();
            let Some(rel) = index_key(root, path) else {
                return WalkState::Continue;
            };
            let stored = indexed.get(&rel);
            let now = match on_disk(path) {
                OnDisk::File(now) => now,
                OnDisk::Absent => return WalkState::Continue,
                OnDisk::Unknown => {
                    // nothing known: held as it is
                    if stored.is_some() {
                        held.fetch_add(1, Relaxed);
                    }
                    return WalkState::Continue;
                }
            };
            let same = matches!(stored, Some(&Some(m)) if Some(m) == now);
            if !same && readable(path) {
                moved.store(true, Relaxed);
                return WalkState::Quit;
            }
            if stored.is_some() {
                held.fetch_add(1, Relaxed);
            }
            WalkState::Continue
        })
    });
    if moved.into_inner() {
        return true;
    }
    // a held file the walk never reached was removed — unless it lies where
    // the walk couldn't look, which a pass keeps too
    let unwalked = unwalked_keys(&unwalked.into_inner().expect("unwalked lock"), root);
    let unseen = indexed.keys().filter(|k| under_any(&unwalked, k)).count();
    held.into_inner() + unseen != indexed.len()
}

/// Whether the worktree holds anything the index doesn't reflect, given the
/// files `git status` calls dirty — and the files the index last held as edits.
///
/// `dirty` alone misses a discarded edit: `git checkout -- f` makes `f` clean,
/// so status stops naming it, while the index still holds the edited version's
/// symbols. So the index's own record of the edits it took in is checked too,
/// and an entry is dropped only once it's clean and the index matches the disk
/// again — the reindex it triggers has landed.
pub(crate) fn has_unindexed_changes(
    store: &Store,
    checkout: i64,
    root: &Path,
    dirty: &[String],
) -> bool {
    let prior = store.edited_files(checkout).unwrap_or_default();
    let unsettled: Vec<String> = prior
        .iter()
        .filter(|f| !dirty.contains(f))
        .filter(|f| has_unindexed_edits(store, checkout, root, std::slice::from_ref(f)))
        .cloned()
        .collect();
    let changed = !unsettled.is_empty() || has_unindexed_edits(store, checkout, root, dirty);
    // a dirty source file is an edit the index holds, or is about to
    let mut edited: Vec<String> = dirty
        .iter()
        .filter(|f| is_source(f))
        .cloned()
        .chain(unsettled)
        .collect();
    edited.sort();
    edited.dedup();
    let mut prior = prior;
    prior.sort();
    if edited != prior {
        let _ = store.set_edited_files(checkout, &edited);
    }
    changed
}

/// `path` below `root`, spelled as the index keys files. `None` outside
/// `root`, or for a name that isn't UTF-8: a lossy key wouldn't lead back to
/// the file, so no pass holds one (Linux only; APFS refuses such names).
pub(crate) fn rel_key(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root).ok()?.to_str().map(str::to_owned)
}

/// The key an index pass holds `path` under, if it holds it at all: a source
/// file below `root`. Every enumeration — git's lists, the filesystem walk,
/// the moved-detectors — asks this, so they agree on the set. What's on disk
/// there is [`on_disk`]'s half, and whether ignore rules let a pass read it
/// [`walk_reaches`]'s.
pub(crate) fn index_key(root: &Path, path: &Path) -> Option<String> {
    rel_key(root, path).filter(|key| is_source(key))
}

/// What one `lstat` says about a path an enumeration named.
enum OnDisk {
    /// A regular file, with its mtime.
    File(Option<i64>),
    /// Not there — or not a regular file: no pass reads a FIFO or a device,
    /// nor follows a symlink (the walk doesn't, and its target, when in the
    /// tree, is held in its own right).
    Absent,
    /// The stat failed (EACCES, ESTALE): nothing is known, so nothing moves.
    Unknown,
}

fn on_disk(path: &Path) -> OnDisk {
    use std::io::ErrorKind::{NotADirectory, NotFound};
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => OnDisk::File(mtime_of(&m)),
        Ok(_) => OnDisk::Absent,
        Err(e) if matches!(e.kind(), NotFound | NotADirectory) => OnDisk::Absent,
        Err(_) => OnDisk::Unknown,
    }
}

/// Admit a path an enumeration named into a pass: its key and mtime when it's
/// a source file on disk the pass hasn't seen yet. Records it in `seen`, which
/// reconcile keeps — so an absent file (git still lists one deleted without
/// `git rm`) stays out and is forgotten, and one that can't be stat'ed stays
/// in, unread.
fn admit(root: &Path, path: &Path, seen: &mut HashSet<String>) -> Option<(String, Option<i64>)> {
    let key = index_key(root, path)?;
    if seen.contains(&key) {
        return None; // an active file re-seen by the walk, or a duplicate
    }
    match on_disk(path) {
        OnDisk::File(mtime) => {
            seen.insert(key.clone());
            Some((key, mtime))
        }
        OnDisk::Absent => None,
        OnDisk::Unknown => {
            seen.insert(key);
            None
        }
    }
}

/// Whether `path` is known to be absent. An I/O error (EACCES, ESTALE) says
/// nothing about it, and treating one as gone forgets what the index holds.
fn gone(path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false))
}

/// Whether a repo-relative path is something a language plugin indexes. Hidden
/// paths (any `.`-prefixed component) aren't: the filesystem walk skips them,
/// and `git ls-files` doesn't, so every enumeration filters through here to
/// index the same set whichever pass finishes.
fn is_source(rel: &str) -> bool {
    let path = Path::new(rel);
    let hidden = path.components().any(|c| match c {
        std::path::Component::Normal(name) => name.as_encoded_bytes().starts_with(b"."),
        _ => false,
    });
    !hidden
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| lang::plugin_for_extension(e).is_some())
}

/// Repo-relative files you're working on this branch: committed changes since
/// the branch diverged from the trunk, plus uncommitted edits. Empty on the
/// trunk itself (where it isn't a useful signal) or outside git. Feeds the
/// branch ranking boost — necessarily a few git calls, but gated to feature
/// branches.
pub(crate) fn branch_changed_files(root: &Path) -> Vec<String> {
    // Reading `.git` beats forking git here: measured on a small repo, each of
    // these four commands costs ~10 ms and almost all of it is process spawn,
    // not git's work. The branch name and the trunk's existence are both plain
    // file lookups, so only the two diffs — which genuinely need git — are
    // left, and they run concurrently since neither reads the other's output.
    let Some(branch) = head_branch(root) else {
        return Vec::new();
    };
    if is_trunk(&branch) {
        return Vec::new();
    }
    let Some(trunk) = trunk_ref(root) else {
        return Vec::new();
    };

    let committed = {
        let root = root.to_path_buf();
        let spec = format!("{trunk}...HEAD");
        // committed branch changes since divergence from the trunk (three-dot)
        std::thread::spawn(move || git_paths(&root, &["diff", "--name-only", "-z", &spec]))
    };
    // uncommitted edits to tracked files
    let working = git_paths(root, &["diff", "--name-only", "-z", "HEAD"]);
    let committed = committed.join().unwrap_or_default();
    let files: HashSet<String> = committed.into_iter().chain(working).collect();
    files.into_iter().collect()
}

/// A cheap fingerprint of the git state that decides which files a branch has
/// changed: the mtimes of `.git/HEAD` (commits, checkouts) and `.git/index`
/// (staging). Two stats, microseconds.
///
/// Deliberately *not* a complete invalidation signal — editing a tracked file
/// touches neither, so a caller must pair this with a freshness window rather
/// than trusting it alone. `None` outside git, or when `.git` names no git
/// dir rq can read (a linked worktree's pointer file is followed), which
/// means "don't cache this".
pub(crate) fn branch_files_stamp(root: &Path) -> Option<String> {
    let (git_dir, _) = git_dirs(root)?;
    let stamp = |name: &str| -> u64 {
        std::fs::metadata(git_dir.join(name))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    Some(format!("{}:{}", stamp("HEAD"), stamp("index")))
}

/// A fingerprint of the git state a worktree check depends on, short of the
/// working files themselves: the checkout, its HEAD commit and the `.git/index`
/// mtime to the nanosecond. A commit, checkout, reset, pull, merge, stash or
/// `git add` moves one of them; an unstaged edit to a tracked file moves
/// neither, so a caller must pair this with a time window. `None` unless HEAD
/// is still `head`, or when rq can't find the git dir by reading `.git` (a
/// directory, or a linked worktree's pointer file), since resolving HEAD
/// then means forking git.
pub(crate) fn git_state_stamp(root: &Path, head: &str) -> Option<String> {
    let (git_dir, _) = git_dirs(root)?;
    if head_state(root)? != head {
        return None;
    }
    let index = std::fs::metadata(git_dir.join("index"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some(format!("{}\n{head}\n{index}", root.display()))
}

/// The checked-out branch, read from the git dir's `HEAD` rather than forked
/// out to `git rev-parse`. `None` for a detached HEAD (no branch to compare).
fn head_branch(root: &Path) -> Option<String> {
    let Some((git_dir, _)) = git_dirs(root) else {
        return is_git_repo(root)
            .then(|| git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]))
            .flatten();
    };
    let head = read_git_file(&git_dir.join("HEAD"))?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Branch names treated as the trunk — the "active files" signal doesn't apply
/// there (you're not on a feature branch).
fn is_trunk(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "trunk")
}

/// The trunk ref to diff against: `main` if it exists, else `master`.
fn trunk_ref(root: &Path) -> Option<String> {
    let Some((_, git_dir)) = git_dirs(root) else {
        return ["main", "master"]
            .into_iter()
            .find(|name| git_output(root, &["rev-parse", "--verify", "--quiet", name]).is_some())
            .map(str::to_string);
    };
    // A branch is a loose ref file or a line in packed-refs; both are cheaper
    // to look at than a `git rev-parse` fork.
    let packed = read_git_file(&git_dir.join("packed-refs")).unwrap_or_default();
    ["main", "master"].into_iter().find_map(|name| {
        let loose = git_dir
            .join("refs/heads")
            .join(name)
            .try_exists()
            .unwrap_or(false);
        let is_packed = packed
            .lines()
            .any(|l| l.ends_with(&format!(" refs/heads/{name}")));
        (loose || is_packed).then(|| name.to_string())
    })
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
pub(crate) fn refresh_file(
    store: &mut Store,
    checkout: Checkout,
    root: &Path,
    rel: &str,
) -> Result<Refresh, Box<dyn std::error::Error>> {
    let path = root.join(rel);
    // Stat before reading: an edit landing mid-read then leaves an mtime newer
    // than what was hashed, so the next check reads again rather than trusting
    // it. An unchanged mtime is the same skip the indexer's walk makes.
    let mtime = file_mtime(&path);
    if mtime.is_some() && store.file_mtime(checkout.id, rel)? == Some(mtime) {
        return Ok(Refresh::Unchanged);
    }
    let source = match read_source(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && root.is_dir() => {
            return Ok(Refresh::Missing);
        }
        Err(_) => return Ok(Refresh::Unchanged), // unreadable now — leave it, don't forget
    };
    let hash = content_hash(&source);
    if store.file_unchanged(checkout.id, rel, &hash)? {
        // touched, not edited: remember the new mtime so the next check stats
        // instead of reading it again
        store.set_file_mtime(checkout.id, rel, mtime)?;
        return Ok(Refresh::Unchanged);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let plugin = lang::plugin_for_extension(ext);
    let symbols = match plugin {
        Some(plugin) => plugin.extract(rel, &source),
        None => Vec::new(),
    };
    // the plugin knows its language even when a file parses to zero symbols
    let language = plugin.map_or("unknown", |p| p.language());
    store.replace_files(
        checkout,
        &[crate::store::FileSymbols {
            path: rel.to_string(),
            language: language.to_string(),
            mtime,
            content_hash: hash,
            generated: is_generated(&source),
            symbols: Some(symbols),
        }],
    )?;
    // Off the sweep path, a changed file is most likely an edit in progress;
    // if it's later discarded, the staleness check has to know to look.
    let _ = store.note_edited_file(checkout.id, rel);
    Ok(Refresh::Updated)
}

/// A file's definitions as it stands on disk now: the index's rows when they
/// reflect this version of it, else parsed live — the file may be unindexed,
/// edited, or in a repo rq has never seen. Writes nothing. Empty when the file
/// can't be read or isn't a language rq knows.
pub(crate) fn current_definitions(
    store: &Store,
    checkout: Option<Checkout>,
    identity: &str,
    root: &Path,
    rel: &str,
) -> Vec<crate::store::SymbolRow> {
    let path = root.join(rel);
    if let Some(checkout) = checkout
        && let Some(mtime) = file_mtime(&path)
        && store.file_mtime(checkout.id, rel).ok() == Some(Some(Some(mtime)))
        && let Ok(rows) = store.symbols_in_file(checkout.id, rel)
    {
        return rows;
    }
    let Some(plugin) = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(lang::plugin_for_extension)
    else {
        return Vec::new();
    };
    let Ok(source) = read_source(&path) else {
        return Vec::new();
    };
    let generated = is_generated(&source);
    plugin
        .extract(rel, &source)
        .into_iter()
        .map(|s| crate::store::SymbolRow::live(s, identity, &root.to_string_lossy(), generated))
        .collect()
}

/// The newest commit in HEAD's history that a remote has — HEAD itself once
/// it's pushed — so a git host can serve it. `None` when nothing is pushed.
pub(crate) fn pushed_head(root: &Path) -> Option<String> {
    let out = git(root)
        .args(["rev-list", "--boundary", "HEAD", "--not", "--remotes", "--"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return git_head(root);
    }
    // unpushed commits, then `-<sha>` boundaries where they meet pushed history
    text.lines()
        .find_map(|l| l.strip_prefix('-'))
        .map(str::to_string)
}

/// Best-effort repository identity: upstream git remote, else the local path.
/// Outside a work tree there's no remote to ask about, so no `git` is forked.
pub(crate) fn detect_identity(root: &Path) -> RepoIdentity {
    // `git remote get-url` rather than reading `.git/config`: git applies
    // `url.*.insteadOf` rewrites, and an identity that disagreed with git's
    // would re-key an existing index.
    let remotes = if is_git_repo(root) {
        &["origin", "upstream"][..]
    } else {
        &[]
    };
    for remote in remotes {
        if let Some(url) = git_output(root, &["remote", "get-url", remote])
            && let Some(id) = RepoIdentity::from_remote_url(&url)
        {
            return id;
        }
    }
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    RepoIdentity::local(&abs.to_string_lossy())
}

/// A git command aimed at `root`. rq picked `root` by walking up to its `.git`,
/// so an inherited `GIT_DIR` (as `git rebase --exec` exports) must not send git
/// to a different repository.
fn git(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .arg("-C")
        .arg(root);
    cmd
}

/// Run a git command in `root`, returning trimmed stdout on success.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = git(root).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// The paths a `-z` git command prints, as the index keys them: unquoted,
/// and only the UTF-8 ones (see [`rel_key`]). Empty when git fails.
fn git_paths(root: &Path, args: &[&str]) -> Vec<String> {
    let Some(out) = git(root)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    out.stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok().map(str::to_owned))
        .collect()
}

fn content_hash(source: &str) -> String {
    // DefaultHasher uses fixed keys, so this is stable across runs — enough for
    // change detection (not cryptographic).
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn file_mtime(path: &Path) -> Option<i64> {
    mtime_of(&std::fs::metadata(path).ok()?)
}

fn mtime_of(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    // nanosecond resolution (like git's racy-mtime handling): two edits within
    // the same second still get distinct mtimes, so an index taken between them
    // can't mistake the second edit for "unchanged". Fits i64 until 2262.
    let nanos = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    Some(nanos as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_whose_version_went_mid_pass_is_parsed_and_written_anyway() {
        // A worker skipped the parse because the repo held this version when
        // the pass began; the checkout holding it has since let go of it.
        let dir = std::env::temp_dir().join(format!("rq-vanished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w.rb"), "class Widget\nend\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        let checkout = store.test_checkout(&RepoIdentity::local("/x"));
        let unparsed = crate::store::FileSymbols {
            path: "w.rb".into(),
            language: "ruby".into(),
            mtime: None,
            content_hash: "gone".into(),
            generated: false,
            symbols: None,
        };
        let written = write_files(&mut store, checkout, &dir, &[unparsed]).unwrap();
        assert_eq!((written.files_indexed, written.symbols), (1, 1));
        let rows = store.symbols_in_file(checkout.id, "w.rb").unwrap();
        assert_eq!(rows[0].name, "Widget");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_file_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("rq-oversized-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.rb");
        // text past the binary sniff, then a sparse tail: no disk to fill
        std::fs::write(&path, "class A\nend\n".repeat(1024)).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_SOURCE + 1).unwrap();
        let read = read_source(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read.unwrap(), "");
    }

    #[test]
    fn generated_files_are_known_by_their_header() {
        for src in [
            "// Code generated by \"stringer -type=Kind\"; DO NOT EDIT.\n\npackage kinds\n",
            "// Copyright 2024\n// License: MIT\n\n// Code generated by protoc-gen-go. DO NOT EDIT.\npackage pb\n",
            "# Generated by the protocol buffer compiler.  DO NOT EDIT!\nimport sys\n",
            "/**\n * @generated\n */\nexport type Foo = {};\n",
        ] {
            assert!(is_generated(src), "generated: {src:?}");
        }
        for src in [
            "package kinds\n\nfunc String() {}\n",
            // a generator's own source prints the marker; it doesn't carry it
            "const HEADER = \"// Code generated by gen; DO NOT EDIT.\";\n",
            "fmt.Println(\"// Code generated; DO NOT EDIT.\")\n",
            // a comment about generation that isn't the marker
            "// Regenerate the docs with `make docs`.\n",
        ] {
            assert!(!is_generated(src), "hand-written: {src:?}");
        }
        let late = format!(
            "{}// Code generated; DO NOT EDIT.\n",
            "x := 1\n".repeat(HEADER_LINES)
        );
        assert!(!is_generated(&late), "only the header counts");
    }

    #[test]
    fn a_filesystem_walk_stops_descending_at_its_deadline() {
        // the walk thread checks the deadline between files, so without this a
        // tree of empty or unreadable dirs could hold it far past its budget
        let dir = std::env::temp_dir().join(format!("rq-walk-deadline-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/x.rb"), "class X\nend\n").unwrap();
        let files =
            |deadline| fs_walk_candidates(vec![dir.clone()], deadline, Unwalked::default()).count();
        assert_eq!(files(None), 1);
        assert_eq!(files(Some(Instant::now())), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwalked_dir_holds_only_what_lies_under_it() {
        let dirs = ["app/sub".to_string()];
        assert!(under_any(&dirs, "app/sub/a.rb"));
        assert!(under_any(&dirs, "app/sub"));
        assert!(!under_any(&dirs, "app/subway/a.rb"));
        assert!(!under_any(&dirs, "app/a.rb"));
        assert!(
            under_any(&[String::new()], "any/a.rb"),
            "the root holds all"
        );
        assert!(!under_any(&[], "app/a.rb"));
    }

    #[test]
    fn sweep_outcome_guards_against_a_failed_empty_walk() {
        // normal warm: completed whole-repo sweep finalizes and completes
        assert_eq!(
            sweep_outcome(true, true, false, true, true),
            (true, Coverage::Complete)
        );
        // a genuinely empty repo (nothing stored before) still completes
        assert_eq!(
            sweep_outcome(true, true, true, false, true),
            (true, Coverage::Complete)
        );
        // THE GUARD (warm only): completed but saw zero files while the index
        // held some → don't finalize (don't wipe), stay warming to retry
        assert_eq!(
            sweep_outcome(true, true, true, true, true),
            (false, Coverage::Warming)
        );
        // an explicit `--index` (unbounded) is trusted: an empty tree reconciles
        assert_eq!(
            sweep_outcome(true, true, true, true, false),
            (true, Coverage::Complete)
        );
        // a budget-cut sweep stays warming and doesn't reconcile
        assert_eq!(
            sweep_outcome(false, true, false, true, true),
            (false, Coverage::Warming)
        );
        // a subtree index is a seed: never reconciles, and leaves coverage
        // warming so later queries keep indexing the rest of the repo
        assert_eq!(
            sweep_outcome(true, false, false, true, true),
            (false, Coverage::Warming)
        );
    }

    /// The auto default tracks the machine. Guards the property that matters —
    /// no ceiling below the core count — rather than a hardcoded number, which
    /// would just restate the implementation.
    #[test]
    fn parse_jobs_defaults_to_one_per_available_core() {
        set_parse_jobs(0); // auto
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // RQ_JOBS wins over auto, so only assert the default when it's unset
        if std::env::var_os("RQ_JOBS").is_none() {
            assert_eq!(parse_jobs(), cores);
        }
        set_parse_jobs(3);
        assert_eq!(parse_jobs(), 3, "an explicit --jobs still wins");
        set_parse_jobs(0);
    }

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
    fn porcelain_z_yields_every_path_including_a_rename_source() {
        let out = b" M a.rb\0M  lib/b.rb\0R  new.rb\0old.rb\0D  gone.rb\0";
        assert_eq!(
            parse_porcelain_z(out),
            ["a.rb", "lib/b.rb", "new.rb", "old.rb", "gone.rb"]
        );
        assert!(parse_porcelain_z(b"").is_empty());
        // a rename from a name that isn't UTF-8 still pairs
        let out = b"R  new.rb\0caf\xe9.rb\0 M a.rb\0";
        assert_eq!(parse_porcelain_z(out), ["new.rb", "a.rb"]);
    }

    #[test]
    fn the_walk_reaches_a_key_under_the_disks_spelling() {
        let root = std::env::temp_dir().join(format!("rq-respell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let nfd = "cafe\u{301}";
        for dir in ["src", nfd, "kept"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("a.rb"), "").unwrap();
        }
        std::fs::write(root.join(".ignore"), "kept/\n").unwrap();
        let folds = std::fs::symlink_metadata(root.join("SRC")).is_ok();
        let (reached, _) = walk_reaches(
            &root,
            ["Src/a.rb", "caf\u{e9}/a.rb", "kept/a.rb", "gone/a.rb"],
        );
        // only where the filesystem holds the two spellings as one name
        let nfd_key = format!("{nfd}/a.rb");
        let normalizes = std::fs::symlink_metadata(root.join("caf\u{e9}")).is_ok();
        assert_eq!(reached.get("Src/a.rb"), folds.then_some("src/a.rb"));
        assert_eq!(
            reached.get("caf\u{e9}/a.rb"),
            normalizes.then_some(nfd_key.as_str())
        );
        assert_eq!(reached.get("kept/a.rb"), None, "ignored under any spelling");
        assert_eq!(reached.get("gone/a.rb"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn index_key_is_a_source_file_below_the_root() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new("/r");
        let non_utf8 = Path::new(std::ffi::OsStr::from_bytes(b"/r/caf\xe9.rb"));
        let cases: [(&Path, Option<&str>); 6] = [
            (Path::new("/r/app/a.rb"), Some("app/a.rb")),
            (Path::new("/elsewhere/a.rb"), None),
            (Path::new("/r/.hidden/a.rb"), None),
            (Path::new("/r/README.md"), None),
            (non_utf8, None),
            (Path::new("/r"), None),
        ];
        for (path, want) in cases {
            assert_eq!(index_key(root, path).as_deref(), want, "{}", path.display());
        }
    }

    #[test]
    fn on_disk_holds_regular_files_only_and_knows_an_error_is_not_absence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rq-on-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        std::fs::write(dir.join("a.rb"), "").unwrap();
        std::fs::write(dir.join("locked/b.rb"), "").unwrap();
        std::os::unix::fs::symlink("a.rb", dir.join("link.rb")).unwrap();
        let lock = |mode| {
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(dir.join("locked"), perms).unwrap();
        };
        lock(0o000);
        let seen = |name: &str| match on_disk(&dir.join(name)) {
            OnDisk::File(_) => "file",
            OnDisk::Absent => "absent",
            OnDisk::Unknown => "unknown",
        };
        let got = [
            "a.rb",
            "link.rb",
            "gone.rb",
            "a.rb/x.rb",
            ".",
            "locked/b.rb",
        ]
        .map(seen);
        lock(0o755);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            got,
            ["file", "absent", "absent", "absent", "absent", "unknown"]
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
