//! End-to-end integration tests for the collab protocol.
//!
//! All tests use an in-memory SQLite database and simulate both agents as
//! sequential calls within a single thread. No real LLMs, no polling delays.

use ironrace_collab::state_machine::CollabSession;
use ironrace_memory::db::collab::{generate_collab_id, SessionUpdate};
use ironrace_memory::db::schema::Database;

fn open_db() -> Database {
    Database::open_in_memory().unwrap()
}

// ── Happy path ────────────────────────────────────────────────────────────────

/// Full planning loop: plan → feedback → revised plan → both approve → PLAN_APPROVED.
/// Exactly 6 messages total: plan, feedback, revised plan, + 2 approval signals (via approve tool),
/// but the approve tool doesn't enqueue a message — only send does.
/// So message count is: plan (1) + feedback (1) + revised plan (1) = 3 DB messages.
#[test]
fn planning_loop_reaches_plan_approved() {
    let db = open_db();
    let session_id = generate_collab_id();
    db.collab_session_create(&session_id, "/repo", "main")
        .unwrap();

    // Register capabilities for both agents
    let claude_caps = vec![
        (
            "planner".to_string(),
            Some("Implementation planning".to_string()),
        ),
        (
            "security-reviewer".to_string(),
            Some("OWASP security review".to_string()),
        ),
        ("code-reviewer".to_string(), None),
    ];
    let count = db
        .collab_caps_register(&session_id, "claude", &claude_caps)
        .unwrap();
    assert_eq!(count, 3);

    let codex_caps = vec![
        (
            "codex-verify".to_string(),
            Some("Verify implementation".to_string()),
        ),
        ("test-runner".to_string(), None),
    ];
    let count = db
        .collab_caps_register(&session_id, "codex", &codex_caps)
        .unwrap();
    assert_eq!(count, 2);

    // Codex reads Claude's caps before starting
    let claude_caps_read = db.collab_caps_get(&session_id, "claude").unwrap();
    assert_eq!(claude_caps_read.len(), 3);
    assert_eq!(claude_caps_read[0].name, "planner");

    // Claude reads Codex's caps
    let codex_caps_read = db.collab_caps_get(&session_id, "codex").unwrap();
    assert_eq!(codex_caps_read.len(), 2);

    // ── Step 1: Claude sends initial plan ────────────────────────────────────
    let plan_hash = "sha256:plan_v1";
    let msg_id1 = generate_collab_id();
    // Run state machine
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    s.on_send(
        &ironrace_collab::Agent::Claude,
        &ironrace_collab::types::Topic::Plan,
        Some(plan_hash),
    )
    .unwrap();
    assert_eq!(s.phase, ironrace_collab::Phase::PlanReview);
    persist(&db, &session_id, &s);
    db.collab_message_send(
        &msg_id1,
        &session_id,
        "claude",
        "codex",
        "plan",
        "Here is my initial plan for the health-check endpoint.",
    )
    .unwrap();

    // ── Step 2: Codex reads message, sends feedback ──────────────────────────
    let msgs = db.collab_message_recv(&session_id, "codex", 10).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].topic, "plan");
    db.collab_message_ack(&msgs[0].id, &session_id).unwrap();

    let msg_id2 = generate_collab_id();
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    s.on_send(
        &ironrace_collab::Agent::Codex,
        &ironrace_collab::types::Topic::Feedback,
        None,
    )
    .unwrap();
    assert_eq!(s.phase, ironrace_collab::Phase::PlanFeedback);
    persist(&db, &session_id, &s);
    db.collab_message_send(
        &msg_id2,
        &session_id,
        "codex",
        "claude",
        "feedback",
        "Looks good — add error handling for 5xx upstream failures.",
    )
    .unwrap();

    // ── Step 3: Claude reads feedback, sends revised plan ────────────────────
    let msgs = db.collab_message_recv(&session_id, "claude", 10).unwrap();
    assert_eq!(msgs.len(), 1);
    db.collab_message_ack(&msgs[0].id, &session_id).unwrap();

    let revised_hash = "sha256:plan_v2";
    let msg_id3 = generate_collab_id();
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    s.on_send(
        &ironrace_collab::Agent::Claude,
        &ironrace_collab::types::Topic::Plan,
        Some(revised_hash),
    )
    .unwrap();
    assert_eq!(s.phase, ironrace_collab::Phase::PlanRevised);
    assert_eq!(s.round, 1);
    persist(&db, &session_id, &s);
    db.collab_message_send(
        &msg_id3,
        &session_id,
        "claude",
        "codex",
        "plan",
        "Revised: added 5xx error handling and retry logic.",
    )
    .unwrap();

    // ── Step 4: Codex approves revised plan ──────────────────────────────────
    let msgs = db.collab_message_recv(&session_id, "codex", 10).unwrap();
    assert_eq!(msgs.len(), 1);
    db.collab_message_ack(&msgs[0].id, &session_id).unwrap();

    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    let consensus = s
        .on_approve(&ironrace_collab::Agent::Codex, revised_hash)
        .unwrap();
    assert!(!consensus, "only codex approved so far");
    persist(&db, &session_id, &s);

    // ── Step 5: Claude approves → consensus ──────────────────────────────────
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    let consensus = s
        .on_approve(&ironrace_collab::Agent::Claude, revised_hash)
        .unwrap();
    assert!(consensus, "both approved — should reach consensus");
    assert_eq!(s.phase, ironrace_collab::Phase::PlanApproved);
    persist(&db, &session_id, &s);

    // ── Verify: both recv queues are empty ───────────────────────────────────
    assert!(db
        .collab_message_recv(&session_id, "claude", 10)
        .unwrap()
        .is_empty());
    assert!(db
        .collab_message_recv(&session_id, "codex", 10)
        .unwrap()
        .is_empty());

    // ── Verify: 3 total messages were exchanged ───────────────────────────────
    // (plan, feedback, revised-plan — approvals don't enqueue messages)
    let total_acked = db.collab_message_count(&session_id, "acked").unwrap();
    assert_eq!(total_acked, 3);

    // ── Verify: final session state ───────────────────────────────────────────
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    assert_eq!(row.phase, "PlanApproved");
    assert!(row.claude_ok);
    assert!(row.codex_ok);
}

// ── Escalation ────────────────────────────────────────────────────────────────

/// After max_rounds revisions without approval, session escalates.
#[test]
fn escalation_at_max_rounds() {
    let db = open_db();
    let session_id = generate_collab_id();
    db.collab_session_create(&session_id, "/repo", "main")
        .unwrap();

    // Set max_rounds = 2 by updating the session via our update helper
    db.collab_session_update(
        &session_id,
        &SessionUpdate {
            phase: "PlanDraft",
            current_owner: "claude",
            round: 0,
            claude_ok: false,
            codex_ok: false,
            content_hash: None,
            rejected_hashes: &[],
        },
    )
    .unwrap();
    // Override max_rounds directly (internal helper for tests)
    db.collab_set_max_rounds(&session_id, 2).unwrap();

    let plan_hash = "sha256:p0";

    // Round 0: claude plan → codex feedback → claude revises (round becomes 1)
    run_plan_feedback_cycle(&db, &session_id, plan_hash, "sha256:p1", "rev1");

    // Round 1: codex feedback → claude revises (round becomes 2 → escalate)
    {
        let row = db.collab_session_get(&session_id).unwrap().unwrap();
        let mut s = load_session(&row);
        s.on_send(
            &ironrace_collab::Agent::Codex,
            &ironrace_collab::types::Topic::Feedback,
            None,
        )
        .unwrap();
        persist(&db, &session_id, &s);
    }
    {
        let row = db.collab_session_get(&session_id).unwrap().unwrap();
        let mut s = load_session(&row);
        let phase = s
            .on_send(
                &ironrace_collab::Agent::Claude,
                &ironrace_collab::types::Topic::Plan,
                Some("sha256:p2"),
            )
            .unwrap();
        assert_eq!(*phase, ironrace_collab::Phase::PlanEscalated);
        persist(&db, &session_id, &s);
    }

    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    assert_eq!(row.phase, "PlanEscalated");
    assert_eq!(row.round, 2);
}

// ── Constraint tests ──────────────────────────────────────────────────────────

#[test]
fn wrong_turn_send_returns_not_your_turn() {
    use ironrace_collab::state_machine::CollabError;

    let db = open_db();
    let session_id = generate_collab_id();
    db.collab_session_create(&session_id, "/repo", "main")
        .unwrap();

    // Codex tries to send before Claude — not their turn
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    let err = s
        .on_send(
            &ironrace_collab::Agent::Codex,
            &ironrace_collab::types::Topic::Plan,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, CollabError::NotYourTurn { .. }));
}

#[test]
fn approve_previously_rejected_hash_returns_already_rejected() {
    use ironrace_collab::state_machine::CollabError;

    let db = open_db();
    let session_id = generate_collab_id();
    db.collab_session_create(&session_id, "/repo", "main")
        .unwrap();

    let hash = "sha256:original";

    // Claude sends plan
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    s.on_send(
        &ironrace_collab::Agent::Claude,
        &ironrace_collab::types::Topic::Plan,
        Some(hash),
    )
    .unwrap();
    persist(&db, &session_id, &s);

    // Codex rejects
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    s.on_send(
        &ironrace_collab::Agent::Codex,
        &ironrace_collab::types::Topic::Reject,
        None,
    )
    .unwrap();
    persist(&db, &session_id, &s);

    // Claude tries to approve the rejected hash — should fail
    let row = db.collab_session_get(&session_id).unwrap().unwrap();
    let mut s = load_session(&row);
    // Re-add the rejected hash to the in-memory session (normally would be loaded from a
    // rejected_hashes store; in this test we're driving the state machine directly)
    s.rejected_hashes.push(hash.to_string());
    let err = s
        .on_approve(&ironrace_collab::Agent::Claude, hash)
        .unwrap_err();
    assert_eq!(err, CollabError::AlreadyRejected);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn load_session(row: &ironrace_memory::db::collab::SessionRow) -> CollabSession {
    let mut s = CollabSession::new(row.id.clone());
    s.phase =
        ironrace_collab::Phase::from_name(&row.phase).unwrap_or(ironrace_collab::Phase::PlanDraft);
    s.current_owner = ironrace_collab::Agent::from_name(&row.current_owner)
        .unwrap_or(ironrace_collab::Agent::Claude);
    s.round = row.round as u32;
    s.max_rounds = row.max_rounds as u32;
    s.claude_ok = row.claude_ok;
    s.codex_ok = row.codex_ok;
    s.content_hash = row.content_hash.clone();
    s
}

fn persist(db: &Database, session_id: &str, s: &ironrace_collab::CollabSession) {
    db.collab_session_update(
        session_id,
        &SessionUpdate {
            phase: s.phase.as_str(),
            current_owner: s.current_owner.as_str(),
            round: s.round as i64,
            claude_ok: s.claude_ok,
            codex_ok: s.codex_ok,
            content_hash: s.content_hash.as_deref(),
            rejected_hashes: &s.rejected_hashes,
        },
    )
    .unwrap();
}

fn run_plan_feedback_cycle(
    db: &Database,
    session_id: &str,
    initial_hash: &str,
    revised_hash: &str,
    msg_suffix: &str,
) {
    // Claude sends plan
    {
        let row = db.collab_session_get(session_id).unwrap().unwrap();
        let mut s = load_session(&row);
        s.on_send(
            &ironrace_collab::Agent::Claude,
            &ironrace_collab::types::Topic::Plan,
            Some(initial_hash),
        )
        .unwrap();
        persist(db, session_id, &s);
        let id = generate_collab_id();
        db.collab_message_send(&id, session_id, "claude", "codex", "plan", msg_suffix)
            .unwrap();
    }
    // Codex sends feedback
    {
        let msgs = db.collab_message_recv(session_id, "codex", 10).unwrap();
        for m in &msgs {
            db.collab_message_ack(&m.id, session_id).unwrap();
        }
        let row = db.collab_session_get(session_id).unwrap().unwrap();
        let mut s = load_session(&row);
        s.on_send(
            &ironrace_collab::Agent::Codex,
            &ironrace_collab::types::Topic::Feedback,
            None,
        )
        .unwrap();
        persist(db, session_id, &s);
        let id = generate_collab_id();
        db.collab_message_send(&id, session_id, "codex", "claude", "feedback", "needs work")
            .unwrap();
    }
    // Claude revises
    {
        let msgs = db.collab_message_recv(session_id, "claude", 10).unwrap();
        for m in &msgs {
            db.collab_message_ack(&m.id, session_id).unwrap();
        }
        let row = db.collab_session_get(session_id).unwrap().unwrap();
        let mut s = load_session(&row);
        s.on_send(
            &ironrace_collab::Agent::Claude,
            &ironrace_collab::types::Topic::Plan,
            Some(revised_hash),
        )
        .unwrap();
        persist(db, session_id, &s);
        let id = generate_collab_id();
        db.collab_message_send(&id, session_id, "claude", "codex", "plan", "revised")
            .unwrap();
    }
}
