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
/// subtrees of it (a partial index of the repo). Paths stay repo-relative and
/// the identity is still the repo's, so a subset slots into the same index;
/// coverage is marked `partial` so a later search won't silently re-index the
/// whole repo over your deliberate subset.
pub fn index_under(
    store: &mut Store,
    root: &Path,
    subdirs: &[String],
) -> Result<Stats, Box<dyn std::error::Error>> {
    let identity = detect_identity(root);
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo_id = store.upsert_repository(&identity, branch.as_deref())?;
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.upsert_checkout(repo_id, &root_display.to_string_lossy(), branch.as_deref())?;

    // walk the whole repo, or just the requested subtrees — paths are always
    // taken relative to `root` so they're repo-relative either way
    let walk_roots: Vec<std::path::PathBuf> = if subdirs.is_empty() {
        vec![root.to_path_buf()]
    } else {
        subdirs.iter().map(|s| root.join(s)).collect()
    };

    let mut stats = Stats::default();
    for walk_root in &walk_roots {
        for result in WalkBuilder::new(walk_root).build() {
            let Ok(entry) = result else { continue };
            let Some((rel, source, plugin)) = source_for(&entry, root) else {
                continue;
            };
            stats.files_seen += 1;

            let hash = content_hash(&source);
            if store.file_unchanged(repo_id, &rel, &hash)? {
                continue;
            }

            let symbols = plugin.extract(&rel, &source);
            let mtime = file_mtime(entry.path());
            let language = symbols
                .first()
                .map(|s| s.language.clone())
                .unwrap_or_else(|| "unknown".to_string());
            store.replace_file_symbols(repo_id, &rel, &language, mtime, &hash, &symbols)?;
            stats.files_indexed += 1;
            stats.symbols += symbols.len();
        }
    }

    // Phase 4: capture per-file git last-commit times for the recency signal.
    // One git call per index (not per search); best-effort.
    if is_git_repo(root) {
        let times = git_commit_times(root, 1000);
        if !times.is_empty() {
            let _ = store.set_file_git_ts(repo_id, &times);
        }
    }

    let status = if subdirs.is_empty() {
        "complete"
    } else {
        "partial"
    };
    store.set_coverage(
        repo_id,
        stats.files_seen as i64,
        stats.files_indexed as i64,
        status,
    )?;
    Ok(stats)
}

/// Opportunistic, time-bounded indexing — warm the index a little per call so
/// no single query blocks on a full walk of a large repo.
///
/// Indexes the `active` (branch) files first — what you're most likely working
/// on, so they stay fresh — then walks the rest until `budget` elapses,
/// skipping files whose mtime is unchanged with a cheap `stat` (no read). Marks
/// coverage `complete` when a full sweep finishes within budget (and then
/// reconciles deletions + captures commit times), otherwise `warming`. Each
/// call resumes the warming from a clean walk; unchanged files are near-free, so
/// repeated calls converge cheaply and pick up new/changed files as they appear.
pub fn index_budgeted(
    store: &mut Store,
    root: &Path,
    active: &[String],
    budget: Duration,
) -> Result<Stats, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + budget;
    let identity = detect_identity(root);
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo_id = store.upsert_repository(&identity, branch.as_deref())?;
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.upsert_checkout(repo_id, &root_display.to_string_lossy(), branch.as_deref())?;

    let stored = store.file_mtimes(repo_id)?;
    let mut stats = Stats::default();
    let mut seen: HashSet<String> = HashSet::new();

    // active files first: they're the working set, so keep them fresh even if the
    // budget cuts the walk short
    for rel in active {
        index_file(
            store,
            repo_id,
            root,
            &root.join(rel),
            &stored,
            &mut stats,
            &mut seen,
        )?;
    }

    let mut completed = true;
    for result in WalkBuilder::new(root).build() {
        if Instant::now() >= deadline {
            completed = false;
            break;
        }
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        index_file(
            store,
            repo_id,
            root,
            entry.path(),
            &stored,
            &mut stats,
            &mut seen,
        )?;
    }

    if completed {
        // a full sweep saw every live file: anything still in the index is gone
        for path in stored.keys() {
            if !seen.contains(path) {
                store.forget_file(repo_id, path)?;
            }
        }
        // commit times feed the recency signal. Only worth the (relatively
        // expensive) `git log` when this sweep actually (re)indexed something —
        // a clean, already-complete repo re-sweeps every query and must not pay
        // a git log each time. New/changed files (files_indexed > 0) refresh it.
        if stats.files_indexed > 0 && is_git_repo(root) {
            let times = git_commit_times(root, 1000);
            if !times.is_empty() {
                let _ = store.set_file_git_ts(repo_id, &times);
            }
        }
    }

    let status = if completed { "complete" } else { "warming" };
    let (files, _) = store.repo_totals(repo_id).unwrap_or((0, 0));
    store.set_coverage(repo_id, files, stats.files_indexed as i64, status)?;
    Ok(stats)
}

/// Index a single file into the store, skipping it when unchanged. The shared
/// step behind both the active-files pass and the walk in [`index_budgeted`]:
/// a cheap mtime match short-circuits before any read; otherwise the content
/// hash guards against a needless re-extract. Records the path in `seen`.
#[allow(clippy::too_many_arguments)]
fn index_file(
    store: &mut Store,
    repo_id: i64,
    root: &Path,
    file: &Path,
    stored: &HashMap<String, Option<i64>>,
    stats: &mut Stats,
    seen: &mut HashSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
        return Ok(());
    };
    let Some(plugin) = lang::plugin_for_extension(ext) else {
        return Ok(());
    };
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    if seen.contains(&rel) {
        return Ok(());
    }
    stats.files_seen += 1;
    let mtime = file_mtime(file);
    // cheap path: same mtime as indexed → unchanged, skip without reading
    if let Some(&stored_mtime) = stored.get(&rel)
        && stored_mtime.is_some()
        && stored_mtime == mtime
    {
        seen.insert(rel);
        return Ok(());
    }
    let Ok(source) = std::fs::read_to_string(file) else {
        return Ok(());
    };
    let hash = content_hash(&source);
    if store.file_unchanged(repo_id, &rel, &hash)? {
        seen.insert(rel);
        return Ok(());
    }
    let symbols = plugin.extract(&rel, &source);
    let language = symbols
        .first()
        .map(|s| s.language.clone())
        .unwrap_or_else(|| "unknown".to_string());
    store.replace_file_symbols(repo_id, &rel, &language, mtime, &hash, &symbols)?;
    stats.files_indexed += 1;
    stats.symbols += symbols.len();
    seen.insert(rel);
    Ok(())
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
    scan_symbols_budgeted(root, &HashSet::new(), None)
}

/// Like [`scan_symbols`], but bounded: stop once `deadline` passes, and skip any
/// file whose repo-relative path is in `skip` (the already-indexed set). Used as
/// a git-repo fallback — the budget then covers *un-indexed* ground rather than
/// re-parsing what the warm pass already has, and stays bounded on a huge repo.
pub fn scan_symbols_budgeted(
    root: &Path,
    skip: &HashSet<String>,
    deadline: Option<Instant>,
) -> Vec<crate::core::Symbol> {
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
        out.extend(plugin.extract(&rel, &source));
    }
    out
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
