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
    /// Every harness, once. `tests::every_variant_is_listed` matches
    /// exhaustively, so a new variant fails to compile until it is added here.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Muse];

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
        // Exhaustive on purpose (no `matches!`, no `_`): a new harness must
        // answer this, because a wrong `false` skips restart recovery and
        // lets a crash launch a second implementer over a live one.
        match self {
            Self::Claude => false,
            Self::Codex | Self::Muse => true,
        }
    }

    /// The harness assigns the session ID (a Codex thread) rather than
    /// taking the controller's (`claude --session-id`), so it is learned
    /// from the first outcome and checked on every later one. Muse continues
    /// the controller's `--session-id`.
    pub fn assigns_own_session_id(self) -> bool {
        match self {
            Self::Codex => true,
            Self::Claude | Self::Muse => false,
        }
    }

    /// A returned verdict spends the session's goal loop, so the next
    /// dispatch needs a fresh session. Claude's `/goal` arms no hook on
    /// `--resume`; a Codex thread and a Muse session continue.
    pub fn rotates_session_after_verdict(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex | Self::Muse => false,
        }
    }

    /// The harness registry entry whose binary this implementer launches.
    pub fn launcher_harness(self) -> crate::launcher::Harness {
        match self {
            Self::Claude => crate::launcher::Harness::Claude,
            Self::Codex => crate::launcher::Harness::Codex,
            Self::Muse => crate::launcher::Harness::Muse,
        }
    }

    /// The `--implementer` spelling, which is also the stored (serde) one.
    /// [`FromStr`] reads it back, and every hint that tells the operator
    /// which command to run builds the flag from it.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Muse => "muse",
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
        Self::ALL
            .into_iter()
            .find(|implementer| implementer.cli_name().eq_ignore_ascii_case(value))
            .ok_or_else(|| {
                MemoryError::Validation(format!(
                    "unknown implementer '{value}'; expected 'claude', 'codex' or 'muse'"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::Implementer;

    const ALL: [Implementer; 3] = Implementer::ALL;

    #[test]
    fn every_variant_is_listed() {
        for implementer in ALL {
            // No `_` arm: a new variant stops this compiling, which is the
            // prompt to add it to `Implementer::ALL`.
            match implementer {
                Implementer::Claude | Implementer::Codex | Implementer::Muse => {}
            }
        }
    }

    #[test]
    fn every_variant_round_trips_through_its_cli_name() {
        for implementer in ALL {
            let name = implementer.cli_name();
            assert_eq!(Implementer::from_str(name).unwrap(), implementer, "{name}");
            assert_eq!(
                serde_json::to_string(&implementer).unwrap(),
                format!("\"{name}\""),
                "the stored spelling is the CLI spelling"
            );
            assert_eq!(name, implementer.label().to_ascii_lowercase());
        }
        let names: Vec<&str> = ALL.iter().map(|i| i.cli_name()).collect();
        let err = Implementer::from_str("gemini").unwrap_err().to_string();
        for name in names {
            assert!(err.contains(&format!("'{name}'")), "{err}");
        }
    }

    #[test]
    fn harness_capabilities_are_stated_for_every_variant() {
        let table: Vec<(Implementer, bool, bool, bool)> = ALL
            .iter()
            .map(|&i| {
                (
                    i,
                    i.recovers_invocations(),
                    i.assigns_own_session_id(),
                    i.rotates_session_after_verdict(),
                )
            })
            .collect();
        assert_eq!(
            table,
            vec![
                (Implementer::Claude, false, false, true),
                (Implementer::Codex, true, true, false),
                (Implementer::Muse, true, false, false),
            ]
        );
    }

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
