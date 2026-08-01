use super::super::agent::Agent;
use super::super::session::tasks_count_from_list;
use super::*;

fn session() -> CollabSession {
    CollabSession::new("test-session")
}

fn session_with_implementer(implementer: Agent) -> CollabSession {
    CollabSession::new_with_implementer("test-session", implementer)
}

fn draft(actor: Agent, hash: &str, s: &CollabSession) -> CollabSession {
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
        Agent::Claude,
        &CollabEvent::PublishCanonical {
            content_hash: hash.to_string(),
        },
    )
    .unwrap()
}

fn review(verdict: &str, s: &CollabSession) -> CollabSession {
    apply_event(
        s,
        Agent::Codex,
        &CollabEvent::SubmitReview {
            verdict: verdict.to_string(),
        },
    )
    .unwrap()
}

/// Run the v1 flow on the supplied starting session through to the point
/// where `final_plan_hash` is set and the session is `PlanLocked`, ready
/// for `SubmitTaskList`. Used by both the default-implementer helper and
/// the codex-implementer helper.
fn drive_to_plan_locked(start: CollabSession, final_hash: &str) -> CollabSession {
    let s = draft(Agent::Claude, "c1", &start);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical", &s);
    let s = review("approve", &s);
    apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::PublishFinal {
            content_hash: final_hash.to_string(),
        },
    )
    .unwrap()
}

fn locked_session(final_hash: &str) -> CollabSession {
    drive_to_plan_locked(session(), final_hash)
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
        Agent::Claude,
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

/// Build a `PlanLocked` session whose `implementer` is `Agent::Codex`, so
/// the v3 batch phase routes ownership to Codex. Constructs the session
/// with the implementer set up front rather than mutating after the fact —
/// the project's immutability rule forbids field-level mutation, and the
/// planning flow doesn't depend on the implementer field anyway.
fn locked_session_with_codex_implementer(final_hash: &str) -> CollabSession {
    drive_to_plan_locked(session_with_implementer(Agent::Codex), final_hash)
}

/// Drive a session from `CodeImplementPending` through the full global
/// review flow to `CodingComplete`. Used by tests that need a representative
/// happy path through the post-batch stage. Under the v3 reorder the
/// sequence is: ImplementationDone → CodeReviewFixGlobal (Codex) →
/// ReviewLocal (Claude audit) → FinalReview (Claude PR).
fn finish_through_global_review(s: &CollabSession) -> CollabSession {
    let s = apply_event(
        s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    apply_event(
        &s,
        Agent::Claude,
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
    let s = draft(Agent::Claude, "c1", &s);
    assert_eq!(s.phase, Phase::PlanParallelDrafts);
    let s = draft(Agent::Codex, "c2", &s);
    assert_eq!(s.phase, Phase::PlanSynthesisPending);
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn test_duplicate_draft_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let err = apply_event(
        &s,
        Agent::Claude,
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
        let s = draft(Agent::Claude, "c1", &s);
        let s = draft(Agent::Codex, "c2", &s);
        let s = canonical("canonical", &s);
        let s = review(verdict, &s);
        assert_eq!(s.phase, Phase::PlanFinalizePending);
        assert_eq!(s.codex_review_verdict.as_deref(), Some(verdict));
        assert_eq!(s.review_round, 1);
    }
}

#[test]
fn test_request_changes_after_one_review_advances_to_finalize() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("v1", &s);
    let s = review("request_changes", &s);

    assert_eq!(s.review_round, MAX_REVIEW_ROUNDS);
    assert_eq!(s.phase, Phase::PlanFinalizePending);
}

#[test]
fn test_invalid_verdict_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical", &s);
    let err = apply_event(
        &s,
        Agent::Codex,
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
    assert_eq!(s.current_owner, Agent::Claude);
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
        Agent::Claude,
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
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: canonical_task_list(0),
            tasks_count: 0,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::EmptyTaskList);
}

#[test]
fn test_task_list_rejects_noncanonical_json() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: "[]".to_string(),
            tasks_count: 0,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::InvalidTaskList);
}

#[test]
fn test_task_list_rejects_invalid_task_body_from_direct_caller() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: serde_json::json!({
                "tasks": [{"id": 1, "acceptance": []}],
            })
            .to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::InvalidTaskList);
}

#[test]
fn test_task_list_rejects_unsafe_plan_file_path_from_direct_caller() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: serde_json::json!({
                "plan_file_path": "../outside-plan.md",
                "tasks": [{"id": 1, "acceptance": ["ok"]}],
            })
            .to_string(),
            tasks_count: 1,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::InvalidTaskList);
}

#[test]
fn test_task_list_rejects_more_than_ten_tasks() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: canonical_task_list(11),
            tasks_count: 11,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::TooManyTasks {
            actual: 11,
            max: 10,
        }
    );
}

#[test]
fn test_task_list_rejects_declared_count_that_hides_oversized_json() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base".to_string(),
            task_list_json: canonical_task_list(11),
            tasks_count: 10,
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::TaskListCountMismatch {
            declared: 10,
            actual: 11,
        }
    );
}

#[test]
fn test_task_list_accepts_exactly_ten_tasks() {
    let s = locked_session("hash-final");
    let s = submit_task_list(&s, "hash-final", 10);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.tasks_count(), Some(10));
}

#[test]
fn test_task_list_rejects_missing_base_sha() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "".to_string(),
            task_list_json: canonical_task_list(1),
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
        Agent::Codex,
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
        Agent::Claude,
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
fn test_implementation_done_jumps_to_global_review() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 3);
    assert_eq!(s.phase, Phase::CodeImplementPending);

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.last_head_sha.as_deref(), Some("batch_head"));
}

#[test]
fn test_implementation_done_rejected_from_codex() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let err = apply_event(
        &s,
        Agent::Codex,
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
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.implementer, Agent::Codex);
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
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));

    // Codex fires it successfully and the phase advances to global review
    // (Codex-owned under v3 reorder: Codex reads the raw post-implementation
    // diff first, before Claude's `/ultrareview-local` audit).
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.last_head_sha.as_deref(), Some("batch_head"));
}

#[test]
fn test_implementation_done_rejected_outside_code_implement_pending() {
    // From PlanLocked: WrongPhase.
    let s = locked_session("hf");
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));

    // From CodeReviewFixGlobalPending: WrongPhase too.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Claude,
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

    // Batch implementation → global review owner (Codex reads raw diff first).
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);

    // Global review+fix: codex → claude (audit turn).
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // Local audit (/ultrareview-local): claude → claude (final review).
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // Final review (includes PR URL): claude → terminal
    let s = apply_event(
        &s,
        Agent::Claude,
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
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "x".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(err, CollabError::SessionLocked);
}

// ── v3 reorder (Codex first): canonical phase sequence ───────────────

#[test]
fn test_v3_phase_sequence_is_global_then_local() {
    // Under the v3 reorder, ImplementationDone routes to
    // CodeReviewFixGlobalPending (Codex's turn) BEFORE
    // CodeReviewLocalPending (Claude's audit).
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);

    // CodeImplementPending -> CodeReviewFixGlobalPending (Codex)
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "h1".to_string(),
        },
    )
    .expect("implementation_done");
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);

    // CodeReviewFixGlobalPending -> CodeReviewLocalPending (Claude)
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "h2".to_string(),
        },
    )
    .expect("review_fix_global");
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // CodeReviewLocalPending -> CodeReviewFinalPending (Claude)
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "h3".to_string(),
        },
    )
    .expect("review_local");
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // CodeReviewFinalPending -> CodingComplete
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FinalReview {
            head_sha: "h4".to_string(),
            pr_url: "https://example/pr".to_string(),
        },
    )
    .expect("final_review");
    assert_eq!(s.phase, Phase::CodingComplete);
}

#[test]
fn test_v1_one_pass_review_cap_survives_v3_reorder() {
    // Regression: v1 planning must stay one-pass even after v3 rewires.
    // Codex can request changes, but that request moves directly to
    // Claude's final execution-plan step rather than back to synthesis.
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("v1", &s);
    let s = review("request_changes", &s);

    assert_eq!(s.phase, Phase::PlanFinalizePending);
    assert_eq!(s.review_round, MAX_REVIEW_ROUNDS);
}

#[test]
fn test_review_local_wrong_sender_rejected() {
    // Under v3 reorder: drive through ImplementationDone (→ CodeReviewFixGlobalPending,
    // Codex) and CodeReviewFixGlobal (→ CodeReviewLocalPending, Claude) so we
    // land at the gate where ReviewLocal is the expected next event, then
    // assert a Codex-sent ReviewLocal is rejected as NotYourTurn.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_code_review_fix_global_wrong_sender_rejected() {
    // Under v3 reorder: after ImplementationDone the phase is
    // CodeReviewFixGlobalPending (Codex owner). A Claude-sent
    // CodeReviewFixGlobal is rejected as NotYourTurn.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
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
    assert_eq!(session.current_owner, Agent::Codex);
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

    // Under v3 reorder: Codex review_fix_global advances to CodeReviewLocalPending
    // (Claude's audit turn) before reaching CodeReviewFinalPending.
    let after_codex = apply_event(
        &session,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "h1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_codex.phase, Phase::CodeReviewLocalPending);
    assert_eq!(after_codex.current_owner, Agent::Claude);

    let after_audit = apply_event(
        &after_codex,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "h1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_audit.phase, Phase::CodeReviewFinalPending);
    assert_eq!(after_audit.current_owner, Agent::Claude);

    let after_claude = apply_event(
        &after_audit,
        Agent::Claude,
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
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift: last_head_sha=h0 not found".to_string(),
        },
    )
    .unwrap();
    assert_eq!(failed.phase, Phase::CodingFailed);
    assert_eq!(failed.current_owner, Agent::Claude);
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
        Agent::Claude,
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
fn test_failure_report_from_codex_implementer_during_batch_phase() {
    // With `implementer == "codex"`, Codex owns CodeImplementPending and
    // can fire a non-drift failure report to abort the batch. Mirror of
    // the Claude-implementer case but exercising the codex-owned path.
    let s = locked_session_with_codex_implementer("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, Agent::Codex);

    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "subagent_failure: task 2 timed out".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(
        s.coding_failure.as_deref(),
        Some("subagent_failure: task 2 timed out")
    );
}

#[test]
fn test_failure_report_rejects_bare_branch_drift_prefix() {
    // The drift carve-out lets the non-owner abort a session, but only
    // with a payload that includes diagnostic content after the prefix.
    // A bare `"branch_drift:"` from an off-turn agent must be rejected
    // so the carve-out can't be abused to abort with no cause.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.current_owner, Agent::Claude);

    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift:".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_failure_report_branch_drift_from_codex_during_batch_phase() {
    // Branch drift is the carve-out: the non-owner may emit it.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.current_owner, Agent::Claude);

    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift: head_sha=abc not found".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(s.current_owner, Agent::Codex);
}

#[test]
fn test_failure_report_rejected_outside_coding_active_phase() {
    let s = locked_session("hf");
    // PlanLocked is not coding-active → FailureReport falls through to the
    // catch-all WrongPhase arm.
    let err = apply_event(
        &s,
        Agent::Claude,
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
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "local gate regressed".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
}

// ── v3: recoverable ("tooling") failure report ─────────────────────────

/// The `counterpart` swap this module expects from the state machine —
/// duplicated here (rather than reaching into `state_machine::mod`'s
/// private helper) so the test's expectation is expressed independently of
/// the implementation.
fn expected_counterpart(agent: Agent) -> Agent {
    match agent {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
    }
}

#[test]
fn test_failure_report_recoverable_from_code_implement_pending_holds_phase() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    let reporter = s.current_owner;
    assert_eq!(reporter, Agent::Claude);

    let s = apply_event(
        &s,
        reporter,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: index.lock EPERM".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, expected_counterpart(reporter));
    assert_eq!(s.recovery_owner, Some(expected_counterpart(reporter)));
    assert_eq!(s.recovery_phase, Some(Phase::CodeImplementPending));
    assert_eq!(s.recovery_origin_owner, Some(reporter));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(
        s.pending_failure.as_deref(),
        Some("git_commit_failed: index.lock EPERM")
    );
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);
}

#[test]
fn test_failure_report_recoverable_from_code_review_fix_global_pending_holds_phase() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    let reporter = s.current_owner;
    assert_eq!(reporter, Agent::Codex);

    let s = apply_event(
        &s,
        reporter,
        &CollabEvent::FailureReport {
            coding_failure: "sandbox_denied: write to /etc blocked".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, expected_counterpart(reporter));
    assert_eq!(s.recovery_owner, Some(expected_counterpart(reporter)));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewFixGlobalPending));
    assert_eq!(s.recovery_origin_owner, Some(reporter));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);
}

#[test]
fn test_failure_report_recoverable_from_code_review_local_pending_holds_phase() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    let reporter = s.current_owner;
    assert_eq!(reporter, Agent::Claude);

    let s = apply_event(
        &s,
        reporter,
        &CollabEvent::FailureReport {
            coding_failure: "disk_full: /dev/sda1 at 100%".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, expected_counterpart(reporter));
    assert_eq!(s.recovery_owner, Some(expected_counterpart(reporter)));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewLocalPending));
    assert_eq!(s.recovery_origin_owner, Some(reporter));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);
}

#[test]
fn test_failure_report_recoverable_from_code_review_final_pending_holds_phase() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    let reporter = s.current_owner;
    assert_eq!(reporter, Agent::Claude);

    let s = apply_event(
        &s,
        reporter,
        &CollabEvent::FailureReport {
            coding_failure: "network_failed: connection reset".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, expected_counterpart(reporter));
    assert_eq!(s.recovery_owner, Some(expected_counterpart(reporter)));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewFinalPending));
    assert_eq!(s.recovery_origin_owner, Some(reporter));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);
}

#[test]
fn test_failure_report_terminal_sets_failed_from_phase_and_clears_pending_failure() {
    // A terminal (non-recoverable) report must still land in CodingFailed
    // with `coding_failure` set, plus the new `failed_from_phase` recording
    // exactly the phase the session was in when the failure hit, and
    // `pending_failure` explicitly cleared.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);

    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "subagent_failure: task 3 crashed".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(
        s.coding_failure.as_deref(),
        Some("subagent_failure: task 3 crashed")
    );
    assert_eq!(s.failed_from_phase, Some(Phase::CodeReviewFixGlobalPending));
    assert_eq!(s.pending_failure, None);
}

#[test]
fn test_no_recoverable_failure_report_ever_reaches_a_terminal_phase() {
    // Exhaustively cross every coding-active phase with every recoverable
    // prefix: none of them may ever produce Phase::CodingFailed or
    // Phase::CodingComplete.
    let recoverable_failures = [
        "git_commit_failed: index.lock EPERM",
        "git_push_failed: non-fast-forward",
        "sandbox_denied: write to /etc blocked",
        "disk_full: /dev/sda1 at 100%",
        "network_failed: connection reset",
        "codex_dispatch_failed: mcp call timed out",
    ];

    // CodeImplementPending
    let base = submit_task_list(&locked_session("hf"), "hf", 1);
    for failure in recoverable_failures {
        let s = apply_event(
            &base,
            base.current_owner,
            &CollabEvent::FailureReport {
                coding_failure: failure.to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeImplementPending);
        assert!(!s.phase.is_coding_terminal());
    }

    // CodeReviewFixGlobalPending
    let base = apply_event(
        &base,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    for failure in recoverable_failures {
        let s = apply_event(
            &base,
            base.current_owner,
            &CollabEvent::FailureReport {
                coding_failure: failure.to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
        assert!(!s.phase.is_coding_terminal());
    }

    // CodeReviewLocalPending
    let base = apply_event(
        &base,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    for failure in recoverable_failures {
        let s = apply_event(
            &base,
            base.current_owner,
            &CollabEvent::FailureReport {
                coding_failure: failure.to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewLocalPending);
        assert!(!s.phase.is_coding_terminal());
    }

    // CodeReviewFinalPending
    let base = apply_event(
        &base,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    for failure in recoverable_failures {
        let s = apply_event(
            &base,
            base.current_owner,
            &CollabEvent::FailureReport {
                coding_failure: failure.to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewFinalPending);
        assert!(!s.phase.is_coding_terminal());
    }
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

// ── Task 5: delegated one-turn completion override ────────────────────

/// Drive a session to `CodeReviewFixGlobalPending` (Codex owner) and have
/// Codex report a recoverable tooling failure, handing control to Claude.
/// Returns the resulting session with `recovery_owner == Some(Claude)`,
/// `recovery_phase == Some(CodeReviewFixGlobalPending)`, and
/// `recovery_origin_owner == Some(Codex)`.
fn session_with_codex_recovery_in_fix_global_pending() -> CollabSession {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: index.lock EPERM".to_string(),
        },
    )
    .unwrap()
}

#[test]
fn test_delegated_completion_override_accepted_and_clears_recovery_state() {
    // Codex reports `git_commit_failed:` while it owns
    // `CodeReviewFixGlobalPending`, handing control to Claude. Claude's
    // `CodeReviewFixGlobal` is accepted once via the override, the phase
    // advances, and every recovery field plus `pending_failure` is cleared
    // in the same transition.
    let s = session_with_codex_recovery_in_fix_global_pending();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.recovery_owner, Some(Agent::Claude));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewFixGlobalPending));
    assert_eq!(s.recovery_origin_owner, Some(Agent::Codex));
    assert_eq!(s.recovery_attempts, 1);
    assert!(s.pending_failure.is_some());

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.pending_failure, None);
    assert_eq!(s.recovery_phase, None);
    assert_eq!(s.recovery_owner, None);
    assert_eq!(s.recovery_origin_owner, None);
    assert_eq!(s.recovery_attempts, 0);
    assert_eq!(s.coding_failure, None);
}

#[test]
fn test_delegated_completion_override_is_one_turn_only() {
    // A second Claude `CodeReviewFixGlobal` after the override has already
    // fired and cleared recovery state must be rejected: the session has
    // moved on to `CodeReviewLocalPending`, so there is no longer a
    // matching `(CodeReviewFixGlobalPending, CodeReviewFixGlobal)` arm —
    // and even if there were, the override's own state is gone.
    let s = session_with_codex_recovery_in_fix_global_pending();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);

    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));
}

#[test]
fn test_delegated_completion_override_accepts_recovery_owner_after_off_turn_dispatch_failure() {
    // Claude flags a Codex MCP dispatch failure off-turn
    // (`codex_dispatch_failed:` is both off-turn-admissible and recoverable
    // per `RECOVERABLE_FAILURE_PREFIXES`).
    // Recovery must go to the counterpart of the interrupted turn's owner,
    // not to the counterpart of the reporting observer: otherwise the
    // dispatch failure would immediately hand control back to unavailable
    // Codex instead of letting Claude complete Codex's turn.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "codex_dispatch_failed: mcp call timed out".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.recovery_owner, Some(Agent::Claude));
    assert_eq!(s.recovery_origin_owner, Some(Agent::Codex));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewFixGlobalPending));

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
}

#[test]
fn test_codex_dispatch_failure_is_not_off_turn_admissible_against_claude_owner() {
    let s = submit_task_list(&locked_session("hf"), "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, Agent::Claude);

    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "codex_dispatch_failed: fabricated report".to_string(),
        },
    )
    .unwrap_err();

    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn test_delegated_completion_override_completed_session_has_no_coding_failure() {
    // Even though a tooling failure was reported and recovered from
    // mid-flight, the session that eventually reaches `CodingComplete` must
    // carry `coding_failure == None` — the recoverable-failure path never
    // touches that field (only `FailureClass::Terminal` does).
    let s = session_with_codex_recovery_in_fix_global_pending();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FinalReview {
            head_sha: "g3".to_string(),
            pr_url: "https://example/pr/1".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodingComplete);
    assert_eq!(s.coding_failure, None);
}

// ── Task 6: retry ceiling ──────────────────────────────────────────────

#[test]
fn test_third_recoverable_report_degrades_to_terminal_after_two_tolerated() {
    // Two successive recoverable reports must stay non-terminal, each
    // bumping `recovery_attempts` (monotonic: 0 -> 1 -> 2). The third
    // report's increment would take `recovery_attempts` to 3, exceeding
    // `MAX_RECOVERY_ATTEMPTS` (2), so it must degrade to `CodingFailed`
    // instead of recovering again — carrying its OWN diagnostic (not the
    // first or second report's) in `coding_failure`.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, Agent::Claude);
    assert_eq!(s.recovery_attempts, 0);

    // Report 1: Claude (current owner) reports; attempts 0 -> 1, still
    // recovering, hands control to Codex.
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert!(!s.phase.is_coding_terminal());
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);

    // Report 2: Codex (now current owner) reports; attempts 1 -> 2, still
    // recovering (right at the ceiling, not past it), hands control back
    // to Claude.
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert!(!s.phase.is_coding_terminal());
    assert_eq!(s.recovery_attempts, 2);
    assert_eq!(s.current_owner, Agent::Claude);
    assert_eq!(s.coding_failure, None);
    assert_eq!(s.failed_from_phase, None);

    // Report 3: Claude (current owner) reports a third time. attempts
    // would go 2 -> 3, exceeding MAX_RECOVERY_ATTEMPTS (2), so this
    // degrades to the terminal path instead of recovering.
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 3 breaks the ceiling".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(
        s.coding_failure.as_deref(),
        Some("git_commit_failed: attempt 3 breaks the ceiling")
    );
    assert_eq!(s.failed_from_phase, Some(Phase::CodeImplementPending));
    assert_eq!(s.pending_failure, None);
    assert_eq!(s.current_owner, Agent::Claude);
    // `recovery_attempts` is left exactly as it was (2): this is neither
    // the `clear_recovery_state` reset (reserved for successful delegated
    // completion) nor a further increment.
    assert_eq!(s.recovery_attempts, 2);
    // Stale recovery pointers are cleared alongside the degrade so a
    // `CodingFailed` session doesn't carry them next to a real
    // `coding_failure`.
    assert_eq!(s.recovery_phase, None);
    assert_eq!(s.recovery_owner, None);
    assert_eq!(s.recovery_origin_owner, None);
}

#[test]
fn test_recovery_attempts_resets_to_zero_on_successful_delegated_completion() {
    // Explicit Task 6 coverage for the second acceptance criterion:
    // `recovery_attempts` resets to 0 once a delegated completion resolves
    // the recovery, rather than lingering at whatever count it reached.
    // (This exact transition is also exercised incidentally by
    // `test_delegated_completion_override_accepted_and_clears_recovery_state`
    // in the Task 5 section above; this test asserts it directly and in
    // isolation so the acceptance criterion has an unambiguous home.)
    let s = session_with_codex_recovery_in_fix_global_pending();
    assert_eq!(s.recovery_attempts, 1);

    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.recovery_attempts, 0);
}

#[test]
fn test_terminal_report_mid_recovery_clears_stale_recovery_pointers() {
    // Hygiene follow-up (flagged independently by two reviewers on Task 6):
    // a Terminal-classified `FailureReport` can arrive while a prior
    // Tooling recovery is still in flight and unresolved — the recovery
    // owner may hit a genuinely unrecoverable error instead of completing
    // the delegated turn. Without clearing the recovery pointer fields
    // here, the resulting `CodingFailed` session would carry a stale
    // `recovery_owner`/`recovery_phase`/`recovery_origin_owner` alongside a
    // real `coding_failure` — exactly the state Task 6's retry-ceiling
    // degrade path already guards against, just reached via a different
    // path (a Terminal report, not the ceiling).
    let s = session_with_codex_recovery_in_fix_global_pending();
    assert_eq!(s.recovery_owner, Some(Agent::Claude));
    assert_eq!(s.recovery_phase, Some(Phase::CodeReviewFixGlobalPending));
    assert_eq!(s.recovery_origin_owner, Some(Agent::Codex));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.current_owner, Agent::Claude);

    // Claude is the current owner (post-recovery-handoff), so it may report
    // on-turn. `subagent_failure:` is not a recoverable prefix -> Terminal.
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "subagent_failure: fix subagent crashed".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(
        s.coding_failure.as_deref(),
        Some("subagent_failure: fix subagent crashed")
    );
    assert_eq!(s.failed_from_phase, Some(Phase::CodeReviewFixGlobalPending));
    assert_eq!(s.pending_failure, None);
    assert_eq!(s.recovery_phase, None);
    assert_eq!(s.recovery_owner, None);
    assert_eq!(s.recovery_origin_owner, None);
    // `recovery_attempts` is diagnostic history at this point, not live
    // state — left as-is, same convention as the retry-ceiling degrade path.
    assert_eq!(s.recovery_attempts, 1);
}

// ── Task 7: ResumeCoding event and terminal-guard carve-out ─────────────

/// Drive a session through the retry-ceiling degrade path (three
/// successive `git_commit_failed:` reports, mirroring
/// `test_third_recoverable_report_degrades_to_terminal_after_two_tolerated`
/// above) to land in `CodingFailed` with a Tooling-classified
/// `coding_failure` and `failed_from_phase` set. This is the realistic way
/// a Tooling `CodingFailed` session comes to exist — via `apply_event`, not
/// a struct literal.
fn session_with_ceiling_degraded_tooling_failure() -> CollabSession {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 2".to_string(),
        },
    )
    .unwrap();
    apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "git_commit_failed: attempt 3 breaks the ceiling".to_string(),
        },
    )
    .unwrap()
}

#[test]
fn test_resume_coding_restores_recorded_phase_with_resumer_as_owner() {
    let s = session_with_ceiling_degraded_tooling_failure();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert_eq!(s.failed_from_phase, Some(Phase::CodeImplementPending));
    assert_eq!(
        classify(s.coding_failure.as_deref().unwrap()),
        FailureClass::Tooling
    );

    // Codex resumes. Resume is gated by `resume_eligibility`, not by
    // `current_owner` (the session is terminal — there is no live owner to
    // check against).
    let s = apply_event(&s, Agent::Codex, &CollabEvent::ResumeCoding).unwrap();

    // Restores the exact recorded phase.
    assert_eq!(s.phase, Phase::CodeImplementPending);
    // The resumer becomes BOTH current_owner and recovery_owner — "the
    // resuming counterpart" in the plan text means "whoever is resuming"
    // (`actor`), not `counterpart(actor)`.
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.recovery_owner, Some(Agent::Codex));
    // recovery_phase mirrors the restored phase so a subsequent
    // `require_actor_or_recovery` call accepts this resumer via the
    // existing Task 5 mechanism.
    assert_eq!(s.recovery_phase, Some(Phase::CodeImplementPending));
    // The old terminal diagnostic moves to `pending_failure` for audit...
    assert_eq!(
        s.pending_failure.as_deref(),
        Some("git_commit_failed: attempt 3 breaks the ceiling")
    );
    // ...and `coding_failure` is cleared.
    assert_eq!(s.coding_failure, None);
    // `failed_from_phase` is retained as a historical record of what phase
    // this session originally failed from (judgment call — see
    // `resume_eligibility`'s doc comment in state_machine/mod.rs).
    assert_eq!(s.failed_from_phase, Some(Phase::CodeImplementPending));
    // A fresh recovery budget starts after an explicit resume. The exhausted
    // pre-resume counter is no longer live state for the restored turn.
    assert_eq!(s.recovery_attempts, 0);
    // Already cleared by the terminal transition (Task 6) and stays clear.
    assert_eq!(s.recovery_origin_owner, None);
}

#[test]
fn test_resume_resets_retry_budget_for_a_subsequent_tooling_failure() {
    // A session that hit the retry ceiling can be explicitly resumed. Its
    // first new tooling failure must receive the normal one-turn handoff,
    // rather than immediately degrading back to CodingFailed because the
    // exhausted pre-resume counter leaked into the new recovery attempt.
    let s = session_with_ceiling_degraded_tooling_failure();
    let s = apply_event(&s, Agent::Codex, &CollabEvent::ResumeCoding).unwrap();
    assert_eq!(s.recovery_attempts, 0);

    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FailureReport {
            coding_failure: "git_push_failed: transient retry after resume".to_string(),
        },
    )
    .unwrap();

    assert_eq!(s.phase, Phase::CodeImplementPending);
    assert_eq!(s.current_owner, Agent::Claude);
    assert_eq!(s.recovery_owner, Some(Agent::Claude));
    assert_eq!(s.recovery_attempts, 1);
    assert_eq!(s.coding_failure, None);
}

#[test]
fn test_resume_coding_rejected_for_branch_drift_session() {
    let s = start_global_review_session("s1", "basesha", "h0").unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "branch_drift: last_head_sha=h0 not found".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    // failed_from_phase IS recorded (Task 4's Terminal branch always sets
    // it) — the rejection here comes from classify(...) != Tooling, not
    // from a missing failed_from_phase.
    assert!(s.failed_from_phase.is_some());

    let err = apply_event(&s, Agent::Claude, &CollabEvent::ResumeCoding).unwrap_err();
    assert!(matches!(err, CollabError::NotResumable { .. }));
}

#[test]
fn test_resume_coding_rejected_for_subagent_failure_session() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::FailureReport {
            coding_failure: "subagent_failure: task 2 timed out".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodingFailed);
    assert!(s.failed_from_phase.is_some());

    let err = apply_event(&s, Agent::Claude, &CollabEvent::ResumeCoding).unwrap_err();
    assert!(matches!(err, CollabError::NotResumable { .. }));
}

#[test]
fn test_resume_coding_from_coding_complete_is_session_locked() {
    // CodingComplete gets zero carve-outs: ResumeCoding from it is rejected
    // exactly like any other event, with the generic SessionLocked — never
    // NotResumable (there is no recorded failure to be ineligible about).
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = finish_through_global_review(&s);
    assert_eq!(s.phase, Phase::CodingComplete);

    let err = apply_event(&s, Agent::Claude, &CollabEvent::ResumeCoding).unwrap_err();
    assert!(matches!(err, CollabError::SessionLocked));
}

#[test]
fn test_double_resume_is_rejected() {
    let s = session_with_ceiling_degraded_tooling_failure();
    let s = apply_event(&s, Agent::Codex, &CollabEvent::ResumeCoding).unwrap();
    assert_eq!(s.phase, Phase::CodeImplementPending);

    // The session is no longer CodingFailed, so a second ResumeCoding falls
    // through to the ordinary WrongPhase catch-all — there is no longer a
    // terminal phase, let alone a matching resume carve-out, to admit it.
    let err = apply_event(&s, Agent::Codex, &CollabEvent::ResumeCoding).unwrap_err();
    assert!(matches!(err, CollabError::WrongPhase { .. }));
}

#[test]
fn test_resume_coding_rejects_legacy_row_with_null_failed_from_phase() {
    // A legacy `CodingFailed` row that predates migration 015/Task 4 never
    // had `failed_from_phase` populated, even though its `coding_failure`
    // happens to classify as a recoverable tooling failure. Such a row is
    // unreachable via `apply_event` (Task 4 always sets `failed_from_phase`
    // on every transition into `CodingFailed`), so it is constructed
    // directly via a struct literal — matching how a real pre-migration DB
    // row would deserialize (NULL failed_from_phase).
    let legacy = CollabSession {
        phase: Phase::CodingFailed,
        current_owner: Agent::Claude,
        coding_failure: Some("git_commit_failed: index.lock EPERM".to_string()),
        failed_from_phase: None,
        ..CollabSession::new("legacy-session")
    };
    assert_eq!(
        classify(legacy.coding_failure.as_deref().unwrap()),
        FailureClass::Tooling
    );

    let err = apply_event(&legacy, Agent::Claude, &CollabEvent::ResumeCoding).unwrap_err();
    match err {
        CollabError::NotResumable { reason } => {
            assert!(
                reason.contains("predates resume support"),
                "expected the NotResumable reason to name \"predates resume support\" \
                 verbatim (Codex note 2 — never guess), got: {reason:?}"
            );
        }
        other => panic!("expected CollabError::NotResumable, got {other:?}"),
    }
}

// ── Review follow-up: the delegated-completion guard's two conjuncts ────

// `require_actor_or_recovery` admits an actor when BOTH
// `session.recovery_owner == Some(actor)` AND
// `session.recovery_phase == Some(session.phase)`. Neither conjunct is
// falsifiable through `apply_event` in the two-agent protocol as it stands:
// every coding phase has exactly one `expected` agent, so an actor that
// fails the recovery clause always falls through to `require_actor` and is
// accepted anyway when it is the expected owner. The two tests below
// therefore drive the guard directly with hand-constructed sessions — states
// a third agent, or a future phase-advancing recovery, would make reachable.
// Without them, deleting either conjunct leaves the whole suite green.

#[test]
fn test_recovery_guard_rejects_an_actor_that_is_not_the_recovery_owner() {
    // Conjunct 1. Recovery is in flight at this exact phase, but the actor
    // is neither the recovery owner nor the expected owner.
    let session = CollabSession {
        phase: Phase::CodeReviewFixGlobalPending,
        current_owner: Agent::Claude,
        pending_failure: Some("git_push_failed: remote rejected".to_string()),
        recovery_phase: Some(Phase::CodeReviewFixGlobalPending),
        recovery_owner: Some(Agent::Claude),
        recovery_attempts: 1,
        total_recovery_attempts: 1,
        ..CollabSession::new("guard-conjunct-1")
    };

    // Claude is the recovery owner and the phase matches, so Claude is in.
    require_actor_or_recovery(&session, Agent::Claude, Agent::Claude).unwrap();

    // Codex is not the recovery owner. Expect the plain `require_actor`
    // rejection, proving the recovery clause did not fire for it.
    let err = require_actor_or_recovery(&session, Agent::Codex, Agent::Claude).unwrap_err();
    match err {
        CollabError::NotYourTurn { expected, got } => {
            assert_eq!(expected, "claude");
            assert_eq!(got, "codex");
        }
        other => panic!("expected CollabError::NotYourTurn, got {other:?}"),
    }
}

#[test]
fn test_recovery_guard_rejects_a_recovery_owner_in_a_different_phase() {
    // Conjunct 2. The actor IS the recovery owner, but the session has moved
    // on from the phase the recovery was recorded against — the delegated
    // completion is scoped to the interrupted phase alone, not to the
    // session for as long as a recovery is open.
    let session = CollabSession {
        phase: Phase::CodeReviewLocalPending,
        current_owner: Agent::Claude,
        pending_failure: Some("git_push_failed: remote rejected".to_string()),
        recovery_phase: Some(Phase::CodeReviewFixGlobalPending),
        recovery_owner: Some(Agent::Codex),
        recovery_attempts: 1,
        total_recovery_attempts: 1,
        ..CollabSession::new("guard-conjunct-2")
    };

    let err = require_actor_or_recovery(&session, Agent::Codex, Agent::Claude).unwrap_err();
    match err {
        CollabError::NotYourTurn { expected, got } => {
            assert_eq!(expected, "claude");
            assert_eq!(got, "codex");
        }
        other => panic!("expected CollabError::NotYourTurn, got {other:?}"),
    }

    // Same session, recovery pointed at the current phase → admitted. This
    // is the control: it is the phase mismatch above that rejects, not
    // something else about the fixture.
    let aligned = CollabSession {
        recovery_phase: Some(Phase::CodeReviewLocalPending),
        ..session
    };
    require_actor_or_recovery(&aligned, Agent::Codex, Agent::Claude).unwrap();
}

// ── Review follow-up: the lifetime recovery ceiling ────────────────────

/// Burn one full per-resume recovery budget: two tolerated Tooling reports,
/// then a third whose increment breaks `MAX_RECOVERY_ATTEMPTS` and degrades
/// the session to `CodingFailed`. Each report comes from whichever agent
/// currently owns the turn, mirroring a real ping-ponging recovery.
fn exhaust_one_recovery_budget(session: &CollabSession) -> CollabSession {
    let out = (1..=3).fold(session.clone(), |current, attempt| {
        let actor = current.current_owner;
        apply_event(
            &current,
            actor,
            &CollabEvent::FailureReport {
                coding_failure: format!("git_commit_failed: attempt {attempt}"),
            },
        )
        .unwrap()
    });
    assert_eq!(out.phase, Phase::CodingFailed);
    out
}

#[test]
fn test_total_recovery_attempts_is_monotonic_across_a_resume() {
    // `recovery_attempts` is the per-resume budget and is refreshed by
    // `ResumeCoding`. `total_recovery_attempts` is the lifetime counter and
    // is not — that asymmetry is the whole point, since `collab_resume` is
    // agent-callable and a resettable-only counter let a session loop
    // failure → ceiling → resume → failure forever.
    let session = submit_task_list(&locked_session("hf"), "hf", 1);
    assert_eq!(session.total_recovery_attempts, 0);

    let failed = exhaust_one_recovery_budget(&session);
    // Two tolerated handoffs incremented both counters; the third degraded
    // to terminal without incrementing either.
    assert_eq!(failed.recovery_attempts, 2);
    assert_eq!(failed.total_recovery_attempts, 2);

    let resumed = apply_event(&failed, Agent::Claude, &CollabEvent::ResumeCoding).unwrap();
    assert_eq!(resumed.recovery_attempts, 0);
    assert_eq!(
        resumed.total_recovery_attempts, 2,
        "resume must not zero the lifetime counter"
    );

    let again = apply_event(
        &resumed,
        resumed.current_owner,
        &CollabEvent::FailureReport {
            coding_failure: "git_push_failed: first failure after resume".to_string(),
        },
    )
    .unwrap();
    assert_eq!(again.recovery_attempts, 1);
    assert_eq!(again.total_recovery_attempts, 3);
}

#[test]
fn test_total_recovery_attempts_survives_a_successful_delegated_completion() {
    // `clear_recovery_state` zeroes the per-resume budget on success. The
    // lifetime counter is deliberately excluded from that reset, so a
    // session that alternates failure/recovery/failure still converges on
    // the ceiling instead of running unbounded.
    let session = session_with_codex_recovery_in_fix_global_pending();
    assert_eq!(session.recovery_attempts, 1);
    assert_eq!(session.total_recovery_attempts, 1);

    let completed = apply_event(
        &session,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();

    assert_eq!(completed.recovery_attempts, 0);
    assert_eq!(completed.total_recovery_attempts, 1);
}

#[test]
fn test_lifetime_ceiling_degrades_to_terminal_with_the_per_resume_budget_unspent() {
    // The binding-ceiling case: `MAX_TOTAL_RECOVERY_ATTEMPTS` (5) is not a
    // multiple of `MAX_RECOVERY_ATTEMPTS` (2), so the lifetime ceiling can
    // and does fire while the per-resume budget still has room. Two
    // exhausted budgets take the lifetime count to 4; after a resume, the
    // first report takes it to 5 and the second breaks it — at
    // `recovery_attempts == 1`, well inside the per-resume ceiling.
    let session = submit_task_list(&locked_session("hf"), "hf", 1);

    let after_first = exhaust_one_recovery_budget(&session);
    let resumed = apply_event(&after_first, Agent::Claude, &CollabEvent::ResumeCoding).unwrap();
    let after_second = exhaust_one_recovery_budget(&resumed);
    assert_eq!(after_second.total_recovery_attempts, 4);

    let resumed = apply_event(&after_second, Agent::Claude, &CollabEvent::ResumeCoding).unwrap();
    assert_eq!(resumed.recovery_attempts, 0);

    let at_ceiling = apply_event(
        &resumed,
        resumed.current_owner,
        &CollabEvent::FailureReport {
            coding_failure: "git_push_failed: fifth lifetime handoff".to_string(),
        },
    )
    .unwrap();
    assert_eq!(at_ceiling.phase, Phase::CodeImplementPending);
    assert_eq!(at_ceiling.recovery_attempts, 1);
    assert_eq!(
        at_ceiling.total_recovery_attempts,
        MAX_TOTAL_RECOVERY_ATTEMPTS
    );

    let past_ceiling = apply_event(
        &at_ceiling,
        at_ceiling.current_owner,
        &CollabEvent::FailureReport {
            coding_failure: "git_push_failed: sixth breaks the lifetime ceiling".to_string(),
        },
    )
    .unwrap();

    assert_eq!(past_ceiling.phase, Phase::CodingFailed);
    assert_eq!(
        past_ceiling.coding_failure.as_deref(),
        Some("git_push_failed: sixth breaks the lifetime ceiling"),
        "the degrade must carry the report that broke the ceiling, not an earlier one"
    );
    assert_eq!(
        past_ceiling.recovery_attempts, 1,
        "the per-resume budget was never exhausted — the lifetime ceiling is what bound"
    );
    assert_eq!(
        past_ceiling.total_recovery_attempts,
        MAX_TOTAL_RECOVERY_ATTEMPTS
    );
    assert_eq!(past_ceiling.recovery_phase, None);
    assert_eq!(past_ceiling.recovery_owner, None);
    assert_eq!(past_ceiling.recovery_origin_owner, None);
}

#[test]
fn test_resume_is_rejected_once_the_lifetime_ceiling_is_reached() {
    // The ceiling has to bound resumes too, not just in-phase handoffs.
    // Otherwise `collab_resume` — agent-callable, and on the unattended
    // successor's permission allowlist — reopens the session indefinitely.
    let session = submit_task_list(&locked_session("hf"), "hf", 1);
    let after_first = exhaust_one_recovery_budget(&session);
    let resumed = apply_event(&after_first, Agent::Claude, &CollabEvent::ResumeCoding).unwrap();
    let after_second = exhaust_one_recovery_budget(&resumed);
    let resumed = apply_event(&after_second, Agent::Claude, &CollabEvent::ResumeCoding).unwrap();
    let exhausted = (1..=2).fold(resumed, |current, attempt| {
        apply_event(
            &current,
            current.current_owner,
            &CollabEvent::FailureReport {
                coding_failure: format!("git_push_failed: lifetime attempt {attempt}"),
            },
        )
        .unwrap()
    });
    assert_eq!(exhausted.phase, Phase::CodingFailed);
    assert_eq!(
        exhausted.total_recovery_attempts,
        MAX_TOTAL_RECOVERY_ATTEMPTS
    );

    let err = apply_event(&exhausted, Agent::Claude, &CollabEvent::ResumeCoding).unwrap_err();
    match err {
        CollabError::NotResumable { reason } => {
            assert!(
                reason.contains("lifetime recovery ceiling"),
                "expected the NotResumable reason to name the lifetime ceiling, got: {reason:?}"
            );
        }
        other => panic!("expected CollabError::NotResumable, got {other:?}"),
    }
}

// ── pilot=claude equivalence suite (issue #246, Task 4) ───────────────
//
// PINNING SUITE — DO NOT MODIFY TO MATCH FUTURE BEHAVIOR.
//
// Every test below pins `apply_event`'s exact `pilot=claude` behavior as it
// exists *today*, hardcoded `Agent::Claude`/`Agent::Codex` arm by arm, before
// a future task (#246 Task 5) rewrites `apply_event` to compute owners
// generically via `pilot()`/`copilot()` helpers instead of the hardcoded
// agents. `pilot=claude` is the codebase's only operational role assignment
// at the time this suite was written (`CollabSession::new` /
// `new_with_implementer` default `pilot` to `Agent::Claude`, and nothing yet
// lets an MCP caller choose otherwise), so these are the ONLY equivalence
// tests this task writes; `pilot=codex` mirror tests are Task 5's job, not
// this one's.
//
// The contract this suite exists to prove: if Task 5's rewrite is truly
// behavior-preserving under `pilot=claude`, every test in this section keeps
// passing UNMODIFIED after that rewrite lands. If a test here needs to
// change to stay green, that is a signal the rewrite altered `pilot=claude`
// behavior, not a cue to "fix" the test to match the new code. Do not edit,
// relax, or delete these assertions to make Task 5 pass — file that as a
// regression instead.
//
// One test per rewritten `apply_event` arm (nine), covering both the
// accepted transition (`phase` + `current_owner`) and the wrong-actor
// rejection, plus one test pinning `new_global_review`'s seeded
// `current_owner`/`implementer`. The `SubmitDraft` arm gets four tests
// instead of one: `match actor { Claude => .., Codex => .. }` are two
// genuinely different code paths, and each has a "first drafter" and a
// "second drafter" branch worth pinning independently.

#[test]
fn pilot_claude_submit_draft_claude_first_then_codex_advances_phase() {
    let s = session();
    assert_eq!(s.pilot, Agent::Claude);
    let s = draft(Agent::Claude, "c1", &s);
    assert_eq!(s.phase, Phase::PlanParallelDrafts);
    assert_eq!(s.current_owner, Agent::Codex);
    let s = draft(Agent::Codex, "c2", &s);
    assert_eq!(s.phase, Phase::PlanSynthesisPending);
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn pilot_claude_submit_draft_codex_first_then_claude_advances_phase() {
    let s = session();
    let s = draft(Agent::Codex, "c1", &s);
    assert_eq!(s.phase, Phase::PlanParallelDrafts);
    assert_eq!(s.current_owner, Agent::Claude);
    let s = draft(Agent::Claude, "c2", &s);
    assert_eq!(s.phase, Phase::PlanSynthesisPending);
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn pilot_claude_submit_draft_duplicate_claude_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let err = apply_event(
        &s,
        Agent::Claude,
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
fn pilot_claude_submit_draft_duplicate_codex_rejected() {
    let s = session();
    let s = draft(Agent::Codex, "c1", &s);
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::SubmitDraft {
            content_hash: "c2".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::AlreadySubmittedDraft {
            agent: "codex".to_string()
        }
    );
}

#[test]
fn pilot_claude_publish_canonical_accepted() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical-hash", &s);
    assert_eq!(s.phase, Phase::PlanCopilotReviewPending);
    assert_eq!(s.current_owner, Agent::Codex);
    assert_eq!(s.canonical_plan_hash.as_deref(), Some("canonical-hash"));
}

#[test]
fn pilot_claude_publish_canonical_wrong_actor_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::PublishCanonical {
            content_hash: "canonical-hash".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_submit_review_accepted() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical-hash", &s);
    let s = review("approve", &s);
    assert_eq!(s.phase, Phase::PlanFinalizePending);
    assert_eq!(s.current_owner, Agent::Claude);
    assert_eq!(s.codex_review_verdict.as_deref(), Some("approve"));
}

#[test]
fn pilot_claude_submit_review_wrong_actor_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical-hash", &s);
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::SubmitReview {
            verdict: "approve".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Codex.to_string(),
            got: Agent::Claude.to_string(),
        }
    );
}

#[test]
fn pilot_claude_publish_final_accepted() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical-hash", &s);
    let s = review("approve", &s);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::PublishFinal {
            content_hash: "final-hash".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::PlanLocked);
    assert_eq!(s.final_plan_hash.as_deref(), Some("final-hash"));
}

#[test]
fn pilot_claude_publish_final_wrong_actor_rejected() {
    let s = session();
    let s = draft(Agent::Claude, "c1", &s);
    let s = draft(Agent::Codex, "c2", &s);
    let s = canonical("canonical-hash", &s);
    let s = review("approve", &s);
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::PublishFinal {
            content_hash: "final-hash".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_submit_task_list_accepted() {
    let s = locked_session("hash-final");
    let s = submit_task_list(&s, "hash-final", 1);
    assert_eq!(s.phase, Phase::CodeImplementPending);
    // Default implementer is `Agent::Claude`.
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn pilot_claude_submit_task_list_wrong_actor_rejected() {
    let s = locked_session("hash-final");
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::SubmitTaskList {
            plan_hash: "hash-final".to_string(),
            base_sha: "base0".to_string(),
            task_list_json: canonical_task_list(1),
            tasks_count: 1,
            head_sha: "head0".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_implementation_done_accepted() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "batch_head".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);
}

#[test]
fn pilot_claude_implementation_done_wrong_actor_rejected() {
    // Default implementer is `Agent::Claude`; `Agent::Codex` is neither the
    // implementer nor holds any recovery standing on a fresh session, so
    // `require_actor_or_recovery` degrades to a plain wrong-actor rejection.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::ImplementationDone {
            head_sha: "h".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_code_review_fix_global_accepted() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn pilot_claude_code_review_fix_global_wrong_actor_rejected() {
    // No recovery state set (the default), so `require_actor_or_recovery`
    // degrades to a plain wrong-actor check against `Agent::Codex`.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Codex.to_string(),
            got: Agent::Claude.to_string(),
        }
    );
}

#[test]
fn pilot_claude_review_local_accepted() {
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, Agent::Claude);
}

#[test]
fn pilot_claude_review_local_wrong_actor_rejected() {
    // No recovery state set (the default), so `require_actor_or_recovery`
    // degrades to a plain wrong-actor check against `Agent::Claude`.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_final_review_accepted() {
    let s = submit_task_list(&locked_session("hf"), "hf", 1);
    let s = finish_through_global_review(&s);
    assert_eq!(s.phase, Phase::CodingComplete);
    assert_eq!(s.current_owner, Agent::Claude);
    assert_eq!(s.pr_url.as_deref(), Some("https://example/pr/1"));
}

#[test]
fn pilot_claude_final_review_wrong_actor_rejected() {
    // No recovery state set (the default), so `require_actor_or_recovery`
    // degrades to a plain wrong-actor check against `Agent::Claude`.
    let s = locked_session("hf");
    let s = submit_task_list(&s, "hf", 1);
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ImplementationDone {
            head_sha: "b".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "g1".to_string(),
        },
    )
    .unwrap();
    let s = apply_event(
        &s,
        Agent::Claude,
        &CollabEvent::ReviewLocal {
            head_sha: "g2".to_string(),
        },
    )
    .unwrap();
    let err = apply_event(
        &s,
        Agent::Codex,
        &CollabEvent::FinalReview {
            head_sha: "g3".to_string(),
            pr_url: "https://example/pr/1".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        CollabError::NotYourTurn {
            expected: Agent::Claude.to_string(),
            got: Agent::Codex.to_string(),
        }
    );
}

#[test]
fn pilot_claude_new_global_review_seeds_owner_and_implementer() {
    // `start_global_review_session` pins `pilot` to `Agent::Claude` today
    // (see its doc comment in `mod.rs`), and `new_global_review` derives
    // `current_owner = counterpart(pilot)` and `implementer = pilot`.
    let session = start_global_review_session("s1", "basesha", "headsha").unwrap();
    assert_eq!(session.pilot, Agent::Claude);
    assert_eq!(session.current_owner, Agent::Codex);
    assert_eq!(session.implementer, Agent::Claude);
}
