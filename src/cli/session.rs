//! What every search shares: the checkout it's asked from, the store, and the branch's changed files.

use super::*;

/// Where a command is asked from, and what the index knows of it. Every
/// command that acts on "this checkout" builds one, so they agree on which
/// checkout that is and whether rq may warm it.
pub(super) struct Here {
    /// The git work tree's root, else the directory itself — never a subdir
    /// of a repo, which would re-key it under subdir-relative paths that the
    /// deletion reconcile would then forget. Canonical, as indexing keys it.
    pub(super) root: PathBuf,
    pub(super) is_git: bool,
    pub(super) coverage: Option<Coverage>,
    pub(super) checkout: Option<Checkout>,
}

impl Here {
    pub(super) fn at(store: &Store, start: &std::path::Path) -> Here {
        let (root, is_git) = checkout_root(start);
        let mut here = Here {
            root,
            is_git,
            coverage: None,
            checkout: None,
        };
        here.refresh(store);
        here
    }

    /// Re-read what the index knows, after a pass here.
    pub(super) fn refresh(&mut self, store: &Store) {
        self.coverage = store.coverage_status(&root_key(&self.root)).ok().flatten();
        self.checkout = checkout_at(store, &self.root);
    }

    /// Whether rq may index here: a git work tree (safe to auto-discover), or
    /// any dir it already tracks — one earns tracking by being explicitly
    /// `--index`ed. Never an unknown non-git dir: don't walk a random directory.
    pub(super) fn warms(&self) -> bool {
        self.is_git || self.coverage.is_some()
    }
}

/// [`Here::root`] for a command at `start`, and whether it's a git work tree.
pub(super) fn checkout_root(start: &std::path::Path) -> (PathBuf, bool) {
    match crate::index::repo_root(start) {
        Some(root) => (root, true),
        None => (
            start.canonicalize().unwrap_or_else(|_| start.to_path_buf()),
            false,
        ),
    }
}

/// Default action: search the index and print ranked results.
/// Everything a search needs that doesn't depend on the query: the open store,
/// the repo it's rooted in, the branch's changed files, and who that repo is.
///
/// Split out because it's the expensive half — opening the store, resolving the
/// root, reading branch files, resolving identity — and none of it varies per
/// query. One search builds one and drops it; a caller answering many can build
/// it once. Deliberately *not* holding the warm decision: that one is entangled
/// with the query (the indexer path-prioritises toward it) and belongs to a
/// single search.
pub(super) struct Session {
    pub(super) store: Store,
    pub(super) cwd: Option<PathBuf>,
    /// The checkout the cwd is in; `None` when there's no cwd to ask from.
    pub(super) here: Option<Here>,
    pub(super) active_paths: Vec<String>,
    pub(super) branch_refresh: Option<BranchRefresh>,
    pub(super) identity: Option<String>,
    /// Where the queries are asked from (`--anchor`), resolved once.
    pub(super) anchor: Option<crate::search::Anchor>,
    /// Whether the worktree has moved since its index ([`worktree_moved`]),
    /// once someone in this process asked — and dispatched the reindex that
    /// answer called for. A batch asks up front; a single search on a miss.
    pub(super) moved: Option<bool>,
}

impl Session {
    /// Resolve the search context, or the exit code to fail with.
    pub(super) fn open(out: Output) -> std::result::Result<Session, ExitCode> {
        let open_span = crate::profile::span("store open");
        let store = open_store_or_fail(out, "rq")?;
        drop(open_span);
        let git_span = crate::profile::span("setup: git root");
        let cwd = std::env::current_dir().ok();
        let here = cwd.as_deref().map(|c| Here::at(&store, c));
        drop(git_span);

        // Files you're changing on this feature branch (and their directory
        // neighbors): the branch ranking boost, and the warm pass's priority set.
        let mut branch_span = crate::profile::span("setup: branch files");
        let (active_paths, branch_refresh, cached_cost) = match &here {
            Some(h) if h.is_git => cached_branch_files(&store, &h.root),
            _ => (Vec::new(), None, None),
        };
        branch_span.note(|| {
            let how = if branch_refresh.is_some() {
                "cached, refreshing alongside"
            } else {
                "cached"
            };
            // The window is derived, not constant — say which one is in force,
            // or a slow repo's backoff looks like rq ignoring stale state.
            let ttl = branch_files_ttl(cached_cost);
            format!("{} changed, {how} ({ttl}s window)", active_paths.len())
        });
        drop(branch_span);

        // Resolve identity from the repo root, cache-first: looked up by checkout root
        // (no `git remote` fork), falling back to git only the first time we see a
        // repo. Computed even for non-git dirs so an explicitly `--index`ed one is
        // still recognized as the current repo below.
        let mut identity_span = crate::profile::span("setup: identity");
        let identity = here.as_ref().map(|h| resolve_identity(&store, &h.root));
        identity_span.note(|| {
            let coverage = here.as_ref().and_then(|h| h.coverage);
            coverage.map_or("unknown", Coverage::as_str).to_string()
        });
        drop(identity_span);
        Ok(Session {
            store,
            cwd,
            here,
            active_paths,
            branch_refresh,
            identity,
            anchor: None,
            moved: None,
        })
    }

    /// Resolve `--anchor` against this session: the repo its file sits in (the
    /// one we're in, when it's under our root), its path there, and what
    /// encloses its line — read live if the index doesn't hold this version.
    pub(super) fn anchor_at(&self, spec: &AnchorSpec) -> crate::search::Anchor {
        let _span = crate::profile::span("setup: anchor");
        let here = self.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        let abs = here.join(&spec.file);
        let abs = abs.canonicalize().unwrap_or(abs);
        let root = self
            .here
            .as_ref()
            .map(|h| h.root.clone())
            .filter(|r| abs.starts_with(r))
            .or_else(|| crate::index::repo_root(&abs))
            .or_else(|| abs.parent().map(PathBuf::from))
            .unwrap_or(here);
        let identity = resolve_identity(&self.store, &root);
        let rel = crate::index::rel_key(&root, &abs)
            .unwrap_or_else(|| spec.file.to_string_lossy().into_owned());
        let checkout = checkout_at(&self.store, &root);
        let defs = crate::index::current_definitions(&self.store, checkout, &identity, &root, &rel);
        crate::search::Anchor::new(root_key(&root), rel, spec.line, &defs)
    }
}

/// How long a branch-file list is served before it's refreshed. A commit or a
/// checkout is caught by the stamp; a bare working-tree edit touches neither
/// `.git/HEAD` nor `.git/index`, so only elapsed time catches that — short
/// enough that a burst of searches shares one computation and the edit you just
/// made is reflected on the next search.
pub(super) const BRANCH_FILES_TTL_SECS: i64 = 15;

/// Longest a branch-file list may be served for, however slow it is to rebuild.
/// The window only governs noticing an *unstaged* edit — every git operation
/// invalidates by stamp regardless — so five minutes is already generous.
pub(super) const BRANCH_FILES_TTL_MAX_SECS: i64 = 300;

/// How many times its own rebuild cost a list may be served for — so the
/// refresh never eats more than about 1% of the time between searches. The
/// floor binds below ~150 ms, which is where every small repo sits.
///
/// The refresh forks two `git diff`s over the whole worktree, which is cheap on
/// a small repo and very much not on a large one — it was measured at 700 ms on
/// a 90k-file monorepo, against a 40-115 ms query it runs *alongside* and
/// competes with for disk. A fixed 15-second window then re-paid that every
/// fifteen seconds of active searching. Scaling the window by the measured cost
/// leaves small repos exactly where they were and backs off only where the
/// evidence says it's needed.
const BRANCH_FILES_WINDOW_MULTIPLE: i64 = 100;

/// How long a cached branch-file list stays good, given what it cost to build.
pub(super) fn branch_files_ttl(cost_ms: Option<u64>) -> i64 {
    // `as i64` would wrap a large cost to a negative and quietly hand it the
    // floor — the opposite of what an expensive rebuild has earned.
    let earned = cost_ms.map_or(0, |ms| {
        i64::try_from(ms)
            .unwrap_or(i64::MAX)
            .saturating_mul(BRANCH_FILES_WINDOW_MULTIPLE)
            / 1000
    });
    earned.clamp(BRANCH_FILES_TTL_SECS, BRANCH_FILES_TTL_MAX_SECS)
}

/// A branch-file recomputation running alongside the search. The git work
/// happens on the thread; the store write waits for the main thread, since a
/// SQLite connection isn't shared.
pub(super) struct BranchRefresh {
    /// Yields the file list and what it cost to build, which sets how long
    /// the result stays good.
    handle: std::thread::JoinHandle<(Vec<String>, u64)>,
    /// The checkout's key ([`root_key`]).
    root: String,
    stamp: String,
}

impl BranchRefresh {
    /// Wait for the recomputation and store it for the next query.
    pub(super) fn store(self, store: &Store) {
        let Ok((files, cost_ms)) = self.handle.join() else {
            return;
        };
        let _ = store.branch_files_set(&self.root, &self.stamp, now_unix(), cost_ms, &files);
    }
}

/// The branch-changed file list, served from the store when it's still good.
/// Returns the list, plus a recomputation to collect after results print when
/// the stored one has aged out.
///
/// The list feeds a *ranking boost*, so serving a slightly old one costs a
/// little ranking quality, while recomputing it first would cost every search
/// the git diff behind it — which is O(tracked files). So the stored list is
/// served immediately and the refresh runs concurrently with the search rather
/// than after it, which usually hides its cost entirely. It feeds only the next
/// query, so nothing about this run's ranking depends on how the race lands.
///
/// The first search in a repo has nothing to serve and computes inline; that's
/// once per repo, like the first index.
fn cached_branch_files(
    store: &Store,
    root: &std::path::Path,
) -> (Vec<String>, Option<BranchRefresh>, Option<u64>) {
    let key = root_key(root);
    let stamp = crate::index::branch_files_stamp(root);
    let cached = store.branch_files_get(&key).ok().flatten();
    let now = now_unix();

    if let (Some(hit), Some(stamp)) = (&cached, &stamp) {
        if &hit.stamp == stamp && now.saturating_sub(hit.written_at) < branch_files_ttl(hit.cost_ms)
        {
            return (hit.files.clone(), None, hit.cost_ms);
        }
        let owned_root = root.to_path_buf();
        let refresh = BranchRefresh {
            handle: std::thread::spawn(move || {
                let t = std::time::Instant::now();
                let files = crate::index::branch_changed_files(&owned_root);
                (files, t.elapsed().as_millis() as u64)
            }),
            root: key,
            stamp: stamp.clone(),
        };
        return (hit.files.clone(), Some(refresh), hit.cost_ms);
    }

    // Nothing cached (or no git state to stamp it with): compute inline.
    let t = std::time::Instant::now();
    let files = crate::index::branch_changed_files(root);
    let cost_ms = t.elapsed().as_millis() as u64;
    if let Some(stamp) = stamp {
        let _ = store.branch_files_set(&key, &stamp, now, cost_ms, &files);
    }
    (files, None, Some(cost_ms))
}

/// The key a checkout is stored under: its canonical root, as indexing
/// records it.
pub(super) fn root_key(root: &std::path::Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The checkout rooted at `root`, once a pass has registered it.
pub(super) fn checkout_at(store: &Store, root: &std::path::Path) -> Option<Checkout> {
    store.checkout(&root_key(root)).ok().flatten()
}

/// The repository's normalized identity for `cwd`, cache-first: look it up by
/// the canonical cwd (the checkout root indexing records), so a known repo (git
/// or explicitly `--index`ed) costs no `git` fork. On a cache miss, a non-git
/// dir resolves to its `local:` path directly (still no fork); only a git work
/// tree we haven't seen yet pays a `git remote` call.
pub(super) fn resolve_identity(store: &Store, cwd: &std::path::Path) -> String {
    if let Ok(canon) = cwd.canonicalize() {
        if let Ok(Some(identity)) = store.identity_for_root(&canon.to_string_lossy()) {
            return identity;
        }
        if crate::index::repo_root(cwd).is_none() {
            return crate::core::RepoIdentity::local(&canon.to_string_lossy()).to_string();
        }
    }
    crate::index::detect_identity(cwd).to_string()
}

/// Open the rq database, honoring `RQ_DB` and creating parent dirs.
pub(super) fn open_store() -> Result<Store, Box<dyn std::error::Error>> {
    let path = db_location()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Store::open(&path)?)
}

/// [`open_store`] for a command, or the exit code of the error it reported
/// as `{mode}: …` (a structured caller gets it as JSON).
pub(super) fn open_store_or_fail(out: Output, mode: &str) -> std::result::Result<Store, ExitCode> {
    open_store().map_err(|e| {
        fail(
            out,
            Failure::Database,
            format_args!("{mode}: cannot open database: {e}"),
        )
    })
}

/// The database path from the environment, or the usage error that refuses it.
/// A relative path would resolve against each caller's cwd, silently splitting
/// the one shared index into a database per directory.
pub(super) fn db_location() -> Result<PathBuf, String> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
    db_location_from(var("RQ_DB"), var("HOME"))
}

pub(super) fn db_location_from(
    rq_db: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    if let Some(db) = rq_db {
        let shown = db.to_string_lossy();
        if std::path::Path::new(&db).is_relative() {
            // the shell doesn't expand a quoted or mid-word `~`
            let hint = match shown.strip_prefix("~/") {
                Some(rest) => format!("$HOME/{rest}"),
                None => format!("$PWD/{shown}"),
            };
            return Err(format!(
                "rq: RQ_DB must be an absolute path, not {shown:?} (e.g. RQ_DB=\"{hint}\")"
            ));
        }
        if shown.ends_with('/') || std::path::Path::new(&db).is_dir() {
            return Err(format!(
                "rq: RQ_DB names a directory, not a database file: {shown:?} (e.g. RQ_DB=\"{}/rq.db\")",
                shown.trim_end_matches('/')
            ));
        }
        return Ok(PathBuf::from(db));
    }
    let Some(home) = home else {
        return Err(
            "rq: HOME is not set, so there's no default index path; set HOME, or RQ_DB to an absolute path".into(),
        );
    };
    if std::path::Path::new(&home).is_relative() {
        return Err(format!(
            "rq: HOME is a relative path ({:?}), so the default index would move with the working directory; set RQ_DB to an absolute path",
            home.to_string_lossy()
        ));
    }
    Ok(PathBuf::from(home).join(".local/share/rq/rq.db"))
}
