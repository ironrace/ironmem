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

/// Consecutive [unresumable](super::dispatch::SessionRefusal::Unresumable)
/// refusals of one session before a harness that
/// [rotates a refused session](Implementer::rotates_refused_session) opens a
/// fresh one. Two, not one: a retained session that could not be read once
/// may read on the next try, and a fresh session loses the conversation.
pub const RESUME_REFUSALS_BEFORE_ROTATION: u32 = 2;

/// What differs between harnesses, named once so `run_issue` asks *why* it
/// branches instead of *which* harness it has. A new harness answers these.
impl Implementer {
    /// Every harness, once. `tests::every_variant_is_listed` checks it
    /// against serde's own list of variants, so a new variant fails that test
    /// until it is added here.
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
    /// dispatch needs a fresh session — and so does a zero-turn dispatch,
    /// whose session never started a goal loop at all (#342). Claude's
    /// `/goal` arms no hook on `--resume`; a Codex thread and a Muse
    /// session continue.
    pub fn rotates_session_after_verdict(self) -> bool {
        match self {
            Self::Claude => true,
            Self::Codex | Self::Muse => false,
        }
    }

    /// The harness continues the controller's session (`--session-id`)
    /// across invocations and can refuse to resume it: Muse will not resume a
    /// session opened with another provider, or one whose retained state it
    /// cannot read. Only the controller can replace that session, so it opens
    /// a fresh one after [`RESUME_REFUSALS_BEFORE_ROTATION`] consecutive
    /// [unresumable](super::dispatch::SessionRefusal::Unresumable) refusals,
    /// or when the run's model differs from the one the session was used with.
    ///
    /// Never on a session still [in use](super::dispatch::SessionRefusal::InUse):
    /// Muse's lock is the one guard against a second implementer that does not
    /// rest on Autopilot's own process evidence, and something still holds
    /// that session. Claude rotates on its own rule (a verdict), and a Codex
    /// thread ID is the harness's own.
    pub fn rotates_refused_session(self) -> bool {
        match self {
            Self::Muse => true,
            Self::Claude | Self::Codex => false,
        }
    }

    /// The harness inherits the controller's environment, `GIT_*` included,
    /// so it may commit under the controller's `GIT_COMMITTER_EMAIL`. Claude
    /// does; a supervised CLI harness (Codex, Muse) is spawned with every
    /// `GIT_*` variable scrubbed, and commits under the worktree's identity
    /// only.
    pub fn inherits_controller_git_env(self) -> bool {
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

    /// `ALL` against serde's own variant list, which an unknown variant's
    /// error names: a variant missing from `ALL` fails here.
    #[test]
    fn every_variant_is_listed() {
        let err = serde_json::from_str::<Implementer>("\"__none__\"")
            .unwrap_err()
            .to_string();
        let (_, listed) = err
            .split_once("expected one of ")
            .unwrap_or_else(|| panic!("serde names no variants: {err}"));
        let listed = listed.split(" at line").next().unwrap_or(listed);
        let variants: Vec<String> = listed
            .split(", ")
            .map(|v| v.trim().trim_matches('`').to_string())
            .collect();
        let all: Vec<String> = ALL.iter().map(|i| i.cli_name().to_string()).collect();
        assert_eq!(variants, all, "serde said: {err}");
    }

    #[test]
    fn only_claude_inherits_the_controllers_git_environment() {
        for implementer in ALL {
            assert_eq!(
                implementer.inherits_controller_git_env(),
                !implementer.recovers_invocations(),
                "{implementer:?}"
            );
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
        let table: Vec<(Implementer, bool, bool, bool, bool)> = ALL
            .iter()
            .map(|&i| {
                (
                    i,
                    i.recovers_invocations(),
                    i.assigns_own_session_id(),
                    i.rotates_session_after_verdict(),
                    i.rotates_refused_session(),
                )
            })
            .collect();
        assert_eq!(
            table,
            vec![
                (Implementer::Claude, false, false, true, false),
                (Implementer::Codex, true, true, false, false),
                (Implementer::Muse, true, false, false, true),
            ]
        );
        // Only a harness that keeps the controller's session, and recovers
        // its invocations, may replace it.
        for i in ALL {
            if i.rotates_refused_session() {
                assert!(i.recovers_invocations() && !i.assigns_own_session_id());
            }
        }
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
