use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    // Planning (v1)
    PlanParallelDrafts,
    PlanSynthesisPending,
    PlanCodexReviewPending,
    PlanClaudeFinalizePending,
    PlanLocked,
    // Coding (v2) — per-task 5-phase debate
    CodeImplementPending,
    CodeReviewPending,
    CodeVerdictPending,
    CodeDebatePending,
    CodeFinalPending,
    // Coding (v2) — local + global review
    CodeReviewLocalPending,
    CodeReviewCodexPending,
    CodeReviewVerdictPending,
    CodeReviewDebatePending,
    CodeReviewFinalPending,
    // Coding (v2) — PR handoff + terminal
    PrReadyPending,
    CodingComplete,
    CodingFailed,
}

impl Phase {
    /// True for phases that permanently end the session. `wait_my_turn` uses
    /// a dynamic terminal set: `PlanLocked` is terminal pre-`task_list`, and
    /// `{CodingComplete, CodingFailed}` is the terminal set post-`task_list`.
    /// This helper returns only the permanently-terminal cases; callers
    /// responsible for the dynamic set check `task_list` on the session.
    pub fn is_terminal_v2(&self) -> bool {
        matches!(self, Self::CodingComplete | Self::CodingFailed)
    }

    /// True if the session is currently inside the v2 coding loop. Used by
    /// `collab_end` to reject early-end calls.
    pub fn is_coding_active(&self) -> bool {
        matches!(
            self,
            Self::CodeImplementPending
                | Self::CodeReviewPending
                | Self::CodeVerdictPending
                | Self::CodeDebatePending
                | Self::CodeFinalPending
                | Self::CodeReviewLocalPending
                | Self::CodeReviewCodexPending
                | Self::CodeReviewVerdictPending
                | Self::CodeReviewDebatePending
                | Self::CodeReviewFinalPending
                | Self::PrReadyPending
        )
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::PlanParallelDrafts => "PlanParallelDrafts",
            Self::PlanSynthesisPending => "PlanSynthesisPending",
            Self::PlanCodexReviewPending => "PlanCodexReviewPending",
            Self::PlanClaudeFinalizePending => "PlanClaudeFinalizePending",
            Self::PlanLocked => "PlanLocked",
            Self::CodeImplementPending => "CodeImplementPending",
            Self::CodeReviewPending => "CodeReviewPending",
            Self::CodeVerdictPending => "CodeVerdictPending",
            Self::CodeDebatePending => "CodeDebatePending",
            Self::CodeFinalPending => "CodeFinalPending",
            Self::CodeReviewLocalPending => "CodeReviewLocalPending",
            Self::CodeReviewCodexPending => "CodeReviewCodexPending",
            Self::CodeReviewVerdictPending => "CodeReviewVerdictPending",
            Self::CodeReviewDebatePending => "CodeReviewDebatePending",
            Self::CodeReviewFinalPending => "CodeReviewFinalPending",
            Self::PrReadyPending => "PrReadyPending",
            Self::CodingComplete => "CodingComplete",
            Self::CodingFailed => "CodingFailed",
        };
        f.write_str(value)
    }
}

impl TryFrom<&str> for Phase {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "PlanParallelDrafts" => Ok(Self::PlanParallelDrafts),
            "PlanSynthesisPending" => Ok(Self::PlanSynthesisPending),
            "PlanCodexReviewPending" => Ok(Self::PlanCodexReviewPending),
            "PlanClaudeFinalizePending" => Ok(Self::PlanClaudeFinalizePending),
            "PlanLocked" => Ok(Self::PlanLocked),
            "CodeImplementPending" => Ok(Self::CodeImplementPending),
            "CodeReviewPending" => Ok(Self::CodeReviewPending),
            "CodeVerdictPending" => Ok(Self::CodeVerdictPending),
            "CodeDebatePending" => Ok(Self::CodeDebatePending),
            "CodeFinalPending" => Ok(Self::CodeFinalPending),
            "CodeReviewLocalPending" => Ok(Self::CodeReviewLocalPending),
            "CodeReviewCodexPending" => Ok(Self::CodeReviewCodexPending),
            "CodeReviewVerdictPending" => Ok(Self::CodeReviewVerdictPending),
            "CodeReviewDebatePending" => Ok(Self::CodeReviewDebatePending),
            "CodeReviewFinalPending" => Ok(Self::CodeReviewFinalPending),
            "PrReadyPending" => Ok(Self::PrReadyPending),
            "CodingComplete" => Ok(Self::CodingComplete),
            "CodingFailed" => Ok(Self::CodingFailed),
            other => Err(format!("unknown collab phase: {other}")),
        }
    }
}
