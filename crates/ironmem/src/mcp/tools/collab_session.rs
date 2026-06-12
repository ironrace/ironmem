use serde_json::{json, Value};
use std::process::Command;

use crate::collab::queue::SessionRecord;
use crate::collab::{
    apply_event, start_global_review_session, Agent, CollabError, CollabEvent, Phase,
};
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

use super::collab_events::{build_collab_event, failure_report_is_off_turn_admissible};
use super::shared::{
    other_agent, require_agent, require_implementer, require_str, MAX_COLLAB_CONTENT_CHARS,
};

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
        // `plan_file_path` is parsed back out of the canonicalized
        // `task_list` JSON so consumers (notably the Codex prompt) can
        // read it as a top-level field instead of re-parsing the JSON
        // blob themselves. Returns `None` until `task_list` is sent or
        // when the optional field was omitted.
        "plan_file_path": plan_file_path_from_task_list(record.session.task_list.as_deref()),
        // `execution_mode` is parsed back out of the canonicalized
        // `task_list` JSON for the same reason as `plan_file_path`.
        // Returns `None` when `task_list` is absent, when the field was
        // omitted (default subagent-driven), or when the payload is
        // malformed. Consumers treat `None` as the default (subagent-driven).
        "execution_mode": execution_mode_from_task_list(record.session.task_list.as_deref()),
        "implementer": record.session.implementer.as_str(),
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

/// Pull `plan_file_path` out of a stored `task_list` JSON payload. Mirrors
/// `tasks_count_from_list` in shape: returns `None` for unset/malformed
/// task_list so a corrupt payload yields `null` in the JSON response
/// rather than panicking the read path.
fn plan_file_path_from_task_list(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let value: Value = serde_json::from_str(raw).ok()?;
    value
        .get("plan_file_path")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Pull `execution_mode` out of a stored `task_list` JSON payload. Returns
/// `None` when `task_list` is unset, when the field was omitted (default
/// subagent-driven path), or when the payload is malformed. Consumers treat
/// `None` the same as the omitted-field default.
fn execution_mode_from_task_list(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let value: Value = serde_json::from_str(raw).ok()?;
    value
        .get("execution_mode")
        .and_then(Value::as_str)
        .map(str::to_string)
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
            | "implementation_done"
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

fn wait_turn_snapshot(record: &SessionRecord, agent: Agent) -> WaitTurnSnapshot {
    let ended = record.ended_at.is_some();
    // Dynamic terminal set, evaluated on a single snapshot: pre-task_list,
    // PlanLocked is terminal so v1 agents can exit cleanly after the plan
    // locks. Post-task_list the v2 coding phase is underway and the terminal
    // set switches to `{CodingComplete, CodingFailed}`.
    let task_list_submitted = record.session.task_list.is_some();
    let phase_is_terminal = if task_list_submitted {
        record.session.phase.is_coding_terminal()
    } else {
        matches!(record.session.phase, crate::collab::Phase::PlanLocked)
            || record.session.phase.is_coding_terminal()
    };
    let is_my_turn = !ended && !phase_is_terminal && record.session.current_owner == agent;
    WaitTurnSnapshot {
        is_my_turn,
        phase: record.session.phase.to_string(),
        current_owner: record.session.current_owner.to_string(),
        ended,
        phase_is_terminal,
    }
}

/// Best-effort initial `task_outcomes` row creation (METRICS_SPEC §5.4). Called
/// immediately after the collab session transaction commits; metrics failures log
/// at warn and are non-fatal — the protocol state is the source of truth.
fn create_initial_task_outcome(app: &App, session_id: &str) {
    let outcome = crate::db::metrics::TaskOutcome {
        task_tag: session_id.to_string(),
        collab_session_id: Some(session_id.to_string()),
        started_at: Some(crate::metrics::now_rfc3339()),
        done_at: None,
        outcome: None,
        review_rounds: 0,
        fix_commits: 0,
        handoffs: 0,
        pr_url: None,
    };
    if let Err(e) = app.db.upsert_task_outcome(&outcome) {
        tracing::warn!("metrics: task_outcomes create failed: {e}");
    }
}

/// Best-effort task_outcomes lifecycle writes (METRICS_SPEC §4/§5.4). Runs
/// AFTER the protocol transaction commits so a metrics failure can never roll
/// back or fail a collab turn; errors log at warn.
///
/// If the process dies between the commit and these writes, the row
/// under-counts or stays in-flight (outcome NULL) — acceptable for a
/// best-effort metrics projection; protocol state is the source of truth.
fn record_task_outcome_transition(
    app: &App,
    session_id: &str,
    before: Phase,
    after: Phase,
    pr_url: Option<&str>,
) {
    if before == after {
        return;
    }
    let entered_review = crate::metrics::phase_bucket(after) == "review"
        && matches!(crate::metrics::phase_bucket(before), "impl" | "rework");
    if entered_review {
        if let Err(e) = app.db.increment_task_review_rounds(session_id) {
            tracing::warn!("metrics: review_rounds increment failed: {e}");
        }
    }
    let now = crate::metrics::now_rfc3339();
    let result = match after {
        Phase::CodingComplete => {
            app.db
                .mark_task_outcome_done(session_id, Some(&now), None, pr_url)
        }
        Phase::CodingFailed => {
            app.db
                .mark_task_outcome_done(session_id, Some(&now), Some("failed"), None)
        }
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!("metrics: task_outcome terminal update failed: {e}");
    }
}

fn ensure_no_conflicting_process_session(
    app: &App,
    requested_session_id: &str,
) -> Result<(), MemoryError> {
    let Some(active_session_id) = app.active_collab_session_snapshot() else {
        return Ok(());
    };
    if active_session_id == requested_session_id {
        return Ok(());
    }

    match app.db.collab_load_session_record(&active_session_id) {
        Ok(record) if record.ended_at.is_none() => Err(MemoryError::Validation(format!(
            "another active collab session is already bound to this MCP process for metrics attribution: {active_session_id}. End it or use a separate server process before switching to {requested_session_id}."
        ))),
        Ok(_) | Err(MemoryError::NotFound(_)) => {
            app.clear_active_collab_session();
            Ok(())
        }
        Err(e) => Err(MemoryError::Validation(format!(
            "could not verify active collab session {active_session_id} before switching metrics attribution: {e}"
        ))),
    }
}

fn ensure_no_conflicting_process_session_tx(
    app: &App,
    tx: &rusqlite::Transaction<'_>,
    requested_session_id: &str,
) -> Result<(), MemoryError> {
    let Some(active_session_id) = app.active_collab_session_snapshot() else {
        return Ok(());
    };
    if active_session_id == requested_session_id {
        return Ok(());
    }

    match crate::collab::queue::load_session_record(tx, &active_session_id) {
        Ok(record) if record.ended_at.is_none() => Err(MemoryError::Validation(format!(
            "another active collab session is already bound to this MCP process for metrics attribution: {active_session_id}. End it or use a separate server process before switching to {requested_session_id}."
        ))),
        Ok(_) | Err(MemoryError::NotFound(_)) => {
            app.clear_active_collab_session();
            Ok(())
        }
        Err(e) => Err(MemoryError::Validation(format!(
            "could not verify active collab session {active_session_id} before switching metrics attribution: {e}"
        ))),
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
    // Optional `implementer` field: routes the v3 batch implementation
    // phase. Default is `Agent::Claude` (historical flow). `Agent::Codex`
    // makes Codex the owner of `CodeImplementPending` and the only valid
    // sender of `implementation_done`. It can be rebound later through
    // `collab_set_implementer` while planning or implementation is active.
    let implementer = match args.get("implementer").and_then(Value::as_str) {
        Some(value) => require_implementer(value)?,
        None => Agent::Claude,
    };
    let session_id = uuid::Uuid::new_v4().to_string();

    app.db.with_transaction(|tx| {
        // Guard against accidental duplicate sessions on the same repo+branch
        // (e.g. a fired ScheduleWakeup replaying the `/collab start` entry
        // command after a session already reached CodingComplete). The check is
        // atomic with the insert inside this transaction.
        if let Some((existing_id, phase)) =
            crate::collab::queue::find_active_session_by_repo_branch(tx, repo_path, branch)?
        {
            return Err(MemoryError::Validation(format!(
                "an active collab session already exists for repo {repo_path} branch {branch}: \
                 {existing_id} (phase {phase}). Resume it with `/collab join {existing_id}`, or \
                if it is finished call collab_end on it before starting a new session here."
            )));
        }
        ensure_no_conflicting_process_session_tx(app, tx, &session_id)?;
        crate::collab::queue::create_session(
            tx,
            &session_id,
            repo_path,
            branch,
            task,
            implementer,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_start",
            &json!({
                "session_id": session_id,
                "repo_path": repo_path,
                "branch": branch,
                "initiator": initiator.as_str(),
                "implementer": implementer.as_str(),
                "has_task": task.is_some(),
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    app.set_active_collab_session(&session_id);
    create_initial_task_outcome(app, &session_id);

    Ok(json!({
        "session_id": session_id,
        "task": task,
        "implementer": implementer.as_str(),
    }))
}

pub(super) fn handle_collab_set_implementer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let implementer = require_implementer(require_str(args, "implementer")?)?;

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let record = crate::collab::queue::load_session_record(tx, session_id)?;
        let can_change = match record.session.phase {
            Phase::PlanParallelDrafts
            | Phase::PlanSynthesisPending
            | Phase::PlanCodexReviewPending
            | Phase::PlanClaudeFinalizePending
            | Phase::PlanLocked => record.session.task_list.is_none(),
            Phase::CodeImplementPending => true,
            Phase::CodeReviewFixGlobalPending
            | Phase::CodeReviewLocalPending
            | Phase::CodeReviewFinalPending
            | Phase::CodingComplete
            | Phase::CodingFailed => false,
        };
        if !can_change {
            return Err(MemoryError::Validation(
                "implementer can only be changed before implementation is complete".to_string(),
            ));
        }

        let previous = record.session.implementer;
        let previous_owner = record.session.current_owner;
        let new_owner = if record.session.phase == Phase::CodeImplementPending {
            Some(implementer)
        } else {
            None
        };
        let owner_changed = new_owner.is_some_and(|owner| owner != previous_owner);
        if previous != implementer || owner_changed {
            crate::collab::queue::set_implementer(tx, session_id, implementer, new_owner)?;
        }
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_set_implementer",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "previous_implementer": previous.as_str(),
                "implementer": implementer.as_str(),
                "phase": record.session.phase.to_string(),
                "previous_owner": previous_owner.as_str(),
                "current_owner": new_owner.unwrap_or(previous_owner).as_str(),
                "changed": previous != implementer || owner_changed,
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        let updated = crate::collab::queue::load_session_record(tx, session_id)?;
        Ok(session_record_json(&updated))
    })
}

pub(super) fn handle_collab_start_code_review(
    app: &App,
    args: &Value,
) -> Result<Value, MemoryError> {
    let repo_path = require_str(args, "repo_path")?;
    let branch = require_str(args, "branch")?;
    let base_sha = require_str(args, "base_sha")?;
    let head_sha = require_str(args, "head_sha")?;
    let initiator = require_agent(require_str(args, "initiator")?)?;
    if initiator != Agent::Claude {
        return Err(MemoryError::Validation(
            "initiator must be 'claude' for collab_start_code_review".to_string(),
        ));
    }
    let task = sanitize::sanitize_content(require_str(args, "task")?, MAX_COLLAB_CONTENT_CHARS)?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let session = start_global_review_session(&session_id, base_sha, head_sha)
        .map_err(collab_error_to_memory_error)?;

    app.db.with_transaction(|tx| {
        // Same duplicate-session guard as `handle_collab_start`: a stray replay
        // of `/collab review` must not fork a second review session on a branch
        // that already has an active one.
        if let Some((existing_id, phase)) =
            crate::collab::queue::find_active_session_by_repo_branch(tx, repo_path, branch)?
        {
            return Err(MemoryError::Validation(format!(
                "an active collab session already exists for repo {repo_path} branch {branch}: \
                 {existing_id} (phase {phase}). Resume it with `/collab join {existing_id}`, or \
                if it is finished call collab_end on it before starting a new session here."
            )));
        }
        ensure_no_conflicting_process_session_tx(app, tx, &session_id)?;
        // Shortcut sessions never enter `CodeImplementPending`, so the
        // `implementer` field is fixed at `Agent::Claude` for uniformity.
        crate::collab::queue::create_session(
            tx,
            &session_id,
            repo_path,
            branch,
            Some(task),
            Agent::Claude,
        )?;
        crate::collab::queue::save_session(tx, &session)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_start_code_review",
            &json!({
                "session_id": session_id,
                "repo_path": repo_path,
                "branch": branch,
                "base_sha": base_sha,
                "head_sha": head_sha,
                "initiator": initiator.as_str(),
                "task": task,
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    app.set_active_collab_session(&session_id);
    create_initial_task_outcome(app, &session_id);

    Ok(json!({ "session_id": session_id, "task": task }))
}

pub(super) fn handle_collab_send(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    ensure_no_conflicting_process_session(app, session_id)?;
    let sender = require_agent(require_str(args, "sender")?)?;
    let topic = require_str(args, "topic")?;
    let content =
        sanitize::sanitize_content(require_str(args, "content")?, MAX_COLLAB_CONTENT_CHARS)?;
    if !is_known_collab_topic(topic) {
        return Err(MemoryError::Validation(format!(
            "unknown collab topic: {topic}"
        )));
    }

    let (response, before, after, pr_url) = app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let record = crate::collab::queue::load_session_record(tx, session_id)?;
        let mut session = record.session;
        let phase_before = session.phase.to_string();
        // Capture the phase enum BEFORE apply_event for lifecycle tracking.
        let phase_before_enum = session.phase;

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
                && failure_report_is_off_turn_admissible(content));
        if !turn_exempt && sender != session.current_owner {
            return Err(MemoryError::Validation(format!(
                "not your turn: phase {} expects sender '{}', got '{}'",
                session.phase, session.current_owner, sender
            )));
        }

        let event = build_collab_event(topic, content, session.phase)?;
        let shortcut_ancestry = session.task_list.is_none()
            && matches!(
                (&session.phase, &event),
                (
                    crate::collab::Phase::CodeReviewFixGlobalPending,
                    crate::collab::CollabEvent::CodeReviewFixGlobal { .. },
                ) | (
                    crate::collab::Phase::CodeReviewLocalPending,
                    crate::collab::CollabEvent::ReviewLocal { .. },
                )
            );
        if shortcut_ancestry {
            let head_sha = match &event {
                crate::collab::CollabEvent::CodeReviewFixGlobal { head_sha } => head_sha,
                crate::collab::CollabEvent::ReviewLocal { head_sha } => head_sha,
                _ => unreachable!(),
            };
            validate_global_review_head_advance(
                &record.repo_path,
                session.last_head_sha.as_deref().ok_or_else(|| {
                    MemoryError::Validation(format!(
                        "last_head_sha is missing for {}",
                        session.phase
                    ))
                })?,
                head_sha,
            )?;
        }

        session = apply_event(&session, sender, &event).map_err(collab_error_to_memory_error)?;
        // Snapshot the post-event pr_url so the lifecycle writer can stamp it
        // on CodingComplete without an extra DB round-trip.
        let post_pr_url = session.pr_url.clone();
        let phase_after_enum = session.phase;
        crate::collab::queue::save_session(tx, &session)?;

        let message_id = crate::collab::queue::send_message(
            tx,
            session_id,
            sender.as_str(),
            other_agent(sender).as_str(),
            topic,
            content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_send",
            &json!({
                "session_id": session_id,
                "sender": sender.as_str(),
                "topic": topic,
                "phase_before": phase_before,
            }),
            Some(&json!({
                "message_id": message_id,
                "phase": session.phase.to_string(),
            })),
        )?;

        Ok((
            json!({
                "message_id": message_id,
                "phase": session.phase.to_string(),
            }),
            phase_before_enum,
            phase_after_enum,
            post_pr_url,
        ))
    })?;

    // Deliberately also set on terminal sends — terminal-but-not-ended sessions still attribute
    // (bucket 'other'); resolve() self-clears once ended_at is set.
    app.set_active_collab_session(session_id);
    record_task_outcome_transition(app, session_id, before, after, pr_url.as_deref());
    Ok(response)
}

fn validate_global_review_head_advance(
    repo_path: &str,
    last_head_sha: &str,
    head_sha: &str,
) -> Result<(), MemoryError> {
    let output = Command::new("git")
        .args([
            "-C",
            repo_path,
            "merge-base",
            "--is-ancestor",
            last_head_sha,
            head_sha,
        ])
        .output()
        .map_err(|err| {
            MemoryError::Validation(format!(
                "git ancestry validation failed: unable to execute git: {err}"
            ))
        })?;

    if output.status.success() {
        return Ok(());
    }

    if output.status.code() == Some(1) {
        return Err(MemoryError::Validation(format!(
            "branch_drift: head_sha {head_sha} is not a descendant of last_head_sha {last_head_sha}"
        )));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        format!("git exited with status {:?}", output.status.code())
    } else {
        stderr.to_string()
    };
    Err(MemoryError::Validation(format!(
        "git ancestry validation failed: {detail}"
    )))
}

pub(super) fn handle_collab_recv(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    ensure_no_conflicting_process_session(app, session_id)?;
    let receiver = require_agent(require_str(args, "receiver")?)?;
    let limit = (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize).min(50);
    let auto_ack = args
        .get("auto_ack")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let result = app.db.with_transaction(|tx| {
        // Blind-drafts invariant: during PlanParallelDrafts, an agent must not
        // see the counterpart's draft until it has submitted its own. This
        // enforces the "parallel" in parallel drafts at the server boundary so
        // the protocol doesn't rely on agent-side discipline alone.
        let session = crate::collab::queue::load_session(tx, session_id)?;
        let suppress_drafts = matches!(session.phase, crate::collab::Phase::PlanParallelDrafts)
            && match receiver {
                Agent::Claude => session.claude_draft_hash.is_none(),
                Agent::Codex => session.codex_draft_hash.is_none(),
            };

        let messages =
            crate::collab::queue::recv_messages(tx, session_id, receiver.as_str(), limit)?;
        let filtered: Vec<_> = messages
            .into_iter()
            .filter(|message| !(suppress_drafts && message.topic == "draft"))
            .collect();

        if auto_ack && !filtered.is_empty() {
            let ids: Vec<String> = filtered.iter().map(|m| m.id.clone()).collect();
            crate::collab::queue::ack_messages_many(tx, session_id, &ids)?;
        }

        let json_messages: Vec<Value> = filtered
            .iter()
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
        Ok(json!({ "messages": json_messages }))
    })?;
    app.set_active_collab_session(session_id);
    Ok(result)
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
    if agent != Agent::Codex {
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
            Agent::Codex,
            &CollabEvent::SubmitReview {
                verdict: "approve".to_string(),
            },
        )
        .map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;
        let _ = crate::collab::queue::send_message(
            tx,
            session_id,
            Agent::Codex.as_str(),
            Agent::Claude.as_str(),
            "review",
            &review_content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_approve",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "content_hash": content_hash,
            }),
            Some(&json!({ "phase": session.phase.to_string() })),
        )?;
        Ok(json!({ "phase": session.phase.to_string() }))
    })
}

pub(super) fn handle_collab_wait_my_turn(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    ensure_no_conflicting_process_session(app, session_id)?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS)
        .clamp(1, WAIT_MY_TURN_MAX_TIMEOUT_SECS);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_millis(WAIT_MY_TURN_POLL_MS);
    let mut cell_set = false;

    loop {
        let record = app.db.collab_load_session_record(session_id)?;
        if !cell_set {
            app.set_active_collab_session(session_id);
            cell_set = true;
        }
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

    let phase = app.db.with_transaction(|tx| {
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
        let ended_phase = session.phase;
        crate::collab::queue::end_session(tx, session_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_end",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "phase": ended_phase.to_string(),
            }),
            Some(&json!({ "ok": true })),
        )?;
        Ok(ended_phase)
    })?;

    // Operator attestation (METRICS_SPEC §12 amendment): the operator ends a
    // CodingComplete session after the PR lands, or abandons a PlanLocked one.
    let now = crate::metrics::now_rfc3339();
    let attested = match phase {
        Phase::CodingComplete => {
            app.db
                .mark_task_outcome_done(session_id, None, Some("merged"), None)
        }
        Phase::PlanLocked => {
            app.db
                .mark_task_outcome_done(session_id, Some(&now), Some("abandoned"), None)
        }
        // CodingFailed: failure_report already wrote 'failed' — no write here.
        _ => Ok(()),
    };
    if let Err(e) = attested {
        tracing::warn!("metrics: task_outcome end attestation failed: {e}");
    }
    if app.active_collab_session_snapshot().as_deref() == Some(session_id) {
        app.clear_active_collab_session();
    }

    Ok(json!({ "ok": true, "session_id": session_id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::queue::SessionRecord;
    use crate::collab::CollabSession;
    use std::sync::Arc;

    // ── test helpers ──────────────────────────────────────────────────────────

    fn test_app() -> Arc<crate::mcp::app::App> {
        use crate::config::{Config, EmbedMode, McpAccessMode};
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        // Leak the tempdir so the DB file outlives this helper.
        std::mem::forget(dir);
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(crate::mcp::app::App::new(config).unwrap())
    }

    fn start_session(app: &crate::mcp::app::App) -> String {
        let args = json!({
            "repo_path": "/tmp/repo", "branch": "main",
            "initiator": "claude", "task": "lifecycle test", "implementer": "claude",
        });
        let out = handle_collab_start(app, &args).unwrap();
        out["session_id"].as_str().unwrap().to_string()
    }

    fn send(
        app: &crate::mcp::app::App,
        sid: &str,
        sender: &str,
        topic: &str,
        content: &str,
    ) -> Value {
        handle_collab_send(
            app,
            &json!({ "session_id": sid, "sender": sender, "topic": topic, "content": content }),
        )
        .unwrap()
    }

    /// Drive v1 planning to PlanLocked and return the final_plan_hash that
    /// must be used in the subsequent task_list payload.
    fn drive_to_plan_locked(app: &crate::mcp::app::App, sid: &str) -> String {
        let plan_text = "final plan";
        let final_plan_hash = super::super::shared::sha256_hex(plan_text);
        send(app, sid, "claude", "draft", "claude draft");
        send(app, sid, "codex", "draft", "codex draft");
        send(app, sid, "claude", "canonical", "canonical plan");
        send(app, sid, "codex", "review", r#"{"verdict":"approve"}"#);
        send(
            app,
            sid,
            "claude",
            "final",
            &format!(r#"{{"plan":"{plan_text}"}}"#),
        );
        final_plan_hash
    }

    /// Drive to CodeImplementPending and return the final_plan_hash.
    fn drive_to_implement(app: &crate::mcp::app::App, sid: &str) -> String {
        let hash = drive_to_plan_locked(app, sid);
        let task_list_content = format!(
            r#"{{"plan_hash":"{hash}","base_sha":"b","head_sha":"b","tasks":[{{"id":1,"title":"t","acceptance":["a"]}}]}}"#
        );
        send(app, sid, "claude", "task_list", &task_list_content);
        hash
    }

    // ── lifecycle tests ───────────────────────────────────────────────────────

    #[test]
    fn collab_start_creates_task_outcome_row_and_sets_active_cell() {
        let app = test_app();
        let sid = start_session(&app);
        let row = app.db.get_task_outcome(&sid).unwrap().expect("row created");
        assert_eq!(row.collab_session_id.as_deref(), Some(sid.as_str()));
        assert!(row.started_at.is_some());
        assert!(
            row.done_at.is_none() && row.outcome.is_none() && row.pr_url.is_none(),
            "fresh row must have no terminal fields set"
        );
        assert_eq!(
            (row.review_rounds, row.fix_commits, row.handoffs),
            (0, 0, 0)
        );
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(sid.as_str())
        );
    }

    #[test]
    fn full_v3_happy_path_yields_review_round_done_at_pr_url_then_merged_on_end() {
        let app = test_app();
        let sid = start_session(&app);

        // v1 planning → PlanLocked, then v3 → CodeImplementPending.
        drive_to_implement(&app, &sid);

        // CodeImplementPending(impl) → CodeReviewFixGlobalPending(rework): no increment yet.
        send(
            &app,
            &sid,
            "claude",
            "implementation_done",
            r#"{"head_sha":"c1"}"#,
        );
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(row.review_rounds, 0, "impl→rework must NOT increment");

        // CodeReviewFixGlobalPending(rework) → CodeReviewLocalPending(review): +1.
        send(
            &app,
            &sid,
            "codex",
            "review_fix_global",
            r#"{"head_sha":"c2"}"#,
        );
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(row.review_rounds, 1, "rework→review entry increments once");

        // CodeReviewLocalPending(review) → CodeReviewFinalPending(review): must NOT increment.
        send(&app, &sid, "claude", "review_local", r#"{"head_sha":"c3"}"#);
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.review_rounds, 1,
            "review→review (Local→Final) must NOT increment"
        );
        assert!(row.done_at.is_none(), "session not yet complete");

        // CodeReviewFinalPending(review) → CodingComplete: sets done_at + pr_url.
        send(
            &app,
            &sid,
            "claude",
            "final_review",
            r#"{"head_sha":"c4","pr_url":"https://github.com/x/y/pull/9"}"#,
        );
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert!(row.done_at.is_some(), "CodingComplete sets done_at");
        assert_eq!(row.pr_url.as_deref(), Some("https://github.com/x/y/pull/9"));
        assert!(
            row.outcome.is_none(),
            "outcome must stay NULL until operator attestation"
        );

        // collab_end from CodingComplete → operator attests "merged".
        handle_collab_end(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome.as_deref(),
            Some("merged"),
            "collab_end from CodingComplete attests merged"
        );
    }

    #[test]
    fn failure_report_marks_outcome_failed() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        // Send failure_report from CodeImplementPending.
        send(
            &app,
            &sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"subagent_failure: 1: env"}"#,
        );
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome.as_deref(),
            Some("failed"),
            "failure_report must set outcome=failed"
        );
        assert!(row.done_at.is_some(), "failure_report must set done_at");
    }

    #[test]
    fn collab_end_from_planlocked_marks_abandoned() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_plan_locked(&app, &sid);

        handle_collab_end(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome.as_deref(),
            Some("abandoned"),
            "collab_end from PlanLocked must mark abandoned"
        );
        assert!(row.done_at.is_some(), "abandoned must set done_at");
    }

    #[test]
    fn recv_refreshes_active_cell_but_status_does_not() {
        let app = test_app();
        let sid = start_session(&app);
        app.clear_active_collab_session();
        // status must NOT steal attribution
        handle_collab_status(&app, &json!({"session_id": sid})).unwrap();
        assert!(
            app.active_collab_session_snapshot().is_none(),
            "status must not steal attribution"
        );
        // recv must set it
        handle_collab_recv(&app, &json!({"session_id": sid, "receiver": "claude"})).unwrap();
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(sid.as_str())
        );
    }

    #[test]
    fn wait_my_turn_refreshes_active_cell() {
        let app = test_app();
        let sid = start_session(&app);
        app.clear_active_collab_session();

        let wait = handle_collab_wait_my_turn(
            &app,
            &json!({"session_id": sid, "agent": "claude", "timeout_secs": 1}),
        )
        .unwrap();

        assert_eq!(wait["is_my_turn"], true);
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(sid.as_str())
        );
    }

    #[test]
    fn second_live_session_cannot_steal_process_attribution_slot() {
        let app = test_app();
        let first = start_session(&app);

        let err = handle_collab_start(
            &app,
            &json!({
                "repo_path": "/tmp/repo",
                "branch": "other-branch",
                "initiator": "claude",
                "task": "second live session",
                "implementer": "claude",
            }),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("already bound to this MCP process"),
            "unexpected error: {err}"
        );
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(first.as_str())
        );
    }

    // ── execution_mode_from_task_list ─────────────────────────────────────────

    #[test]
    fn execution_mode_from_task_list_returns_none_when_absent() {
        let raw = r#"{"plan_hash":"h","base_sha":"b","head_sha":"x","tasks":[]}"#;
        assert_eq!(execution_mode_from_task_list(Some(raw)), None);
    }

    #[test]
    fn execution_mode_from_task_list_returns_value_when_present() {
        let raw = r#"{"plan_hash":"h","base_sha":"b","head_sha":"x","execution_mode":"mechanical_direct","tasks":[]}"#;
        assert_eq!(
            execution_mode_from_task_list(Some(raw)),
            Some("mechanical_direct".to_string())
        );
    }

    #[test]
    fn execution_mode_from_task_list_returns_none_for_null_task_list() {
        assert_eq!(execution_mode_from_task_list(None), None);
    }

    // ── session_record_json exposes execution_mode ────────────────────────────

    fn make_record(task_list: Option<&str>) -> SessionRecord {
        let mut session = CollabSession::new("test-session");
        session.task_list = task_list.map(str::to_string);
        SessionRecord {
            session,
            repo_path: "/tmp/repo".to_string(),
            branch: "main".to_string(),
            task: None,
            ended_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn collab_status_returns_execution_mode_when_set() {
        let task_list_json = r#"{"plan_hash":"h","base_sha":"b","head_sha":"x","execution_mode":"mechanical_direct","tasks":[{"id":1,"title":"t","acceptance":["ok"]}]}"#;
        let record = make_record(Some(task_list_json));
        let status = session_record_json(&record);
        assert_eq!(
            status["execution_mode"].as_str(),
            Some("mechanical_direct"),
            "collab_status must surface execution_mode from canonicalized task_list"
        );
    }

    #[test]
    fn collab_status_returns_null_execution_mode_when_omitted() {
        let task_list_json = r#"{"plan_hash":"h","base_sha":"b","head_sha":"x","tasks":[{"id":1,"title":"t","acceptance":["ok"]}]}"#;
        let record = make_record(Some(task_list_json));
        let status = session_record_json(&record);
        assert!(
            status["execution_mode"].is_null(),
            "collab_status must return null execution_mode when field is absent from task_list"
        );
    }

    #[test]
    fn collab_status_returns_null_execution_mode_when_no_task_list() {
        let record = make_record(None);
        let status = session_record_json(&record);
        assert!(
            status["execution_mode"].is_null(),
            "collab_status must return null execution_mode when task_list is not yet set"
        );
    }
}
