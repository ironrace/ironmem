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

    #[error("final_plan_hash not set — session has not reached PlanLocked")]
    PlanNotFinalized,

    #[error("base_sha is required")]
    MissingBaseSha,
}
