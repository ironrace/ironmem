//! Typed agent identity for collab sessions.
//!
//! **Boundary note:** Generic harness identity (which AI assistant is running)
//! is represented by [`crate::harness::HarnessId`] and the extensible
//! [`crate::harness::REGISTRY`].  This `Agent` enum is *not* a general harness
//! identifier — it is the **two-party collab protocol role type**.  The
//! current collab protocol version is intentionally Claude↔Codex-specific;
//! adding a third party would require a v2 protocol, not a new enum variant.
//! The compiler keeps `HarnessId` and `Agent` distinct: harness-generic code
//! uses `HarnessId`; collab-protocol code uses `Agent`.
//!
//! Pre-refactor the roles lived as `String`/`&str` everywhere: in
//! `CollabSession.current_owner`, `CollabSession.implementer`, the `actor`
//! parameter of `apply_event`, and the `sender` parameter of `collab_send`.
//! The DB CHECK constraint and an application-layer `require_agent` validator
//! were the only invariant guards.  This enum collapses those four `String`
//! representations into one type so the compiler enforces the invariant.
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

    /// Returns the [`crate::harness::HarnessId`] that corresponds to this
    /// collab role — the single name source linking the closed protocol enum
    /// to the open harness registry.
    ///
    /// `Agent::Claude.harness_id()` resolves to registry id `"claude"`;
    /// `Agent::Codex.harness_id()` resolves to `"codex"`.  The returned id
    /// is identical to `as_str()` so that DB values, wire bytes, and registry
    /// lookups all agree on the same string.
    pub fn harness_id(self) -> crate::harness::HarnessId {
        crate::harness::HarnessId::new_unchecked(self.as_str())
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

/// The two-role pairing for a collab session, bundled into a single named
/// type so callers cannot silently transpose `pilot` and `implementer`.
///
/// Before this type existed, the role pair was threaded through call sites
/// as two trailing positional `Agent` arguments — and different functions
/// disagreed on the order (`create_session`/`collab_create_session` took
/// `(implementer, pilot)`, while `CollabSession::new_with_roles` took
/// `(pilot, implementer)`). A caller copying a call-site pattern from one
/// function to the other could silently swap the roles. Named fields make
/// that transposition a compile error instead of a silent bug: every
/// construction site must write `pilot: ..., implementer: ...` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CollabRoles {
    pub pilot: Agent,
    pub implementer: Agent,
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

    #[test]
    fn harness_id_matches_registry_entry() {
        use crate::harness;
        for agent in [Agent::Claude, Agent::Codex] {
            let hid = agent.harness_id();
            let spec = harness::by_id(hid.as_str(), harness::REGISTRY).unwrap_or_else(|| {
                panic!(
                    "Agent {:?} harness_id '{}' must resolve in REGISTRY",
                    agent,
                    hid.as_str()
                )
            });
            assert_eq!(spec.id, hid.as_str());
            // Wire form and registry id must agree
            assert_eq!(agent.as_str(), spec.id);
        }
    }
}
