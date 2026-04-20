#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollabEvent {
    // v1 planning
    SubmitDraft {
        content_hash: String,
    },
    PublishCanonical {
        content_hash: String,
    },
    SubmitReview {
        verdict: String,
    },
    PublishFinal {
        content_hash: String,
    },
    // v3 coding
    SubmitTaskList {
        plan_hash: String,
        base_sha: String,
        task_list_json: String,
        tasks_count: u32,
        head_sha: String,
    },
    CodeImplement {
        head_sha: String,
    },
    /// Codex per-task: reviewed Claude's implementation and applied fixes
    /// directly (may be a no-op commit if clean). `head_sha` is the post-fix
    /// HEAD that Claude pulls for the final phase.
    CodeReviewFix {
        head_sha: String,
    },
    CodeFinal {
        head_sha: String,
    },
    ReviewLocal {
        head_sha: String,
    },
    /// Codex global: reviewed the full task stack and applied fixes directly.
    /// Mirrors `CodeReviewFix` at the global scope.
    CodeReviewFixGlobal {
        head_sha: String,
    },
    /// Claude's final global turn — includes the opened PR URL so the session
    /// advances straight to `CodingComplete` in one send (no separate
    /// `pr_opened`).
    FinalReview {
        head_sha: String,
        pr_url: String,
    },
    /// Emitted by either agent when branch drift, gate exhaustion, `gh_auth`,
    /// or any other unrecoverable error occurs during coding. Transitions to
    /// `CodingFailed` from any coding-active phase. Stores `coding_failure`.
    FailureReport {
        coding_failure: String,
    },
}

impl CollabEvent {
    /// Short name for the variant, used in error messages.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::SubmitDraft { .. } => "SubmitDraft",
            Self::PublishCanonical { .. } => "PublishCanonical",
            Self::SubmitReview { .. } => "SubmitReview",
            Self::PublishFinal { .. } => "PublishFinal",
            Self::SubmitTaskList { .. } => "SubmitTaskList",
            Self::CodeImplement { .. } => "CodeImplement",
            Self::CodeReviewFix { .. } => "CodeReviewFix",
            Self::CodeFinal { .. } => "CodeFinal",
            Self::ReviewLocal { .. } => "ReviewLocal",
            Self::CodeReviewFixGlobal { .. } => "CodeReviewFixGlobal",
            Self::FinalReview { .. } => "FinalReview",
            Self::FailureReport { .. } => "FailureReport",
        }
    }
}
