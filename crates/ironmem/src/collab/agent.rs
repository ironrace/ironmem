//! Typed agent identity for collab sessions.
//!
//! The protocol has exactly two agents — `Claude` and `Codex`. Pre-refactor
//! they lived as `String`/`&str` everywhere: in `CollabSession.current_owner`,
//! `CollabSession.implementer`, the `actor` parameter of `apply_event`, and
//! the `sender` parameter of `collab_send`. The DB CHECK constraint and an
//! application-layer `require_agent` validator were the only invariant
//! guards. This enum collapses those four `String` representations into one
//! type so the compiler enforces the invariant.
//!
//! `Display`/`FromStr` use the canonical lowercase wire form (`"claude"` /
//! `"codex"`) — same byte forms the DB stores and the MCP layer accepts —
//! so existing on-disk and on-wire payloads round-trip without translation.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// Canonical lowercase wire form. Use this for DB writes, JSON output,
    /// and any string comparison against external input.
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Agent {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            other => Err(format!(
                "unknown agent '{other}': expected 'claude' or 'codex'"
            )),
        }
    }
}

impl TryFrom<&str> for Agent {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_display_and_from_str() {
        for agent in [Agent::Claude, Agent::Codex] {
            assert_eq!(agent.to_string().parse::<Agent>().unwrap(), agent);
        }
    }

    #[test]
    fn rejects_unknown_string() {
        let err = "gemini".parse::<Agent>().unwrap_err();
        assert!(err.contains("unknown agent"));
    }

    #[test]
    fn as_str_is_lowercase_canonical() {
        assert_eq!(Agent::Claude.as_str(), "claude");
        assert_eq!(Agent::Codex.as_str(), "codex");
    }
}
