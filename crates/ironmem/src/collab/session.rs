//! `CollabSession` — single source of truth for collab session state.

use super::phase::Phase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabSession {
    pub id: String,
    pub phase: Phase,
    pub current_owner: String,
    pub claude_draft_hash: Option<String>,
    pub codex_draft_hash: Option<String>,
    pub canonical_plan_hash: Option<String>,
    pub final_plan_hash: Option<String>,
    pub codex_review_verdict: Option<String>,
    pub review_round: u8,
    // v3 coding fields. `tasks_count` is not stored — it is derived from
    // `task_list` via `tasks_count_from_list` so there is a single source of
    // truth for task cardinality. `task_review_round` and `global_review_round`
    // are vestigial (v2 held per-task and global verdict cycles; v3 is linear
    // and never increments them) but remain as columns to avoid a migration.
    pub task_list: Option<String>,
    pub current_task_index: Option<u32>,
    pub task_review_round: u8,
    pub global_review_round: u8,
    pub base_sha: Option<String>,
    pub last_head_sha: Option<String>,
    pub pr_url: Option<String>,
    pub coding_failure: Option<String>,
}

impl CollabSession {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: Phase::PlanParallelDrafts,
            current_owner: "claude".to_string(),
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            current_task_index: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: None,
            last_head_sha: None,
            pr_url: None,
            coding_failure: None,
        }
    }

    /// Construct a session pre-positioned at the v3 global-review stage.
    /// Used by the coding-review shortcut (`collab_start_code_review`) for
    /// orchestrators that already completed per-task coding via
    /// `subagent-driven-development`. The no-op `CodeReviewLocalPending`
    /// handshake is collapsed — `head_sha` is supplied here instead.
    pub fn new_global_review(
        id: impl Into<String>,
        base_sha: impl Into<String>,
        head_sha: impl Into<String>,
    ) -> Self {
        let head = head_sha.into();
        Self {
            id: id.into(),
            phase: Phase::CodeReviewFixGlobalPending,
            current_owner: "codex".to_string(),
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            current_task_index: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: Some(base_sha.into()),
            last_head_sha: Some(head),
            pr_url: None,
            coding_failure: None,
        }
    }

    /// Task cardinality derived from the stored `task_list` JSON. Canonical
    /// shape is `{"tasks":[…]}`; any other shape yields `None`. Returns `None`
    /// when `task_list` is unset (pre-`SubmitTaskList`).
    pub fn tasks_count(&self) -> Option<u32> {
        tasks_count_from_list(self.task_list.as_deref())
    }

    /// Apply the per-task advance rule. Resets `task_review_round` and either
    /// increments `current_task_index` or transitions into local review.
    ///
    /// `task_list` and `current_task_index` are invariants of every
    /// coding-active phase; if either is missing the state machine has already
    /// drifted and we panic rather than silently treat it as zero tasks.
    pub(super) fn advance_task(&mut self) {
        self.task_review_round = 0;
        let total = self
            .tasks_count()
            .expect("task_list must be set and well-formed in coding-active phase");
        let current = self
            .current_task_index
            .expect("current_task_index must be set in coding-active phase");
        let next = current.saturating_add(1);
        if next >= total {
            self.phase = Phase::CodeReviewLocalPending;
            self.current_owner = "claude".to_string();
        } else {
            self.current_task_index = Some(next);
            self.phase = Phase::CodeImplementPending;
            self.current_owner = "claude".to_string();
        }
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
