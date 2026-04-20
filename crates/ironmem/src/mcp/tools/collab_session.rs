use serde_json::{json, Value};

use crate::collab::queue::SessionRecord;
use crate::collab::{apply_event, CollabError, CollabEvent, Phase};
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

use super::collab_events::{build_collab_event, failure_report_is_branch_drift};
use super::shared::{other_agent, require_agent, require_str, MAX_COLLAB_CONTENT_CHARS};

pub(super) fn collab_error_to_memory_error(error: CollabError) -> MemoryError {
    MemoryError::Validation(error.to_string())
}

pub(super) fn session_record_json(record: &SessionRecord) -> Value {
    json!({
        "id": record.session.id.as_str(),
        "phase": record.session.phase.to_string(),
        "current_owner": record.session.current_owner.as_str(),
        "repo_path": record.repo_path.as_str(),
        "branch": record.branch.as_str(),
        "task": record.task.as_deref(),
        "claude_draft_hash": record.session.claude_draft_hash.as_deref(),
        "codex_draft_hash": record.session.codex_draft_hash.as_deref(),
        "canonical_plan_hash": record.session.canonical_plan_hash.as_deref(),
        "final_plan_hash": record.session.final_plan_hash.as_deref(),
        "codex_review_verdict": record.session.codex_review_verdict.as_deref(),
        "review_round": record.session.review_round,
        "task_list": record.session.task_list.as_deref(),
        "tasks_count": record.session.tasks_count(),
        "current_task_index": record.session.current_task_index,
        "task_review_round": record.session.task_review_round,
        "global_review_round": record.session.global_review_round,
        "base_sha": record.session.base_sha.as_deref(),
        "last_head_sha": record.session.last_head_sha.as_deref(),
        "pr_url": record.session.pr_url.as_deref(),
        "coding_failure": record.session.coding_failure.as_deref(),
        "ended_at": record.ended_at.as_deref(),
        "created_at": record.created_at.as_str(),
        "updated_at": record.updated_at.as_str(),
    })
}

/// True for every topic the collab_send handler accepts — v1 planning
/// vocabulary plus the v3 coding vocabulary. The topic string `final` is
/// intentionally reused across versions; dispatch happens on the current
/// phase inside `build_collab_event`.
pub(super) fn is_known_collab_topic(topic: &str) -> bool {
    matches!(
        topic,
        "draft"
            | "canonical"
            | "review"
            | "final"
            | "task_list"
            | "implement"
            | "review_fix"
            | "review_local"
            | "review_fix_global"
            | "final_review"
            | "failure_report"
    )
}

/// Polling cadence for `collab_wait_my_turn`. Short enough that
/// turn transitions feel immediate, long enough that idle waits don't
/// hammer SQLite.
const WAIT_MY_TURN_POLL_MS: u64 = 500;
/// Default timeout (seconds) applied when the caller omits `timeout_secs`.
const WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard cap on `timeout_secs` — clients that want longer should re-poll.
const WAIT_MY_TURN_MAX_TIMEOUT_SECS: u64 = 60;

/// Snapshot of session state read by `wait_my_turn` on each poll tick. Taken
/// in one `load_session_record` call so `task_list_submitted` and `phase` are
/// always from the same row — a concurrent `collab_send(task_list)` commit
/// cannot interleave into this view and produce an inconsistent terminal-set
/// decision. The returned status is stale-but-consistent: the next tick picks
/// up the new phase.
struct WaitTurnSnapshot {
    is_my_turn: bool,
    phase: String,
    current_owner: String,
    ended: bool,
    phase_is_terminal: bool,
}

fn wait_turn_snapshot(record: &SessionRecord, agent: &str) -> WaitTurnSnapshot {
    let ended = record.ended_at.is_some();
    // Dynamic terminal set, evaluated on a single snapshot: pre-task_list,
    // PlanLocked is terminal so v1 agents can exit cleanly after the plan
    // locks. Post-task_list the v2 coding phase is underway and the terminal
    // set switches to `{CodingComplete, CodingFailed}`.
    let task_list_submitted = record.session.task_list.is_some();
    let phase_is_terminal = if task_list_submitted {
        record.session.phase.is_terminal_v2()
    } else {
        matches!(record.session.phase, crate::collab::Phase::PlanLocked)
            || record.session.phase.is_terminal_v2()
    };
    let is_my_turn = !ended && !phase_is_terminal && record.session.current_owner == agent;
    WaitTurnSnapshot {
        is_my_turn,
        phase: record.session.phase.to_string(),
        current_owner: record.session.current_owner.clone(),
        ended,
        phase_is_terminal,
    }
}

pub(super) fn handle_collab_start(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_path = require_str(args, "repo_path")?;
    let branch = require_str(args, "branch")?;
    let initiator = require_agent(require_str(args, "initiator")?)?;
    let task_owned = args
        .get("task")
        .and_then(Value::as_str)
        .map(|value| sanitize::sanitize_content(value, MAX_COLLAB_CONTENT_CHARS))
        .transpose()?
        .map(ToString::to_string);
    let task = task_owned.as_deref();
    let session_id = uuid::Uuid::new_v4().to_string();

    app.db.with_transaction(|tx| {
        crate::collab::queue::create_session(tx, &session_id, repo_path, branch, task)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_start",
            &json!({
                "session_id": session_id,
                "repo_path": repo_path,
                "branch": branch,
                "initiator": initiator,
                "has_task": task.is_some(),
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "session_id": session_id, "task": task }))
}

pub(super) fn handle_collab_send(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let sender = require_agent(require_str(args, "sender")?)?;
    let topic = require_str(args, "topic")?;
    let content =
        sanitize::sanitize_content(require_str(args, "content")?, MAX_COLLAB_CONTENT_CHARS)?;
    if !is_known_collab_topic(topic) {
        return Err(MemoryError::Validation(format!(
            "unknown collab topic: {topic}"
        )));
    }

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let mut session = crate::collab::queue::load_session_record(tx, session_id)?.session;
        let phase_before = session.phase.to_string();

        // Upstream turn gate: reject sends from the non-owner before any
        // payload parsing or event dispatch. Two carve-outs:
        //   1. `PlanParallelDrafts` — both agents submit drafts
        //      independently; current_owner there is a "next-expected" hint
        //      and the state-machine arm uses its own "already-submitted"
        //      guard.
        //   2. `failure_report` with a `branch_drift:` prefix — either agent
        //      must be able to abort the session when they detect branch
        //      drift, even if it is not their turn. The deeper check in
        //      `apply_event` validates the prefix and rejects generic
        //      off-turn failure reports as NotYourTurn.
        let turn_exempt = matches!(session.phase, crate::collab::Phase::PlanParallelDrafts)
            || (topic == "failure_report"
                && sender != session.current_owner
                && failure_report_is_branch_drift(content));
        if !turn_exempt && sender != session.current_owner {
            return Err(MemoryError::Validation(format!(
                "not your turn: phase {} expects sender '{}', got '{}'",
                session.phase, session.current_owner, sender
            )));
        }

        let event = build_collab_event(topic, content, &session)?;

        session = apply_event(&session, sender, &event).map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;

        let message_id = crate::collab::queue::send_message(
            tx,
            session_id,
            sender,
            other_agent(sender),
            topic,
            content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_send",
            &json!({
                "session_id": session_id,
                "sender": sender,
                "topic": topic,
                "phase_before": phase_before,
            }),
            Some(&json!({
                "message_id": message_id,
                "phase": session.phase.to_string(),
            })),
        )?;

        Ok(json!({
            "message_id": message_id,
            "phase": session.phase.to_string(),
        }))
    })
}

pub(super) fn handle_collab_recv(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let receiver = require_agent(require_str(args, "receiver")?)?;
    let limit = (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize).min(50);

    // Blind-drafts invariant: during PlanParallelDrafts, an agent must not see
    // the counterpart's draft until it has submitted its own. This enforces
    // the "parallel" in parallel drafts at the server boundary so the protocol
    // doesn't rely on agent-side discipline alone.
    let session = app.db.collab_load_session(session_id)?;
    let suppress_drafts = matches!(session.phase, crate::collab::Phase::PlanParallelDrafts)
        && match receiver {
            "claude" => session.claude_draft_hash.is_none(),
            "codex" => session.codex_draft_hash.is_none(),
            _ => false,
        };

    let messages = app.db.collab_recv_messages(session_id, receiver, limit)?;
    let filtered: Vec<Value> = messages
        .into_iter()
        .filter(|message| !(suppress_drafts && message.topic == "draft"))
        .map(|message| {
            json!({
                "id": message.id,
                "sender": message.sender,
                "topic": message.topic,
                "content": message.content,
                "created_at": message.created_at,
            })
        })
        .collect();
    Ok(json!({ "messages": filtered }))
}

pub(super) fn handle_collab_ack(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let message_id = require_str(args, "message_id")?;
    let session_id = require_str(args, "session_id")?;
    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        crate::collab::queue::ack_message(tx, session_id, message_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_ack",
            &json!({
                "session_id": session_id,
                "message_id": message_id,
            }),
            Some(&json!({ "ok": true })),
        )?;
        Ok(())
    })?;
    Ok(json!({ "ok": true }))
}

pub(super) fn handle_collab_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let record = app.db.collab_load_session_record(session_id)?;
    let mut status = session_record_json(&record);
    // Surface the locked plan text alongside the hashes so a fresh agent
    // joining mid-session can build a task_list (or continue a review round)
    // without having to re-derive content it previously sent but already had
    // acked off its inbox.
    if record.session.canonical_plan_hash.is_some() {
        if let Some(content) = app
            .db
            .collab_latest_message_content(session_id, "canonical")?
        {
            status["canonical_plan"] = Value::String(content);
        }
    }
    if record.session.final_plan_hash.is_some() {
        if let Some(content) = app.db.collab_latest_message_content(session_id, "final")? {
            status["final_plan"] = Value::String(content);
        }
    }
    Ok(status)
}

pub(super) fn handle_collab_approve(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    if agent != "codex" {
        return Err(MemoryError::Validation(
            "agent must be 'codex' for collab_approve".to_string(),
        ));
    }
    let content_hash = require_str(args, "content_hash")?;
    let review_content = json!({
        "verdict": "approve",
        "content_hash": content_hash,
    })
    .to_string();

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let session = crate::collab::queue::load_session(tx, session_id)?;
        let expected_hash = session
            .canonical_plan_hash
            .as_deref()
            .ok_or_else(|| MemoryError::Validation("canonical_plan_hash is not set".to_string()))?;
        if content_hash != expected_hash {
            return Err(MemoryError::Validation(
                "content_hash does not match canonical_plan_hash".to_string(),
            ));
        }
        let session = apply_event(
            &session,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: "approve".to_string(),
            },
        )
        .map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;
        let _ = crate::collab::queue::send_message(
            tx,
            session_id,
            "codex",
            "claude",
            "review",
            &review_content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_approve",
            &json!({
                "session_id": session_id,
                "agent": agent,
                "content_hash": content_hash,
            }),
            Some(&json!({ "phase": session.phase.to_string() })),
        )?;
        Ok(json!({ "phase": session.phase.to_string() }))
    })
}

pub(super) fn handle_collab_wait_my_turn(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS)
        .clamp(1, WAIT_MY_TURN_MAX_TIMEOUT_SECS);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_millis(WAIT_MY_TURN_POLL_MS);

    loop {
        let record = app.db.collab_load_session_record(session_id)?;
        let snap = wait_turn_snapshot(&record, agent);

        if snap.is_my_turn
            || snap.ended
            || snap.phase_is_terminal
            || std::time::Instant::now() >= deadline
        {
            return Ok(json!({
                "is_my_turn": snap.is_my_turn,
                "phase": snap.phase,
                "current_owner": snap.current_owner,
                "session_ended": snap.ended,
            }));
        }

        std::thread::sleep(poll_interval);
    }
}

pub(super) fn handle_collab_end(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    app.db.with_transaction(|tx| {
        // collab_end is valid only from PlanLocked (pre-task_list), or from
        // the two v2 terminal phases. Rejecting during any active planning
        // or coding phase prevents either agent from killing a session the
        // counterpart is still working in.
        let session = crate::collab::queue::load_session(tx, session_id)?;
        let allowed = matches!(
            session.phase,
            Phase::PlanLocked | Phase::CodingComplete | Phase::CodingFailed
        );
        if !allowed {
            return Err(MemoryError::Validation(format!(
                "collab_end rejected in active phase {}; end is only valid from PlanLocked (pre-task_list), CodingComplete, or CodingFailed",
                session.phase
            )));
        }
        crate::collab::queue::end_session(tx, session_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_end",
            &json!({
                "session_id": session_id,
                "agent": agent,
                "phase": session.phase.to_string(),
            }),
            Some(&json!({ "ok": true })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "ok": true, "session_id": session_id }))
}
