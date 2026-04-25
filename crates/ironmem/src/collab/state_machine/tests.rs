use super::super::session::tasks_count_from_list;
use super::*;

fn session() -> CollabSession {
    CollabSession::new("test-session")
}

fn draft(actor: &str, hash: &str, s: &CollabSession) -> CollabSession {
    apply_event(
        s,
        actor,
        &CollabEvent::SubmitDraft {
            content_hash: hash.to_string(),
        },
    )
    .unwrap()
}

fn canonical(hash: &str, s: &CollabSession) -> CollabSession {
    apply_event(
        s,
        "claude",
        &CollabEvent::PublishCanonical {
            content_hash: hash.to_string(),
        },
    )
    .unwrap()
}

fn review(verdict: &str, s: &CollabSession) -> CollabSession {
    apply_event(
        s,
        "codex",
        &CollabEvent::SubmitReview {
            verdict: verdict.to_string(),
        },
    )
    .unwrap()
}

/// Run the v1 flow to the point where `final_plan_hash` is set and the
/// session is `PlanLocked`, ready for `SubmitTaskList`.
fn locked_session(final_hash: &str) -> CollabSession {
    let s = session();
    let s = draft("claude", "c1", &s);
    let s = draft("codex", "c2", &s);
    let s = canonical("canonical", &s);
    let s = review("approve", &s);
    apply_event(
        &s,
        "claude",
        &CollabEvent::PublishFinal {
            content_hash: final_hash.to_string(),
        },
    )
    .unwrap()
}

/// Build a canonical `{"tasks":[…]}` JSON of `count` placeholder tasks so
/// the derived `tasks_count_from_list` matches what we pass in the event.
fn canonical_task_list(count: u32) -> String {
    let tasks: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            serde_json::json!({
                "id": i as i64 + 1,
                "title": format!("task-{}", i + 1),
                "acceptance": ["ok"],
            })
        })
        .collect();
    serde_json::json!({ "tasks": tasks }).to_string()
}

fn submit_task_list(s: &CollabSession, plan_hash: &str, tasks_count: u32) -> CollabSession {
    apply_event(
        s,
        "claude",
        &CollabEvent::SubmitTaskList {
            plan_hash: plan_hash.to_string(),
            base_sha: "base0".to_string(),
            task_list_json: canonical_task_list(tasks_count),
            tasks_count,
            head_sha: "head0".to_string(),
        },
    )
    .unwrap()
}

/// Build a `PlanLocked` session with `implementer == "codex"` so transitions
/// out of the batch phase route to Codex. Mirrors `locked_session` but
/// stamps the implementer field after planning completes (planning itself
/// does not depend on `implementer`).
fn locked_session_with_codex_implementer(final_hash: &str) -> CollabSession {
    let mut s = locked_session(final_hash);
    s.implementer = "codex".to_string();
    s
}

/// Drive a session from `CodeImplementPending` through the full global
/// review flow to `CodingComplete`. Used by tests that need a representative
/// happy path through the post-batch stage.
fn finish_through_global_review(s: &CollabSession) -> CollabSession {
    let s = apply_event(
        s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ReviewLocal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        "codex",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    apply_event(
        &s,
        "claude",
        &CollabEvent::FinalReview {
            head_sha: "g3".to_string(),
            pr_url: "https://example/pr/1".to_string(),
        },
    )
    .unwrap()
}

// ── v1 regression ────────────────────────────────────────────────────

#[test]
fn test_parallel_drafts_both_submit_advances_phase() {
    let s = session();
    let s = draft("claude", "c1", &s);
    assert_eq!(s.phase, Phase::PlanParallelDrafts);
    let s = draft("codex", "c2", &s);
    assert_eq!(s.phase, Phase::PlanSynthesisPending);
    assert_eq!(s.current_owner, "claude");
}

#[test]
fn test_duplicate_draft_rejected() {
    let s = session();
    let s = draft("claude", "c1", &s);
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::SubmitDraft {
            content_hash: "c2".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::AlreadySubmittedDraft {
            agent: "claude".to_string()
        }
    );
}

#[test]
fn test_codex_review_approve_advances_to_finalize() {
    for verdict in ["approve", "approve_with_minor_edits"] {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let s = review(verdict, &s);
        assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
        assert_eq!(s.codex_review_verdict.as_deref(), Some(verdict));
        assert_eq!(s.review_round, 1);
    }
}

#[test]
fn test_request_changes_at_cap_forces_finalize() {
    let s = session();
    let s = draft("claude", "c1", &s);
    let s = draft("codex", "c2", &s);
    let s = canonical("v1", &s);
    let s = review("request_changes", &s);
    let s = canonical("v2", &s);
    let s = review("request_changes", &s);

    assert_eq!(s.review_round, MAX_REVIEW_ROUNDS);
    assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
}

#[test]
fn test_invalid_verdict_rejected() {
    let s = session();
    let s = draft("claude", "c1", &s);
    let s = draft("codex", "c2", &s);
    let s = canonical("canonical", &s);
    let err = apply_event(
        &s,
        "codex",
        &CollabEvent::SubmitReview {
            verdict: "looks good to me".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::InvalidVerdictValue("looks good to me".to_string())
    );
}

// ── v3: PlanLocked → task_list transition ────────────────────────────

#[test]
fn test_task_list_transitions_to_code_implement() {
    let s = locked_session("hash-final");
    assert_eq!(s.phase, Phase::PlanLocked);
    let s = submit_task_list(&s, "hash-final", 2);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, "claude");
    assert_eq!(s.tasks_count(), Some(2));
    assert_eq!(s.task_review_round, 0);
    assert_eq!(s.global_review_round, 0);
    assert_eq!(s.base_sha.as_deref(), Some("base0"));
    assert_eq!(s.last_head_sha.as_deref(), Some("head0"));
}

#[test]
fn test_task_list_rejects_plan_hash_mismatch() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::SubmitTaskList {
            plan_hash: "wrong".to_string(),
            base_sha: "base".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::PlanHashMismatch { .. }));
}

#[test]
fn test_task_list_rejects_empty_tasks() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 0,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::EmptyTaskList);
}

#[test]
fn test_task_list_rejects_missing_base_sha() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::MissingBaseSha);
}

#[test]
fn test_task_list_rejected_from_non_claude() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        "codex",
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "b".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_task_list_rejected_before_plan_locked() {
    let s = session();
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::SubmitTaskList {
            plan_hash: "x".to_string(),
            base_sha: "b".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));
}

// ── v3: batch implementation → global review ─────────────────────────

#[test]
fn test_implementation_done_jumps_to_local_review() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 3);
    assert_eq!(s.phase, Phase::CodeImplementPending);

    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, "claude");
    assert_eq!(s.last_head_sha.as_deref(), Some("batch_head"));
}

#[test]
fn test_implementation_done_rejected_from_codex() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let err = apply_event(
        &s,
        "codex",
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_task_list_under_codex_implementer_makes_codex_owner() {
    // With `implementer == "codex"`, transitioning out of PlanLocked sets
    // `current_owner = "codex"` so Codex is the agent expected to drive
    // the batch phase.
    let s = locked_session_with_codex_implementer("hash-final");
    let s = submit_task_list(&s, "hash-final", 2);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, "codex");
    assert_eq!(s.implementer, "codex");
}

#[test]
fn test_implementation_done_under_codex_implementer_requires_codex_actor() {
    let s = locked_session_with_codex_implementer("hf");
    let s = submit_task_list(&s, "hf", 1);

    // Claude trying to fire `implementation_done` is rejected — the
    // owner check now reads from `session.implementer`, not a hardcoded
    // "claude".
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));

    // Codex fires it successfully and the phase advances to local review
    // (Claude-owned, since Claude always provides the second-opinion
    // local pass regardless of who implemented).
    let s = apply_event(
        &s,
        "codex",
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, "claude");
    assert_eq!(s.last_head_sha.as_deref(), Some("batch_head"));
}

#[test]
fn test_implementation_done_rejected_outside_code_implement_pending() {
    // From PlanLocked: WrongPhase.
    let s = locked_session("hf");
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));

    // From CodeReviewLocalPending: WrongPhase too.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "again".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));
}

// ── v3: global review, linear 3-phase flow ───────────────────────────

#[test]
fn test_global_review_linear_flow_ends_in_coding_complete() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);

    // Batch implementation → local review owner.
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);

    // Local: claude → codex
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ReviewLocal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, "codex");

    // Global review+fix: codex → claude
    let s = apply_event(
        &s,
        "codex",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, "claude");

    // Final review (includes PR URL): claude → terminal
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::FinalReview {
            head_sha: "g3".to_string(),
            pr_url: "https://example/pr/1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingComplete);
    assert_eq!(s.pr_url.as_deref(), Some("https://example/pr/1"));
    assert_eq!(s.last_head_sha.as_deref(), Some("g3"));

    // Terminal: further events rejected.
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "x".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::SessionLocked);
}

#[test]
fn test_review_local_wrong_sender_rejected() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        "codex",
        &CollabEvent::ReviewLocal {
            head_sha: "g".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_code_review_fix_global_wrong_sender_rejected() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ReviewLocal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn start_global_review_session_seeds_codex_owned_review_phase() {
    let session = start_global_review_session("s1", "basesha", "headsha").unwrap();
    assert_eq!(session.id, "s1");
    assert_eq!(session.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(session.current_owner, "codex");
    assert_eq!(session.base_sha.as_deref(), Some("basesha"));
    assert_eq!(session.last_head_sha.as_deref(), Some("headsha"));
    assert!(session.task_list.is_none());
    assert!(session.final_plan_hash.is_none());
    assert_eq!(session.review_round, 0);
}

#[test]
fn start_global_review_session_rejects_empty_base_sha() {
    let err = start_global_review_session("s1", "", "headsha").unwrap_err();
    assert!(matches!(err, CollabError::MissingBaseSha));
}

#[test]
fn start_global_review_session_rejects_empty_head_sha() {
    let err = start_global_review_session("s1", "basesha", "").unwrap_err();
    assert!(matches!(err, CollabError::MissingHeadSha));
}

#[test]
fn start_global_review_session_flows_into_final_review() {
    let session = start_global_review_session("s1", "basesha", "h0").unwrap();

    let after_codex = apply_event(
        &session,
        "codex",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "h1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_codex.phase, Phase::CodeReviewFinalPending);
    assert_eq!(after_codex.current_owner, "claude");

    let after_claude = apply_event(
        &after_codex,
        "claude",
        &CollabEvent::FinalReview {
            head_sha: "h1".to_string(),
            pr_url: "https://github.com/acme/repo/pull/1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_claude.phase, Phase::CodingComplete);
    assert_eq!(
        after_claude.pr_url.as_deref(),
        Some("https://github.com/acme/repo/pull/1")
    );
}

#[test]
fn start_global_review_session_accepts_branch_drift_failure_from_non_owner() {
    let session = start_global_review_session("s1", "basesha", "h0").unwrap();

    let failed = apply_event(
        &session,
        "claude",
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift: last_head_sha=h0 not found".to_string(),
        },
    )
    .unwrap();
    assert_eq!(failed.phase, Phase::CodingFailed);
    assert_eq!(failed.current_owner, "claude");
}

// ── v3: failure report ───────────────────────────────────────────────

#[test]
fn test_failure_report_from_code_implement_pending_transitions_to_failed() {
    // The new batch phase is coding-active, so a non-drift failure from the
    // current owner transitions to CodingFailed.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);

    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::FailureReport {
            coding_failure: "subagent_failure: task 2 timed out".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(
        s.coding_failure.as_deref(),
        Some("subagent_failure: task 2 timed out")
    );
}

#[test]
fn test_failure_report_branch_drift_from_codex_during_batch_phase() {
    // Branch drift is the carve-out: the non-owner may emit it.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.current_owner, "claude");

    let s = apply_event(
        &s,
        "codex",
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift: head_sha=abc not found".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(s.current_owner, "codex");
}

#[test]
fn test_failure_report_rejected_outside_coding_active_phase() {
    let s = locked_session("hf");
    // PlanLocked is not coding-active → FailureReport falls through to the
    // catch-all WrongPhase arm.
    let err = apply_event(
        &s,
        "claude",
        &CollabEvent::FailureReport {
            coding_failure: "nope".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));
}

#[test]
fn test_failure_report_from_code_review_final_pending_transitions_to_failed() {
    // FailureReport must be accepted in every coding-active phase,
    // including `CodeReviewFinalPending`.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::ReviewLocal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        "codex",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);

    let s = apply_event(
        &s,
        "claude",
        &CollabEvent::FailureReport {
            coding_failure: "local gate regressed".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
}

// ── helper: full batch happy path retains audit fields ───────────────

#[test]
fn test_full_batch_happy_path_retains_task_list_audit() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 4);
    let s = finish_through_global_review(&s);

    assert_eq!(s.phase, Phase::CodingComplete);
    assert_eq!(s.tasks_count(), Some(4));
    assert!(s.task_list.is_some());
    assert_eq!(s.pr_url.as_deref(), Some("https://example/pr/1"));
}

#[test]
fn test_tasks_count_from_list_only_accepts_canonical_shape() {
    // Derived tasks_count requires `{"tasks":[...]}`; bare arrays and
    // objects without `tasks` return None.
    let raw = canonical_task_list(3);
    assert_eq!(tasks_count_from_list(Some(&raw)), Some(3));
    assert_eq!(tasks_count_from_list(None), None);
    assert_eq!(tasks_count_from_list(Some("{}")), None);
    // Bare array — rejected by the single derivation path.
    assert_eq!(
        tasks_count_from_list(Some("[{\"id\":1,\"title\":\"t\"}]")),
        None
    );
    // Malformed JSON — swallowed by `ok()` and returns None.
    assert_eq!(tasks_count_from_list(Some("not json")), None);
}
