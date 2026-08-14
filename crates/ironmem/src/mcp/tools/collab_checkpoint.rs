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

use serde_json::{json, Value};

use crate::collab::CollabCheckpoint;
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::shared::require_str;

/// What comparing the checkpoint against live git HEAD actually established.
///
/// Three states, deliberately not two. `checkpoint_divergence` collapses
/// "checked, no drift" and "could not check" into a single `None` — which is
/// safe for the gate paths it was written for, and wrong here: git missing from
/// `PATH`, an unreadable repo, or a path that is not a repo at all are exactly
/// the environments where a checkpoint is most likely stale, and answering
/// `diverged: false` there reports an unverified claim as verified. That is a
/// smaller instance of the failure issue #273 exists to end, so this tool calls
/// [`super::collab_session::git_head_sha`] directly — as that function's doc
/// comment instructs any caller that must tell the two apart — and reports the
/// third state as itself.
enum HeadCheck {
    /// Live HEAD was read. `diverged` is a real finding either way.
    Checked {
        repo_head_sha: String,
        diverged: bool,
    },
    /// Live HEAD could not be read, so nothing about drift is known. An
    /// *operational* failure, distinct from a stale checkpoint — hence not an
    /// error: it must not cost the caller a write it can no longer retry
    /// accurately, and the checkpoint is still the best record available.
    Unreadable { detail: String },
}

impl HeadCheck {
    fn read(repo_path: &str, checkpoint: &CollabCheckpoint) -> Self {
        match super::collab_session::git_head_sha(repo_path) {
            Ok(head) => Self::Checked {
                diverged: head != checkpoint.head_sha,
                repo_head_sha: head,
            },
            // `git_head_sha` reports every failure as `Validation`, whose
            // `Display` prefixes "Validation error:" — misleading for what is
            // an environment problem, and about the repo rather than the
            // caller's arguments. Unwrap that one variant; anything else keeps
            // its full rendering rather than being silently reshaped.
            Err(MemoryError::Validation(detail)) => Self::Unreadable { detail },
            Err(other) => Self::Unreadable {
                detail: other.to_string(),
            },
        }
    }

    /// `true`, `false`, or JSON `null` for "the check did not run". A caller
    /// that treats the value as a plain boolean reads `null` as falsy, which is
    /// why [`Self::label`] is reported beside it in words.
    fn diverged(&self) -> Value {
        match self {
            Self::Checked { diverged, .. } => json!(diverged),
            Self::Unreadable { .. } => Value::Null,
        }
    }

    /// Whether the check ran at all — the field that keeps `diverged: null`
    /// from being read as "no drift".
    fn label(&self) -> &'static str {
        match self {
            Self::Checked { .. } => "checked",
            Self::Unreadable { .. } => "unreadable",
        }
    }

    /// The HEAD actually read, so a caller told it has drifted can file an
    /// accurate checkpoint without shelling out to git itself.
    fn repo_head_sha(&self) -> Value {
        match self {
            Self::Checked { repo_head_sha, .. } => json!(repo_head_sha),
            Self::Unreadable { .. } => Value::Null,
        }
    }
}

pub(super) fn handle_collab_checkpoint(app: &App, args: &Value) -> Result<Value, MemoryError> {
    // Presence-checked here purely so an absent `session_id` fails with this
    // module's own message before the payload parse does. The parsed
    // `checkpoint.session_id` below — trimmed, sentinel-checked, and validated
    // non-blank — is THE session id from here on: it addresses the
    // existence check, keys the row, and is echoed back, so the session whose
    // liveness was verified is always the session the row is written under.
    // Reading the argument a second time would reintroduce that gap for the
    // price of a `trim()` disagreement between two parsers.
    require_str(args, "session_id")?;

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

    app.db.with_transaction(|tx| {
        // Re-checked under the write transaction: the read above resolved
        // `repo_path` for the git call, and the session could have ended
        // between the two.
        crate::collab::queue::ensure_active(tx, session_id)?;
        // Unconditional last-writer-wins, with no `updated_at` guard — safe
        // here because this tool writes a fully-formed payload rather than
        // doing a read-modify-write, so there is no stale in-memory checkpoint
        // to overwrite a newer stored one with. It also calls `validate()`
        // itself, so nothing is repeated here.
        crate::collab::queue::upsert_checkpoint(tx, &checkpoint)?;

        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_checkpoint",
            &json!({
                "session_id": session_id,
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

        Ok(())
    })?;

    let mut response = json!({
        "session_id": session_id,
        "status": checkpoint.status.as_str(),
        "head_sha": checkpoint.head_sha,
        "diverged": head_check.diverged(),
        "head_check": head_check.label(),
        "repo_head_sha": head_check.repo_head_sha(),
    });
    if let HeadCheck::Unreadable { detail } = &head_check {
        // Only present when there is something to explain, and always
        // alongside `head_check: "unreadable"` — the caller is told why the
        // check could not run, not left to infer it from a missing verdict.
        response["head_check_error"] = json!(detail);
    }
    Ok(response)
}
