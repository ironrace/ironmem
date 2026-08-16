/// Maximum characters of a rejected `head_sha` echoed back in a refusal.
///
/// Every refusal that formats a `head_sha` does so on a path where that value
/// has just *failed* `is_hex_sha`, so the 64-character ceiling a passing shape
/// check would have guaranteed is not available to it. Wide enough to show a
/// full 64-char object name and still signal that something followed it.
///
/// Both seed-site refusals share this bound: [`CollabError::MalformedHeadSha`]
/// (the `collab_start_code_review` shortcut, whose `head_sha` arrives through
/// `require_str` and so carries no upstream cap at all) and the `task_list`
/// refusal in `parse_task_list_event`. The reported-head refusal in
/// `validate_global_review_head_advance` is bounded by the outer `content` cap
/// (`MAX_COLLAB_CONTENT_CHARS`) rather than this one.
pub const MAX_ECHOED_HEAD_SHA_CHARS: usize = 80;

/// Render `head_sha` for inclusion in a refusal, capped at
/// [`MAX_ECHOED_HEAD_SHA_CHARS`] characters with a trailing `…` when cut.
///
/// The cut is `chars().take()` rather than a byte slice on purpose: the bound
/// lands in the middle of bytes the caller chose, and slicing a multibyte
/// value there panics inside the error path — the one place a panic is least
/// affordable. Comparing byte lengths detects the cut because `echoed` is
/// always a prefix of `head_sha`.
pub fn echo_head_sha(head_sha: &str) -> String {
    let echoed: String = head_sha.chars().take(MAX_ECHOED_HEAD_SHA_CHARS).collect();
    if echoed.len() < head_sha.len() {
        format!("{echoed}…")
    } else {
        echoed
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CollabError {
    #[error("not your turn: expected {expected}, got {got}")]
    NotYourTurn { expected: String, got: String },

    #[error("draft already submitted by {agent}")]
    AlreadySubmittedDraft { agent: String },

    #[error("invalid verdict value: {0}")]
    InvalidVerdictValue(String),

    #[error("wrong phase: expected {expected}, got {got}")]
    WrongPhase { expected: String, got: String },

    #[error("session is locked")]
    SessionLocked,

    /// `expected` is intentionally elided from the Display string: the
    /// stored `final_plan_hash` must not leak to callers that probe with
    /// arbitrary hashes. The field is retained for structured logging on
    /// the server side.
    #[error("plan_hash mismatch: got {got}")]
    PlanHashMismatch { expected: String, got: String },

    #[error("task_list must contain at least one task")]
    EmptyTaskList,

    #[error(
        "task_list contains {actual} tasks; a collab issue may contain at most {max} tasks; split it into smaller issues"
    )]
    TooManyTasks { actual: u32, max: u32 },

    #[error("task_list_json must contain a canonical tasks array")]
    InvalidTaskList,

    #[error("task_list task count mismatch: declared {declared}, parsed {actual}")]
    TaskListCountMismatch { declared: u32, actual: u32 },

    #[error("final_plan_hash not set — session has not reached PlanLocked")]
    PlanNotFinalized,

    #[error("base_sha is required")]
    MissingBaseSha,

    #[error("head_sha is required but missing or empty")]
    MissingHeadSha,

    /// A `head_sha` that is present but not a git object name. Distinct from
    /// [`CollabError::MissingHeadSha`] because the remedies differ: that one
    /// says "you omitted it", this one says "what you sent will not identify
    /// a commit". Raised only at the shortcut's seed site, where the value is
    /// still the caller's to correct — see the skip arm in
    /// `validate_global_review_head_advance` for why the same condition on a
    /// *stored* `last_head_sha` is not an error there.
    ///
    /// `head_sha` carries the *echo* of the offending value, not the value
    /// itself: construct it through [`echo_head_sha`] so a caller cannot use
    /// this refusal to reflect an arbitrarily long string back through the
    /// JSON-RPC error body and the server's `tracing::error!` line. The field
    /// exists only to be shown, so bounding it at construction loses nothing.
    ///
    /// The remedy is spelled out rather than implied, and matches the sibling
    /// `task_list` refusal in `parse_task_list_event` word for word: the
    /// reader on both paths is an agent with a shell, and `git rev-parse HEAD`
    /// is the whole fix.
    #[error(
        "head_sha {head_sha} is not a git object name (7-64 hex characters). Run \
         `git rev-parse HEAD` in the session's repo and send the full sha it prints \
         — a revision expression such as HEAD, most branch names, or an abbreviation \
         shorter than 7 characters does not pin one commit, and this value becomes \
         the fixed point every later drift check in the session measures against."
    )]
    MalformedHeadSha { head_sha: String },

    /// `ResumeCoding` was rejected: either the `CodingFailed` session's
    /// stored `coding_failure` does not classify as a recoverable tooling
    /// failure, or `failed_from_phase` was never recorded (a pre-migration
    /// row). `reason` states which, as a fact about the session's stored
    /// state — never a guess about what actually happened during coding.
    #[error("session cannot be resumed: {reason}")]
    NotResumable { reason: String },
}
