use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    // Planning (v1)
    PlanParallelDrafts,
    PlanSynthesisPending,
    PlanCodexReviewPending,
    PlanClaudeFinalizePending,
    PlanLocked,
    // Coding (v3) — per-task 3-phase linear
    CodeImplementPending,
    CodeReviewFixPending,
    CodeFinalPending,
    // Coding (v3) — global review, 3-phase linear
    CodeReviewLocalPending,
    CodeReviewFixGlobalPending,
    CodeReviewFinalPending,
    // Coding (v3) — terminal
    CodingComplete,
    CodingFailed,
}

/// Authoritative mapping between `Phase` variants and the DB string forms.
/// String values are byte-identical to what the old match-based `Display`
/// and `TryFrom` produced — changing them would corrupt stored sessions.
const PHASE_NAMES: &[(Phase, &str)] = &[
    (Phase::PlanParallelDrafts, "PlanParallelDrafts"),
    (Phase::PlanSynthesisPending, "PlanSynthesisPending"),
    (Phase::PlanCodexReviewPending, "PlanCodexReviewPending"),
    (
        Phase::PlanClaudeFinalizePending,
        "PlanClaudeFinalizePending",
    ),
    (Phase::PlanLocked, "PlanLocked"),
    (Phase::CodeImplementPending, "CodeImplementPending"),
    (Phase::CodeReviewFixPending, "CodeReviewFixPending"),
    (Phase::CodeFinalPending, "CodeFinalPending"),
    (Phase::CodeReviewLocalPending, "CodeReviewLocalPending"),
    (
        Phase::CodeReviewFixGlobalPending,
        "CodeReviewFixGlobalPending",
    ),
    (Phase::CodeReviewFinalPending, "CodeReviewFinalPending"),
    (Phase::CodingComplete, "CodingComplete"),
    (Phase::CodingFailed, "CodingFailed"),
];

impl Phase {
    /// True for phases that permanently end the session. `wait_my_turn` uses
    /// a dynamic terminal set: `PlanLocked` is terminal pre-`task_list`, and
    /// `{CodingComplete, CodingFailed}` is the terminal set post-`task_list`.
    /// This helper returns only the permanently-terminal cases; callers
    /// responsible for the dynamic set check `task_list` on the session.
    pub fn is_terminal_v2(&self) -> bool {
        matches!(self, Self::CodingComplete | Self::CodingFailed)
    }

    /// True if the session is currently inside the v3 coding loop. Used by
    /// `collab_end` to reject early-end calls.
    pub fn is_coding_active(&self) -> bool {
        matches!(
            self,
            Self::CodeImplementPending
                | Self::CodeReviewFixPending
                | Self::CodeFinalPending
                | Self::CodeReviewLocalPending
                | Self::CodeReviewFixGlobalPending
                | Self::CodeReviewFinalPending
        )
    }

    /// The single `CollabEvent` variant each active phase expects. Used by the
    /// catch-all `WrongPhase` arm to build a uniform error message. Terminal
    /// phases return a placeholder that the catch-all never reaches because
    /// `CodingComplete`/`CodingFailed` short-circuit to `SessionLocked` first.
    pub(super) fn expected_event(&self) -> &'static str {
        match self {
            Self::PlanParallelDrafts => "SubmitDraft",
            Self::PlanSynthesisPending => "PublishCanonical",
            Self::PlanCodexReviewPending => "SubmitReview",
            Self::PlanClaudeFinalizePending => "PublishFinal",
            Self::PlanLocked => "SubmitTaskList",
            Self::CodeImplementPending => "CodeImplement",
            Self::CodeReviewFixPending => "CodeReviewFix",
            Self::CodeFinalPending => "CodeFinal",
            Self::CodeReviewLocalPending => "ReviewLocal",
            Self::CodeReviewFixGlobalPending => "CodeReviewFixGlobal",
            Self::CodeReviewFinalPending => "FinalReview",
            Self::CodingComplete | Self::CodingFailed => "SessionLocked",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = PHASE_NAMES
            .iter()
            .find(|(p, _)| p == self)
            .map(|(_, n)| *n)
            .unwrap_or("UNKNOWN");
        f.write_str(name)
    }
}

impl FromStr for Phase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PHASE_NAMES
            .iter()
            .find(|(_, n)| *n == s)
            .map(|(p, _)| *p)
            .ok_or_else(|| format!("unknown collab phase: {s}"))
    }
}

impl TryFrom<&str> for Phase {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}
