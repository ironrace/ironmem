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
    /// Deterministic 32-char drawer id of the canonical plan body once
    /// accepted (room `collab-plans`). NULL on pre-009 sessions → legacy
    /// inline path in `collab_status`. See issue #90.
    pub canonical_plan_drawer_id: Option<String>,
    /// Deterministic 32-char drawer id of the final (parsed) plan body.
    pub final_plan_drawer_id: Option<String>,
    pub codex_review_verdict: Option<String>,
    pub review_round: u8,
    // v3 coding fields. `tasks_count` is not stored — it is derived from
    // `task_list` via `tasks_count_from_list` so there is a single source of
    // truth for task cardinality. `task_review_round` and `global_review_round`
    // are vestigial (v2 held per-task verdict cycles; v3 batch mode runs all
    // tasks in a single Claude-driven phase) but remain as columns to avoid
    // disturbing the wire format.
    pub task_list: Option<String>,
    /// Deterministic 32-char drawer id for the accepted task-list JSON. NULL on
    /// pre-014 sessions, which still have `task_list` and can be rendered as
    /// legacy compact refs without a dereferenceable drawer id.
    pub task_list_drawer_id: Option<String>,
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
    /// and rebindable via `collab_set_implementer` until implementation
    /// completes. The DB CHECK constraint enforces the allowed set as
    /// defense-in-depth.
    pub implementer: Agent,
    // Recovery-state fields (issue #197). All six persist as nullable
    // columns added in migration 015 and stay NULL/0 for the common case
    // where no tooling failure is in flight.
    /// Classified failure kind pending recovery (see `failure_class::classify`),
    /// `None` when no failure is in flight. Same storage shape as
    /// `coding_failure` — a plain string, not a typed enum, since the
    /// classification vocabulary is still open-ended.
    pub pending_failure: Option<String>,
    /// The `Phase` the session was in when the failure was recorded, so
    /// recovery can resume in place. Wire-encoded exactly like the
    /// non-nullable `phase` column (`Phase::to_string()` / `FromStr`).
    ///
    /// **Not a "session is currently failed" indicator.** `ResumeCoding`
    /// (`state_machine::apply_event`) deliberately leaves this field set
    /// after a successful resume, as a historical record of what phase the
    /// session originally failed from — it does NOT clear it. A non-null
    /// `failed_from_phase` on an active (non-`CodingFailed`) session is
    /// normal and expected once that session has ever been resumed; only
    /// `session.phase == Phase::CodingFailed` means the session is actually
    /// down. Any caller exposing this field (e.g. `collab_status`) should
    /// present it as audit history, not as a live-status flag.
    pub failed_from_phase: Option<Phase>,
    /// Sub-phase of the recovery flow itself, distinct from the session's
    /// normal `phase` column. Same encoding as `failed_from_phase`.
    pub recovery_phase: Option<Phase>,
    /// Which `Agent` currently drives recovery. Same encoding as
    /// `current_owner`/`implementer` (`Agent::as_str()` / `FromStr`).
    pub recovery_owner: Option<Agent>,
    /// Which `Agent` owned the session when the failure occurred, so
    /// recovery can hand control back. Same encoding as `recovery_owner`.
    ///
    /// **Audit-only — deliberately not exposed in `collab_status` or the
    /// session-handoff block.** `mcp/tools/collab_session.rs`'s
    /// `session_record_json` and `mcp/tools/handoff.rs`'s
    /// `compose_handoff_block` both surface `recovery_owner` (who to hand
    /// the turn to) but intentionally omit this field, since a dispatcher
    /// routing the recovery turn only needs the destination, not the
    /// origin. Before adding it to either surface, re-check whether a real
    /// caller need has emerged — this omission was a deliberate scope
    /// decision (issue #197 task 9), not an oversight.
    pub recovery_origin_owner: Option<Agent>,
    /// How many recovery attempts have been made so far.
    ///
    /// The DB column (`recovery_attempts INTEGER`) is nullable — legacy
    /// pre-015 rows have no value — but this Rust field is a plain `u8`,
    /// not `Option<u8>`: a NULL in the DB is read back as `0`, never as an
    /// error. `load_session_record` must read the column as `Option<i64>`
    /// first and map `None -> 0` (clamping only applies to the `Some` arm);
    /// `save_session` always writes a concrete `i64`, so a NULL can only
    /// occur on a row that has never been through `save_session` — i.e. a
    /// genuinely legacy row.
    pub recovery_attempts: u8,
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
            canonical_plan_drawer_id: None,
            final_plan_drawer_id: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            task_list_drawer_id: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: None,
            last_head_sha: None,
            pr_url: None,
            coding_failure: None,
            implementer,
            pending_failure: None,
            failed_from_phase: None,
            recovery_phase: None,
            recovery_owner: None,
            recovery_origin_owner: None,
            recovery_attempts: 0,
        }
    }

    /// Construct a session pre-positioned at the v3 global-review stage.
    /// Used by the coding-review shortcut (`collab_start_code_review`) for
    /// orchestrators that already completed per-task coding via
    /// `subagent-driven-development`. The shortcut seeds Codex's
    /// `CodeReviewFixGlobalPending` turn directly — `head_sha` is supplied
    /// here instead of via an `implementation_done` send. From there the
    /// flow follows the canonical v3 order: Codex `review_fix_global` →
    /// Claude `review_local` (audit of Codex's commits via
    /// `/ultrareview-local`) → Claude `final_review` (PR creation).
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
            canonical_plan_drawer_id: None,
            final_plan_drawer_id: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            task_list_drawer_id: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: Some(base_sha.into()),
            last_head_sha: Some(head),
            pr_url: None,
            coding_failure: None,
            implementer: Agent::Claude,
            pending_failure: None,
            failed_from_phase: None,
            recovery_phase: None,
            recovery_owner: None,
            recovery_origin_owner: None,
            recovery_attempts: 0,
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
