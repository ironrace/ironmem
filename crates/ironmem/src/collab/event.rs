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
    // v2 coding
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
    CodeReview {
        head_sha: String,
    },
    CodeVerdict {
        verdict: String,
        head_sha: String,
    },
    CodeComment {
        head_sha: String,
    },
    CodeFinal {
        head_sha: String,
    },
    ReviewLocal {
        head_sha: String,
    },
    ReviewGlobal {
        verdict: String,
        head_sha: String,
    },
    VerdictGlobal {
        verdict: String,
        head_sha: String,
    },
    CommentGlobal {
        head_sha: String,
    },
    FinalReview {
        head_sha: String,
    },
    PrOpened {
        pr_url: String,
        head_sha: String,
    },
    /// Emitted by either agent when branch drift, gate exhaustion, `gh_auth`,
    /// or any other unrecoverable error occurs during coding. Transitions to
    /// `CodingFailed` from any coding-active phase. Stores `coding_failure`.
    FailureReport {
        coding_failure: String,
    },
}
