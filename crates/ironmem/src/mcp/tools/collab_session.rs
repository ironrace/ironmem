use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::process::Command;
use uuid::Uuid;

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
    collab_counterpart, require_agent, require_implementer, require_pilot, require_str, sha256_hex,
    MAX_COLLAB_CONTENT_CHARS,
};

/// Wing under which collaboration-owned drawer artifacts are filed. Runtime
/// collab paths dereference these drawers by id; each dedicated room keeps its
/// artifacts auditable/filterable even though the generic drawer FTS index
/// still sees their content.
const COLLAB_WING: &str = "ironrace-memory";
const COLLAB_PLAN_ROOM: &str = "collab-plans";
const COLLAB_TASK_LIST_ROOM: &str = "collab-task-lists";
const COLLAB_MESSAGE_ROOM: &str = "collab-messages";

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
    let id = crate::db::drawers::generate_id(&body, COLLAB_WING, COLLAB_PLAN_ROOM);
    let zero = vec![0.0f32; EMBED_DIM];
    crate::db::schema::Database::insert_drawer_tx(
        tx,
        &id,
        &body,
        &zero,
        COLLAB_WING,
        COLLAB_PLAN_ROOM,
        &format!("collab:{session_id}:{topic}"),
        "collab",
    )?;
    Ok(id)
}

/// Store the accepted, canonicalized task-list JSON as a drawer. Status
/// responses can then return a compact `task_list_ref` while flows that need the
/// full checklist can dereference the id deliberately.
fn store_collab_task_list_drawer(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    content: &str,
) -> Result<String, MemoryError> {
    use ironrace_embed::embedder::EMBED_DIM;
    let id = crate::db::drawers::generate_id(content, COLLAB_WING, COLLAB_TASK_LIST_ROOM);
    let zero = vec![0.0f32; EMBED_DIM];
    crate::db::schema::Database::insert_drawer_tx(
        tx,
        &id,
        content,
        &zero,
        COLLAB_WING,
        COLLAB_TASK_LIST_ROOM,
        &format!("collab:{session_id}:task_list"),
        "collab",
    )?;
    Ok(id)
}

/// Store an accepted collab message body as an immutable, opaque-reference
/// drawer. Queue rows retain the per-session delivery metadata; this drawer is
/// only the stable body reference shared by compact `collab_recv` responses.
///
/// The id must not be content-addressed: a client that can guess a message
/// body must not be able to derive a reference that bypasses `collab_recv`'s
/// restricted-mode redaction.
fn store_collab_message_drawer(
    tx: &rusqlite::Transaction<'_>,
    content: &str,
) -> Result<String, MemoryError> {
    use ironrace_embed::embedder::EMBED_DIM;
    let id = Uuid::new_v4().simple().to_string();
    let zero = vec![0.0f32; EMBED_DIM];
    crate::db::schema::Database::insert_drawer_tx(
        tx,
        &id,
        content,
        &zero,
        COLLAB_WING,
        COLLAB_MESSAGE_ROOM,
        "",
        "collab",
    )?;
    Ok(id)
}

fn task_list_ref_json(drawer_id: Option<&str>, body: Option<&str>) -> Option<Value> {
    let body = body?;
    Some(json!({
        "drawer_id": drawer_id,
        "hash": sha256_hex(body),
    }))
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
        "task_list_ref": task_list_ref_json(
            record.session.task_list_drawer_id.as_deref(),
            record.session.task_list.as_deref(),
        ),
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
        "pilot": record.session.pilot.as_str(),
        "task_review_round": record.session.task_review_round,
        "global_review_round": record.session.global_review_round,
        "base_sha": record.session.base_sha.as_deref(),
        "last_head_sha": record.session.last_head_sha.as_deref(),
        "pr_url": record.session.pr_url.as_deref(),
        "coding_failure": record.session.coding_failure.as_deref(),
        // Recovery-state exposure (issue #197 task 9). `pending_failure` is
        // the diagnostic for an in-flight *recoverable* failure — set only
        // by the `Tooling` arm of `apply_event`'s `FailureReport` handling
        // (state_machine/mod.rs), which never also sets `coding_failure`;
        // the two are mutually exclusive by construction, enforced there
        // and covered by `state_machine::tests`. `failed_from_phase`/
        // `recovery_phase` serialize via `Phase::to_string()` like the
        // top-level `phase` field.
        //
        // `recovery_origin_owner` and `total_recovery_attempts` were both
        // added by review. Without the origin, nothing on this surface
        // distinguishes a completion event sent by the phase's own expected
        // agent from one sent by a delegated recovery owner. Without the
        // lifetime counter, `recovery_attempts` alone can never read above
        // `MAX_RECOVERY_ATTEMPTS`, so a session looping through
        // `collab_resume` looks healthy from here no matter how many
        // handoffs it has actually burned.
        "pending_failure": record.session.pending_failure.as_deref(),
        "failed_from_phase": record.session.failed_from_phase.map(|p| p.to_string()),
        "recovery_phase": record.session.recovery_phase.map(|p| p.to_string()),
        "recovery_owner": record.session.recovery_owner.map(|a| a.as_str()),
        "recovery_origin_owner": record.session.recovery_origin_owner.map(|a| a.as_str()),
        "recovery_attempts": record.session.recovery_attempts,
        "total_recovery_attempts": record.session.total_recovery_attempts,
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
            | ORPHAN_RECOVERED_TOPIC
    )
}

/// Topic recording that a turn found a dirty worktree on the NORMAL path and
/// recovered work a previous turn left behind — evidence that the previous
/// turn died without reporting (OOM, container kill, sandbox teardown), since
/// a reported failure would have left `pending_failure` non-null.
///
/// Deliberately NOT a `failure_report`. A tooling failure parks the phase,
/// hands the turn to the counterpart, and spends `recovery_attempts` against
/// a ceiling of [`MAX_RECOVERY_ATTEMPTS`] — but the sender here has
/// *succeeded*: it preserved the orphaned work, ran the gates, and is carrying
/// its own turn to completion. Charging it a recovery attempt would let three
/// successful recoveries exhaust a session's lifetime budget.
///
/// So this topic is non-advancing: it is recorded and returns before any
/// `CollabEvent` is built, leaving phase, owner and both recovery counters
/// untouched. [`super::collab_events::build_collab_event`] has no arm for it
/// and must never gain one.
pub(super) const ORPHAN_RECOVERED_TOPIC: &str = "orphan_recovered";

/// Polling cadence for `collab_wait_my_turn`. Short enough that
/// turn transitions feel immediate, long enough that idle waits don't
/// hammer SQLite.
const WAIT_MY_TURN_POLL_MS: u64 = 500;
/// Default timeout (seconds) applied when the caller omits `timeout_secs`.
const WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard cap on `timeout_secs` — clients that want longer should re-poll.
const WAIT_MY_TURN_MAX_TIMEOUT_SECS: u64 = 60;

/// Actionable state captured after a wait's handoff/generation claim commits.
///
/// A later change to any field here settles the wait even when the requested
/// agent does not own the new phase. In particular, a Codex-owned
/// `CodeImplementPending` can advance to Codex-owned
/// `CodeReviewFixGlobalPending`; Claude must wake to dispatch the next prompt
/// rather than incorrectly treating the prior process's normal exit as a
/// silent failure. Recovery data is included because it changes how Claude
/// routes a delegated completion even when a phase remains parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitTurnBaseline {
    phase: String,
    current_owner: String,
    ended: bool,
    phase_is_terminal: bool,
    implementer: String,
    pilot: String,
    coding_failure: Option<String>,
    pending_failure: Option<String>,
    failed_from_phase: Option<String>,
    recovery_phase: Option<String>,
    recovery_owner: Option<String>,
    recovery_origin_owner: Option<String>,
    recovery_attempts: u8,
    total_recovery_attempts: u8,
}

/// Snapshot of session state read by `wait_my_turn` on each poll tick. Taken
/// in one `load_session_record` call so `task_list_submitted` and `phase` are
/// always from the same row — a concurrent `collab_send(task_list)` commit
/// cannot interleave into this view and produce an inconsistent terminal-set
/// decision. The returned status is stale-but-consistent: the next tick picks
/// up the new phase.
struct WaitTurnSnapshot {
    baseline: WaitTurnBaseline,
    is_my_turn: bool,
}

fn wait_turn_snapshot(record: &SessionRecord, agent: Agent) -> WaitTurnSnapshot {
    let ended = record.ended_at.is_some();
    // Dynamic terminal set, evaluated on a single snapshot: pre-task_list,
    // PlanLocked is terminal so v3 agents can exit cleanly after the plan
    // locks. Post-task_list the v3 coding phase is underway and the terminal
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
        baseline: WaitTurnBaseline {
            phase: record.session.phase.to_string(),
            current_owner: record.session.current_owner.to_string(),
            ended,
            phase_is_terminal,
            implementer: record.session.implementer.to_string(),
            pilot: record.session.pilot.to_string(),
            coding_failure: record.session.coding_failure.clone(),
            pending_failure: record.session.pending_failure.clone(),
            failed_from_phase: record
                .session
                .failed_from_phase
                .map(|phase| phase.to_string()),
            recovery_phase: record.session.recovery_phase.map(|phase| phase.to_string()),
            recovery_owner: record.session.recovery_owner.map(|agent| agent.to_string()),
            recovery_origin_owner: record
                .session
                .recovery_origin_owner
                .map(|agent| agent.to_string()),
            recovery_attempts: record.session.recovery_attempts,
            total_recovery_attempts: record.session.total_recovery_attempts,
        },
        is_my_turn,
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
/// Invariant: one live collab session may own a repository-and-branch
/// attribution scope at a time. Stale, missing, and slot-releasing sessions
/// self-heal by clearing only their matching scope. "Slot-releasing" is
/// `CodingComplete` alone ([`Phase::releases_start_slot`]) — a `CodingFailed`
/// session still holds its scope so a replayed start cannot strand its
/// recovery state. The guard protects correctness whenever metrics get
/// re-enabled — the conflict check is NOT gated on IRONMEM_METRICS.
///
/// A turn is refused rather than risking ambiguous attribution; the raw DB error
/// detail lives in the server log, not in the MCP response.
fn check_conflicting_session(
    load_result: Result<crate::collab::queue::SessionRecord, MemoryError>,
    active_session_id: &str,
    requested_session_id: &str,
) -> Result<(), MemoryError> {
    match load_result {
        Ok(record)
            if record.ended_at.is_none() && !record.session.phase.releases_start_slot() =>
        {
            Err(MemoryError::Validation(format!(
                "another active collab session is already bound to this repository and branch for metrics attribution: {active_session_id}. End it or use a different repository branch before switching to {requested_session_id}."
            )))
        }
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
    repo_path: &str,
    branch: &str,
) -> Result<(), MemoryError> {
    let Some(active_session_id) = app.active_collab_session_snapshot_for_scope(repo_path, branch)
    else {
        return Ok(());
    };
    if active_session_id == requested_session_id {
        return Ok(());
    }

    let load_result = app.db.collab_load_session_record(&active_session_id);
    check_conflicting_session(load_result, &active_session_id, requested_session_id)?;
    app.clear_active_collab_session_for_scope_if_matches(&active_session_id, repo_path, branch);
    Ok(())
}

fn ensure_no_conflicting_process_session_tx(
    app: &App,
    tx: &rusqlite::Transaction<'_>,
    requested_session_id: &str,
    repo_path: &str,
    branch: &str,
) -> Result<(), MemoryError> {
    let Some(active_session_id) = app.active_collab_session_snapshot_for_scope(repo_path, branch)
    else {
        return Ok(());
    };
    if active_session_id == requested_session_id {
        return Ok(());
    }

    let load_result = crate::collab::queue::load_session_record(tx, &active_session_id);
    check_conflicting_session(load_result, &active_session_id, requested_session_id)?;
    app.clear_active_collab_session_for_scope_if_matches(&active_session_id, repo_path, branch);
    Ok(())
}

fn scope_for_session(app: &App, session_id: &str) -> Result<(String, String), MemoryError> {
    let record = app.db.collab_load_session_record(session_id)?;
    Ok((record.repo_path, record.branch))
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
    // Optional `pilot` field: selects which agent leads v1 planning. Default
    // is `Agent::Claude` (historical flow). Resolved before `implementer` so
    // an omitted `implementer` can default to whichever pilot was chosen.
    //
    // Three cases: absent → default, present-string → validate, present-non-string → error.
    // Explicit `null` is treated as non-string and rejected.
    let pilot = match args.get("pilot") {
        None => Agent::Claude,
        Some(Value::String(s)) => require_pilot(s)?,
        Some(_) => {
            return Err(MemoryError::Validation(
                "pilot must be a string".to_string(),
            ))
        }
    };
    // Optional `implementer` field: routes the v3 batch implementation
    // phase. Defaults to the resolved `pilot` (so a `pilot=codex` caller who
    // omits `implementer` gets `implementer=codex` too). `Agent::Codex`
    // makes Codex the owner of `CodeImplementPending` and the only valid
    // sender of `implementation_done`. It can be rebound later through
    // `collab_set_implementer` while planning or implementation is active.
    //
    // Three cases: absent → default, present-string → validate, present-non-string → error.
    // Explicit `null` is treated as non-string and rejected.
    let implementer = match args.get("implementer") {
        None => pilot,
        Some(Value::String(s)) => require_implementer(s)?,
        Some(_) => {
            return Err(MemoryError::Validation(
                "implementer must be a string".to_string(),
            ))
        }
    };
    let session_id = uuid::Uuid::new_v4().to_string();

    app.db.with_transaction(|tx| {
        // Guard against accidental duplicate sessions on the same repo+branch
        // (e.g. a fired ScheduleWakeup replaying the `/collab start` entry
        // command while a session is still mid-flight, or after it reached
        // CodingFailed and is awaiting a resume). The check is atomic with the
        // insert inside this transaction. A session at CodingComplete
        // deliberately does NOT block the start — see
        // `find_active_session_by_repo_branch`.
        if let Some((existing_id, phase)) =
            crate::collab::queue::find_active_session_by_repo_branch(tx, repo_path, branch)?
        {
            return Err(MemoryError::Validation(format!(
                "an active collab session already exists for repo {repo_path} branch {branch}: \
                 {existing_id} (phase {phase}). Resume it with `/collab join {existing_id}`, or \
                 if it is finished call collab_end on it before starting a new session here."
            )));
        }
        ensure_no_conflicting_process_session_tx(app, tx, &session_id, repo_path, branch)?;
        crate::collab::queue::create_session(
            tx,
            &session_id,
            repo_path,
            branch,
            task,
            crate::collab::CollabRoles { pilot, implementer },
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
                "pilot": pilot.as_str(),
                "has_task": task.is_some(),
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    app.set_active_collab_session_for_scope(&session_id, repo_path, branch);
    create_initial_task_outcome(app, &session_id);

    Ok(json!({
        "session_id": session_id,
        "task": task,
        "implementer": implementer.as_str(),
        "pilot": pilot.as_str(),
    }))
}

/// Shared preamble for [`handle_collab_set_implementer`] and
/// [`handle_collab_set_pilot`]: bind/validate the actor generation, confirm
/// the session is still active, load its current record, and confirm the
/// caller is the session's current pilot — the only role either tool permits
/// to reassign a role. Must run inside the caller's write transaction (see
/// [`super::handoff::ensure_actor_generation_current`]'s own doc comment for
/// why).
///
/// Each caller supplies its own `unauthorized` closure so the rejection text
/// stays byte-for-byte what that specific tool has always returned. Both
/// tools perform the identical underlying check (caller must equal the
/// current pilot) but have always worded the rejection differently ("is not
/// the pilot" vs. "is the copilot") — unifying that wording would be an
/// observable behavior change, since the exact error text is part of each
/// tool's API.
///
/// **Caveat: authorization here is caller-asserted, not authenticated.** The
/// `agent` value comes from the caller's own claim, not from any
/// process-bound identity check. This check defeats an honest client
/// attempting to take a turn it does not own; it does not defeat an agent
/// that lies about which identity it is. Both [`handle_collab_set_implementer`]
/// and [`handle_collab_set_pilot`] inherit this caveat from this helper.
fn ensure_caller_is_current_pilot(
    app: &App,
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    agent: Agent,
    maybe_token: Option<&str>,
    unauthorized: impl FnOnce(Agent, Agent) -> MemoryError,
) -> Result<SessionRecord, MemoryError> {
    super::handoff::ensure_actor_generation_current(app, tx, session_id, agent, maybe_token)?;
    crate::collab::queue::ensure_active(tx, session_id)?;
    let record = crate::collab::queue::load_session_record(tx, session_id)?;
    let current_pilot = record.session.pilot;
    if agent != current_pilot {
        return Err(unauthorized(agent, current_pilot));
    }
    Ok(record)
}

/// Rebind a live session's `implementer` role.
///
/// # Authorization policy
///
/// Two rules, both enforced below inside a single transaction, in this order:
///
/// 1. **Permitted caller.** The request's `agent` must equal the session's
///    *current* pilot — the same rule [`handle_collab_set_pilot`] enforces for
///    reassigning the pilot role. The implementer cannot hand off its own role;
///    only the pilot may rebind who implements. This runs *before* the phase
///    check below, so an unauthorized caller is refused regardless of phase.
///    Enforced by the shared [`ensure_caller_is_current_pilot`] helper — see
///    its doc comment for the caller-asserted-identity caveat that applies to
///    this check.
/// 2. **Phase.** Allowed anywhere in planning up to and including
///    `CodeImplementPending` (see the `can_change` match below); refused once
///    code review has started or coding has finished. This is looser than
///    `collab_set_pilot`'s single pre-draft phase, because the implementer
///    role has no role-dependent planning artifact analogous to a draft.
pub(super) fn handle_collab_set_implementer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let implementer = require_implementer(require_str(args, "implementer")?)?;

    app.db.with_transaction(|tx| {
        // Rule 1 first: authorization before state, mirroring
        // `handle_collab_set_pilot`. An unauthorized caller is refused
        // regardless of which phase the session is in.
        let record = ensure_caller_is_current_pilot(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
            |caller, current_pilot| {
                MemoryError::Validation(format!(
                    "collab_set_implementer refused: caller '{}' is not the pilot of this \
                     session; only the current pilot '{}' may reassign the implementer",
                    caller.as_str(),
                    current_pilot.as_str()
                ))
            },
        )?;

        let can_change = match record.session.phase {
            Phase::PlanParallelDrafts
            | Phase::PlanSynthesisPending
            | Phase::PlanCopilotReviewPending
            | Phase::PlanFinalizePending
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

/// Rebind a live session's `pilot` role.
///
/// # Authorization policy
///
/// This is the one tool that can change who leads a session, so its policy is
/// deliberately narrower than [`handle_collab_set_implementer`]'s. Three rules,
/// all enforced below inside a single transaction:
///
/// 1. **Latest safe state.** Allowed *only* at [`Phase::PlanParallelDrafts`]
///    with `claude_draft_hash.is_none() && codex_draft_hash.is_none()`. Every
///    other phase is refused — including any `collab_start_code_review`
///    session, which begins at `CodeReviewFixGlobalPending` and therefore never
///    passes through the reassignable state at all. This is stricter than
///    `collab_set_implementer`'s "anywhere in planning" on purpose: the pilot
///    decides who may submit `canonical`/`review`/`final`, so once a
///    role-dependent artifact exists, changing the pilot is a live-role rewrite
///    of work already done — not configuration of work not yet started.
/// 2. **Permitted caller.** The request's `agent` must equal the session's
///    *current* pilot. This stops an *honest* client from promoting itself
///    without being handed the role by the agent that already holds it — it
///    does **not** defeat a caller willing to misrepresent its own identity,
///    since `agent` is caller-asserted, not authenticated. It is therefore
///    not a turn-seizure-proof primitive against a lying caller, only against
///    an honest one that has not been handed the role.
///    Enforced by the shared [`ensure_caller_is_current_pilot`] helper — see
///    its doc comment for the caller-asserted-identity caveat that applies to
///    this check.
/// 3. **Atomicity.** The pilot change and `current_owner = new_pilot` happen in
///    the *same* `set_pilot` UPDATE inside the one transaction, so owner and
///    pilot are never observable in an inconsistent pairing. Moving the owner
///    is safe here because at `PlanParallelDrafts` `current_owner` is only the
///    next-expected hint — the drafting arm of `apply_event` has its own
///    `AlreadySubmittedDraft` guard and does not consult the owner to decide
///    whether a draft may land.
///
/// Every rejection names the phase (or the caller's role) and the rule
/// violated, so a caller can tell *why* it was refused without reading this.
///
/// # Deliberate deferral
///
/// The plugin prose assertions that pin `collab_set_implementer` into the
/// shipped `collab.md` command files — `scripts/check_collab_turn_templates.py`
/// and the two in `crates/ironmem/tests/plugin_metadata.rs` — are **not**
/// mirrored for `collab_set_pilot`. Mirroring them would require editing a
/// plugin command file, which is out of scope for this change; documenting the
/// tool in `collab.md` and adding the matching assertions is deferred.
pub(super) fn handle_collab_set_pilot(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    // `require_pilot` rather than `require_agent` for this field only, so a
    // bad value names `pilot` in the error text — the same reason the model
    // handler above validates its own role field with `require_implementer`.
    // Identical accept-set either way; only the message differs.
    let pilot = require_pilot(require_str(args, "pilot")?)?;

    app.db.with_transaction(|tx| {
        // Rule 2 first: authorization before state. An unauthorized caller is
        // told it is the copilot regardless of which phase the session is in.
        let record = ensure_caller_is_current_pilot(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
            |caller, current_pilot| {
                MemoryError::Validation(format!(
                    "collab_set_pilot refused: caller '{}' is the copilot of this session; \
                     only the current pilot '{}' may reassign the pilot role",
                    caller.as_str(),
                    current_pilot.as_str()
                ))
            },
        )?;
        let previous = record.session.pilot;

        // Rule 1: the latest safe state, and only it.
        if record.session.phase != Phase::PlanParallelDrafts {
            return Err(MemoryError::Validation(format!(
                "collab_set_pilot refused in phase {}: the pilot may only be reassigned in \
                 PlanParallelDrafts, before any role-dependent artifact exists",
                record.session.phase
            )));
        }
        if record.session.claude_draft_hash.is_some() || record.session.codex_draft_hash.is_some() {
            return Err(MemoryError::Validation(format!(
                "collab_set_pilot refused in phase PlanParallelDrafts: a draft has already been \
                 submitted (claude={}, codex={}); the pilot may only be reassigned before either \
                 draft lands",
                record.session.claude_draft_hash.is_some(),
                record.session.codex_draft_hash.is_some()
            )));
        }

        // Rule 3: pilot and owner move together, in one UPDATE. Unconditional
        // even when `previous == pilot`, so a no-op reassignment still leaves
        // `current_owner` consistent with the pilot it names.
        let previous_owner = record.session.current_owner;
        crate::collab::queue::set_pilot(tx, session_id, pilot, Some(pilot))?;

        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_set_pilot",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "previous_pilot": previous.as_str(),
                "pilot": pilot.as_str(),
                "phase": record.session.phase.to_string(),
                "previous_owner": previous_owner.as_str(),
                "current_owner": pilot.as_str(),
                "changed": previous != pilot || previous_owner != pilot,
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
    // `initiator` names the *dispatcher* allowed to invoke this shortcut —
    // orthogonal to `pilot` below, which names the review-flow lead. This
    // check stays unconditional: it rejects a non-claude initiator even when
    // `pilot=codex`.
    if initiator != Agent::Claude {
        return Err(MemoryError::Validation(
            "initiator must be 'claude' for collab_start_code_review".to_string(),
        ));
    }
    let task = sanitize::sanitize_content(require_str(args, "task")?, MAX_COLLAB_CONTENT_CHARS)?;
    // Optional `pilot` field: same pattern and default as `collab_start`
    // (Task 7). Resolved before `start_global_review_session` — that call
    // needs the real value, not a hardcoded stand-in — and before the
    // transaction opens.
    let pilot = match args.get("pilot").and_then(Value::as_str) {
        Some(value) => require_pilot(value)?,
        None => Agent::Claude,
    };
    let session_id = uuid::Uuid::new_v4().to_string();
    let session = start_global_review_session(&session_id, base_sha, head_sha, pilot)
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
        ensure_no_conflicting_process_session_tx(app, tx, &session_id, repo_path, branch)?;
        // Shortcut sessions never enter `CodeImplementPending`, so `implementer`
        // is seeded from the resolved `pilot` for uniformity (there is no
        // separate implementer selection on this entry point). `create_session`
        // also seeds `current_owner` from the same `pilot` argument (Task 4).
        // The immediately-following `save_session(tx, &session)` is the
        // authoritative write and overwrites all three fields — implementer,
        // pilot, and current_owner — with `session`'s actual values (current_owner
        // becomes `counterpart(pilot)`, since `new_global_review` starts the
        // *copilot* at `CodeReviewFixGlobalPending`) inside this same
        // transaction, so no reader ever observes the values passed here —
        // they're set to the real resolved `pilot` purely so this call reads
        // correctly on its own, not because any intermediate state is
        // externally visible.
        crate::collab::queue::create_session(
            tx,
            &session_id,
            repo_path,
            branch,
            Some(task),
            crate::collab::CollabRoles {
                pilot,
                implementer: pilot,
            },
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
                "pilot": pilot.as_str(),
                "task": task,
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    app.set_active_collab_session_for_scope(&session_id, repo_path, branch);
    create_initial_task_outcome(app, &session_id);

    Ok(json!({ "session_id": session_id, "task": task, "pilot": pilot.as_str() }))
}

pub(super) fn handle_collab_send(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let (repo_path, branch) = scope_for_session(app, session_id)?;
    ensure_no_conflicting_process_session(app, session_id, &repo_path, &branch)?;
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
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            sender,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
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
                && failure_report_is_off_turn_admissible(
                    content,
                    sender,
                    session.current_owner,
                    session.phase,
                    session.implementer,
                ));
        if !turn_exempt && sender != session.current_owner {
            return Err(MemoryError::Validation(format!(
                "not your turn: phase {} expects sender '{}', got '{}'",
                session.phase, session.current_owner, sender
            )));
        }

        // Non-advancing topic: record the incident and return before the event
        // builder. Everything below this point mutates session state, and an
        // orphan record must not — see ORPHAN_RECOVERED_TOPIC.
        if topic == ORPHAN_RECOVERED_TOPIC {
            let drawer_id = store_collab_message_drawer(tx, content)?;
            let message_id = crate::collab::queue::record_incident(
                tx,
                session_id,
                sender.as_str(),
                topic,
                content,
                &drawer_id,
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
                    "phase": phase_before,
                })),
            )?;
            // `save_session` is deliberately not called: the session row is
            // returned to the caller exactly as it was loaded.
            return Ok((
                json!({ "message_id": message_id, "phase": phase_before }),
                phase_before_enum,
                phase_before_enum,
                None,
            ));
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
        } else if topic == "task_list" {
            if let Some(task_list) = session.task_list.as_deref() {
                session.task_list_drawer_id =
                    Some(store_collab_task_list_drawer(tx, session_id, task_list)?);
            }
        }
        // Snapshot the post-event pr_url so the lifecycle writer can stamp it
        // on CodingComplete without an extra DB round-trip.
        let post_pr_url = session.pr_url.clone();
        let phase_after_enum = session.phase;
        crate::collab::queue::save_session(tx, &session)?;

        let drawer_id = store_collab_message_drawer(tx, content)?;
        let message_id = crate::collab::queue::send_message(
            tx,
            session_id,
            sender.as_str(),
            collab_counterpart(sender).as_str(),
            topic,
            content,
            &drawer_id,
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

    // Deliberately also set on terminal sends — terminal-but-not-ended sessions
    // still attribute (bucket 'other') until a newer session claims this scope.
    app.set_active_collab_session_for_scope(session_id, &repo_path, &branch);
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
    let (repo_path, branch) = scope_for_session(app, session_id)?;
    ensure_no_conflicting_process_session(app, session_id, &repo_path, &branch)?;
    let receiver = require_agent(require_str(args, "receiver")?)?;
    let limit = (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize).min(50);
    let auto_ack = args
        .get("auto_ack")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);

    let result = app.db.with_transaction(|tx| {
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            receiver,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
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

        let redact_content = app.config.mcp_access_mode.redacts_sensitive_content();
        let json_messages: Vec<Value> = filtered
            .iter()
            .map(|message| {
                if redact_content {
                    // Queue message IDs are random UUIDs; retain only that
                    // delivery metadata and explicit redaction markers. The
                    // drawer id, hash, preview, and body are all derived from
                    // (or reveal) the sensitive message content.
                    json!({
                        "id": message.id,
                        "sender": message.sender,
                        "topic": message.topic,
                        "created_at": message.created_at,
                        "content_redacted": true,
                        "hash_redacted": true,
                    })
                } else {
                    // Keep the trusted response field order and values exactly
                    // as before the restricted-mode rendering branch.
                    let mut out = json!({
                        "id": message.id,
                        "sender": message.sender,
                        "topic": message.topic,
                        "created_at": message.created_at,
                        "drawer_id": message.drawer_id,
                        "hash": sha256_hex(&message.content),
                        // Char-boundary safe: take 200 Rust chars, not bytes.
                        "first_200_chars": message.content.chars().take(200).collect::<String>(),
                    });
                    // Pre-016 queue rows have no drawer reference. Preserve
                    // their usable legacy body even under the compact default
                    // rather than returning a reference the receiver cannot
                    // dereference.
                    if full || message.drawer_id.is_none() {
                        out["content"] = Value::String(message.content.clone());
                    }
                    out
                }
            })
            .collect();
        Ok(json!({ "messages": json_messages }))
    })?;
    app.set_active_collab_session_for_scope(session_id, &repo_path, &branch);
    Ok(result)
}

pub(super) fn handle_collab_ack(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let message_id = require_str(args, "message_id")?;
    let session_id = require_str(args, "session_id")?;
    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        // Resolve the receiver from the target message so we can run the
        // generation guard. A missing message surfaces as NotFound — same
        // behavior as the ack_message call that follows.
        let receiver_str: Option<String> = tx
            .query_row(
                "SELECT receiver FROM messages WHERE id = ?1 AND session_id = ?2",
                rusqlite::params![message_id, session_id],
                |row| row.get(0),
            )
            .optional()?;
        let receiver_str = receiver_str.ok_or_else(|| {
            MemoryError::NotFound(format!(
                "message {message_id} not found in session {session_id}"
            ))
        })?;
        let agent = receiver_str
            .parse::<crate::collab::Agent>()
            .map_err(|e| MemoryError::Validation(format!("invalid receiver in message: {e}")))?;
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
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

/// Read the required leading plan-file marker without exposing the plan body.
fn plan_file_path_from_plan(body: &str) -> Option<String> {
    let first_nonblank = body.lines().find(|line| !line.trim().is_empty())?.trim();
    first_nonblank
        .strip_prefix("<!-- plan_file_path: ")?
        .strip_suffix(" -->")
        .map(str::to_string)
}

/// Build the reference-only plan shape surfaced by `collab_status`. A caller
/// that needs plan content deliberately dereferences `drawer_id`; status never
/// includes body text, including when `verbose:true` is requested.
fn plan_ref_json(drawer_id: Option<&str>, hash: Option<&str>, body: &str) -> Value {
    json!({
        "drawer_id": drawer_id,
        "hash": hash,
        "plan_file_path": plan_file_path_from_plan(body),
    })
}

/// Render one accepted plan (`canonical` or `final`) into `status` as a
/// reference. Legacy sessions still read their persisted message internally so
/// callers retain a verifiable hash/path, but their plan text never crosses the
/// MCP boundary.
fn render_plan(
    db: &crate::db::schema::Database,
    status: &mut Value,
    session_id: &str,
    kind: &str, // "canonical" | "final"
    drawer_id: Option<&str>,
    hash: Option<&str>,
) -> Result<Option<String>, MemoryError> {
    let ref_key = format!("{kind}_plan_ref");
    let body = match drawer_id {
        Some(id) => {
            let drawer = db.get_drawer(id)?.ok_or_else(|| {
                MemoryError::Validation(format!(
                    "{kind}_plan_drawer_id {id} points to a missing drawer"
                ))
            })?;
            status[ref_key] = plan_ref_json(Some(id), hash, &drawer.content);
            Some(drawer.content)
        }
        None => {
            // Legacy (pre-009): drawer id NULL. Read from messages only to
            // expose a compact reference, never to inline a plan body.
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
                        status[ref_key] = plan_ref_json(None, hash, &body);
                        Some(body)
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
                        None
                    }
                }
            } else {
                None
            }
        }
    };
    Ok((kind == "final")
        .then(|| body.as_deref().and_then(plan_file_path_from_plan))
        .flatten())
}

pub(super) fn handle_collab_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let record = app.db.collab_load_session_record(session_id)?;
    let mut status = session_record_json(&record);
    // Turns that recovered work a previous turn left behind. Non-zero means a
    // turn died without reporting, so the session's own history understates
    // how much went wrong — see ORPHAN_RECOVERED_TOPIC.
    status["orphans_recovered"] = json!(app.db.with_connection(|conn| {
        crate::collab::queue::count_incidents(conn, session_id, ORPHAN_RECOVERED_TOPIC)
    })?);
    // `verbose` remains accepted for backwards-compatible requests, but plans
    // are always references: large plan bodies must never transit status.
    let include_task_list = args
        .get("include_task_list")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if include_task_list {
        status["task_list"] = record
            .session
            .task_list
            .as_deref()
            .and_then(|_| {
                task_list_ref_json(
                    record.session.task_list_drawer_id.as_deref(),
                    record.session.task_list.as_deref(),
                )
            })
            .unwrap_or(Value::Null);
    }
    render_plan(
        &app.db,
        &mut status,
        session_id,
        "canonical",
        record.session.canonical_plan_drawer_id.as_deref(),
        record.session.canonical_plan_hash.as_deref(),
    )?;
    if let Some(plan_file_path) = render_plan(
        &app.db,
        &mut status,
        session_id,
        "final",
        record.session.final_plan_drawer_id.as_deref(),
        record.session.final_plan_hash.as_deref(),
    )? {
        status["plan_file_path"] = Value::String(plan_file_path);
    }
    for ag in [Agent::Claude, Agent::Codex] {
        let g = app
            .db
            .with_connection(|c| crate::collab::read_actor_generation(c, session_id, ag))?;
        let (generation, pending) = match g {
            Some(a) => (a.generation, a.pending.is_some()),
            None => (0, false),
        };
        status[format!("{}_generation", ag.as_str())] = json!(generation);
        status[format!("{}_handoff_pending", ag.as_str())] = json!(pending);
    }
    Ok(status)
}

/// `collab_approve` — the copilot's one-pass verdict on the pilot's canonical
/// plan. The approver is whichever agent is *not* the session's pilot, so the
/// role gate below has to read the session first; it cannot be a constant
/// check on the parsed argument the way it was when Claude was hardcoded as
/// the only pilot.
pub(super) fn handle_collab_approve(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let content_hash = require_str(args, "content_hash")?;
    let review_content = json!({
        "verdict": "approve",
        "content_hash": content_hash,
    })
    .to_string();

    app.db.with_transaction(|tx| {
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        crate::collab::queue::ensure_active(tx, session_id)?;
        let session = crate::collab::queue::load_session(tx, session_id)?;
        // Role gate. `apply_event`'s `SubmitReview` arm enforces exactly this
        // rule (`require_actor(actor, copilot(session))`) and is the primary
        // enforcement point — this check is defense-in-depth, NOT redundant:
        // it fails the call before any drawer or queue write, and it reports
        // the expected approver by name instead of a bare turn violation.
        // Do not delete it as duplicated logic.
        let expected_approver = crate::collab::copilot(&session);
        if agent != expected_approver {
            return Err(MemoryError::Validation(format!(
                "agent must be '{}' for collab_approve",
                expected_approver.as_str()
            )));
        }
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
            agent,
            &CollabEvent::SubmitReview {
                verdict: "approve".to_string(),
            },
        )
        .map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;
        let drawer_id = store_collab_message_drawer(tx, &review_content)?;
        let _ = crate::collab::queue::send_message(
            tx,
            session_id,
            agent.as_str(),
            collab_counterpart(agent).as_str(),
            "review",
            &review_content,
            &drawer_id,
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

/// Gap between `collab_wait_my_turn` snapshot reads.
pub(super) const WAIT_MY_TURN_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(WAIT_MY_TURN_POLL_MS);

/// Floor for how long a `collab_wait_my_turn` call polls once its claim has
/// actually committed, regardless of how long the request sat queued before
/// that — see [`wait_my_turn_deadline`]. One poll interval is the minimum
/// useful floor: anything smaller wouldn't survive even a single snapshot
/// read.
pub(super) const WAIT_MY_TURN_MIN_POLL_WINDOW: std::time::Duration = WAIT_MY_TURN_POLL_INTERVAL;

/// How long `collab_wait_my_turn` polls before answering "not your turn".
///
/// Parsed in ONE place so the asynchronous long-poll in `server` and the
/// synchronous fallback below cannot disagree about the bound — a disagreement
/// would show up as a wait that returns early or runs long, both of which look
/// like protocol bugs rather than a parsing difference.
pub(super) fn wait_my_turn_timeout(args: &Value) -> std::time::Duration {
    let secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS)
        .clamp(1, WAIT_MY_TURN_MAX_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// When a `collab_wait_my_turn` request ARRIVED on its connection.
///
/// A newtype rather than a bare `Instant` because [`wait_my_turn_deadline`]
/// takes this and [`ClaimCommittedAt`] adjacently: as plain `Instant`s a swapped
/// call site would compile, pass every test, and silently turn a client's
/// requested 60s long poll into ~115s — the exact overrun the deadline formula
/// exists to make impossible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArrivedAt(pub(crate) std::time::Instant);

/// When a `collab_wait_my_turn` request's claim actually COMMITTED, i.e. when
/// [`wait_my_turn_begin`] returned `Ok`. See [`ArrivedAt`] for why this is a
/// newtype.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaimCommittedAt(pub(crate) std::time::Instant);

/// Deadline for the `collab_wait_my_turn` poll loop, given when the request
/// ARRIVED (`arrived_at`) and when its claim actually COMMITTED
/// (`claim_committed_at`, i.e. `wait_my_turn_begin` returned `Ok`).
///
/// A request queued behind other mutations on the same connection can sit for
/// nearly its whole requested timeout before `wait_my_turn_begin` even runs —
/// at which point `arrived_at + timeout` may already be in the past, leaving
/// the poll loop zero iterations to observe a settled state. The deadline is
/// therefore the LATER of the original arrival-based bound (unaffected when
/// dispatch is prompt — the two instants are equal) and a floor
/// measured from the commit instant, capped at the client's own requested
/// timeout from that point so the floor can never stretch the wait past what
/// was actually asked for.
pub(super) fn wait_my_turn_deadline(
    arrived_at: ArrivedAt,
    claim_committed_at: ClaimCommittedAt,
    args: &Value,
) -> std::time::Instant {
    let timeout = wait_my_turn_timeout(args);
    let arrival_deadline = arrived_at.0 + timeout;
    let floor_deadline = claim_committed_at.0 + timeout.min(WAIT_MY_TURN_MIN_POLL_WINDOW);
    arrival_deadline.max(floor_deadline)
}

/// Claim the generation and capture the matching wait baseline in one
/// transaction.
///
/// A handoff claim is a write, so separating it from the baseline read would
/// admit a committed phase/owner/recovery transition into the baseline and
/// silently miss the wake it should cause. Reading the session record through
/// the same transaction gives a transactionally consistent post-claim point.
fn wait_my_turn_claim_and_capture_baseline(
    app: &App,
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    agent: Agent,
    args: &Value,
) -> Result<WaitTurnBaseline, MemoryError> {
    super::handoff::ensure_actor_generation_current(
        app,
        tx,
        session_id,
        agent,
        super::handoff::opt_handoff_token(args).as_deref(),
    )?;
    let record = crate::collab::queue::load_session_record(tx, session_id)?;
    Ok(wait_turn_snapshot(&record, agent).baseline)
}

/// Validate the arguments, settle the generation, and capture one baseline
/// before any polling.
///
/// Split out of the handler so the claim happens exactly ONCE per request even
/// when the polling is driven from outside — claiming on every poll would try
/// to re-consume a one-time handoff token and fail on the second iteration.
///
/// The scoped metrics attribution is claimed here too, exactly once,
/// immediately after the generation claim commits and guarded by
/// `ensure_no_conflicting_process_session` above. It must NOT be re-stamped by
/// the poll loop: the ordering barrier is released as soon as this function
/// returns `Ok` (see `server::dispatch_wait_my_turn`), so a still-polling wait
/// for session A would otherwise clobber the cell back to A after a newer,
/// later-queued request had legitimately bound it to B — producing spurious
/// "already bound to this repository and branch for metrics attribution"
/// refusals.
pub(super) fn wait_my_turn_begin(app: &App, args: &Value) -> Result<WaitTurnBaseline, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let (repo_path, branch) = scope_for_session(app, session_id)?;
    ensure_no_conflicting_process_session(app, session_id, &repo_path, &branch)?;
    let agent = require_agent(require_str(args, "agent")?)?;

    let baseline = app.db.with_transaction(|tx| {
        wait_my_turn_claim_and_capture_baseline(app, tx, session_id, agent, args)
    })?;

    app.set_active_collab_session_for_scope(session_id, &repo_path, &branch);
    Ok(baseline)
}

/// One snapshot read. Returns the response body and whether it SETTLES the wait
/// — a baseline state change, my turn, session ended, or a terminal phase —
/// i.e. whether the caller should stop polling before the deadline.
///
/// Deliberately free of sleeping and of any write — including the
/// scoped attribution claim, which belongs to
/// [`wait_my_turn_begin`] and happens exactly once per request. That keeps a
/// long poll from mutating shared state after its barrier was already released,
/// and lets a caller drive it from an async loop without holding the dispatch
/// thread across the wait.
pub(super) fn wait_my_turn_poll(
    app: &App,
    args: &Value,
    baseline: &WaitTurnBaseline,
) -> Result<(Value, bool), MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    let record = app.db.collab_load_session_record(session_id)?;
    let snap = wait_turn_snapshot(&record, agent);
    let settled = snap.is_my_turn
        || snap.baseline.ended
        || snap.baseline.phase_is_terminal
        || snap.baseline != *baseline;

    Ok((
        json!({
            "is_my_turn": snap.is_my_turn,
            "phase": snap.baseline.phase,
            "current_owner": snap.baseline.current_owner,
            "session_ended": snap.baseline.ended,
        }),
        settled,
    ))
}

/// Synchronous fallback for callers that reach `call_tool`/`dispatch` directly.
///
/// `server::dispatch_wait_my_turn` normally drives the same primitives with an
/// ASYNC sleep, because this loop's `std::thread::sleep` would otherwise hold
/// the single dispatch thread — and therefore every connection — for the whole
/// timeout. Kept so the tool still behaves correctly for direct callers.
pub(super) fn handle_collab_wait_my_turn(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let timeout = wait_my_turn_timeout(args);
    let baseline = wait_my_turn_begin(app, args)?;

    // No `wait_my_turn_deadline` call needed here: this function IS the
    // entire request handling for a direct/synchronous caller, so there is no
    // queueing delay between "arrived" and "claim committed" — both are this
    // exact instant. `ArrivedAt` and `ClaimCommittedAt` would be identical,
    // which collapses `wait_my_turn_deadline` back to plain `now() + timeout`.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let (body, settled) = wait_my_turn_poll(app, args, &baseline)?;
        if settled {
            return Ok(body);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(json!({ "unchanged": true }));
        }
        std::thread::sleep(WAIT_MY_TURN_POLL_INTERVAL);
    }
}

pub(super) fn handle_collab_end(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    let (phase, repo_path, branch) = app.db.with_transaction(|tx| {
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        // PlanFinalizePending has one narrow abort path: the current owner may
        // end a plan that cannot be finalized (for example, because it needs
        // more than the bounded task budget). The owner check prevents the
        // counterpart from killing an in-flight finalization turn. The other
        // endable phases are PlanLocked (pre-task_list) and the two v3
        // terminal phases.
        let record = crate::collab::queue::load_session_record(tx, session_id)?;
        let session = record.session;
        if session.phase == Phase::PlanFinalizePending && agent != session.current_owner {
            return Err(MemoryError::Validation(format!(
                "collab_end from PlanClaudeFinalizePending requires current owner {}; got {}",
                session.current_owner, agent
            )));
        }
        let allowed = matches!(
            session.phase,
            Phase::PlanFinalizePending
                | Phase::PlanLocked
                | Phase::CodingComplete
                | Phase::CodingFailed
        );
        if !allowed {
            return Err(MemoryError::Validation(format!(
                "collab_end rejected in active phase {}; end is only valid from PlanClaudeFinalizePending (by the current owner), PlanLocked (pre-task_list), CodingComplete, or CodingFailed",
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
        Ok((ended_phase, record.repo_path, record.branch))
    })?;

    // Operator attestation (METRICS_SPEC §12 amendment): the operator ends a
    // CodingComplete session after the PR lands, or abandons a pre-coding
    // session during finalization / after the plan locks.
    if crate::search::tunables::metrics_enabled() {
        let now = crate::metrics::now_rfc3339();
        let attested = match phase {
            Phase::CodingComplete => {
                app.db
                    .mark_task_outcome_done(session_id, None, Some("merged"), None)
            }
            Phase::PlanFinalizePending | Phase::PlanLocked => {
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
    app.clear_active_collab_session_for_scope_if_matches(session_id, &repo_path, &branch);

    Ok(json!({ "ok": true, "session_id": session_id }))
}

/// Resume a tooling-class `CodingFailed` session back to the phase it failed
/// from. Honors the same generation-lease / `handoff_token` check every
/// other collab writer does; admissibility (whether this specific session is
/// eligible to resume at all) is entirely `apply_event`'s call via the
/// `ResumeCoding` event — this handler does not reimplement or duplicate any
/// of that eligibility logic, it only plumbs the request through and
/// surfaces `CollabError::NotResumable` as a validation error via
/// `collab_error_to_memory_error`.
///
/// After the protocol transaction commits, this also clears any stale
/// `outcome='failed'`/`done_at` row that an earlier terminal `failure_report`
/// wrote for this session (METRICS_SPEC §5.4 amendment, task 10) — a
/// resumed session must be able to complete normally afterward. The clear is
/// best-effort cleanup, run after commit so a database error can never roll
/// back or fail a collab turn. This clear deliberately ignores
/// `IRONMEM_METRICS`: the kill switch may have been enabled after the terminal
/// failure wrote its outcome, and resuming must not leave that stale failed
/// outcome behind.
pub(super) fn handle_collab_resume(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let (repo_path, branch) = scope_for_session(app, session_id)?;
    ensure_no_conflicting_process_session(app, session_id, &repo_path, &branch)?;
    let agent = require_agent(require_str(args, "agent")?)?;

    let (phase, current_owner) = app.db.with_transaction(|tx| {
        super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        // A session already `collab_end`-ed cannot be resumed, even if it
        // was `CodingFailed` at the time — `ensure_active` only rejects on
        // `ended_at`, not on phase, so a still-open `CodingFailed` session
        // (the common case: nobody has called `collab_end` on it yet) passes
        // through untouched.
        crate::collab::queue::ensure_active(tx, session_id)?;
        let record = crate::collab::queue::load_session_record(tx, session_id)?;
        if let Some((newer_session_id, phase)) =
            crate::collab::queue::find_active_session_by_repo_branch(
                tx,
                &record.repo_path,
                &record.branch,
            )?
        {
            if newer_session_id != session_id {
                return Err(MemoryError::Validation(format!(
                    "cannot resume collab session {session_id}: newer active session {newer_session_id} \
                     (phase {phase}) owns repo {} branch {}",
                    record.repo_path, record.branch
                )));
            }
        }
        let session = record.session;
        let next = apply_event(&session, agent, &CollabEvent::ResumeCoding)
            .map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &next)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_resume",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
            }),
            Some(&json!({
                "phase": next.phase.to_string(),
                "current_owner": next.current_owner.to_string(),
            })),
        )?;
        Ok((next.phase, next.current_owner))
    })?;

    // Clear the stale `outcome='failed'`/`done_at` row that `failure_report`
    // wrote before this resume (METRICS_SPEC §5.4 amendment, task 10). Runs
    // after the transaction commits, same as `handle_collab_end`'s operator
    // attestation block — metrics failures never roll back or fail a
    // collab turn.
    if let Err(e) = app.db.clear_failed_task_outcome(session_id) {
        tracing::warn!(session_id = %session_id, error = %e, "metrics: clear_failed_task_outcome failed");
    }

    // The session is active again post-resume, same bookkeeping
    // `handle_collab_send` performs on every successful send.
    app.set_active_collab_session_for_scope(session_id, &repo_path, &branch);

    Ok(json!({
        "ok": true,
        "session_id": session_id,
        "phase": phase.to_string(),
        "current_owner": current_owner.to_string(),
    }))
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
        start_session_in_scope(app, "/tmp/repo", "main")
    }

    fn start_session_in_scope(app: &crate::mcp::app::App, repo_path: &str, branch: &str) -> String {
        let args = json!({
            "repo_path": repo_path, "branch": branch,
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

    /// Drive v1 planning to the pilot-owned finalization phase.
    fn drive_to_plan_finalize_pending(app: &crate::mcp::app::App, sid: &str) {
        send(app, sid, "claude", "draft", "claude draft");
        send(app, sid, "codex", "draft", "codex draft");
        send(app, sid, "claude", "canonical", "canonical plan");
        send(app, sid, "codex", "review", r#"{"verdict":"approve"}"#);
    }

    /// Drive v1 planning to PlanLocked and return the final_plan_hash that
    /// must be used in the subsequent task_list payload.
    fn drive_to_plan_locked(app: &crate::mcp::app::App, sid: &str) -> String {
        let plan_text = "final plan";
        let final_plan_hash = super::super::shared::sha256_hex(plan_text);
        drive_to_plan_finalize_pending(app, sid);
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

    /// Drive the normal v3 lifecycle through its terminal success phase while
    /// deliberately leaving `collab_end` uncalled.
    fn drive_to_coding_complete(app: &crate::mcp::app::App, sid: &str) {
        drive_to_implement(app, sid);
        send(
            app,
            sid,
            "claude",
            "implementation_done",
            r#"{"head_sha":"c1"}"#,
        );
        send(
            app,
            sid,
            "codex",
            "review_fix_global",
            r#"{"head_sha":"c2"}"#,
        );
        send(app, sid, "claude", "review_local", r#"{"head_sha":"c3"}"#);
        send(
            app,
            sid,
            "claude",
            "final_review",
            r#"{"head_sha":"c4","pr_url":"https://github.com/x/y/pull/9"}"#,
        );
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

    /// Renamed from `failure_report_marks_outcome_failed` (task 10): since
    /// task 4, `failure_report` has two distinct behaviors — a recoverable
    /// (`Tooling`-classified) report leaves `outcome`/`done_at` untouched,
    /// while a terminal report sets them. This test exercises the TERMINAL
    /// branch only (`subagent_failure:` is not one of the six recoverable
    /// prefixes — see `recoverable_failure_report_leaves_task_outcome_untouched`
    /// for the recoverable counterpart).
    #[test]
    fn terminal_failure_report_marks_outcome_failed() {
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

    /// Counterpart to `terminal_failure_report_marks_outcome_failed` (task
    /// 10): a RECOVERABLE (`Tooling`-classified) `failure_report` leaves
    /// `session.phase` unchanged (established in task 4), so
    /// `record_task_outcome_transition`'s `before == after` early return
    /// fires before the `CodingFailed` match arm is ever reached — the
    /// `task_outcomes` row must stay untouched (`outcome`/`done_at` still
    /// `None`).
    #[test]
    fn recoverable_failure_report_leaves_task_outcome_untouched() {
        let _g = metrics_on_guard();
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        // git_commit_failed: is one of the six Task-1 recoverable prefixes.
        send(
            &app,
            &sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: index.lock EPERM"}"#,
        );

        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome, None,
            "a recoverable report must not set outcome"
        );
        assert_eq!(
            row.done_at, None,
            "a recoverable report must not set done_at"
        );
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
    fn collab_end_from_plan_finalize_pending_lets_owner_abort_oversized_plan() {
        let _g = metrics_on_guard();
        let app = test_app();
        let sid = start_session(&app);
        drive_to_plan_finalize_pending(&app, &sid);

        handle_collab_end(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert!(
            record.ended_at.is_some(),
            "oversized plan abort must end session"
        );
        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(row.outcome.as_deref(), Some("abandoned"));
        assert!(
            row.done_at.is_some(),
            "oversized plan abort must finish outcome"
        );
    }

    #[test]
    fn collab_end_from_plan_finalize_pending_rejects_non_owner() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_plan_finalize_pending(&app, &sid);

        let err =
            handle_collab_end(&app, &json!({"session_id": sid, "agent": "codex"})).unwrap_err();

        assert!(
            err.to_string().contains(
                "collab_end from PlanClaudeFinalizePending requires current owner claude"
            ),
            "unexpected error: {err}"
        );
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

        assert_eq!(
            wait,
            json!({
                "is_my_turn": true,
                "phase": "PlanParallelDrafts",
                "current_owner": "claude",
                "session_ended": false,
            })
        );
        assert_eq!(
            app.active_collab_session_snapshot().as_deref(),
            Some(sid.as_str())
        );
    }

    #[test]
    fn wait_my_turn_timeout_returns_a_compact_unchanged_frame() {
        let app = test_app();
        let sid = start_session(&app);

        let wait = handle_collab_wait_my_turn(
            &app,
            &json!({"session_id": sid, "agent": "codex", "timeout_secs": 1}),
        )
        .unwrap();

        assert_eq!(
            wait,
            json!({"unchanged": true}),
            "an other-owned session that remains unsettled through the timeout must return the compact frame"
        );
    }

    #[test]
    fn wait_my_turn_claim_baseline_is_captured_in_the_claim_transaction() {
        let app = test_app();
        let sid = start_session(&app);
        let token = app
            .db
            .with_transaction(|tx| {
                crate::collab::handoff::issue_or_reuse_handoff(tx, &sid, Agent::Codex)
            })
            .unwrap()
            .token;
        let wait_args = json!({
            "session_id": sid,
            "agent": "codex",
            "handoff_token": token,
        });

        let baseline = app
            .db
            .with_transaction(|tx| {
                let baseline = wait_my_turn_claim_and_capture_baseline(
                    &app,
                    tx,
                    wait_args["session_id"].as_str().unwrap(),
                    Agent::Codex,
                    &wait_args,
                )?;

                let mut session = crate::collab::queue::load_session(
                    tx,
                    wait_args["session_id"].as_str().unwrap(),
                )?;
                session.phase = Phase::PlanSynthesisPending;
                crate::collab::queue::save_session(tx, &session)?;
                let after = wait_turn_snapshot(
                    &crate::collab::queue::load_session_record(
                        tx,
                        wait_args["session_id"].as_str().unwrap(),
                    )?,
                    Agent::Codex,
                )
                .baseline;

                assert_ne!(
                    baseline, after,
                    "the later transaction mutation must not be folded into the baseline"
                );
                Ok(baseline)
            })
            .unwrap();

        assert_eq!(baseline.phase, "PlanParallelDrafts");
        assert_eq!(baseline.current_owner, "claude");
    }

    #[test]
    fn wait_my_turn_settles_when_phase_changes_with_the_same_nonwaiting_owner() {
        let app = test_app();
        let args = json!({
            "repo_path": "/tmp/repo",
            "branch": "main",
            "initiator": "claude",
            "task": "same-owner phase wake",
            "implementer": "codex",
        });
        let sid = handle_collab_start(&app, &args).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        drive_to_implement(&app, &sid);
        let wait_args = json!({"session_id": sid, "agent": "claude"});

        let baseline = wait_my_turn_begin(&app, &wait_args).unwrap();
        let (_, settled_before) = wait_my_turn_poll(&app, &wait_args, &baseline).unwrap();
        assert!(!settled_before, "Codex still owns CodeImplementPending");

        send(
            &app,
            wait_args["session_id"].as_str().unwrap(),
            "codex",
            "implementation_done",
            r#"{"head_sha":"c1"}"#,
        );

        let (body, settled_after) = wait_my_turn_poll(&app, &wait_args, &baseline).unwrap();
        assert!(
            settled_after,
            "a same-owner phase transition must wake Claude"
        );
        assert_eq!(body["is_my_turn"], json!(false));
        assert_eq!(body["phase"], json!("CodeReviewFixGlobalPending"));
        assert_eq!(body["current_owner"], json!("codex"));
        assert_eq!(body["session_ended"], json!(false));
    }

    #[test]
    fn wait_my_turn_settles_when_pilot_changes_without_owner_change() {
        // Task 13: pilot reassignment must be observable even when it moves
        // neither `current_owner` nor `phase`. This test proves the baseline
        // comparison genuinely detects pilot-only changes via the derived
        // PartialEq (not just decorative field population).
        let app = test_app();
        let args = json!({
            "repo_path": "/tmp/repo",
            "branch": "main",
            "initiator": "claude",
            "task": "pilot change test",
            "implementer": "codex",
        });
        let sid = handle_collab_start(&app, &args).unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        drive_to_implement(&app, &sid);
        let wait_args = json!({"session_id": sid, "agent": "claude"});

        let baseline = wait_my_turn_begin(&app, &wait_args).unwrap();
        assert_eq!(baseline.pilot, "claude", "initial pilot is claude");
        let (_, settled_before) = wait_my_turn_poll(&app, &wait_args, &baseline).unwrap();
        assert!(!settled_before, "Codex still owns CodeImplementPending");

        // Direct DB manipulation to change pilot without moving current_owner
        // or phase — simulating the "Task 10 no-op case" mentioned in the task
        // context (defense in depth for a scenario that can't happen via the
        // normal API, but must still be detectable).
        let mut session = app.db.collab_load_session(&sid).unwrap();
        session.pilot = crate::collab::Agent::Codex;
        app.db.collab_save_session(&session).unwrap();

        let (body, settled_after) = wait_my_turn_poll(&app, &wait_args, &baseline).unwrap();
        assert!(
            settled_after,
            "a pilot-only change (without phase/owner movement) must still wake Claude"
        );
        assert_eq!(body["is_my_turn"], json!(false));
        assert_eq!(body["phase"], json!("CodeImplementPending"));
        assert_eq!(body["current_owner"], json!("codex"));
        assert_eq!(body["session_ended"], json!(false));
    }

    // ── wait_my_turn_deadline ──────────────────────────────────────────────────

    #[test]
    fn wait_my_turn_deadline_matches_arrival_when_dispatched_promptly() {
        let now = std::time::Instant::now();
        let args = json!({ "timeout_secs": 5 });
        let timeout = wait_my_turn_timeout(&args);

        // No queueing delay: arrival and claim-commit are the same instant.
        let deadline = wait_my_turn_deadline(ArrivedAt(now), ClaimCommittedAt(now), &args);

        assert_eq!(deadline, now + timeout);
    }

    #[test]
    fn wait_my_turn_deadline_floors_at_min_window_when_arrival_deadline_already_passed() {
        let timeout_secs = 5;
        let args = json!({ "timeout_secs": timeout_secs });
        // Arrival is old enough that arrival + timeout is already past.
        let arrived_at = ArrivedAt(
            std::time::Instant::now() - std::time::Duration::from_secs(timeout_secs + 10),
        );
        let claim_committed_at = ClaimCommittedAt(std::time::Instant::now());

        let deadline = wait_my_turn_deadline(arrived_at, claim_committed_at, &args);

        let timeout = wait_my_turn_timeout(&args);
        assert_eq!(
            deadline,
            claim_committed_at.0 + WAIT_MY_TURN_MIN_POLL_WINDOW.min(timeout)
        );
    }

    #[test]
    fn wait_my_turn_deadline_never_extends_past_requested_timeout_from_commit() {
        let args = json!({ "timeout_secs": 1 });
        let arrived_at = ArrivedAt(std::time::Instant::now() - std::time::Duration::from_secs(30));
        let claim_committed_at = ClaimCommittedAt(std::time::Instant::now());

        let deadline = wait_my_turn_deadline(arrived_at, claim_committed_at, &args);

        assert!(deadline <= claim_committed_at.0 + std::time::Duration::from_secs(1));
    }

    #[test]
    fn scoped_slots_allow_distinct_repos_and_branches_but_reject_duplicate_scope() {
        let app = test_app();
        let repo_main = start_session_in_scope(&app, "/tmp/repo", "main");
        let other_repo_main = start_session_in_scope(&app, "/tmp/other-repo", "main");
        let repo_feature = start_session_in_scope(&app, "/tmp/repo", "feature");

        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(repo_main.as_str())
        );
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/other-repo", "main")
                .as_deref(),
            Some(other_repo_main.as_str())
        );
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "feature")
                .as_deref(),
            Some(repo_feature.as_str())
        );

        let err = handle_collab_start(
            &app,
            &json!({
                "repo_path": "/tmp/repo",
                "branch": "main",
                "initiator": "claude",
                "task": "duplicate scope",
                "implementer": "claude",
            }),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("an active collab session already exists for repo /tmp/repo branch main"),
            "unexpected error: {err}"
        );
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(repo_main.as_str())
        );
    }

    #[test]
    fn coding_complete_session_releases_its_scope_without_collab_end() {
        let app = test_app();
        let completed = start_session(&app);
        drive_to_coding_complete(&app, &completed);

        let next = start_session_in_scope(&app, "/tmp/repo", "main");

        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(next.as_str())
        );
        assert_ne!(
            completed, next,
            "a CodingComplete session must release its exact scope for the next session"
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

    // ── G.1: send/recv to a second scoped session coexist ─────────────────────

    #[test]
    fn send_and_recv_to_second_live_session_in_another_scope_are_allowed() {
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
                    crate::collab::CollabRoles {
                        pilot: crate::collab::Agent::Claude,
                        implementer: crate::collab::Agent::Claude,
                    },
                )
            })
            .unwrap();

        handle_collab_send(
            &app,
            &json!({
                "session_id": second,
                "sender": "claude",
                "topic": "draft",
                "content": "a valid draft payload",
            }),
        )
        .unwrap();
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(first.as_str())
        );
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/other", "other-branch")
                .as_deref(),
            Some(second)
        );

        handle_collab_recv(
            &app,
            &json!({
                "session_id": second,
                "receiver": "codex",
            }),
        )
        .unwrap();
        assert_eq!(
            app.active_collab_session_snapshot(),
            None,
            "multiple scopes must not pretend there is one active session"
        );
    }

    // ── G.2: an ended scope does not affect a new scope ───────────────────────

    #[test]
    fn start_in_new_scope_leaves_ended_scope_binding_inspectable() {
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
            "collab_start in another scope must succeed: {:?}",
            result.unwrap_err()
        );
        let new_sid = result.unwrap()["session_id"].as_str().unwrap().to_string();
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(first.as_str()),
            "an ended binding is only self-healed in its own scope"
        );
        assert_eq!(
            app.active_collab_session_snapshot_for_scope("/tmp/new-repo", "new-branch")
                .as_deref(),
            Some(new_sid.as_str())
        );
    }

    // ── G.3: start self-heals when cell holds a missing session ──────────────

    #[test]
    fn start_self_heals_when_cell_holds_missing_session() {
        let app = test_app();
        app.set_active_collab_session_for_scope("ghost-session-id", "/tmp/repo", "main");

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

    /// Renamed from `failure_report_marks_outcome_failed_and_end_does_not_overwrite`
    /// (task 10) for the same reason as `terminal_failure_report_marks_outcome_failed`
    /// above: `subagent_failure:` drives the TERMINAL branch of `failure_report`,
    /// not the recoverable one — the name now says so explicitly.
    #[test]
    fn terminal_failure_report_marks_outcome_failed_and_end_does_not_overwrite() {
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
                    crate::collab::CollabRoles {
                        pilot: crate::collab::Agent::Claude,
                        implementer: crate::collab::Agent::Claude,
                    },
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
    fn request_changes_advances_to_finalize_and_rejects_canonical_resend() {
        // One-pass planning review (MAX_REVIEW_ROUNDS = 1): a `request_changes`
        // verdict no longer returns to synthesis. It advances to
        // PlanFinalizePending, where Codex's requested changes are folded
        // into the `final` plan — there is no second canonical round. So a
        // canonical re-send after review is rejected (the phase now expects
        // `final`), and the canonical drawer id stays pinned to the single v1 body.
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

        // Re-sending canonical is no longer accepted: planning advanced to finalize.
        let err = handle_collab_send(
            &app,
            &json!({
                "session_id": sid, "sender": "claude",
                "topic": "canonical", "content": "CANONICAL V2",
            }),
        )
        .expect_err(
            "canonical re-send must be rejected after one-pass review advances to finalize",
        );
        let msg = format!("{err:?}");
        assert!(
            msg.contains("PublishFinal") && msg.contains("PublishCanonical"),
            "error must name the finalize-phase mismatch, got: {msg}"
        );

        // The canonical drawer id remains pinned to the single accepted v1 body.
        let record = app.db.collab_load_session_record(&sid).unwrap();
        let id = record.session.canonical_plan_drawer_id.unwrap();
        let id_v1 =
            crate::db::drawers::generate_id("CANONICAL V1", "ironrace-memory", "collab-plans");
        assert_eq!(
            id, id_v1,
            "canonical drawer id must stay pinned to the v1 body"
        );
        assert_eq!(
            app.db.get_drawer(&id).unwrap().unwrap().content,
            "CANONICAL V1"
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
            include_superseded: false,
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
        assert!(
            plan_ref["plan_file_path"].is_null(),
            "a plan without a marker must expose no plan_file_path"
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
        assert!(
            plan_ref["plan_file_path"].is_null(),
            "a plan without a marker must expose no plan_file_path"
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
    fn status_default_returns_compact_task_list_ref_not_body() {
        let app = test_app();
        let sid = start_session(&app);
        let final_hash = drive_to_plan_locked(&app, &sid);
        let big_title = "TASK-LIST-BODY-SHOULD-NOT-INLINE".repeat(200);
        let task_list = json!({
            "plan_hash": final_hash,
            "base_sha": "base",
            "head_sha": "base",
            "tasks": [{
                "id": 1,
                "title": big_title,
                "acceptance": ["done"]
            }]
        })
        .to_string();
        send(&app, &sid, "claude", "task_list", &task_list);

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();
        let task_ref = &status["task_list_ref"];
        assert!(task_ref.is_object(), "task_list_ref must be an object");
        let drawer_id = task_ref["drawer_id"]
            .as_str()
            .expect("new sessions must have a task_list drawer id");
        assert_eq!(drawer_id.len(), 32);
        assert!(task_ref["hash"].is_string());
        assert_eq!(status["tasks_count"].as_u64(), Some(1));
        assert!(
            status.get("task_list").is_none(),
            "full task_list body must be absent by default"
        );
        assert!(
            !serde_json::to_string(&status).unwrap().contains(&big_title),
            "full task_list body must not appear in default status"
        );

        let drawer = app.db.get_drawer(drawer_id).unwrap().unwrap();
        assert_eq!(drawer.room, "collab-task-lists");
        assert_eq!(drawer.content, task_list);
    }

    #[test]
    fn status_include_task_list_returns_compact_task_list_ref() {
        let app = test_app();
        let sid = start_session(&app);
        let final_hash = drive_to_plan_locked(&app, &sid);
        let task_list = json!({
            "plan_hash": final_hash,
            "base_sha": "base",
            "head_sha": "base",
            "tasks": [{
                "id": 1,
                "title": "task title",
                "acceptance": ["done"]
            }]
        })
        .to_string();
        send(&app, &sid, "claude", "task_list", &task_list);

        let status = handle_collab_status(
            &app,
            &json!({ "session_id": sid, "include_task_list": true }),
        )
        .unwrap();

        assert_eq!(status["task_list"], status["task_list_ref"]);
        assert!(status["task_list"]["drawer_id"].is_string());
        assert!(status["task_list"]["hash"].is_string());
        assert!(
            !serde_json::to_string(&status).unwrap().contains(&task_list),
            "include_task_list must not inline the task-list JSON"
        );
    }

    #[test]
    fn status_verbose_returns_canonical_ref_not_body() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        send(&app, &sid, "claude", "canonical", "FULL CANONICAL");

        let status =
            handle_collab_status(&app, &json!({ "session_id": sid, "verbose": true })).unwrap();

        assert!(
            status["canonical_plan_ref"].is_object(),
            "verbose must include the compact reference"
        );
        assert!(
            status.get("canonical_plan").is_none(),
            "verbose must not inline the canonical body"
        );
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("FULL CANONICAL"),
            "full canonical body must not transit collab_status"
        );
    }

    #[test]
    fn status_verbose_returns_final_ref_and_file_path_not_body() {
        let app = test_app();
        let sid = start_session(&app);
        let final_plan = "<!-- plan_file_path: docs/iron/plans/issue-207.md -->\n\nFULL FINAL";
        drive_to_final(&app, &sid, "canonical plan", final_plan);

        let status =
            handle_collab_status(&app, &json!({ "session_id": sid, "verbose": true })).unwrap();

        assert!(
            status["final_plan_ref"].is_object(),
            "verbose must include the compact final reference"
        );
        assert_eq!(
            status["plan_file_path"].as_str(),
            Some("docs/iron/plans/issue-207.md")
        );
        assert!(
            status.get("final_plan").is_none(),
            "verbose must not inline the final body"
        );
        assert!(
            !serde_json::to_string(&status).unwrap().contains(final_plan),
            "full final body must not transit collab_status"
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
            assert!(status["canonical_plan_ref"].is_object());
            assert!(status["final_plan_ref"].is_object());
            assert!(status.get("canonical_plan").is_none());
            assert!(status.get("final_plan").is_none());
        }
    }

    #[test]
    fn status_legacy_null_drawer_does_not_inline_full_body() {
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

        assert!(
            status.get("canonical_plan").is_none(),
            "legacy NULL-drawer path must not inline the full body"
        );
    }

    #[test]
    fn status_legacy_null_final_drawer_returns_file_path_without_body() {
        let app = test_app();
        let sid = start_session(&app);
        let final_plan = "<!-- plan_file_path: docs/iron/plans/legacy.md -->\n\nLEGACY FINAL";
        drive_to_final(&app, &sid, "canonical plan", final_plan);

        // Simulate a pre-009 session whose final drawer id was never recorded.
        let mut s = app.db.collab_load_session(&sid).unwrap();
        s.final_plan_drawer_id = None;
        app.db.collab_save_session(&s).unwrap();

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        assert!(status.get("final_plan").is_none());
        assert_eq!(
            status["plan_file_path"].as_str(),
            Some("docs/iron/plans/legacy.md")
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

    // ── generation-lease guard (#91) ──────────────────────────────────────────

    /// Gen-0 flow: a single-app session can send/recv without any token.
    /// Proves the guard does not break the legacy zero-handoff path.
    #[test]
    fn gen0_legacy_flow_unchanged() {
        let app = test_app();
        let sid = start_session(&app);

        // send a draft — the guarded path must allow it at gen 0.
        let result = handle_collab_send(
            &app,
            &json!({
                "session_id": sid,
                "sender": "claude",
                "topic": "draft",
                "content": "gen0 draft payload",
            }),
        );
        assert!(
            result.is_ok(),
            "gen-0 send must succeed without a handoff token: {:?}",
            result.unwrap_err()
        );
    }

    /// Stale predecessor: after the successor claims the handoff token the
    /// predecessor's next guarded call must fail with "stale collab generation".
    #[test]
    fn stale_predecessor_send_rejected_after_claim() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        let pred = test_app_with_db_path(db_path.clone(), dir.path());
        let succ = test_app_with_db_path(db_path, dir.path());

        // Start a session on predecessor (binds gen 0 on first guarded call).
        let sid = {
            let out = handle_collab_start(
                &pred,
                &json!({
                    "repo_path": "/tmp/repo",
                    "branch": "handoff-branch",
                    "initiator": "claude",
                    "task": "handoff test",
                    "implementer": "claude",
                }),
            )
            .unwrap();
            out["session_id"].as_str().unwrap().to_string()
        };

        // Predecessor makes its first guarded call — binds at gen 0.
        handle_collab_send(
            &pred,
            &json!({
                "session_id": sid,
                "sender": "claude",
                "topic": "draft",
                "content": "pred draft",
            }),
        )
        .unwrap();

        // Issue a handoff token for "claude" via the predecessor's DB.
        let token = pred
            .db
            .with_transaction(|tx| {
                crate::collab::issue_or_reuse_handoff(tx, &sid, crate::collab::Agent::Claude)
            })
            .unwrap()
            .token;

        // Successor claims the token via a guarded recv (advances DB gen to 1).
        handle_collab_recv(
            &succ,
            &json!({
                "session_id": sid,
                "receiver": "claude",
                "handoff_token": token,
            }),
        )
        .unwrap();

        // Predecessor tries to send again — cached gen 0, DB gen 1 → stale error.
        let err = handle_collab_send(
            &pred,
            &json!({
                "session_id": sid,
                "sender": "claude",
                "topic": "draft",
                "content": "pred second attempt",
            }),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("stale collab generation"),
            "expected stale collab generation error, got: {err}"
        );
    }

    /// collab_status must expose claude_generation and claude_handoff_pending
    /// correctly after a handoff is issued, without leaking the token itself.
    #[test]
    fn collab_status_exposes_generation_and_pending_without_token() {
        let app = test_app();
        let sid = start_session(&app);

        // Issue a handoff for claude directly.
        let issued = app
            .db
            .with_transaction(|tx| {
                crate::collab::issue_or_reuse_handoff(tx, &sid, crate::collab::Agent::Claude)
            })
            .unwrap();

        let status = handle_collab_status(&app, &json!({"session_id": sid})).unwrap();

        assert_eq!(
            status["claude_generation"],
            json!(0),
            "claude_generation must be 0 (pending does not advance active generation)"
        );
        assert_eq!(
            status["claude_handoff_pending"],
            json!(true),
            "claude_handoff_pending must be true after issue"
        );
        assert_eq!(
            status["codex_handoff_pending"],
            json!(false),
            "codex_handoff_pending must be false (no handoff issued for codex)"
        );

        // The serialized status must not contain the raw token string.
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(
            !serialized.contains(&issued.token),
            "serialized status must not expose the handoff token: token={}, status={}",
            issued.token,
            serialized
        );
    }

    // ── Task 8: collab_resume ───────────────────────────────────────────────

    /// Drive `app`'s session `sid` to a genuinely tooling-class `CodingFailed`
    /// state via real `handle_collab_send` calls — three successive
    /// `git_commit_failed:` failure_reports, mirroring the state machine's
    /// own `session_with_ceiling_degraded_tooling_failure` helper. Turn
    /// ownership naturally alternates claude -> codex -> claude as each
    /// recoverable report hands control to the counterpart, so the ceiling
    /// break on the third report is reported by claude (the session's
    /// implementer and `current_owner` right after `task_list`).
    fn drive_to_tooling_coding_failed(app: &crate::mcp::app::App, sid: &str) {
        drive_to_implement(app, sid);
        send(
            app,
            sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: attempt 1"}"#,
        );
        send(
            app,
            sid,
            "codex",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: attempt 2"}"#,
        );
        send(
            app,
            sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: attempt 3 breaks the ceiling"}"#,
        );
    }

    /// A genuinely tooling-class `CodingFailed` session (reached via real
    /// `handle_collab_send` calls, not a hand-constructed struct) resumes to
    /// its recorded `failed_from_phase`, with the resumer as the new owner.
    #[test]
    fn collab_resume_restores_recorded_phase_for_tooling_failure() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_tooling_coding_failed(&app, &sid);

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::CodingFailed);
        assert_eq!(
            record.session.failed_from_phase,
            Some(Phase::CodeImplementPending)
        );

        // Codex resumes — mirrors the state-machine unit test's choice of
        // resumer to prove the resumer need not be the original reporter.
        let out =
            handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "codex" })).unwrap();

        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["phase"], json!("CodeImplementPending"));
        assert_eq!(out["current_owner"], json!("codex"));

        let after = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(after.session.phase, Phase::CodeImplementPending);
        assert_eq!(after.session.current_owner, crate::collab::Agent::Codex);
        assert_eq!(
            after.session.failed_from_phase,
            Some(Phase::CodeImplementPending),
            "failed_from_phase is a historical record and survives a successful resume"
        );
    }

    /// task 10: `collab_resume` clears the stale `outcome='failed'`/`done_at`
    /// row that the ceiling-degrade transition into `CodingFailed` wrote via
    /// `record_task_outcome_transition` (the degrade DOES move `session.phase`
    /// from `CodeImplementPending` to `CodingFailed`, unlike a plain
    /// recoverable report, so the terminal write fires here same as any
    /// other transition into `CodingFailed`).
    #[test]
    fn collab_resume_clears_stale_failed_outcome() {
        let _g = metrics_on_guard();
        let app = test_app();
        let sid = start_session(&app);
        drive_to_tooling_coding_failed(&app, &sid);

        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome.as_deref(),
            Some("failed"),
            "ceiling-degrade into CodingFailed must set outcome=failed"
        );
        assert!(row.done_at.is_some(), "ceiling-degrade must set done_at");

        handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "codex" })).unwrap();

        let row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            row.outcome, None,
            "collab_resume must clear the stale failed outcome"
        );
        assert_eq!(
            row.done_at, None,
            "collab_resume must clear the stale done_at"
        );
    }

    #[test]
    fn collab_resume_clears_stale_failed_outcome_when_metrics_are_disabled() {
        let _g = crate::metrics::METRICS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");

        let app = test_app();
        let sid = start_session(&app);
        drive_to_tooling_coding_failed(&app, &sid);

        let before = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(before.outcome.as_deref(), Some("failed"));
        assert!(before.done_at.is_some());

        std::env::set_var("IRONMEM_METRICS", "0");
        handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "codex" })).unwrap();
        std::env::remove_var("IRONMEM_METRICS");

        let after = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(after.outcome, None);
        assert_eq!(after.done_at, None);
    }

    #[test]
    fn collab_resume_allows_restored_phase_completion_and_clears_recovery_state() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_tooling_coding_failed(&app, &sid);

        handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "codex" })).unwrap();

        // The resumed Codex owner completes the restored implementation phase.
        // This exercises the tool-level turn gate and delegated-completion
        // override together, rather than only asserting the resume snapshot.
        send(
            &app,
            &sid,
            "codex",
            "implementation_done",
            r#"{"head_sha":"resumed-implementation-head"}"#,
        );

        let after = app.db.collab_load_session_record(&sid).unwrap().session;
        assert_eq!(after.phase, Phase::CodeReviewFixGlobalPending);
        assert_eq!(after.current_owner, crate::collab::Agent::Codex);
        assert_eq!(
            after.last_head_sha.as_deref(),
            Some("resumed-implementation-head")
        );
        assert_eq!(after.coding_failure, None);
        assert_eq!(after.pending_failure, None);
        assert_eq!(after.recovery_phase, None);
        assert_eq!(after.recovery_owner, None);
        assert_eq!(after.recovery_origin_owner, None);
        assert_eq!(after.recovery_attempts, 0);
    }

    /// A stale predecessor process — one whose cached generation for the
    /// resuming agent predates a successor claiming that agent's handoff
    /// token — is rejected by the same generation-lease guard every other
    /// collab writer honors. Mirrors `stale_predecessor_send_rejected_after_claim`.
    #[test]
    fn collab_resume_rejects_stale_generation_resumer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        let pred = test_app_with_db_path(db_path.clone(), dir.path());
        let succ = test_app_with_db_path(db_path, dir.path());

        // Drive the whole session to a tooling CodingFailed state via pred —
        // this binds pred's cached generation at 0 for both claude and codex
        // (every guarded call along the way runs through pred).
        let sid = start_session(&pred);
        drive_to_tooling_coding_failed(&pred, &sid);

        // Issue a handoff token for "claude" (the agent about to attempt a
        // stale resume) via pred's DB.
        let token = pred
            .db
            .with_transaction(|tx| {
                crate::collab::issue_or_reuse_handoff(tx, &sid, crate::collab::Agent::Claude)
            })
            .unwrap()
            .token;

        // Successor claims the token via a guarded recv, advancing the DB
        // generation for claude to 1.
        handle_collab_recv(
            &succ,
            &json!({
                "session_id": sid,
                "receiver": "claude",
                "handoff_token": token,
            }),
        )
        .unwrap();

        // Predecessor attempts to resume as claude — still cached at gen 0,
        // DB is now at gen 1 -> rejected before apply_event ever runs.
        let err = handle_collab_resume(&pred, &json!({ "session_id": sid, "agent": "claude" }))
            .unwrap_err();

        assert!(
            err.to_string().contains("stale collab generation"),
            "expected stale collab generation error, got: {err}"
        );
    }

    #[test]
    fn collab_resume_allows_another_repository_branch_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let first = test_app_with_db_path(db_path.clone(), dir.path());
        let second = test_app_with_db_path(db_path, dir.path());

        // Bind the first process to one live session.
        let _first_sid = start_session(&first);

        // Build a separate, resumable session through a separate process so
        // both sessions are live in the same database without tripping the
        // scoped attribution guard during setup.
        let second_sid = handle_collab_start(
            &second,
            &json!({
                "repo_path": "/tmp/other-repo",
                "branch": "other-branch",
                "initiator": "claude",
                "task": "second lifecycle test",
                "implementer": "claude",
            }),
        )
        .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        drive_to_tooling_coding_failed(&second, &second_sid);

        let result = handle_collab_resume(
            &first,
            &json!({ "session_id": second_sid, "agent": "codex" }),
        );
        assert!(
            result.is_ok(),
            "cross-scope resume must be allowed: {result:?}"
        );
    }

    #[test]
    fn collab_resume_from_cold_start_restores_the_failed_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let old_owner = test_app_with_db_path(db_path.clone(), dir.path());

        let old_session = start_session_in_scope(&old_owner, "/tmp/repo", "main");
        drive_to_tooling_coding_failed(&old_owner, &old_session);

        // A cold process has no in-memory binding. The durable failed session
        // still owns its scope, so it must be able to resume instead of being
        // stranded by a newer same-scope start.
        let cold_resumer = test_app_with_db_path(db_path, dir.path());
        let out = handle_collab_resume(
            &cold_resumer,
            &json!({ "session_id": old_session, "agent": "codex" }),
        )
        .unwrap();

        assert_eq!(out["ok"], json!(true));
        assert_eq!(
            cold_resumer
                .db
                .collab_load_session_record(&old_session)
                .unwrap()
                .session
                .phase,
            Phase::CodeImplementPending,
            "resume must restore the recorded phase"
        );
        assert_eq!(
            cold_resumer
                .active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .as_deref(),
            Some(old_session.as_str()),
            "the cold resumer must restore its process-local scope binding"
        );
    }

    /// A semantic-failure (`subagent_failure:`) `CodingFailed` session is
    /// rejected with the deterministic `NotResumable` error, surfaced as a
    /// validation error whose text names the session as unresumable.
    #[test]
    fn collab_resume_rejects_semantic_failure_session() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        // claude is current_owner right after task_list; subagent_failure:
        // is not off-turn-admissible, so it must be sent on-turn.
        send(
            &app,
            &sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"subagent_failure: 1: env"}"#,
        );

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::CodingFailed);

        let err = handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "claude" }))
            .unwrap_err();

        assert!(
            err.to_string().contains("session cannot be resumed"),
            "expected the deterministic NotResumable error text, got: {err}"
        );
    }

    /// A `branch_drift:` semantic failure is likewise rejected — proves the
    /// classification gate, not just the specific prefix used above.
    #[test]
    fn collab_resume_rejects_branch_drift_session() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        // `branch_drift:` is off-turn admissible, so codex (not the current
        // owner claude) may report it directly.
        send(
            &app,
            &sid,
            "codex",
            "failure_report",
            r#"{"coding_failure":"branch_drift: head_sha abc not found"}"#,
        );

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::CodingFailed);

        let err = handle_collab_resume(&app, &json!({ "session_id": sid, "agent": "codex" }))
            .unwrap_err();

        assert!(
            err.to_string().contains("session cannot be resumed"),
            "expected the deterministic NotResumable error text, got: {err}"
        );
    }

    // ── Task 9: status exposure + off-turn admission ────────────────────────

    /// Required acceptance criterion: `collab_status` on a recovering session
    /// reports the unchanged phase, `current_owner = claude`, a non-null
    /// `pending_failure`, and a null `coding_failure`. Drives a real
    /// `git_commit_failed:` on-turn report from Codex in
    /// `CodeReviewFixGlobalPending` (Codex owns that phase after
    /// `implementation_done`), which flips ownership to the counterpart
    /// (Claude) without moving the phase.
    #[test]
    fn collab_status_on_recovering_session_reports_pending_failure_not_coding_failure() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);
        send(
            &app,
            &sid,
            "claude",
            "implementation_done",
            r#"{"head_sha":"c1"}"#,
        );

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::CodeReviewFixGlobalPending);
        assert_eq!(record.session.current_owner, crate::collab::Agent::Codex);

        send(
            &app,
            &sid,
            "codex",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: index.lock EPERM"}"#,
        );

        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();

        assert_eq!(
            status["phase"],
            json!("CodeReviewFixGlobalPending"),
            "a recoverable report must leave phase unchanged"
        );
        assert_eq!(status["current_owner"], json!("claude"));
        assert_eq!(
            status["pending_failure"],
            json!("git_commit_failed: index.lock EPERM")
        );
        assert_eq!(
            status["coding_failure"],
            Value::Null,
            "coding_failure must stay null for a recoverable (non-terminal) report"
        );
        assert_eq!(status["failed_from_phase"], Value::Null);
        assert_eq!(
            status["recovery_phase"],
            json!("CodeReviewFixGlobalPending")
        );
        assert_eq!(status["recovery_owner"], json!("claude"));
        assert_eq!(status["recovery_attempts"], json!(1));
        // The origin is what separates a completion event produced by the
        // delegated recovery owner from one produced by the phase's own
        // expected agent — `recovery_owner` alone cannot express that.
        assert_eq!(status["recovery_origin_owner"], json!("codex"));
        // The lifetime counter tracks the per-resume budget on the first
        // handoff and diverges from it only after a resume, so this assertion
        // pins its presence; `state_machine::tests` covers the divergence.
        assert_eq!(status["total_recovery_attempts"], json!(1));
    }

    /// Required acceptance criterion: the branch-drift off-turn path behaves
    /// exactly as before. A non-owner (`codex`, while `claude` owns
    /// `CodeImplementPending`) may send a `branch_drift:` failure_report
    /// off-turn; this must still succeed and still land the session in
    /// `CodingFailed` (branch drift stays `Terminal` — Task 9 does not widen
    /// its classification or its off-turn admissibility).
    #[test]
    fn branch_drift_off_turn_admission_is_unchanged() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.current_owner, crate::collab::Agent::Claude);

        // Codex is NOT current_owner here, but branch_drift: is off-turn
        // admissible — this must succeed exactly as it did before task 9.
        send(
            &app,
            &sid,
            "codex",
            "failure_report",
            r#"{"coding_failure":"branch_drift: head_sha abc not found"}"#,
        );

        let after = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(after.session.phase, Phase::CodingFailed);
        assert_eq!(
            after.session.coding_failure.as_deref(),
            Some("branch_drift: head_sha abc not found")
        );
    }

    /// Regression guard for the off-turn gate itself: task 9 must NOT widen
    /// `failure_report_is_off_turn_admissible` to admit the five new
    /// recoverable-but-not-off-turn prefixes (`git_commit_failed:` etc). An
    /// off-turn `git_commit_failed:` report from a non-owner must still be
    /// rejected as `NotYourTurn`, same as before task 9 existed.
    #[test]
    fn off_turn_git_commit_failed_report_is_still_rejected() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.current_owner, crate::collab::Agent::Claude);

        // Codex is NOT current_owner and git_commit_failed: is not in
        // OFF_TURN_FAILURE_PREFIXES — this must be rejected before
        // apply_event ever classifies it.
        let err = handle_collab_send(
            &app,
            &json!({
                "session_id": sid,
                "sender": "codex",
                "topic": "failure_report",
                "content": r#"{"coding_failure":"git_commit_failed: index.lock EPERM"}"#,
            }),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("not your turn"),
            "expected a not-your-turn rejection, got: {err}"
        );

        // Confirm the session was untouched by the rejected send.
        let after = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(after.session.phase, Phase::CodeImplementPending);
        assert_eq!(after.session.pending_failure, None);
    }

    // ── Task 11: end-to-end MCP recovery regression ─────────────────────────

    /// Codex review note 3: prove the full recovery path through the
    /// tool-level turn gate in `handle_collab_send`, not just through
    /// `apply_event` directly (already covered by `state_machine::tests`
    /// from Task 5). `handle_collab_send` has its OWN pre-`apply_event` turn
    /// gate (`sender == session.current_owner`) that is a separate, earlier
    /// check from `apply_event`'s `require_actor_or_recovery`. This test
    /// exercises the whole delegated-completion sequence: Codex reports a
    /// recoverable `git_commit_failed:` failure from `CodeReviewFixGlobalPending`
    /// (which flips `current_owner` to Claude per Task 4), then Claude sends
    /// `review_fix_global` to complete Codex's interrupted turn. Claude's
    /// send only succeeds because `current_owner` was already flipped to
    /// Claude in the DB by the time it lands — if a future refactor changes
    /// `handle_collab_send`'s turn gate to check a different field, or
    /// tightens it without accounting for recovery-flipped ownership, this
    /// test fails at the `send()` `.unwrap()` inside the helper, which is
    /// the correct failure mode.
    #[test]
    fn full_recovery_path_through_tool_level_turn_gate_clears_on_delegated_completion() {
        let _g = metrics_on_guard();
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        // 1. Claude finishes implementation → CodeReviewFixGlobalPending, Codex owns.
        send(
            &app,
            &sid,
            "claude",
            "implementation_done",
            r#"{"head_sha":"c1"}"#,
        );
        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::CodeReviewFixGlobalPending);
        assert_eq!(record.session.current_owner, crate::collab::Agent::Codex);

        // 2. Codex hits a recoverable tooling failure mid-turn.
        send(
            &app,
            &sid,
            "codex",
            "failure_report",
            r#"{"coding_failure":"git_commit_failed: index.lock EPERM"}"#,
        );

        // 3. collab_status reflects the recovering session: phase unchanged,
        //    ownership flipped to Claude, pending_failure set, coding_failure
        //    null, and no failed outcome recorded.
        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();
        assert_eq!(
            status["phase"],
            json!("CodeReviewFixGlobalPending"),
            "a recoverable report must leave phase unchanged"
        );
        assert_eq!(status["current_owner"], json!("claude"));
        assert_eq!(
            status["pending_failure"],
            json!("git_commit_failed: index.lock EPERM")
        );
        assert_eq!(
            status["coding_failure"],
            Value::Null,
            "coding_failure must stay null for a recoverable (non-terminal) report"
        );
        let outcome_row = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            outcome_row.outcome, None,
            "a recoverable report must not record a failed outcome"
        );

        // 4. Claude completes Codex's interrupted turn. This send MUST
        //    succeed — if the tool-level owner gate incorrectly rejects
        //    Claude here, `send()`'s `.unwrap()` panics, which is the
        //    correct failure mode for this regression test.
        send(
            &app,
            &sid,
            "claude",
            "review_fix_global",
            r#"{"head_sha":"c2"}"#,
        );

        // 5. Phase advances and all recovery state clears.
        let status = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();
        assert_eq!(status["phase"], json!("CodeReviewLocalPending"));
        assert_eq!(status["current_owner"], json!("claude"));
        assert_eq!(status["pending_failure"], Value::Null);
        assert_eq!(status["failed_from_phase"], Value::Null);
        assert_eq!(status["recovery_phase"], Value::Null);
        assert_eq!(status["recovery_owner"], Value::Null);
        assert_eq!(status["recovery_origin_owner"], Value::Null);
        assert_eq!(status["recovery_attempts"], json!(0));
        // The lifetime counter is the one field a successful delegated
        // completion must NOT clear — it is what bounds a session across
        // resumes, so a reset here would silently reopen the loop the
        // counter exists to close.
        assert_eq!(status["total_recovery_attempts"], json!(1));
    }

    #[test]
    fn off_turn_codex_dispatch_failure_hands_recovery_to_claude() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);
        send(
            &app,
            &sid,
            "claude",
            "implementation_done",
            r#"{"head_sha":"impl-head"}"#,
        );

        // Claude observes that Codex never ran its global-review turn. This
        // is the one recoverable failure that is valid from off-turn.
        send(
            &app,
            &sid,
            "claude",
            "failure_report",
            r#"{"coding_failure":"codex_dispatch_failed: process exited 137"}"#,
        );

        let recovering = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();
        assert_eq!(recovering["phase"], json!("CodeReviewFixGlobalPending"));
        assert_eq!(recovering["current_owner"], json!("claude"));
        assert_eq!(recovering["recovery_owner"], json!("claude"));

        // Claude can now complete the interrupted Codex-owned phase exactly
        // once instead of the protocol returning control to unavailable Codex.
        send(
            &app,
            &sid,
            "claude",
            "review_fix_global",
            r#"{"head_sha":"recovered-head"}"#,
        );
        let after = handle_collab_status(&app, &json!({ "session_id": sid })).unwrap();
        assert_eq!(after["phase"], json!("CodeReviewLocalPending"));
        assert_eq!(after["pending_failure"], Value::Null);
    }

    #[test]
    fn codex_cannot_report_dispatch_failure_while_claude_owns_the_turn() {
        let app = test_app();
        let sid = start_session(&app);
        drive_to_implement(&app, &sid);

        let err = handle_collab_send(
            &app,
            &json!({
                "session_id": sid,
                "sender": "codex",
                "topic": "failure_report",
                "content": r#"{"coding_failure":"codex_dispatch_failed: fabricated report"}"#,
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not your turn"));

        let after = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(after.session.phase, Phase::CodeImplementPending);
        assert_eq!(after.session.current_owner, crate::collab::Agent::Claude);
        assert_eq!(after.session.pending_failure, None);
    }
    // ── pilot=codex MCP-surface coverage (issue #246, Task 6) ────────────────
    //
    // `collab_start` does accept a `pilot` argument now, but these tests
    // deliberately bypass it: they start a default session and rebind
    // `pilot` directly on the stored row — the same direct-field-write trick
    // the legacy-drawer tests above use. That keeps them a *storage-level*
    // pin, so the authorization rules stay proven for any session whose
    // stored `pilot` is codex no matter which path set it there
    // (`collab_start`'s argument, `collab_set_pilot`, or a direct write).
    // The argument path itself is covered end-to-end in
    // `tests/mcp_protocol.rs`. Every
    // authorization assertion is mirrored under `pilot=claude`, because "no
    // new role combination is accepted" is only provable two-directionally:
    // a one-sided table cannot distinguish "the copilot approves" from
    // "both agents approve".

    /// Rebind a started session's `pilot` role in place. Only `pilot` is
    /// touched: `implementer` is an independent knob and none of these tests
    /// reach a coding phase.
    fn set_pilot(app: &crate::mcp::app::App, sid: &str, pilot: Agent) {
        let mut session = app.db.collab_load_session(sid).unwrap();
        session.pilot = pilot;
        app.db.collab_save_session(&session).unwrap();
    }

    /// Drive a `pilot`-led session to `PlanCopilotReviewPending` and return
    /// the canonical plan hash `collab_approve` must be called with. Drafts
    /// are agent-keyed (either side may go first); synthesis is the pilot's.
    fn drive_to_copilot_review(app: &crate::mcp::app::App, sid: &str, pilot: Agent) -> String {
        send(app, sid, "claude", "draft", "claude draft");
        send(app, sid, "codex", "draft", "codex draft");
        send(app, sid, pilot.as_str(), "canonical", "canonical plan");
        sha256_hex("canonical plan")
    }

    fn approve(
        app: &crate::mcp::app::App,
        sid: &str,
        agent: Agent,
        content_hash: &str,
    ) -> Result<Value, MemoryError> {
        handle_collab_approve(
            app,
            &json!({
                "session_id": sid,
                "agent": agent.as_str(),
                "content_hash": content_hash,
            }),
        )
    }

    /// Topics currently queued for `receiver`. Deliberately does not auto-ack,
    /// so the same inbox can be inspected more than once per test.
    fn inbox_topics(app: &crate::mcp::app::App, sid: &str, receiver: Agent) -> Vec<String> {
        let out = handle_collab_recv(
            app,
            &json!({ "session_id": sid, "receiver": receiver.as_str(), "limit": 50 }),
        )
        .unwrap();
        out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["topic"].as_str().unwrap().to_string())
            .collect()
    }

    /// The most recent `collab_approve` WAL row as `(params, result)`.
    fn last_approve_wal(app: &crate::mcp::app::App) -> (Value, Value) {
        let conn = rusqlite::Connection::open(&app.config.db_path).unwrap();
        let (params, result): (String, String) = conn
            .query_row(
                "SELECT params, result FROM wal_log WHERE operation = 'collab_approve' \
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (
            serde_json::from_str(&params).unwrap(),
            serde_json::from_str(&result).unwrap(),
        )
    }

    #[test]
    fn approve_under_pilot_codex_lets_claude_approve_and_routes_review_to_codex() {
        let app = test_app();
        let sid = start_session(&app);
        set_pilot(&app, &sid, Agent::Codex);
        let hash = drive_to_copilot_review(&app, &sid, Agent::Codex);

        let out = approve(&app, &sid, Agent::Claude, &hash).unwrap();
        assert_eq!(out["phase"], json!("PlanClaudeFinalizePending"));

        assert!(
            inbox_topics(&app, &sid, Agent::Codex).contains(&"review".to_string()),
            "under pilot=codex the approval must be routed claude→codex"
        );
        assert!(
            !inbox_topics(&app, &sid, Agent::Claude).contains(&"review".to_string()),
            "the approver must not be queued its own review message"
        );

        // The WAL payload shape is pilot-independent; only `agent` differs.
        let (params, result) = last_approve_wal(&app);
        assert_eq!(
            params,
            json!({ "session_id": sid, "agent": "claude", "content_hash": hash })
        );
        assert_eq!(result, json!({ "phase": "PlanClaudeFinalizePending" }));
    }

    #[test]
    fn approve_under_pilot_claude_is_unchanged() {
        // Byte-for-byte pin of today's behavior: `pilot` defaults to Claude,
        // Codex approves, and the review is routed codex→claude.
        let app = test_app();
        let sid = start_session(&app);
        assert_eq!(
            app.db.collab_load_session(&sid).unwrap().pilot,
            Agent::Claude
        );
        let hash = drive_to_copilot_review(&app, &sid, Agent::Claude);

        let out = approve(&app, &sid, Agent::Codex, &hash).unwrap();
        assert_eq!(out["phase"], json!("PlanClaudeFinalizePending"));

        assert!(inbox_topics(&app, &sid, Agent::Claude).contains(&"review".to_string()));
        assert!(!inbox_topics(&app, &sid, Agent::Codex).contains(&"review".to_string()));

        let record = app.db.collab_load_session_record(&sid).unwrap();
        assert_eq!(record.session.phase, Phase::PlanFinalizePending);
        assert_eq!(record.session.current_owner, Agent::Claude);
        assert_eq!(
            record.session.codex_review_verdict.as_deref(),
            Some("approve")
        );

        let (params, result) = last_approve_wal(&app);
        assert_eq!(
            params,
            json!({ "session_id": sid, "agent": "codex", "content_hash": hash })
        );
        assert_eq!(result, json!({ "phase": "PlanClaudeFinalizePending" }));
    }

    /// Two-directional negative-authorization table for `collab_approve`.
    /// Under *each* pilot assignment the copilot is the only accepted
    /// approver: the pilot is refused by the handler gate, refused again by
    /// the state machine underneath it, and the accepted call routes the
    /// `review` message in exactly one direction.
    #[test]
    fn approve_authorization_table_is_two_directional() {
        for pilot in [Agent::Claude, Agent::Codex] {
            let app = test_app();
            let sid = start_session(&app);
            set_pilot(&app, &sid, pilot);
            let hash = drive_to_copilot_review(&app, &sid, pilot);
            let expected = collab_counterpart(pilot);

            // Wrong role: refused by the handler's role gate, naming the
            // agent that actually may approve.
            let err = approve(&app, &sid, pilot, &hash).unwrap_err();
            assert!(
                err.to_string().contains(&format!(
                    "agent must be '{}' for collab_approve",
                    expected.as_str()
                )),
                "pilot={pilot} gate must name the expected approver, got: {err}"
            );

            // …and refused again by `apply_event`'s `SubmitReview` arm, which
            // is the primary enforcement point. The handler gate is
            // defense-in-depth on top of this, not a substitute for it.
            let session = app.db.collab_load_session(&sid).unwrap();
            let state_machine_err = apply_event(
                &session,
                pilot,
                &CollabEvent::SubmitReview {
                    verdict: "approve".to_string(),
                },
            )
            .unwrap_err();
            assert!(
                matches!(state_machine_err, CollabError::NotYourTurn { .. }),
                "pilot={pilot} state machine must reject the wrong approver with NotYourTurn, \
                 got: {state_machine_err:?}"
            );

            // The refused call must have left the session untouched.
            let record = app.db.collab_load_session_record(&sid).unwrap();
            assert_eq!(record.session.phase, Phase::PlanCopilotReviewPending);
            assert_eq!(record.session.codex_review_verdict, None);
            assert!(!inbox_topics(&app, &sid, Agent::Claude).contains(&"review".to_string()));
            assert!(!inbox_topics(&app, &sid, Agent::Codex).contains(&"review".to_string()));

            // Right role: accepted, and routed to the pilot only.
            let out = approve(&app, &sid, expected, &hash).unwrap();
            assert_eq!(out["phase"], json!("PlanClaudeFinalizePending"));
            assert!(inbox_topics(&app, &sid, pilot).contains(&"review".to_string()));
            assert!(!inbox_topics(&app, &sid, expected).contains(&"review".to_string()));
        }
    }

    #[test]
    fn approve_still_rejects_a_content_hash_mismatch_under_pilot_codex() {
        // The `canonical_plan_hash` check is unrelated to role assignment and
        // must survive the de-hardcoding of the gate above it.
        let app = test_app();
        let sid = start_session(&app);
        set_pilot(&app, &sid, Agent::Codex);
        drive_to_copilot_review(&app, &sid, Agent::Codex);

        let err = approve(&app, &sid, Agent::Claude, "deadbeef").unwrap_err();
        assert!(err
            .to_string()
            .contains("content_hash does not match canonical_plan_hash"));
    }

    #[test]
    fn blind_draft_suppression_holds_for_both_receivers_regardless_of_pilot() {
        // The suppression keys on the *receiving* agent's own draft-hash
        // column, which is identity-keyed rather than role-keyed. To prove
        // that in fact rather than by assertion, this test exercises both
        // pilot assignments (outer loop) crossed with both draft-first
        // orderings (inner loop) and checks that suppression behaves
        // identically in all four cases. Both receiver directions are
        // exercised because a one-sided test cannot tell an identity-keyed
        // rule apart from one that happens to name the right agent, and both
        // pilot values are exercised because a predicate that secretly keyed
        // on `session.pilot` would otherwise pass undetected.
        for pilot in [Agent::Claude, Agent::Codex] {
            for first_drafter in [Agent::Claude, Agent::Codex] {
                let app = test_app();
                let sid = start_session(&app);
                set_pilot(&app, &sid, pilot);
                let waiting = collab_counterpart(first_drafter);

                send(&app, &sid, first_drafter.as_str(), "draft", "first draft");
                assert!(
                    inbox_topics(&app, &sid, waiting).is_empty(),
                    "pilot={pilot}: {waiting} must not see {first_drafter}'s draft before \
                     submitting its own"
                );

                send(&app, &sid, waiting.as_str(), "draft", "second draft");
                assert!(
                    inbox_topics(&app, &sid, waiting).contains(&"draft".to_string()),
                    "pilot={pilot}: the counterpart's draft must appear once {waiting} has drafted"
                );
            }
        }
    }

    /// The whole point of `orphan_recovered` is that it records an incident
    /// without being one. A `failure_report` would park the phase, hand the
    /// turn to the counterpart, and burn `recovery_attempts` against a ceiling
    /// of 2 — but the worker sending this has *succeeded*: it preserved a dead
    /// turn's work and is carrying on. Assert the entire mutable session record
    /// is untouched, not just the phase, so a future change that quietly
    /// advances a counter fails here.
    #[test]
    fn orphan_recovered_records_the_incident_without_advancing_the_session() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");

        let before = app.db.collab_load_session_record(&sid).unwrap().session;
        send(
            &app,
            &sid,
            "claude",
            "orphan_recovered",
            r#"{"phase":"PlanParallelDrafts","recovered_sha":"deadbeef",
                "detail":"dirty worktree on a normal turn"}"#,
        );
        let after = app.db.collab_load_session_record(&sid).unwrap().session;

        assert_eq!(before.phase, after.phase, "phase must not advance");
        assert_eq!(
            before.current_owner, after.current_owner,
            "the turn must not change hands"
        );
        assert_eq!(
            before.recovery_attempts, after.recovery_attempts,
            "recording an orphan must not spend the recovery budget"
        );
        assert_eq!(
            before.total_recovery_attempts, after.total_recovery_attempts,
            "recording an orphan must not spend the lifetime recovery budget"
        );
        assert_eq!(
            before.pending_failure, after.pending_failure,
            "orphan_recovered is not a failure report"
        );
    }

    /// A record nobody can see is not a record. `collab_status` is the one
    /// surface the orchestrator and the human both read, so the incident count
    /// has to show up there rather than only in the `messages` table.
    #[test]
    fn collab_status_surfaces_recorded_orphans() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");

        let clean = handle_collab_status(&app, &json!({ "session_id": &sid })).unwrap();
        assert_eq!(
            clean["orphans_recovered"], 0,
            "a session with no incident must report zero, not null"
        );

        send(
            &app,
            &sid,
            "claude",
            "orphan_recovered",
            r#"{"detail":"dirty worktree on a normal turn"}"#,
        );

        let after = handle_collab_status(&app, &json!({ "session_id": &sid })).unwrap();
        assert_eq!(
            after["orphans_recovered"], 1,
            "the recorded incident must be visible from collab_status"
        );
    }

    /// An orphan record is a record, not correspondence. If it landed in the
    /// counterpart's inbox it would be handed to the next worker that calls
    /// `collab_recv`, whose templates enforce a one-recv rule and expect a
    /// specific topic — so a recovery incident would corrupt the next turn's
    /// input.
    #[test]
    fn orphan_recovered_never_lands_in_either_inbox() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(
            &app,
            &sid,
            "claude",
            "orphan_recovered",
            r#"{"detail":"dirty worktree on a normal turn"}"#,
        );

        for receiver in [Agent::Claude, Agent::Codex] {
            assert!(
                !inbox_topics(&app, &sid, receiver).contains(&"orphan_recovered".to_string()),
                "{receiver} must not be delivered an orphan record"
            );
        }
    }

    /// Defense in depth: even if a future caller reaches the event builder with
    /// this topic, there is no `CollabEvent` for it and there must never be.
    #[test]
    fn orphan_recovered_never_becomes_a_collab_event() {
        let err = super::super::collab_events::build_collab_event(
            "orphan_recovered",
            "{}",
            crate::collab::Phase::PlanParallelDrafts,
        )
        .expect_err("orphan_recovered must not construct an event");
        assert!(
            err.to_string().contains("orphan_recovered"),
            "the rejection should name the topic, got: {err}"
        );
    }

    /// It is the sender's own turn when they find the dirty worktree, so the
    /// ordinary turn gate applies with no `failure_report`-style off-turn
    /// carve-out.
    #[test]
    fn orphan_recovered_is_rejected_off_turn() {
        let app = test_app();
        let sid = start_session(&app);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");
        // Synthesis is Claude's turn under the default pilot; Codex is off-turn.
        let err = handle_collab_send(
            &app,
            &json!({ "session_id": &sid, "sender": "codex",
                     "topic": "orphan_recovered", "content": "{}" }),
        )
        .expect_err("off-turn orphan_recovered must be rejected");
        assert!(
            err.to_string().contains("not your turn"),
            "expected the standard turn gate, got: {err}"
        );
    }

    #[test]
    fn collab_send_under_pilot_codex_routes_to_the_counterpart() {
        let app = test_app();
        let sid = start_session(&app);
        set_pilot(&app, &sid, Agent::Codex);
        send(&app, &sid, "claude", "draft", "claude draft");
        send(&app, &sid, "codex", "draft", "codex draft");

        // Synthesis is the pilot's turn under any assignment, so the
        // canonical plan is sent by Codex here and must land in Claude's inbox.
        send(&app, &sid, "codex", "canonical", "canonical plan");
        assert!(inbox_topics(&app, &sid, Agent::Claude).contains(&"canonical".to_string()));
        assert!(!inbox_topics(&app, &sid, Agent::Codex).contains(&"canonical".to_string()));
    }
}
