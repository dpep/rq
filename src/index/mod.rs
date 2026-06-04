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
    run_index(store, root, &[], subdirs, None)
}

/// Opportunistic, time-bounded indexing — warm the index a little per call so no
/// single query blocks on a full walk of a large repo. `active` (branch) files
/// are parsed first and ignore the budget (the working set stays fresh); the
/// rest of the walk honors `budget`. A sweep that finishes within budget marks
/// coverage `complete` (and reconciles deletions), otherwise `warming`.
pub fn index_budgeted(
    store: &mut Store,
    root: &Path,
    active: &[String],
    budget: Duration,
) -> Result<Stats, Box<dyn std::error::Error>> {
    run_index(store, root, active, &[], Some(Instant::now() + budget))
}

/// The shared indexing core behind both the explicit (`index_under`) and
/// opportunistic (`index_budgeted`) paths. Collect candidates serially (cheap,
/// mtime-skipping unchanged files into `seen` for deletion reconcile), parse the
/// changed/new ones in parallel, then write them in one batched transaction.
///
/// `active` files are parsed first and ignore `deadline` (the working set);
/// `subdirs` (empty = whole repo) scope the walk; `deadline` bounds it
/// (`None` = unbounded). A whole-repo sweep that finishes within the deadline
/// reconciles deletions and is marked `complete`; a bounded sweep cut short is
/// `warming`; a deliberate subtree is `partial` (never auto-warmed over).
fn run_index(
    store: &mut Store,
    root: &Path,
    active: &[String],
    subdirs: &[String],
    deadline: Option<Instant>,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let identity = detect_identity(root);
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo_id = store.upsert_repository(&identity, branch.as_deref())?;
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.upsert_checkout(repo_id, &root_display.to_string_lossy(), branch.as_deref())?;

    let stored = store.file_mtimes(repo_id)?;
    let mut seen: HashSet<String> = HashSet::new();

    // active (branch) files first: always parsed, so the working set stays fresh
    // even when a tight budget cuts the walk short
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

    // walk the whole repo, or just the requested subtrees — paths stay relative
    // to `root` so they're repo-relative either way
    let walk_roots: Vec<std::path::PathBuf> = if subdirs.is_empty() {
        vec![root.to_path_buf()]
    } else {
        subdirs.iter().map(|s| root.join(s)).collect()
    };
    let mut walk_to_parse: Vec<std::path::PathBuf> = Vec::new();
    let mut completed = true;
    'walk: for walk_root in &walk_roots {
        for result in WalkBuilder::new(walk_root).build() {
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                completed = false;
                break 'walk;
            }
            let Ok(entry) = result else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            note_candidate(root, entry.path(), &stored, &mut seen, &mut walk_to_parse);
        }
    }

    // parse in parallel (the expensive, CPU-bound step), then write in one
    // batched transaction
    let (active_parsed, _) = parse_files(root, &active_to_parse, None);
    let (walk_parsed, parsed_all) = parse_files(root, &walk_to_parse, deadline);
    if !parsed_all {
        completed = false;
    }
    let mut parsed = active_parsed;
    parsed.extend(walk_parsed);
    let (files_indexed, symbols) = store.replace_files(repo_id, &parsed)?;
    let stats = Stats {
        files_seen: seen.len(),
        files_indexed,
        symbols,
    };

    let whole_repo = subdirs.is_empty();
    // a finished whole-repo sweep saw every live file → anything still indexed
    // (but not seen) was deleted on disk
    if completed && whole_repo {
        for path in stored.keys() {
            if !seen.contains(path) {
                store.forget_file(repo_id, path)?;
            }
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
/// known language or can't be read. Touches no store — safe to run on a worker
/// thread (each call builds its own Tree-sitter parser).
fn parse_file(root: &Path, file: &Path) -> Option<crate::store::FileSymbols> {
    let ext = file.extension().and_then(|e| e.to_str())?;
    let plugin = lang::plugin_for_extension(ext)?;
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    let source = std::fs::read_to_string(file).ok()?;
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

/// Parse many files across the available CPUs, stopping early once `deadline`
/// passes. Returns the parsed files and whether *all* of them were parsed (false
/// if the deadline cut it short). Parsing is the expensive, CPU-bound step;
/// writing stays serialized in one batched transaction by the caller.
fn parse_files(
    root: &Path,
    paths: &[std::path::PathBuf],
    deadline: Option<Instant>,
) -> (Vec<crate::store::FileSymbols>, bool) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let past = |d: Option<Instant>| d.is_some_and(|d| Instant::now() >= d);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(paths.len());

    if workers <= 1 {
        let mut out = Vec::new();
        for p in paths {
            if past(deadline) {
                return (out, false);
            }
            if let Some(parsed) = parse_file(root, p) {
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
                        if let Some(parsed) = parse_file(root, p) {
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

/// Walk `root` and extract every symbol in-memory, without touching the store.
/// This is search Layer 4's live scan — it lets `rq` answer even when a
/// repository has never been indexed.
pub fn scan_symbols(root: &Path) -> Vec<crate::core::Symbol> {
    scan_symbols_budgeted(root, &HashSet::new(), None, None)
}

/// Like [`scan_symbols`], but bounded and filtered:
/// - stop once `deadline` passes;
/// - skip any file whose repo-relative path is in `skip` (already indexed);
/// - when `needle` is set, parse only files that contain it (case-insensitive
///   substring) — a ripgrep-style pre-filter that skips the expensive
///   tree-sitter parse on files that can't hold an exact/prefix/substring match.
///
/// The pre-filter still *reads* every candidate file (to scan its bytes) but
/// parses only the survivors, which is where the cost is. It can't see a fuzzy
/// abbreviation (`usr` isn't a substring of `user`); callers fall back to an
/// unfiltered scan when a filtered one comes up empty.
pub fn scan_symbols_budgeted(
    root: &Path,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
    needle: Option<&str>,
) -> Vec<crate::core::Symbol> {
    let needle = needle.map(str::as_bytes).filter(|n| !n.is_empty());
    let mut out = Vec::new();
    for result in WalkBuilder::new(root).build() {
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            break;
        }
        let Ok(entry) = result else { continue };
        // cheap skip before any read: an already-indexed file adds nothing here
        if !skip.is_empty()
            && entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
            && skip.contains(rel.to_string_lossy().as_ref())
        {
            continue;
        }
        let Some((rel, source, plugin)) = source_for(&entry, root) else {
            continue;
        };
        // ripgrep-style: parse only files that actually contain the query
        if let Some(n) = needle
            && !contains_ascii_ci(source.as_bytes(), n)
        {
            continue;
        }
        out.extend(plugin.extract(&rel, &source));
    }
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

/// If `entry` is a source file rq can parse, return its repo-relative path,
/// contents, and the matching plugin.
fn source_for(
    entry: &ignore::DirEntry,
    root: &Path,
) -> Option<(String, String, Box<dyn lang::LanguagePlugin>)> {
    if !entry.file_type().is_some_and(|t| t.is_file()) {
        return None;
    }
    let path = entry.path();
    let ext = path.extension().and_then(|e| e.to_str())?;
    let plugin = lang::plugin_for_extension(ext)?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let source = std::fs::read_to_string(path).ok()?;
    Some((rel, source, plugin))
}

/// Result of revalidating a single file against what's on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Content hash still matches the index.
    Unchanged,
    /// File changed; its symbols were re-extracted.
    Updated,
    /// File no longer on disk; its symbols were forgotten.
    Deleted,
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

/// Lazily revalidate one indexed file against disk: re-extract it if its
/// content changed, forget it if it's gone. This is the staleness check search
/// runs over its top results.
pub fn refresh_file(
    store: &mut Store,
    repository_id: i64,
    root: &Path,
    rel: &str,
) -> Result<Refresh, Box<dyn std::error::Error>> {
    let path = root.join(rel);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            store.forget_file(repository_id, rel)?;
            return Ok(Refresh::Deleted);
        }
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
