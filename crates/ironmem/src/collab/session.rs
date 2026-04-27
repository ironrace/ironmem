//! `CollabSession` — single source of truth for collab session state.

use super::agent::Agent;
use super::phase::Phase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabSession {
    pub id: String,
    pub phase: Phase,
    pub current_owner: Agent,
    pub claude_draft_hash: Option<String>,
    pub codex_draft_hash: Option<String>,
    pub canonical_plan_hash: Option<String>,
    pub final_plan_hash: Option<String>,
    pub codex_review_verdict: Option<String>,
    pub review_round: u8,
    // v3 coding fields. `tasks_count` is not stored — it is derived from
    // `task_list` via `tasks_count_from_list` so there is a single source of
    // truth for task cardinality. `task_review_round` and `global_review_round`
    // are vestigial (v2 held per-task verdict cycles; v3 batch mode runs all
    // tasks in a single Claude-driven phase) but remain as columns to avoid
    // disturbing the wire format.
    pub task_list: Option<String>,
    pub task_review_round: u8,
    pub global_review_round: u8,
    pub base_sha: Option<String>,
    pub last_head_sha: Option<String>,
    pub pr_url: Option<String>,
    pub coding_failure: Option<String>,
    /// Which agent runs the v3 batch implementation phase. `Agent::Claude`
    /// (the default) keeps the historical flow where Claude orchestrates
    /// per-task subagents inline. `Agent::Codex` routes
    /// `CodeImplementPending` to Codex instead — Claude still publishes
    /// `task_list`, but Codex drives its own `subagent-driven-development`
    /// end-to-end and emits `implementation_done`. Set at `collab_start`
    /// and immutable thereafter; the DB CHECK constraint enforces the
    /// allowed set as defense-in-depth.
    pub implementer: Agent,
}

impl CollabSession {
    pub fn new(id: impl Into<String>) -> Self {
        Self::new_with_implementer(id, Agent::Claude)
    }

    /// Construct a fresh planning-stage session with an explicit
    /// `implementer`. Used by tests and any caller that wants the
    /// non-default `Agent::Codex` batch ownership; production code should
    /// go through `collab_start` (which validates and persists the
    /// implementer at INSERT time).
    pub fn new_with_implementer(id: impl Into<String>, implementer: Agent) -> Self {
        Self {
            id: id.into(),
            phase: Phase::PlanParallelDrafts,
            current_owner: Agent::Claude,
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: None,
            last_head_sha: None,
            pr_url: None,
            coding_failure: None,
            implementer,
        }
    }

    /// Construct a session pre-positioned at the v3 global-review stage.
    /// Used by the coding-review shortcut (`collab_start_code_review`) for
    /// orchestrators that already completed per-task coding via
    /// `subagent-driven-development`. The no-op `CodeReviewLocalPending`
    /// handshake is collapsed — `head_sha` is supplied here instead.
    /// `implementer` is fixed at `Agent::Claude` because the shortcut
    /// never enters `CodeImplementPending`; the field is preserved only so
    /// the session record shape stays uniform with full-flow sessions.
    pub fn new_global_review(
        id: impl Into<String>,
        base_sha: impl Into<String>,
        head_sha: impl Into<String>,
    ) -> Self {
        let head = head_sha.into();
        Self {
            id: id.into(),
            phase: Phase::CodeReviewFixGlobalPending,
            current_owner: Agent::Codex,
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: Some(base_sha.into()),
            last_head_sha: Some(head),
            pr_url: None,
            coding_failure: None,
            implementer: Agent::Claude,
        }
    }

    /// Task cardinality derived from the stored `task_list` JSON. Canonical
    /// shape is `{"tasks":[…]}`; any other shape yields `None`. Returns `None`
    /// when `task_list` is unset (pre-`SubmitTaskList`). Used by the MCP
    /// `collab_status` response for audit visibility — the v3 batch flow does
    /// not iterate tasks server-side.
    pub fn tasks_count(&self) -> Option<u32> {
        tasks_count_from_list(self.task_list.as_deref())
    }
}

/// Count tasks in a stored `task_list` JSON payload. Canonical shape is
/// `{"tasks":[…]}`; anything else is rejected. Kept narrow on purpose so a
/// corrupt payload yields `None` instead of silently advancing the state
/// machine with a wrong count.
pub fn tasks_count_from_list(raw: Option<&str>) -> Option<u32> {
    let raw = raw?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let tasks = value.get("tasks")?.as_array()?;
    u32::try_from(tasks.len()).ok()
}
