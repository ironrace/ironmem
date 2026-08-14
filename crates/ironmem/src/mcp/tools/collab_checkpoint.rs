//! The `collab_checkpoint` MCP tool — the durable, first-class write path for
//! v3 batch implementation progress (issue #273).
//!
//! This replaces the `add_drawer(logical_key="collab-checkpoint:<id>")`
//! convention. The difference that matters is not the storage medium but the
//! enforcement: a checkpoint written here is what `implementation_done` will
//! demand as proof (Task 7), so a controller can no longer commit and hand off
//! while the recorded progress stays frozen at task 1.
//!
//! The tool **reports** head divergence, it never **refuses** on it. Writing a
//! checkpoint is how an operator *fixes* drift, so a write that failed on drift
//! would make the recovery path unreachable — and would defeat the
//! operator-attested escape hatch (`attested_by=operator` plus
//! `acknowledged_divergence`) that Task 10 builds on. Enforcement belongs at
//! the gate that decides what to do with a checkpoint, not at the write.
//!
//! What it *does* refuse is a caller who is not the current process for the
//! agent it claims to be — the generation lease every other session-scoped
//! collab write takes. See [`handle_collab_checkpoint`].

use serde_json::{json, Value};

use crate::collab::CollabCheckpoint;
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::collab_session::HeadCheck;
use super::shared::{require_agent, require_str};

/// Write the session's current checkpoint.
///
/// `agent` is **required**, and the generation lease below is why. A checkpoint
/// is a durable, session-scoped write, so a superseded process — one whose
/// successor has already claimed the session via `session_handoff` — must not
/// be able to land its stale view of progress. Nothing downstream can undo
/// that: `updated_at` is server-stamped, so a stale process's content arrives
/// carrying a *fresh* timestamp, and a Task 7 gate asking "is this checkpoint
/// recent?" would be answered by the very anti-backdating stamp that exists to
/// stop exactly this. The check has to be at the write, and it cannot be
/// optional-when-supplied: an authorization check a caller may omit is not a
/// check, it is a suggestion.
///
/// This is not an obstacle to Task 10's operator-attested backfill. An
/// operator backfill *is* a takeover by a non-incumbent process, which is what
/// `session_handoff` plus `handoff_token` already exists to authorize and
/// audit. An `agent`-less operator path would be an unauthenticated bypass of
/// the head-consistency gate itself, since anyone reaching it could write
/// `attested_by=operator` with a fabricated range —
/// [`CollabCheckpoint::validate`] checks that range is non-blank, not that it
/// is real.
///
/// Deliberately does NOT call `ensure_no_conflicting_process_session`, which
/// seven sibling handlers do: that guard rebinds this process's metrics
/// attribution scope, and a checkpoint is a progress record rather than a turn
/// — it neither starts nor claims a session, so there is no attribution to
/// move. The lease above, not that guard, is what makes this call authorized.
pub(super) fn handle_collab_checkpoint(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let agent = require_agent(require_str(args, "agent")?)?;

    // Unlike its neighbours this handler does NOT pull `session_id` out with
    // `shared::require_str` first. The payload parser reads the same key and
    // refuses a strict superset of what that helper does — absent, non-string,
    // blank, *and* the `none` absent-sentinel — with a message that says which
    // of those went wrong, so a second reading could only ever subtract
    // information or disagree. And disagree it would: the two differ on
    // trimming, so a `" <id> "` transcribed out of a turn template would parse
    // into one session id and be looked up under another. Keeping a single
    // reading makes the session whose liveness is checked the same session the
    // row is keyed by, by construction rather than by a comparison.
    let checkpoint = CollabCheckpoint::from_json(args)
        .map_err(|err| MemoryError::Validation(err.to_string()))?;
    let session_id = checkpoint.session_id.as_str();

    // The git read sits OUTSIDE the transaction on purpose. `with_transaction`
    // replays its closure on `SQLITE_BUSY_SNAPSHOT`, and its doc comment
    // enumerates every closure that reaches outside the database precisely
    // because a `Command::output()` there holds the write transaction open
    // across a process spawn — and, sitting between the reads that pin the
    // snapshot and the first write, widens the very window that triggers the
    // retry. Nothing about reading HEAD needs the transaction's snapshot: the
    // repo is not in the database, and `repo_path` is fixed at session start.
    let record = app.db.with_connection(|conn| {
        crate::collab::queue::ensure_active(conn, session_id)?;
        crate::collab::queue::load_session_record(conn, session_id)
    })?;
    let head_check = HeadCheck::read(&record.repo_path, &checkpoint);

    let (claim, updated_at) = app.db.with_transaction(|tx| {
        // Inside the transaction because a token claim is itself a DB write
        // that must be atomic with the upsert it authorizes — a claim that
        // committed while the checkpoint rolled back would burn a one-time
        // token for nothing. Replay-safe under `SQLITE_BUSY_SNAPSHOT` for the
        // reason `with_transaction`'s exception 1 already documents.
        let claim = super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        // Re-checked under the write transaction: the read above resolved
        // `repo_path` for the git call, and the session could have ended
        // between the two. Deliberately left unpinned by any test — driving
        // the interleaving from one would take a second connection racing a
        // `collab_end` between two statements of this handler, which is more
        // machinery than the check is worth; it is defense in depth over the
        // read above, not the only thing standing between an ended session and
        // a write.
        crate::collab::queue::ensure_active(tx, session_id)?;
        // Unconditional last-writer-wins, with no `updated_at` guard. The
        // hazard that carries is NOT a stale in-memory checkpoint — this tool
        // takes a fully-formed payload and never does read-modify-write — it is
        // a stale *process*: a superseded agent overwriting its successor's
        // progress, and doing it with a fresh server stamp. That is precisely
        // what the generation lease above refuses, and it is the reason this
        // handler takes one. `upsert_checkpoint` calls `validate()` itself, so
        // nothing is repeated here.
        crate::collab::queue::upsert_checkpoint(tx, &checkpoint)?;
        // Read the stamp back rather than computing a second clock value here,
        // so the response reports what the row actually says. Doubles as a
        // read-back of the row just written: `load_current_checkpoint`
        // re-validates it, so a combination that would poison every later read
        // fails here, inside the transaction that wrote it.
        let updated_at = crate::collab::queue::load_current_checkpoint(tx, session_id)?
            .ok_or_else(|| {
                // Unreachable: the upsert above ran in this transaction. Db
                // rather than Validation because the caller did nothing wrong;
                // it renders as an opaque internal error and is logged in full.
                MemoryError::Db(rusqlite::Error::QueryReturnedNoRows)
            })?
            .updated_at;

        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_checkpoint",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "status": checkpoint.status.as_str(),
                "head_sha": checkpoint.head_sha,
                "attested_by": checkpoint.attested_by.as_str(),
            }),
            // The audit trail records the head check in the same three states
            // the response does: an entry saying `diverged: false` where the
            // check never ran would be a false record of a verification.
            Some(&json!({
                "completed_task_ids": checkpoint.completed_task_ids,
                "diverged": head_check.diverged(),
                "head_check": head_check.label(),
            })),
        )?;

        Ok((claim, updated_at))
    })?;
    // Only after the commit — publishing a claim whose transaction may still
    // roll back is the cache poisoning `GenerationClaim` exists to prevent.
    claim.publish(app);

    let mut response = json!({
        "session_id": session_id,
        "agent": agent.as_str(),
        "status": checkpoint.status.as_str(),
        "head_sha": checkpoint.head_sha,
        // Echoed so a caller can see the server's stamp rather than trusting a
        // bare success: it is the anti-backdating field, and the one a resumer
        // uses to tell a fresh checkpoint from a frozen one.
        "updated_at": updated_at,
        "diverged": head_check.diverged(),
        "head_check": head_check.label(),
        "repo_head_sha": head_check.repo_head_sha(),
    });
    if let Some(detail) = head_check.unreadable_detail() {
        // Only present when there is something to explain, and always
        // alongside `head_check: "unreadable"` — the caller is told why the
        // check could not run, not left to infer it from a missing verdict.
        response["head_check_error"] = json!(detail);
    }
    Ok(response)
}
