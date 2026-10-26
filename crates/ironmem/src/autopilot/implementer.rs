//! Select the implementation harness independently of its model.
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Implementer {
    #[default]
    Claude,
    Codex,
}

/// What differs between harnesses, named once so `run_issue` asks *why* it
/// branches instead of *which* harness it has. A new harness answers these.
impl Implementer {
    /// The wall-clock bound used when the repo sets none. Codex reserves its
    /// unknown spend per invocation, so it is never unbounded; Claude keeps
    /// the repo's optional bound.
    pub fn fallback_wall_clock_secs(self) -> Option<u64> {
        match self {
            Self::Claude => None,
            Self::Codex => Some(super::codex_dispatch::CODEX_DISPATCH_TIMEOUT_SECS),
        }
    }

    /// Each invocation leaves durable artifacts that restart recovery must
    /// adopt or hold, and a dispatch error does not prove its process dead.
    /// Its unknown spend is reserved before launch against that invocation's
    /// ID, so its outcome's cost is never banked after. Claude's sessions are
    /// found through its registry instead, and Claude reports its cost.
    pub fn recovers_invocations(self) -> bool {
        self == Self::Codex
    }

    /// The harness assigns the session ID (a Codex thread) rather than
    /// taking the controller's (`claude --session-id`), so it is learned
    /// from the first outcome and checked on every later one.
    pub fn assigns_own_session_id(self) -> bool {
        self == Self::Codex
    }

    /// A returned verdict spends the session's goal loop, so the next
    /// dispatch needs a fresh session. Claude's `/goal` arms no hook on
    /// `--resume`; a Codex thread continues.
    pub fn rotates_session_after_verdict(self) -> bool {
        self == Self::Claude
    }
}

impl FromStr for Implementer {
    type Err = MemoryError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(MemoryError::Validation(format!(
                "unknown implementer '{value}'; expected 'claude' or 'codex'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::Implementer;

    #[test]
    fn implementer_defaults_to_claude_and_round_trips_snake_case() {
        assert_eq!(Implementer::default(), Implementer::Claude);
        assert_eq!(
            Implementer::from_str("CLAUDE").unwrap(),
            Implementer::Claude
        );
        assert_eq!(Implementer::from_str("CodEx").unwrap(), Implementer::Codex);
        assert_eq!(
            serde_json::to_string(&Implementer::Codex).unwrap(),
            "\"codex\""
        );
        assert_eq!(
            serde_json::from_str::<Implementer>("\"claude\"").unwrap(),
            Implementer::Claude
        );
    }

    #[test]
    fn unknown_implementer_names_both_valid_values() {
        let err = Implementer::from_str("gemini").unwrap_err().to_string();
        assert!(err.contains("expected 'claude' or 'codex'"), "got: {err}");
    }
}
