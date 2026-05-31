//! Indexing — walk a checkout, extract symbols, persist incrementally.
//!
//! Decoupled from search: it only writes. Unchanged files (same content hash)
//! are skipped, and coverage is recorded so search can judge its own confidence.

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
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(plugin) = lang::plugin_for_extension(ext) else {
            continue;
        };
        stats.files_seen += 1;

        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let hash = content_hash(&source);
        if store.file_unchanged(repo_id, &rel, &hash)? {
            continue;
        }

        let symbols = plugin.extract(&rel, &source);
        let mtime = file_mtime(path);
        let language = symbols
            .first()
            .map(|s| s.language.clone())
            .unwrap_or_else(|| ext.to_string());
        store.replace_file_symbols(repo_id, &rel, &language, mtime, &hash, &symbols)?;
        stats.files_indexed += 1;
        stats.symbols += symbols.len();
    }

    store.set_coverage(
        repo_id,
        stats.files_seen as i64,
        stats.files_indexed as i64,
        "complete",
    )?;
    Ok(stats)
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
}
