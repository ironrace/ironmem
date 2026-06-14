use serde_json::{json, Value};
use std::process::Command;

use crate::collab::queue::SessionRecord;
use crate::collab::{
    apply_event, start_global_review_session, Agent, CollabError, CollabEvent, Phase,
};
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

use super::collab_events::{
    build_collab_event, failure_report_is_off_turn_admissible, parse_final_payload,
};
use super::shared::{
    other_agent, require_agent, require_implementer, require_str, MAX_COLLAB_CONTENT_CHARS,
};

/// Wing/room under which accepted plan bodies are filed as drawers. Runtime
/// collab paths dereference these drawers by id; the dedicated room keeps them
/// auditable/filterable even though the generic drawer FTS index still sees
/// their content.
const COLLAB_PLAN_WING: &str = "ironrace-memory";
const COLLAB_PLAN_ROOM: &str = "collab-plans";

pub(super) fn collab_error_to_memory_error(error: CollabError) -> MemoryError {
    MemoryError::Validation(error.to_string())
}

/// Store an accepted plan body as a `collab-plans` drawer and return its
/// deterministic 32-char id. `topic` must be `"canonical"` (body = raw
/// content) or `"final"` (body = parsed plan text, so `final_plan_hash` —
/// sha256 of the parsed text — verifies the stored body); any other topic is
/// rejected loudly rather than silently filed. These drawers are dereferenced
/// by id and are not intended for recall, so they carry a full zero vector
/// rather than an empty slice: `insert_drawer_tx` does not validate embedding
/// length, but the HNSW index-load path (`load_all_vectors` in `db/schema.rs`)
/// skips — with a per-row warning — any drawer whose embedding length is not
/// `EMBED_DIM`. A zero vector stays loadable (contributing no vector signal)
/// and avoids that warn/skip. Note the generic drawer FTS index still sees
/// their content (see the `COLLAB_PLAN_ROOM` comment).
fn store_collab_plan_drawer(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    topic: &str,
    content: &str,
) -> Result<String, MemoryError> {
    use ironrace_embed::embedder::EMBED_DIM;
    let body = match topic {
        "canonical" => content.to_string(),
        "final" => parse_final_payload(content)?,
        other => {
            return Err(MemoryError::Validation(format!(
                "store_collab_plan_drawer: unexpected topic {other:?} (want canonical|final)"
            )))
        }
    };
    let id = crate::db::drawers::generate_id(&body, COLLAB_PLAN_WING, COLLAB_PLAN_ROOM);
    let zero = vec![0.0f32; EMBED_DIM];
    crate::db::schema::Database::insert_drawer_tx(
        tx,
        &id,
        &body,
        &zero,
        COLLAB_PLAN_WING,
        COLLAB_PLAN_ROOM,
        &format!("collab:{session_id}:{topic}"),
        "collab",
    )?;
    Ok(id)
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
        "canonical_plan_drawer_id": record.session.canonical_plan_drawer_id.as_deref(),
        "final_plan_drawer_id": record.session.final_plan_drawer_id.as_deref(),
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
    if !crate::search::tunables::metrics_enabled() {
        return;
    }
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
        tracing::warn!(session_id = %session_id, error = %e, "metrics: task_outcomes create failed");
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
    if !crate::search::tunables::metrics_enabled() {
        return;
    }
    if before == after {
        return;
    }
    let entered_review = crate::metrics::phase_bucket(after) == "review"
        && matches!(crate::metrics::phase_bucket(before), "impl" | "rework");
    if entered_review {
        if let Err(e) = app.db.increment_task_review_rounds(session_id) {
            tracing::warn!(session_id = %session_id, error = %e, "metrics: review_rounds increment failed");
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
        tracing::warn!(
            session_id = %session_id,
            before = %before,
            after = %after,
            error = %e,
            "metrics: task_outcome terminal update failed"
        );
    }
}

/// Shared decision logic for the process-attribution conflict guard.
///
/// Invariant: one live collab session may own the process attribution slot at a time
/// (any repo). Stale or missing sessions self-heal by clearing the cell. Returns
/// `Ok(())` when the cell should be cleared (stale/missing); the live-session
/// arm returns `Err` directly. The guard protects correctness whenever metrics
/// get re-enabled — the conflict check is NOT gated on IRONMEM_METRICS.
///
/// A turn is refused rather than risking ambiguous attribution; the raw DB error
/// detail lives in the server log, not in the MCP response.
fn check_conflicting_session(
    load_result: Result<crate::collab::queue::SessionRecord, MemoryError>,
    active_session_id: &str,
    requested_session_id: &str,
) -> Result<(), MemoryError> {
    match load_result {
        Ok(record) if record.ended_at.is_none() => Err(MemoryError::Validation(format!(
            "another active collab session is already bound to this MCP process for metrics attribution: {active_session_id}. End it or use a separate server process before switching to {requested_session_id}."
        ))),
        // `NotFound` is what the session loader returns for a missing row — matched
        // explicitly so only a confirmed-missing session clears the cell; any new
        // error variant lands in the warn arm below instead of being mistaken for
        // a missing session.
        Err(crate::error::MemoryError::NotFound(_)) => {
            tracing::warn!(
                session_id = %active_session_id,
                "metrics attribution: active collab session not found — clearing cell for new session"
            );
            Ok(())
        }
        Ok(_) => {
            // Session ended — self-heal silently.
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                session_id = %active_session_id,
                error = %e,
                "could not verify active collab session before switching metrics attribution"
            );
            // Refuse the turn rather than risk ambiguous attribution;
            // detail is in the server log above.
            Err(MemoryError::Validation(format!(
                "could not verify active collab session {active_session_id} before switching \
                 metrics attribution to {requested_session_id}"
            )))
        }
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

    let load_result = app.db.collab_load_session_record(&active_session_id);
    check_conflicting_session(load_result, &active_session_id, requested_session_id)?;
    app.clear_active_collab_session();
    Ok(())
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

    let load_result = crate::collab::queue::load_session_record(tx, &active_session_id);
    check_conflicting_session(load_result, &active_session_id, requested_session_id)?;
    app.clear_active_collab_session();
    Ok(())
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
        // Persist the accepted plan body by reference. `apply_event` only
        // records the content hash; here we file the body itself as a
        // `collab-plans` drawer and stamp its id on the session so later
        // phases can dereference the full plan text without re-sending it.
        if topic == "canonical" {
            session.canonical_plan_drawer_id =
                Some(store_collab_plan_drawer(tx, session_id, topic, content)?);
        } else if topic == "final" {
            session.final_plan_drawer_id =
                Some(store_collab_plan_drawer(tx, session_id, topic, content)?);
        }
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

/// Build the compact plan reference object surfaced by `collab_status` when an
/// accepted plan body is filed as a drawer (post-009 sessions). The full body
/// is dereferenced by `drawer_id` only on `verbose:true`; here we expose a
/// fixed-size peek so a joining agent can recognize the plan without paying for
/// the whole body.
fn plan_ref_json(drawer_id: &str, hash: Option<&str>, body: &str) -> Value {
    json!({
        "drawer_id": drawer_id,
        "hash": hash,
        // char-boundary safe: take 200 CHARS, not bytes.
        "first_200_chars": body.chars().take(200).collect::<String>(),
    })
}

/// Render one accepted plan (`canonical` or `final`) into `status`. Post-009
/// sessions carry a `drawer_id`: we emit a compact `<kind>_plan_ref` and, only
/// under `verbose`, inline the full `<kind>_plan` body. Pre-009 sessions have a
/// NULL drawer id; we preserve the historical inline-from-messages behavior.
fn render_plan(
    db: &crate::db::schema::Database,
    status: &mut Value,
    session_id: &str,
    kind: &str, // "canonical" | "final"
    drawer_id: Option<&str>,
    hash: Option<&str>,
    verbose: bool,
) -> Result<(), MemoryError> {
    let ref_key = format!("{kind}_plan_ref");
    let body_key = format!("{kind}_plan");
    match drawer_id {
        Some(id) => {
            let drawer = db.get_drawer(id)?.ok_or_else(|| {
                MemoryError::Validation(format!(
                    "{kind}_plan_drawer_id {id} points to a missing drawer"
                ))
            })?;
            status[ref_key] = plan_ref_json(id, hash, &drawer.content);
            if verbose {
                status[body_key] = Value::String(drawer.content);
            }
        }
        None => {
            // Legacy (pre-009): drawer id NULL. Inline full body from messages
            // when the plan hash is present. Canonical was always raw text;
            // final was stored as {"plan": "..."} but is normalized here so
            // collab_status consistently exposes final_plan as plan text.
            // The `parse_final_payload` below cannot fail on a persisted final
            // message: `build_collab_event` (at the top of `handle_collab_send`,
            // before `send_message` persists the raw content) already ran
            // `build_v1_final_event` → `parse_final_payload`, so any stored
            // `final` is a well-formed {"plan":...} envelope.
            if hash.is_some() {
                match db.collab_latest_message_content(session_id, kind)? {
                    Some(content) => {
                        let body = if kind == "final" {
                            parse_final_payload(&content)?
                        } else {
                            content
                        };
                        status[body_key] = Value::String(body);
                    }
                    None => {
                        // A locked plan hash with no backing message is an
                        // out-of-band inconsistency (the message is written in
                        // the same tx as the hash). Surface it rather than
                        // silently omitting the body so it is observable.
                        tracing::warn!(
                            session_id = %session_id,
                            kind = %kind,
                            "collab_status: {kind}_plan_hash set but no {kind} message found"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn handle_collab_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let record = app.db.collab_load_session_record(session_id)?;
    let mut status = session_record_json(&record);
    // Surface the locked plan alongside the hashes so a fresh agent joining
    // mid-session can build a task_list (or continue a review round) without
    // re-deriving content it previously sent but already acked off its inbox.
    // By default this is a compact reference (drawer id + hash + 200-char peek);
    // the full body is inlined only on `verbose:true`.
    let verbose = args
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    render_plan(
        &app.db,
        &mut status,
        session_id,
        "canonical",
        record.session.canonical_plan_drawer_id.as_deref(),
        record.session.canonical_plan_hash.as_deref(),
        verbose,
    )?;
    render_plan(
        &app.db,
        &mut status,
        session_id,
        "final",
        record.session.final_plan_drawer_id.as_deref(),
        record.session.final_plan_hash.as_deref(),
        verbose,
    )?;
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
    if crate::search::tunables::metrics_enabled() {
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
            tracing::warn!(session_id = %session_id, error = %e, "metrics: task_outcome end attestation failed");
        }
    }
    if app.active_collab_session_snapshot().as_deref() == Some(session_id) {
        app.clear_active_collab_session();
        // Leaving a *different* session's cell intact is intentional: that session still owns the slot.
    }

    Ok(json!({ "ok": true, "session_id": session_id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::queue::SessionRecord;
    use crate::collab::CollabSession;
    use std::path::{Path, PathBuf};
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

    fn test_app_with_db_path(db_path: PathBuf, root: &Path) -> Arc<crate::mcp::app::App> {
        use crate::config::{Config, EmbedMode, McpAccessMode};
        let config = Config {
            db_path,
            model_dir: root.join("model"),
            model_dir_explicit: true,
            state_dir: root.join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
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
            &json!({ "plan": plan_text }).to_string(),
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

    /// Serialize on the shared metrics env lock and force metrics ON. The
    /// lifecycle writes are gated on `IRONMEM_METRICS`, so any test that
    /// asserts a `task_outcomes` row exists races a parallel test flipping
    /// the kill switch (the suppression test below and the `mcp::server`
    /// metrics tests) unless it holds the same lock.
    fn metrics_on_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        guard
    }

    #[test]
    fn collab_start_creates_task_outcome_row_and_sets_active_cell() {
        let _g = metrics_on_guard();
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
        let _g = metrics_on_guard();
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
        let _g = metrics_on_guard();
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
        let _g = metrics_on_guard();
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

    // ── A: kill-switch gating ─────────────────────────────────────────────────

    #[test]
    fn metrics_kill_switch_suppresses_task_outcomes_row_on_collab_start() {
        // IRONMEM_METRICS=0 → collab_start must NOT create any task_outcomes row.
        let _g = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_METRICS", "0");

        let app = test_app();
        let sid = start_session(&app);

        // The session must exist (protocol state) …
        let record = app.db.collab_load_session_record(&sid);
        assert!(
            record.is_ok(),
            "session must be created regardless of metrics kill switch"
        );

        // … but no task_outcomes row must have been written.
        let row = app.db.get_task_outcome(&sid).unwrap();
        assert!(
            row.is_none(),
            "IRONMEM_METRICS=0 must suppress task_outcomes row creation"
        );

        std::env::remove_var("IRONMEM_METRICS");
    }

    // ── G.1: send/recv to second live session rejected, cell unchanged ─────────

    #[test]
    fn send_to_second_live_session_is_rejected_and_cell_unchanged() {
        let app = test_app();
        let first = start_session(&app);

        // Seed a second session directly via queue (simulating another process).
        let second = "second-session-id";
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    second,
                    "/tmp/other",
                    "other-branch",
                    None,
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();

        // collab_send to the second session must be rejected.
        let err = handle_collab_send(
            &app,
            &json!({
                "session_id": second,
                "sender": "claude",
                "topic": "draft",
                "content": "a valid draft payload",
            }),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("already bound to this MCP process"),
            "expected conflict error, got: {err}"
        );
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(first.as_str()),
            "cell must still hold the first session after rejected send"
        );

        // collab_recv to the second session must also be rejected.
        let err = handle_collab_recv(
            &app,
            &json!({
                "session_id": second,
                "receiver": "codex",
            }),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("already bound to this MCP process"),
            "expected conflict error on recv, got: {err}"
        );
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(first.as_str()),
            "cell must still hold first session after rejected recv"
        );
    }

    // ── G.2: start self-heals when cell holds an ended session ────────────────

    #[test]
    fn start_self_heals_when_cell_holds_ended_session() {
        let app = test_app();
        let first = start_session(&app);

        // End the first session directly (simulates cross-process end; cell still holds it).
        app.db
            .with_transaction(|tx| crate::collab::queue::end_session(tx, &first))
            .unwrap();
        // Cell still points to the ended session.
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(first.as_str())
        );

        // Starting a new session on a different repo/branch must succeed.
        let result = handle_collab_start(
            &app,
            &json!({
                "repo_path": "/tmp/new-repo",
                "branch": "new-branch",
                "initiator": "claude",
                "task": "new task",
                "implementer": "claude",
            }),
        );
        assert!(
            result.is_ok(),
            "collab_start must self-heal ended session in cell: {:?}",
            result.unwrap_err()
        );
        let new_sid = result.unwrap()["session_id"].as_str().unwrap().to_string();
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(new_sid.as_str()),
            "cell must be rebound to new session after self-heal"
        );
    }

    // ── G.3: start self-heals when cell holds a missing session ──────────────

    #[test]
    fn start_self_heals_when_cell_holds_missing_session() {
        let app = test_app();
        app.set_active_collab_session("ghost-session-id");

        let result = handle_collab_start(
            &app,
            &json!({
                "repo_path": "/tmp/repo",
                "branch": "main",
                "initiator": "claude",
                "task": "task after ghost",
                "implementer": "claude",
            }),
        );
        assert!(
            result.is_ok(),
            "collab_start must self-heal ghost session in cell: {:?}",
            result.unwrap_err()
        );
        let new_sid = result.unwrap()["session_id"].as_str().unwrap().to_string();
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(new_sid.as_str()),
            "cell must be rebound to new session after ghost self-heal"
        );
    }

    // ── G.4: collab_end from CodingFailed leaves outcome 'failed' ─────────────

    #[test]
    fn failure_report_marks_outcome_failed_and_end_does_not_overwrite() {
        let _g = metrics_on_guard();
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

        // Now call collab_end from CodingFailed — outcome must remain 'failed'.
        handle_collab_end(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome.as_deref(),
            Some("failed"),
            "collab_end from CodingFailed must NOT overwrite outcome to merged"
        );
    }

    // ── G.5: collab_end for other session leaves cell intact ──────────────────

    #[test]
    fn end_of_other_session_leaves_cell_intact() {
        let app = test_app();
        let first = start_session(&app);

        // Create a second session and drive it to PlanLocked (an endable phase).
        let second_id = uuid::Uuid::new_v4().to_string();
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    &second_id,
                    "/tmp/other-repo",
                    "other-branch",
                    None,
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
        // Drive second to PlanLocked by saving the phase directly.
        app.db
            .with_transaction(|tx| {
                let mut s = crate::collab::queue::load_session(tx, &second_id)?;
                s.phase = crate::collab::Phase::PlanLocked;
                // Set a dummy final_plan_hash so it's a valid PlanLocked state.
                s.final_plan_hash = Some("dummy-hash".to_string());
                crate::collab::queue::save_session(tx, &s)
            })
            .unwrap();

        // End the second session — cell holds first, must remain first.
        handle_collab_end(&app, &json!({"session_id": second_id, "agent": "claude"})).unwrap();
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(first.as_str()),
            "ending a different session must not clear the cell"
        );
    }

    // ── plan-by-reference (#90) drawer-persistence tests ──────────────────────

    #[test]
    fn canonical_send_stores_collab_plan_drawer() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "CANONICAL BODY");

        let record = app.db.collab_load_session_record(&sid).unwrap();
        let drawer_id = record
            .session
            .canonical_plan_drawer_id
            .expect("canonical_plan_drawer_id must be set after canonical send");
        assert_eq!(
            drawer_id.len(),
            32,
            "drawer id must be the 32-char deterministic id"
        );

        let drawer = app
            .db
            .get_drawer(&drawer_id)
            .unwrap()
            .expect("stored plan drawer must be fetchable by id");
        assert_eq!(drawer.content, "CANONICAL BODY");
        assert_eq!(drawer.room, "collab-plans");
    }

    #[test]
    fn final_send_stores_parsed_plan_body_drawer() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "canonical plan");
        send(&app, &sid, "codex", "review", r#"{"verdict":"approve"}"#);
        send(
            &app,
            &sid,
            "claude",
            "final",
            r#"{"plan":"FINAL BODY TEXT"}"#,
        );

        let record = app.db.collab_load_session_record(&sid).unwrap();
        let drawer_id = record
            .session
            .final_plan_drawer_id
            .expect("final_plan_drawer_id must be set after final send");

        let drawer = app
            .db
            .get_drawer(&drawer_id)
            .unwrap()
            .expect("stored final plan drawer must be fetchable by id");
        // Body must be the PARSED plan text, not the JSON wrapper, so the
        // sha256(final_plan_hash) verifies against the stored body.
        assert_eq!(drawer.content, "FINAL BODY TEXT");
        assert_eq!(drawer.room, "collab-plans");
    }

    #[test]
    fn final_drawer_body_hashes_to_final_plan_hash() {
        // The central correctness claim: the stored final drawer body is the
        // parsed plan text, so its sha256 equals final_plan_hash. This pins the
        // contract that `build_v1_final_event` (hash) and
        // `store_collab_plan_drawer` (body) parse identically.
        let app = test_app();
        let sid = start_session(&app);
        drive_to_final(&app, &sid, "canonical plan", "FINAL BODY TEXT");

        let record = app.db.collab_load_session_record(&sid).unwrap();
        let drawer_id = record.session.final_plan_drawer_id.clone().unwrap();
        let final_plan_hash = record.session.final_plan_hash.clone().unwrap();
        let drawer = app.db.get_drawer(&drawer_id).unwrap().unwrap();
        assert_eq!(
            super::super::shared::sha256_hex(&drawer.content),
            final_plan_hash,
            "sha256(final drawer body) must equal final_plan_hash"
        );
    }

    #[test]
    fn revision_round_canonical_re_send_overwrites_drawer_id() {
        // request_changes returns to synthesis; a second, different canonical
        // body must re-stamp canonical_plan_drawer_id to the v2-derived id.
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "CANONICAL V1");
        send(
            &app,
            &sid,
            "codex",
            "review",
            r#"{"verdict":"request_changes"}"#,
        );
        send(&app, &sid, "claude", "canonical", "CANONICAL V2");

        let record = app.db.collab_load_session_record(&sid).unwrap();
        let id = record.session.canonical_plan_drawer_id.unwrap();
        let id_v1 =
            crate::db::drawers::generate_id("CANONICAL V1", "ironrace-memory", "collab-plans");
        let id_v2 =
            crate::db::drawers::generate_id("CANONICAL V2", "ironrace-memory", "collab-plans");
        assert_eq!(id, id_v2, "drawer id must point at the v2 body");
        assert_ne!(id, id_v1);
        assert_eq!(
            app.db.get_drawer(&id).unwrap().unwrap().content,
            "CANONICAL V2"
        );
    }

    #[test]
    fn store_collab_plan_drawer_rejects_unexpected_topic() {
        // The defensive `other =>` arm must fail loudly, not silently file.
        let app = test_app();
        app.db
            .with_transaction(|tx| {
                let result = store_collab_plan_drawer(tx, "sess", "draft", "x");
                assert!(result.is_err(), "unexpected topic must error");
                let msg = format!("{:?}", result.err().unwrap());
                assert!(
                    msg.contains("unexpected topic"),
                    "error must name the unexpected-topic cause, got: {msg}"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn status_verbose_false_returns_compact_ref_not_body() {
        // Explicit verbose:false must behave like the default (compact ref).
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "d");
        send(&app, &sid, "codex", "draft", "d");
        send(&app, &sid, "claude", "canonical", "CANONICAL BODY");

        let status =
            handle_collab_status(&app, &json!({ "session_id": sid, "verbose": false })).unwrap();
        assert!(status["canonical_plan_ref"].is_object());
        assert!(
            status.get("canonical_plan").is_none(),
            "explicit verbose:false must omit the full body"
        );
    }

    #[test]
    fn canonical_drawer_id_is_deterministic_for_same_body() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "CANONICAL BODY");

        let record = app.db.collab_load_session_record(&sid).unwrap();
        let drawer_id = record
            .session
            .canonical_plan_drawer_id
            .expect("canonical_plan_drawer_id must be set");
        assert_eq!(
            drawer_id,
            crate::db::drawers::generate_id("CANONICAL BODY", "ironrace-memory", "collab-plans"),
        );
    }

    #[test]
    fn search_is_safe_after_plan_drawer_stored() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "CANONICAL BODY");

        // The zero-embedding plan drawer must not break the search pipeline.
        let filters = crate::db::drawers::SearchFilters {
            wing: None,
            room: None,
            limit: 10,
        };
        let result = crate::search::pipeline::search(&app, "canonical body", &filters);
        assert!(
            result.is_ok(),
            "search must not panic or error: {:?}",
            result.err()
        );
    }

    // ── plan-by-reference (#90) collab_status compact-reference tests ──────────

    /// Drive draft → draft → canonical(<body>) → review(approve) → final(<body>)
    /// so both canonical and final plan drawers are stored.
    fn drive_to_final(
        app: &crate::mcp::app::App,
        sid: &str,
        canonical_body: &str,
        final_body: &str,
    ) {
        send(app, sid, "claude", "draft", "claude draft");
        send(app, sid, "codex", "draft", "codex draft");
        send(app, sid, "claude", "canonical", canonical_body);
        send(app, sid, "codex", "review", r#"{"verdict":"approve"}"#);
        send(
            app,
            sid,
            "claude",
            "final",
            &json!({ "plan": final_body }).to_string(),
        );
    }

    #[test]
    fn status_default_returns_compact_canonical_ref_not_body() {
        let app = test_app();
        let sid = start_session(&app);
        let big = "X".repeat(20_000);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", &big);

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        let plan_ref = &status["canonical_plan_ref"];
        assert!(plan_ref.is_object(), "canonical_plan_ref must be an object");
        assert_eq!(
            plan_ref["drawer_id"].as_str().unwrap().len(),
            32,
            "drawer_id must be the 32-char deterministic id"
        );
        assert!(
            plan_ref["hash"].is_string(),
            "hash must be a string in the compact ref"
        );
        let first_200 = plan_ref["first_200_chars"].as_str().unwrap();
        assert!(
            first_200.chars().count() <= 200,
            "first_200_chars must be at most 200 chars"
        );
        assert!(
            status.get("canonical_plan").is_none(),
            "full canonical_plan body must be absent by default"
        );
        assert!(
            !serde_json::to_string(&status).unwrap().contains(&big),
            "full canonical body must not appear anywhere in default status"
        );
    }

    #[test]
    fn status_default_returns_compact_final_ref_not_body() {
        let app = test_app();
        let sid = start_session(&app);
        let big = "Y".repeat(20_000);
        drive_to_final(&app, &sid, "canonical plan", &big);

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        let plan_ref = &status["final_plan_ref"];
        assert!(plan_ref.is_object(), "final_plan_ref must be an object");
        assert_eq!(
            plan_ref["drawer_id"].as_str().unwrap().len(),
            32,
            "drawer_id must be the 32-char deterministic id"
        );
        assert!(
            plan_ref["hash"].is_string(),
            "hash must be a string in the compact ref"
        );
        let first_200 = plan_ref["first_200_chars"].as_str().unwrap();
        assert!(
            first_200.chars().count() <= 200,
            "first_200_chars must be at most 200 chars"
        );
        assert!(
            status.get("final_plan").is_none(),
            "full final_plan body must be absent by default"
        );
        assert!(
            !serde_json::to_string(&status).unwrap().contains(&big),
            "full final body must not appear anywhere in default status"
        );
    }

    #[test]
    fn status_verbose_returns_full_canonical_body() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "FULL CANONICAL");

        let status =
            handle_collab_status(&app, &json!({ "session_id": sid, "verbose": true })).unwrap();

        assert_eq!(
            status["canonical_plan"], "FULL CANONICAL",
            "verbose must inline the full canonical body"
        );
        assert!(
            status["canonical_plan_ref"].is_object(),
            "verbose must still include the compact reference"
        );
    }

    #[test]
    fn status_verbose_returns_full_final_body() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_final(&app, &sid, "canonical plan", "FULL FINAL");

        let status =
            handle_collab_status(&app, &json!({ "session_id": sid, "verbose": true })).unwrap();

        assert_eq!(
            status["final_plan"], "FULL FINAL",
            "verbose must inline the full final body"
        );
        assert!(
            status["final_plan_ref"].is_object(),
            "verbose must still include the compact final reference"
        );
    }

    #[test]
    fn status_verbose_plan_drawers_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let sid;

        {
            let app = test_app_with_db_path(db_path.clone(), dir.path());
            sid = start_session(&app);
            drive_to_final(&app, &sid, "REOPEN CANONICAL", "REOPEN FINAL");
            let status = handle_collab_status(&app, &json!({ "session_id": &sid })).unwrap();
            assert!(status["canonical_plan_ref"].is_object());
            assert!(status["final_plan_ref"].is_object());
        }

        {
            let app = test_app_with_db_path(db_path, dir.path());
            let status =
                handle_collab_status(&app, &json!({ "session_id": &sid, "verbose": true }))
                    .unwrap();
            assert_eq!(status["canonical_plan"], "REOPEN CANONICAL");
            assert_eq!(status["final_plan"], "REOPEN FINAL");
        }
    }

    #[test]
    fn status_legacy_null_drawer_inlines_full_body() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "LEGACY BODY");

        // Simulate a pre-009 session whose drawer id was never recorded.
        let mut s = app.db.collab_load_session(&sid).unwrap();
        s.canonical_plan_drawer_id = None;
        app.db.collab_save_session(&s).unwrap();

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        assert_eq!(
            status["canonical_plan"], "LEGACY BODY",
            "legacy NULL-drawer path must inline the full body from messages"
        );
        assert!(
            status.get("canonical_plan_ref").is_none(),
            "legacy path must not emit a compact reference"
        );
    }

    #[test]
    fn status_legacy_null_final_drawer_inlines_parsed_final_plan() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_final(&app, &sid, "canonical plan", "LEGACY FINAL");

        // Simulate a pre-009 session whose final drawer id was never recorded.
        let mut s = app.db.collab_load_session(&sid).unwrap();
        s.final_plan_drawer_id = None;
        app.db.collab_save_session(&s).unwrap();

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        assert_eq!(
            status["final_plan"], "LEGACY FINAL",
            "legacy NULL-drawer final path must normalize the raw final message to plan text"
        );
        assert!(
            status.get("final_plan_ref").is_none(),
            "legacy final path must not emit a compact reference"
        );
    }

    #[test]
    fn status_dangling_drawer_id_errors() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "BODY");

        // Point the session at a drawer id that does not exist.
        let mut s = app.db.collab_load_session(&sid).unwrap();
        s.canonical_plan_drawer_id = Some("0".repeat(32));
        app.db.collab_save_session(&s).unwrap();

        let result = handle_collab_status(&app, &json!({ "session_id": sid }));
        assert!(
            result.is_err(),
            "a dangling drawer id must surface as an error, not a silent empty ref"
        );
    }

    #[test]
    fn status_dangling_final_drawer_id_errors() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_final(&app, &sid, "canonical plan", "FINAL");

        // Point the session at a drawer id that does not exist.
        let mut s = app.db.collab_load_session(&sid).unwrap();
        s.final_plan_drawer_id = Some("1".repeat(32));
        app.db.collab_save_session(&s).unwrap();

        let result = handle_collab_status(&app, &json!({ "session_id": sid }));
        assert!(
            result.is_err(),
            "a dangling final drawer id must surface as an error, not a silent fallback"
        );
    }
}
