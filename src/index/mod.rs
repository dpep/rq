//! Indexing — walk a checkout, extract symbols, persist incrementally.
//!
//! Decoupled from search: it only writes. Unchanged files (same content hash)
//! are skipped, and coverage is recorded so search can judge its own confidence.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

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

/// Index the repository rooted at `root` into `store`.
pub fn index_path(store: &mut Store, root: &Path) -> Result<Stats, Box<dyn std::error::Error>> {
    let identity = detect_identity(root);
    let branch = git_output(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo_id = store.upsert_repository(&identity, branch.as_deref())?;
    let root_display = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.upsert_checkout(repo_id, &root_display.to_string_lossy(), branch.as_deref())?;

    let mut stats = Stats::default();
    for result in WalkBuilder::new(root).build() {
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

    // Phase 4: capture per-file git last-commit times for the recency signal.
    // One git call per index (not per search); best-effort.
    if is_git_repo(root) {
        let times = git_commit_times(root, 1000);
        if !times.is_empty() {
            let _ = store.set_file_git_ts(repo_id, &times);
        }
    }

    store.set_coverage(
        repo_id,
        stats.files_seen as i64,
        stats.files_indexed as i64,
        "complete",
    )?;
    Ok(stats)
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
    let mut out = Vec::new();
    for result in WalkBuilder::new(root).build() {
        let Ok(entry) = result else { continue };
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
/// is gated on this so a stray query never walks a non-repo directory.
pub fn is_git_repo(root: &Path) -> bool {
    git_output(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
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
