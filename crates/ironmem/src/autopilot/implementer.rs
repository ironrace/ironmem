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
    Muse,
}

/// What differs between harnesses, named once so `run_issue` asks *why* it
/// branches instead of *which* harness it has. A new harness answers these.
impl Implementer {
    /// The wall-clock bound used when the repo sets none. A supervised CLI
    /// harness (Codex, Muse) reserves its unknown spend per invocation, so it
    /// is never unbounded; Claude keeps the repo's optional bound.
    pub fn fallback_wall_clock_secs(self) -> Option<u64> {
        match self {
            Self::Claude => None,
            Self::Codex | Self::Muse => Some(super::invocation::SUPERVISED_DISPATCH_TIMEOUT_SECS),
        }
    }

    /// A supervised CLI harness (Codex, Muse): each invocation leaves durable
    /// artifacts that restart recovery must adopt or hold, and a dispatch
    /// error does not prove its process dead. Its unknown spend is reserved
    /// before launch against that invocation's ID, so its outcome's cost is
    /// never banked after. Claude's sessions are found through its registry
    /// instead, and Claude reports its cost.
    pub fn recovers_invocations(self) -> bool {
        matches!(self, Self::Codex | Self::Muse)
    }

    /// The harness assigns the session ID (a Codex thread) rather than
    /// taking the controller's (`claude --session-id`), so it is learned
    /// from the first outcome and checked on every later one. Muse continues
    /// the controller's `--session-id`.
    pub fn assigns_own_session_id(self) -> bool {
        self == Self::Codex
    }

    /// A returned verdict spends the session's goal loop, so the next
    /// dispatch needs a fresh session. Claude's `/goal` arms no hook on
    /// `--resume`; a Codex thread and a Muse session continue.
    pub fn rotates_session_after_verdict(self) -> bool {
        self == Self::Claude
    }

    /// The harness registry entry whose binary this implementer launches.
    pub fn launcher_harness(self) -> crate::launcher::Harness {
        match self {
            Self::Claude => crate::launcher::Harness::Claude,
            Self::Codex => crate::launcher::Harness::Codex,
            Self::Muse => crate::launcher::Harness::Muse,
        }
    }

    /// The operator-facing name, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Muse => "Muse",
        }
    }
}

impl FromStr for Implementer {
    type Err = MemoryError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "muse" => Ok(Self::Muse),
            _ => Err(MemoryError::Validation(format!(
                "unknown implementer '{value}'; expected 'claude', 'codex' or 'muse'"
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
    fn unknown_implementer_names_every_valid_value() {
        let err = Implementer::from_str("gemini").unwrap_err().to_string();
        assert!(
            err.contains("expected 'claude', 'codex' or 'muse'"),
            "got: {err}"
        );
    }

    #[test]
    fn muse_is_a_recovering_harness_that_keeps_the_controllers_session() {
        assert_eq!(Implementer::from_str("MUSE").unwrap(), Implementer::Muse);
        assert_eq!(
            serde_json::to_string(&Implementer::Muse).unwrap(),
            "\"muse\""
        );
        assert!(Implementer::Muse.recovers_invocations());
        assert!(!Implementer::Muse.assigns_own_session_id());
        assert!(!Implementer::Muse.rotates_session_after_verdict());
        assert_eq!(Implementer::Muse.fallback_wall_clock_secs(), Some(1200));
        assert_eq!(Implementer::Muse.label(), "Muse");
        assert!(matches!(
            Implementer::Muse.launcher_harness(),
            crate::launcher::Harness::Muse
        ));
    }
}
