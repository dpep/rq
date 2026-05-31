//! Repository identity — the normalized name a logical project is keyed by.
//!
//! Two checkouts of the same project (clone, fork) should resolve to the same
//! identity so symbols and learned behavior aggregate. See `docs/ARCHITECTURE.md`.

use std::fmt;

/// Normalized identity of a logical project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoIdentity {
    /// Derived from an upstream git remote, e.g. `github.com/org/repo`.
    Remote(String),
    /// Fallback: an absolute local path, rendered as `local:/abs/path`.
    Local(String),
    /// An explicit, user-provided name.
    Named(String),
}

impl RepoIdentity {
    /// Build an identity from a git remote URL, normalizing the common
    /// transports to `host/path` form. Returns `None` if no host/path can be
    /// recovered (callers fall back to [`RepoIdentity::Local`]).
    ///
    /// ```text
    /// git@github.com:org/repo.git        -> github.com/org/repo
    /// https://github.com/org/repo.git    -> github.com/org/repo
    /// ssh://git@github.com/org/repo      -> github.com/org/repo
    /// ```
    pub fn from_remote_url(url: &str) -> Option<RepoIdentity> {
        normalize_remote(url).map(RepoIdentity::Remote)
    }

    /// Build a `local:` identity from an absolute path.
    pub fn local(path: &str) -> RepoIdentity {
        RepoIdentity::Local(path.to_string())
    }
}

impl fmt::Display for RepoIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoIdentity::Remote(s) => f.write_str(s),
            RepoIdentity::Local(p) => write!(f, "local:{p}"),
            RepoIdentity::Named(n) => f.write_str(n),
        }
    }
}

/// Reduce a git remote URL to canonical `host/path` (no scheme, user, port,
/// or trailing `.git`).
fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // scp-like syntax: [user@]host:path
    let rest = if let Some(stripped) = strip_scheme(url) {
        // scheme://[user@]host[:port]/path
        stripped
    } else if let Some((host_part, path)) = url.split_once(':') {
        // git@github.com:org/repo.git
        let host = host_part.rsplit('@').next().unwrap_or(host_part);
        return assemble(host, path);
    } else {
        return None;
    };

    let (authority, path) = rest.split_once('/')?;
    let host_with_user = authority;
    let host = host_with_user.rsplit('@').next().unwrap_or(host_with_user);
    let host = host.split(':').next().unwrap_or(host); // drop :port
    assemble(host, path)
}

/// Strip a known scheme prefix, returning the remainder (`authority/path`).
fn strip_scheme(url: &str) -> Option<&str> {
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return Some(rest);
        }
    }
    None
}

fn assemble(host: &str, path: &str) -> Option<String> {
    let host = host.trim().trim_matches('/');
    let path = path.trim().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_scp_syntax() {
        assert_eq!(
            RepoIdentity::from_remote_url("git@github.com:dpep/rq.git"),
            Some(RepoIdentity::Remote("github.com/dpep/rq".into()))
        );
    }

    #[test]
    fn normalizes_https() {
        assert_eq!(
            RepoIdentity::from_remote_url("https://github.com/dpep/rq.git"),
            Some(RepoIdentity::Remote("github.com/dpep/rq".into()))
        );
    }

    #[test]
    fn normalizes_ssh_with_user_and_port() {
        assert_eq!(
            RepoIdentity::from_remote_url("ssh://git@github.com:22/dpep/rq"),
            Some(RepoIdentity::Remote("github.com/dpep/rq".into()))
        );
    }

    #[test]
    fn forks_and_clones_share_identity() {
        let a = RepoIdentity::from_remote_url("git@github.com:dpep/rq.git");
        let b = RepoIdentity::from_remote_url("https://github.com/dpep/rq");
        assert_eq!(a, b);
    }

    #[test]
    fn empty_and_garbage_return_none() {
        assert_eq!(RepoIdentity::from_remote_url(""), None);
        assert_eq!(RepoIdentity::from_remote_url("not-a-url"), None);
    }

    #[test]
    fn display_renders_each_variant() {
        assert_eq!(
            RepoIdentity::Remote("github.com/dpep/rq".into()).to_string(),
            "github.com/dpep/rq"
        );
        assert_eq!(
            RepoIdentity::local("/home/dpep/rq").to_string(),
            "local:/home/dpep/rq"
        );
        assert_eq!(RepoIdentity::Named("work".into()).to_string(), "work");
    }
}
