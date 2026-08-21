//! Who invoked rq — the caller label recorded alongside each search.
//!
//! Deliberately coarse. This answers "how much of rq's traffic is agents, and
//! which flags do they reach for", so the labels are a small fixed vocabulary
//! rather than a fingerprint. Nothing here is read by ranking.

use std::io::IsTerminal;

/// Longest caller label kept. Env values are arbitrary strings; a label is a
/// histogram bucket, so cap it rather than let one write a paragraph.
const MAX_LABEL: usize = 24;

/// The caller label for this invocation.
pub(crate) fn detect() -> String {
    detect_from(|k| std::env::var(k).ok(), std::io::stdout().is_terminal())
}

/// The pure half, so the taxonomy is testable without touching the real
/// environment. Ordered most-specific first: a known agent beats the terminal
/// check, because an agent's shell is not a terminal but a script's may be.
pub(crate) fn detect_from(env: impl Fn(&str) -> Option<String>, tty: bool) -> String {
    let get = |k: &str| env(k).filter(|v| !v.is_empty() && v != "0");

    if get("CLAUDECODE").is_some() {
        // The entrypoint distinguishes a shell call from an MCP or SDK one.
        // `cli` is the overwhelming case and stays unadorned so the common
        // bucket keeps a stable name.
        return match get("CLAUDE_CODE_ENTRYPOINT").as_deref() {
            None | Some("cli") => "claude-code".to_string(),
            Some(entry) => format!("claude-code:{}", label(entry)),
        };
    }
    // Emerging convention: `AI_AGENT=<tool>_<version>_agent`. Keep the tool —
    // the version would split one bucket into a new one every release.
    if let Some(v) = get("AI_AGENT") {
        return label(v.split('_').next().unwrap_or_default());
    }
    if get("CURSOR_TRACE_ID").is_some() || get("CURSOR_AGENT").is_some() {
        return "cursor".to_string();
    }
    if get("CI").is_some() || get("GITHUB_ACTIONS").is_some() {
        return "ci".to_string();
    }
    // Nothing identified the caller: a terminal means a person typed it, and
    // anything else is a pipe we can't attribute. Never guess "agent" here —
    // an unattributed pipe is a real answer, and a wrong label is worse than a
    // vague one.
    if tty {
        "human".to_string()
    } else {
        "piped".to_string()
    }
}

/// Normalize an env-derived fragment into a label: lowercase, `[a-z0-9.-]`,
/// bounded. Anything that survives to empty is `unknown`.
fn label(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(MAX_LABEL)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an env lookup from pairs.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn identifies_claude_code_and_its_entrypoint() {
        assert_eq!(
            detect_from(env(&[("CLAUDECODE", "1")]), false),
            "claude-code"
        );
        // the common entrypoint doesn't get its own bucket
        assert_eq!(
            detect_from(
                env(&[("CLAUDECODE", "1"), ("CLAUDE_CODE_ENTRYPOINT", "cli")]),
                false
            ),
            "claude-code"
        );
        assert_eq!(
            detect_from(
                env(&[("CLAUDECODE", "1"), ("CLAUDE_CODE_ENTRYPOINT", "mcp")]),
                false
            ),
            "claude-code:mcp"
        );
    }

    #[test]
    fn keeps_the_tool_from_ai_agent_but_not_its_version() {
        assert_eq!(
            detect_from(env(&[("AI_AGENT", "claude-code_2-1-236_agent")]), false),
            "claude-code"
        );
    }

    #[test]
    fn falls_back_to_the_terminal_check() {
        assert_eq!(detect_from(env(&[]), true), "human");
        assert_eq!(detect_from(env(&[]), false), "piped");
        // an agent is still an agent when it happens to hold a terminal
        assert_eq!(
            detect_from(env(&[("CLAUDECODE", "1")]), true),
            "claude-code"
        );
    }

    #[test]
    fn ignores_unset_and_falsey_values() {
        assert_eq!(detect_from(env(&[("CLAUDECODE", "")]), true), "human");
        assert_eq!(detect_from(env(&[("CLAUDECODE", "0")]), true), "human");
        assert_eq!(detect_from(env(&[("CI", "0")]), true), "human");
    }

    #[test]
    fn sanitizes_a_hostile_label() {
        let long = "x".repeat(100);
        assert_eq!(label(&long).len(), MAX_LABEL);
        assert_eq!(label("Foo Bar/../;drop"), "foobar..drop");
        assert_eq!(label("   "), "unknown");
    }
}
