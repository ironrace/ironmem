//! SQLite-backed queue and session persistence for the collab protocol.

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::{
    Agent, AttestationCheck, AttestedBy, CheckpointStatus, CollabCheckpoint, CollabRoles,
    CollabSession, Phase,
};
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub sender: String,
    pub receiver: String,
    pub topic: String,
    pub content: String,
    pub drawer_id: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub agent: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session: CollabSession,
    pub repo_path: String,
    pub branch: String,
    pub task: Option<String>,
    pub ended_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn create_session(
    conn: &Connection,
    id: &str,
    repo_path: &str,
    branch: &str,
    task: Option<&str>,
    roles: CollabRoles,
) -> Result<(), MemoryError> {
    // Keep `roles.pilot` / `roles.implementer` field-qualified through this
    // function body rather than destructuring into bare locals — the
    // `CollabRoles` struct exists specifically so a positional mix-up here
    // (e.g. between the `implementer`, `pilot`, and `current_owner` slots
    // below) is caught by name, not by argument order.
    //
    // `Agent` is a closed enum so the canonical wire form is guaranteed —
    // no application-layer string validation is needed here. The DB CHECK
    // constraint on the column remains as defense-in-depth against direct
    // SQL writes.
    //
    // Recovery-state columns (pending_failure, failed_from_phase,
    // recovery_phase, recovery_owner, recovery_origin_owner,
    // recovery_attempts, total_recovery_attempts; migration 015) are
    // deliberately omitted here — they
    // have no `DEFAULT` and are all nullable, so a fresh row lands on NULL,
    // which `load_session_record` maps to `None`/`0` exactly like a legacy
    // pre-015 row. `save_session` is the only writer for these fields.
    // `current_owner` is seeded to `pilot` explicitly rather than relying on
    // the schema's `DEFAULT 'claude'` — the pilot drafts first at
    // `PlanParallelDrafts`, so a `pilot=codex` session must be born owned by
    // `codex`, not fall through to the claude default. `CollabSession::new_with_roles`
    // seeds `current_owner` the same way. That constructor is a plain `pub fn`
    // on a re-exported type — NOT `#[cfg(test)]`-gated — so nothing prevents a
    // future production caller; it simply has none today, and every production
    // row is created via this function directly. The two seedings are therefore
    // kept in sync by convention, not by the compiler: if a production caller of
    // `new_with_roles` is ever added, this INSERT and that constructor become a
    // real invariant that needs a test asserting they agree. The schema's
    // `DEFAULT 'claude'` is a fallback for rows written without this column, not
    // a constraint on what a writer may put there.
    conn.execute(
        "INSERT INTO collab_sessions (id, repo_path, branch, task, implementer, pilot, current_owner)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            repo_path,
            branch,
            task,
            roles.implementer.as_str(),
            roles.pilot.as_str(),
            roles.pilot.as_str()
        ],
    )?;
    Ok(())
}

pub fn set_implementer(
    conn: &Connection,
    session_id: &str,
    implementer: Agent,
    current_owner: Option<Agent>,
) -> Result<(), MemoryError> {
    let updated = if let Some(owner) = current_owner {
        conn.execute(
            "UPDATE collab_sessions
             SET implementer = ?2,
                 current_owner = ?3,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![session_id, implementer.as_str(), owner.as_str()],
        )?
    } else {
        conn.execute(
            "UPDATE collab_sessions
             SET implementer = ?2,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![session_id, implementer.as_str()],
        )?
    };
    if updated == 0 {
        return Err(MemoryError::NotFound(format!(
            "session {session_id} not found"
        )));
    }
    Ok(())
}

/// Rebind a session's `pilot` role, optionally also updating
/// `current_owner` in the same statement. Mirrors `set_implementer` above
/// exactly — see that function's shape for why the with/without-owner split
/// exists (a single UPDATE per case, rather than a variable column list).
pub fn set_pilot(
    conn: &Connection,
    session_id: &str,
    pilot: Agent,
    current_owner: Option<Agent>,
) -> Result<(), MemoryError> {
    let updated = if let Some(owner) = current_owner {
        conn.execute(
            "UPDATE collab_sessions
             SET pilot = ?2,
                 current_owner = ?3,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![session_id, pilot.as_str(), owner.as_str()],
        )?
    } else {
        conn.execute(
            "UPDATE collab_sessions
             SET pilot = ?2,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![session_id, pilot.as_str()],
        )?
    };
    if updated == 0 {
        return Err(MemoryError::NotFound(format!(
            "session {session_id} not found"
        )));
    }
    Ok(())
}

/// Mark a session as ended. Subsequent mutating operations should check
/// `ended_at` via `ensure_active` and refuse to proceed.
pub fn end_session(conn: &Connection, session_id: &str) -> Result<(), MemoryError> {
    let updated = conn.execute(
        "UPDATE collab_sessions SET ended_at = datetime('now') WHERE id = ?1 AND ended_at IS NULL",
        params![session_id],
    )?;
    if updated == 0 {
        // Either session missing or already ended — surface the distinction.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM collab_sessions WHERE id = ?1",
                params![session_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(MemoryError::NotFound(format!(
                "session {session_id} not found"
            )));
        }
        // Already ended — idempotent success.
    }
    Ok(())
}

/// Return an error if the session has `ended_at` set.
pub fn ensure_active(conn: &Connection, session_id: &str) -> Result<(), MemoryError> {
    let ended: Option<String> = conn
        .query_row(
            "SELECT ended_at FROM collab_sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| MemoryError::NotFound(format!("session {session_id} not found")))?;
    if ended.is_some() {
        return Err(MemoryError::Validation(format!(
            "session {session_id} has ended"
        )));
    }
    Ok(())
}

/// Find the session that currently *reserves the start slot* for a
/// `repo_path` + `branch`, if any, returning `(id, phase)`.
///
/// `CodingComplete` is excluded even before an explicit `collab_end`:
/// completion needs operator attestation, which is a human step of unbounded
/// duration, and holding the slot for it would block the next session on that
/// branch indefinitely.
///
/// `CodingFailed` is deliberately NOT excluded. A tooling-class failure stays
/// resumable (`ResumeCoding` is legal from `CodingFailed`), and
/// [`super::super::mcp::tools::collab_session`]'s resume guard refuses to
/// reclaim a scope owned by a newer live session — so releasing the slot here
/// would let a replayed `collab_start` strand the failed session's plan,
/// task list, and recovery columns with no API to get them back.
///
/// Use [`find_active_session_by_repo_branch_including_terminal`] for
/// attribution lookups, which must still see a `CodingComplete` session.
pub fn find_active_session_by_repo_branch(
    conn: &Connection,
    repo_path: &str,
    branch: &str,
) -> Result<Option<(String, String)>, MemoryError> {
    conn.query_row(
        "SELECT id, phase FROM collab_sessions
         WHERE repo_path = ?1 AND branch = ?2 AND ended_at IS NULL
           AND phase <> 'CodingComplete'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        params![repo_path, branch],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(MemoryError::from)
}

/// Newest session for a `repo_path` + `branch` that has not been
/// `collab_end`-ed, returning `(id, phase)` — terminal coding phases included.
///
/// This is the *attribution* lookup, and it deliberately differs from
/// [`find_active_session_by_repo_branch`] (the start-slot lookup). A session
/// sitting at `CodingComplete` awaiting operator attestation still owns the
/// work happening in its workspace: `MetricsContext::resolve` stamps such
/// sessions with bucket `other`, so the hook must see them too or transcript
/// rows and MCP rows would disagree about the same session.
///
/// `phase` is returned as the raw column string (not parsed into [`Phase`]) on
/// purpose, so display callers can treat it as an opaque value; parsing here
/// would add a failure path they do not need. Use [`load_session`] when a
/// typed [`Phase`] is required.
pub fn find_active_session_by_repo_branch_including_terminal(
    conn: &Connection,
    repo_path: &str,
    branch: &str,
) -> Result<Option<(String, String)>, MemoryError> {
    conn.query_row(
        "SELECT id, phase FROM collab_sessions
         WHERE repo_path = ?1 AND branch = ?2 AND ended_at IS NULL
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        params![repo_path, branch],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(MemoryError::from)
}

pub fn load_session(conn: &Connection, session_id: &str) -> Result<CollabSession, MemoryError> {
    Ok(load_session_record(conn, session_id)?.session)
}

pub fn load_session_record(
    conn: &Connection,
    session_id: &str,
) -> Result<SessionRecord, MemoryError> {
    // Named-column reads insulate this loader from positional drift: a
    // future migration that inserts a column anywhere in the SELECT list
    // would silently misalign hardcoded indices. The SELECT order is still
    // listed explicitly so the query plan stays predictable.
    conn.query_row(
        "SELECT id, phase, current_owner, repo_path, branch,
                claude_draft_hash, codex_draft_hash, canonical_plan_hash,
                final_plan_hash, codex_review_verdict,
                review_round, task, ended_at,
                task_list, task_list_drawer_id,
                task_review_round, global_review_round,
                base_sha, last_head_sha, pr_url, coding_failure,
                canonical_plan_drawer_id, final_plan_drawer_id,
                created_at, updated_at, implementer, pilot,
                pending_failure, failed_from_phase, recovery_phase,
                recovery_owner, recovery_origin_owner, recovery_attempts,
                total_recovery_attempts
         FROM collab_sessions
         WHERE id = ?1",
        params![session_id],
        |row| {
            let phase = parse_text_column::<Phase>(row, "phase")?;
            let current_owner = parse_text_column::<Agent>(row, "current_owner")?;
            let implementer = parse_text_column::<Agent>(row, "implementer")?;
            let pilot = parse_text_column::<Agent>(row, "pilot")?;
            let review_round_i: i64 = row.get("review_round")?;
            let review_round = review_round_i.clamp(0, u8::MAX as i64) as u8;
            let task_list: Option<String> = row.get("task_list")?;
            let task_review_round_i: i64 = row.get("task_review_round")?;
            let global_review_round_i: i64 = row.get("global_review_round")?;
            let failed_from_phase = parse_optional_text_column::<Phase>(row, "failed_from_phase")?;
            let recovery_phase = parse_optional_text_column::<Phase>(row, "recovery_phase")?;
            let recovery_owner = parse_optional_text_column::<Agent>(row, "recovery_owner")?;
            let recovery_origin_owner =
                parse_optional_text_column::<Agent>(row, "recovery_origin_owner")?;
            // Nullable in the DB (legacy pre-015 rows have no value), but the
            // Rust field is a plain `u8` — map the missing case to `0` rather
            // than propagating an `Option`.
            let recovery_attempts_i: Option<i64> = row.get("recovery_attempts")?;
            let recovery_attempts =
                recovery_attempts_i.map_or(0, |n| n.clamp(0, u8::MAX as i64) as u8);
            let total_recovery_attempts_i: Option<i64> = row.get("total_recovery_attempts")?;
            let total_recovery_attempts =
                total_recovery_attempts_i.map_or(0, |n| n.clamp(0, u8::MAX as i64) as u8);
            Ok(SessionRecord {
                session: CollabSession {
                    id: row.get("id")?,
                    phase,
                    current_owner,
                    claude_draft_hash: row.get("claude_draft_hash")?,
                    codex_draft_hash: row.get("codex_draft_hash")?,
                    canonical_plan_hash: row.get("canonical_plan_hash")?,
                    final_plan_hash: row.get("final_plan_hash")?,
                    canonical_plan_drawer_id: row.get("canonical_plan_drawer_id")?,
                    final_plan_drawer_id: row.get("final_plan_drawer_id")?,
                    codex_review_verdict: row.get("codex_review_verdict")?,
                    review_round,
                    task_list,
                    task_list_drawer_id: row.get("task_list_drawer_id")?,
                    task_review_round: task_review_round_i.clamp(0, u8::MAX as i64) as u8,
                    global_review_round: global_review_round_i.clamp(0, u8::MAX as i64) as u8,
                    base_sha: row.get("base_sha")?,
                    last_head_sha: row.get("last_head_sha")?,
                    pr_url: row.get("pr_url")?,
                    coding_failure: row.get("coding_failure")?,
                    pilot,
                    implementer,
                    pending_failure: row.get("pending_failure")?,
                    failed_from_phase,
                    recovery_phase,
                    recovery_owner,
                    recovery_origin_owner,
                    recovery_attempts,
                    total_recovery_attempts,
                },
                repo_path: row.get("repo_path")?,
                branch: row.get("branch")?,
                task: row.get("task")?,
                ended_at: row.get("ended_at")?,
                created_at: row.get("created_at")?,
                updated_at: row.get("updated_at")?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MemoryError::NotFound(format!("session {session_id} not found")))
}

/// Read a TEXT column and parse it via `FromStr`, surfacing any parse
/// failure as a `FromSqlConversionFailure` so the row scan returns a
/// proper rusqlite error rather than panicking.
fn parse_text_column<T>(row: &rusqlite::Row<'_>, column: &str) -> rusqlite::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw: String = row.get(column)?;
    raw.parse::<T>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("column {column}: {err}"),
            )),
        )
    })
}

/// Read a nullable TEXT column and parse it via `FromStr` when present.
/// `None` (SQL NULL) maps to `Ok(None)`; a present-but-unparseable value
/// surfaces the same `FromSqlConversionFailure` shape as `parse_text_column`
/// so a corrupt row still fails the row scan instead of panicking.
fn parse_optional_text_column<T>(
    row: &rusqlite::Row<'_>,
    column: &str,
) -> rusqlite::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw: Option<String> = row.get(column)?;
    raw.map(|s| {
        s.parse::<T>().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("column {column}: {err}"),
                )),
            )
        })
    })
    .transpose()
}

pub fn save_session(conn: &Connection, session: &CollabSession) -> Result<(), MemoryError> {
    // `implementer` may be rebound by `collab_set_implementer` while a
    // planning or implementation handoff is still active, so keep it in the
    // full-session update list alongside the rest of the state.
    let updated = conn.execute(
        "UPDATE collab_sessions
         SET phase = ?1,
             current_owner = ?2,
             claude_draft_hash = ?3,
             codex_draft_hash = ?4,
             canonical_plan_hash = ?5,
             final_plan_hash = ?6,
             codex_review_verdict = ?7,
             review_round = ?8,
             task_list = ?9,
             task_list_drawer_id = ?10,
             task_review_round = ?11,
             global_review_round = ?12,
             base_sha = ?13,
             last_head_sha = ?14,
             pr_url = ?15,
             coding_failure = ?16,
             canonical_plan_drawer_id = ?17,
             final_plan_drawer_id = ?18,
             implementer = ?19,
             pending_failure = ?20,
             failed_from_phase = ?21,
             recovery_phase = ?22,
             recovery_owner = ?23,
             recovery_origin_owner = ?24,
             recovery_attempts = ?25,
             total_recovery_attempts = ?26,
             pilot = ?27,
             updated_at = datetime('now')
        WHERE id = ?28",
        params![
            session.phase.to_string(),
            session.current_owner.as_str(),
            session.claude_draft_hash.as_deref(),
            session.codex_draft_hash.as_deref(),
            session.canonical_plan_hash.as_deref(),
            session.final_plan_hash.as_deref(),
            session.codex_review_verdict.as_deref(),
            session.review_round as i64,
            session.task_list.as_deref(),
            session.task_list_drawer_id.as_deref(),
            session.task_review_round as i64,
            session.global_review_round as i64,
            session.base_sha.as_deref(),
            session.last_head_sha.as_deref(),
            session.pr_url.as_deref(),
            session.coding_failure.as_deref(),
            session.canonical_plan_drawer_id.as_deref(),
            session.final_plan_drawer_id.as_deref(),
            session.implementer.as_str(),
            session.pending_failure.as_deref(),
            session.failed_from_phase.map(|p| p.to_string()),
            session.recovery_phase.map(|p| p.to_string()),
            session.recovery_owner.map(|a| a.as_str()),
            session.recovery_origin_owner.map(|a| a.as_str()),
            session.recovery_attempts as i64,
            session.total_recovery_attempts as i64,
            session.pilot.as_str(),
            session.id.as_str(),
        ],
    )?;
    if updated == 0 {
        return Err(MemoryError::NotFound(format!(
            "session {} not found",
            session.id
        )));
    }
    Ok(())
}

/// Write the session's one current checkpoint, replacing any prior one.
///
/// `session_id` is the table's primary key, so this is an upsert rather than
/// an append: exactly one current checkpoint per session, matching the
/// one-logical-keyed-drawer semantics this table replaced. History lives in
/// the git log and the `wal_log` audit trail.
///
/// `updated_at` is stamped here from the server clock and deliberately ignored
/// from the caller's payload — otherwise a caller could backdate a checkpoint
/// and make a stale one look fresh. The stamp is `strftime('%s','now')` rather
/// than a `SystemTime` conversion so there is no fallible step to swallow:
/// `SystemTime::now()` before `UNIX_EPOCH` has to produce *some* value, and any
/// integer fallback collides with a real one — a `0` fallback in particular
/// writes the exact sentinel [`CollabCheckpoint::updated_at`] documents as
/// "has not been through a write", onto a row that just was. SQLite's numeric
/// affinity stores the returned text into the `INTEGER` column as an integer.
///
/// **This upsert is unconditional last-writer-wins.** There is no
/// `WHERE excluded.updated_at >= updated_at` guard, so a caller holding a
/// stale in-memory checkpoint overwrites a newer stored one and writes
/// progress *backwards* — a smaller version of the regression issue #273 is
/// about — and with `updated_at` at second granularity two writes in the same
/// second are not even distinguishable after the fact. That is the right
/// contract for a primitive handed a fully-formed struct, but it makes the
/// read-modify-write the tool layer's obligation: a caller that loads a
/// checkpoint, advances it, and writes it back must hold one transaction
/// across the load and the write.
///
/// Safe to run more than once: it is a pure upsert with no accumulation, so a
/// closure passed to `Database::with_transaction` — which replays its closure
/// on `SQLITE_BUSY_SNAPSHOT` — may call it.
///
/// Calls [`CollabCheckpoint::validate`] before writing anything. Every field
/// on `CollabCheckpoint` is `pub`, so a caller can build one directly (as
/// `load_current_checkpoint` itself must, reconstructing from a row) without
/// going through `from_json`'s checks — and migration 020's CHECK on
/// `acknowledged_divergence` is one-directional, permitting
/// `attested_by = 'operator'` with the column left NULL. Without this call an
/// invalid struct — e.g. that exact operator/no-divergence combination —
/// would insert cleanly and then permanently fail every subsequent
/// `load_current_checkpoint` for that session: a write-succeeds,
/// read-always-fails poison row keyed by `session_id`, with no way to read or
/// fix it back out. [`CollabCheckpoint::validate`]'s doc comment names both
/// entry points as owing this call; this is the write side. It is also what
/// keeps a blank `head_sha` out of the table, migration 020 having `NOT NULL`
/// on that column and no `CHECK (head_sha <> '')`.
///
/// `validate()` alone is not enough to close that poison-row hole, because it
/// covers only the three `String` fields and the attestation correlation — it
/// never looks at the task id fields, and migration 020 has no CHECK on either
/// column. The loader is stricter: [`checked_task_id_column`] refuses a
/// `task_id` or `next_task_id` of `0` and [`parse_stored_completed_task_ids`]
/// refuses a `0` entry, both mirroring `from_json`'s 1-based rule. So the
/// write path runs the loader's own helpers over what it is about to store,
/// keeping the write gate at least as strict as the read gate: a struct built
/// field-by-field with a `0` in any of them would otherwise insert cleanly and
/// then permanently fail every subsequent `load_current_checkpoint` for that
/// session, which is the same poison row by a different field.
pub fn upsert_checkpoint(
    conn: &Connection,
    checkpoint: &CollabCheckpoint,
) -> Result<(), MemoryError> {
    checkpoint.validate().map_err(|err| {
        MemoryError::Validation(format!(
            "checkpoint for session {}: {err}",
            checkpoint.session_id
        ))
    })?;

    checked_task_id_column(
        checkpoint.task_id.map(i64::from),
        "task_id",
        &checkpoint.session_id,
    )?;
    checked_task_id_column(
        checkpoint.next_task_id.map(i64::from),
        "next_task_id",
        &checkpoint.session_id,
    )?;

    let completed = checkpoint
        .completed_task_ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // Run the stored form through the loader's parser rather than the `Vec`
    // through a second zero check, so what is written is exactly what the read
    // path is willing to take back.
    parse_stored_completed_task_ids(&completed, &checkpoint.session_id)?;

    conn.execute(
        "INSERT INTO collab_checkpoints (
             session_id, task_id, task_title, status, head_sha, commit_sha,
             completed_task_ids, next_task_id, gates_result, gates_sha,
             gates_commands, summary, attested_by, acknowledged_divergence,
             attestation_check, updated_at
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             strftime('%s','now')
         )
         ON CONFLICT(session_id) DO UPDATE SET
             task_id = excluded.task_id,
             task_title = excluded.task_title,
             status = excluded.status,
             head_sha = excluded.head_sha,
             commit_sha = excluded.commit_sha,
             completed_task_ids = excluded.completed_task_ids,
             next_task_id = excluded.next_task_id,
             gates_result = excluded.gates_result,
             gates_sha = excluded.gates_sha,
             gates_commands = excluded.gates_commands,
             summary = excluded.summary,
             attested_by = excluded.attested_by,
             acknowledged_divergence = excluded.acknowledged_divergence,
             attestation_check = excluded.attestation_check,
             updated_at = excluded.updated_at",
        params![
            checkpoint.session_id,
            checkpoint.task_id,
            checkpoint.task_title,
            checkpoint.status.as_str(),
            checkpoint.head_sha,
            checkpoint.commit_sha,
            completed,
            checkpoint.next_task_id,
            checkpoint.gates_result,
            checkpoint.gates_sha,
            checkpoint.gates_commands,
            checkpoint.summary,
            checkpoint.attested_by.as_str(),
            checkpoint.acknowledged_divergence,
            checkpoint.attestation_check.map(AttestationCheck::as_str),
        ],
    )?;
    Ok(())
}

/// Parse the stored `completed_task_ids` column strictly, refusing to do what
/// `CollabCheckpoint::from_json`'s own parser refuses: silently drop an
/// unparseable entry. A `filter_map(...ok())` here would let a corrupted
/// value like `"1,2,X,4"` load as `[1, 2, 4]` — a checkpoint that quietly
/// under-reports progress with no error anywhere in the path. That matters
/// because `CollabCheckpoint::covers_all_tasks` reads exactly this field to
/// gate the `implementation_done` transition (Tasks 7-10), so a silently
/// shortened list would let a corrupted row look like partial progress
/// instead of failing loudly.
///
/// Deliberately not a call into `CollabCheckpoint::from_json`'s private
/// parser: that function parses a comma-separated *string value already
/// extracted from JSON*, whereas this reads directly off the SQL row. Both
/// enforce the same "no entry may fail to parse" rule; keeping them separate
/// avoids coupling this loader to `checkpoint.rs`'s JSON-shaped error
/// plumbing for a few lines of logic.
///
/// The sort/dedup at the end mirrors that parser's `BTreeSet` for the same
/// reason: `CollabCheckpoint::completed_task_ids`' doc promises that equal
/// progress is equal *data*, so a diff or equality over checkpoints reflects
/// real progress rather than the order ids were appended in. It is not what
/// makes coverage correct — `covers_all_tasks` builds its own set and is safe
/// either way — so `load_current_checkpoint_normalizes_a_stored_task_id_list`
/// pins it directly; without that test the two lines can be deleted with the
/// suite still green.
fn parse_stored_completed_task_ids(raw: &str, session_id: &str) -> Result<Vec<u32>, MemoryError> {
    let mut ids = Vec::new();
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let id: u32 = piece.parse().map_err(|_| {
            MemoryError::Validation(format!(
                "checkpoint for session {session_id}: completed_task_ids contains a \
                 non-numeric entry {piece:?} in stored value {raw:?}"
            ))
        })?;
        // Task ids are 1-based, mirroring `checkpoint.rs::parse_completed_task_ids`'s
        // own zero rejection. Without this a corrupted "0,1" would load as a
        // phantom task id 0 that from_json's write path would have refused.
        if id == 0 {
            return Err(MemoryError::Validation(format!(
                "checkpoint for session {session_id}: completed_task_ids entries must be \
                 task ids of 1 or greater, got 0 in stored value {raw:?}"
            )));
        }
        ids.push(id);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Narrow a nullable `INTEGER` column to `u32`, refusing to do what a bare
/// `as u32` cast would: silently wrap a negative or over-`u32::MAX` value
/// into an unrelated small number instead of failing. `task_id` and
/// `next_task_id` have no CHECK constraint in migration 020, so a direct SQL
/// write (or a future writer with a bug) can put an out-of-range value in
/// either column; `CollabCheckpoint::from_json`'s own `optional_task_id`
/// already refuses one on the write path via `u32::try_from`, so the loader
/// owes the same refusal rather than quietly reinterpreting corrupt data as
/// some other task id.
///
/// Also rejects `0`, mirroring `optional_task_id`'s *other* rejection ground
/// (task ids are 1-based). Range and zero are the parser's full refusal set
/// for this field — matching only range here would still let a phantom task
/// id `0` reach the `implementation_done` gate.
fn checked_task_id_column(
    raw: Option<i64>,
    field: &str,
    session_id: &str,
) -> Result<Option<u32>, MemoryError> {
    raw.map(|n| {
        let id = u32::try_from(n).map_err(|_| {
            MemoryError::Validation(format!(
                "checkpoint for session {session_id}: {field} value {n} does not fit in u32"
            ))
        })?;
        if id == 0 {
            return Err(MemoryError::Validation(format!(
                "checkpoint for session {session_id}: {field} must be a task id of 1 or \
                 greater, got 0"
            )));
        }
        Ok(id)
    })
    .transpose()
}

/// Load the session's one current checkpoint, or `None` when it has never
/// written one. `None` is materially different from a stale checkpoint and
/// callers must keep them distinct: it means the session predates migration
/// 020 or the implementer has not checkpointed at all.
///
/// Rebuilds the struct field-by-field from the row rather than going through
/// [`CollabCheckpoint::from_json`] — every field on the type is `pub`
/// precisely so this loader can do that — which means every rule
/// `from_json` enforces at parse time is bypassed here unless re-applied.
/// [`CollabCheckpoint::validate`] exists to be that re-application: migration
/// 020's CHECK on `acknowledged_divergence` is deliberately one-directional
/// (it permits `attested_by = 'operator'` with no acknowledged range), so
/// without this call a row the schema allows but the domain rules forbid
/// would load clean and hand the `implementation_done` gate a checkpoint
/// claiming the operator escape hatch while naming nothing it vouches for.
/// The same call is what refuses a stored `head_sha` of `''` or the word
/// `none`: migration 020 has `NOT NULL` on the column and no
/// `CHECK (head_sha <> '')`, so a direct SQL write can put either there.
pub fn load_current_checkpoint(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<CollabCheckpoint>, MemoryError> {
    let row = conn
        .query_row(
            "SELECT session_id, task_id, task_title, status, head_sha, commit_sha,
                    completed_task_ids, next_task_id, gates_result, gates_sha,
                    gates_commands, summary, attested_by, acknowledged_divergence,
                    attestation_check, updated_at
             FROM collab_checkpoints
             WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, String>("session_id")?,
                    row.get::<_, Option<i64>>("task_id")?,
                    row.get::<_, Option<String>>("task_title")?,
                    row.get::<_, String>("status")?,
                    row.get::<_, String>("head_sha")?,
                    row.get::<_, Option<String>>("commit_sha")?,
                    row.get::<_, String>("completed_task_ids")?,
                    row.get::<_, Option<i64>>("next_task_id")?,
                    row.get::<_, String>("gates_result")?,
                    row.get::<_, Option<String>>("gates_sha")?,
                    row.get::<_, Option<String>>("gates_commands")?,
                    row.get::<_, Option<String>>("summary")?,
                    row.get::<_, String>("attested_by")?,
                    row.get::<_, Option<String>>("acknowledged_divergence")?,
                    row.get::<_, Option<String>>("attestation_check")?,
                    row.get::<_, i64>("updated_at")?,
                ))
            },
        )
        .optional()?;

    let Some((
        row_session_id,
        task_id,
        task_title,
        status_raw,
        head_sha,
        commit_sha,
        completed_raw,
        next_task_id,
        gates_result,
        gates_sha,
        gates_commands,
        summary,
        attested_by_raw,
        acknowledged_divergence,
        attestation_check_raw,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };

    let status = status_raw.parse::<CheckpointStatus>().map_err(|err| {
        MemoryError::Validation(format!("checkpoint for session {row_session_id}: {err}"))
    })?;
    let attested_by = attested_by_raw.parse::<AttestedBy>().map_err(|err| {
        MemoryError::Validation(format!("checkpoint for session {row_session_id}: {err}"))
    })?;
    // Parsed rather than carried as a string, so a value migration 021's CHECK
    // somehow admitted — or a row written by a future author against a widened
    // vocabulary — fails here instead of reaching a reader as an unrecognised
    // verdict it would render verbatim. Same belt-and-braces `status` and
    // `attested_by` get above.
    let attestation_check = attestation_check_raw
        .map(|raw| {
            raw.parse::<AttestationCheck>().map_err(|err| {
                MemoryError::Validation(format!("checkpoint for session {row_session_id}: {err}"))
            })
        })
        .transpose()?;
    let completed_task_ids = parse_stored_completed_task_ids(&completed_raw, &row_session_id)?;
    let task_id = checked_task_id_column(task_id, "task_id", &row_session_id)?;
    let next_task_id = checked_task_id_column(next_task_id, "next_task_id", &row_session_id)?;

    let checkpoint = CollabCheckpoint {
        session_id: row_session_id.clone(),
        task_id,
        task_title,
        status,
        head_sha,
        commit_sha,
        completed_task_ids,
        next_task_id,
        gates_result,
        gates_sha,
        gates_commands,
        summary,
        attested_by,
        acknowledged_divergence,
        attestation_check,
        updated_at,
    };

    // See this function's doc comment: this is the required call Task 2's
    // `validate` doc comment names both entry points as owing.
    checkpoint.validate().map_err(|err| {
        MemoryError::Validation(format!("checkpoint for session {row_session_id}: {err}"))
    })?;

    Ok(Some(checkpoint))
}

/// Persist a message that references an already-written drawer.
///
/// This low-level helper does not create the drawer. Production callers must
/// insert the drawer and this message in one SQLite transaction so a successful
/// collab write never leaves a dangling drawer reference.
pub fn send_message(
    conn: &Connection,
    session_id: &str,
    sender: &str,
    receiver: &str,
    topic: &str,
    content: &str,
    drawer_id: &str,
) -> Result<String, MemoryError> {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO messages (id, session_id, sender, receiver, topic, content, drawer_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, session_id, sender, receiver, topic, content, drawer_id],
    )?;
    Ok(id)
}

/// Record a session incident that is not correspondence.
///
/// Unlike [`send_message`], the row is self-addressed and inserted with
/// `status = 'recorded'` rather than the default `'pending'`. Both matter:
/// [`recv_messages`] filters on `receiver = ? AND status = 'pending'`, so an
/// incident addressed to the counterpart would be handed to the next worker
/// that calls `collab_recv` — whose templates enforce a one-recv rule and
/// expect a specific topic — corrupting that turn's input. This is a record
/// for the session history, not a message to anyone.
pub fn record_incident(
    conn: &Connection,
    session_id: &str,
    agent: &str,
    topic: &str,
    content: &str,
    drawer_id: &str,
) -> Result<String, MemoryError> {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO messages
           (id, session_id, sender, receiver, topic, content, drawer_id, status)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, 'recorded')",
        params![id, session_id, agent, topic, content, drawer_id],
    )?;
    Ok(id)
}

/// Count incidents of `topic` recorded against a session by
/// [`record_incident`]. Counts regardless of `status` so a record can never be
/// hidden by a future inbox-state change.
pub fn count_incidents(
    conn: &Connection,
    session_id: &str,
    topic: &str,
) -> Result<i64, MemoryError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND topic = ?2",
        params![session_id, topic],
        |row| row.get(0),
    )?)
}

pub fn recv_messages(
    conn: &Connection,
    session_id: &str,
    receiver: &str,
    limit: usize,
) -> Result<Vec<Message>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, sender, receiver, topic, content, drawer_id, status, created_at
         FROM messages
         WHERE session_id = ?1 AND receiver = ?2 AND status = 'pending'
         ORDER BY rowid ASC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, receiver, limit as i64], |row| {
        Ok(Message {
            id: row.get(0)?,
            session_id: row.get(1)?,
            sender: row.get(2)?,
            receiver: row.get(3)?,
            topic: row.get(4)?,
            content: row.get(5)?,
            drawer_id: row.get(6)?,
            status: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    Ok(messages)
}

/// Return the latest message `content` for a given `(session_id, topic)` pair,
/// regardless of status. Used by `collab_status` so a fresh Claude session
/// joining at `PlanLocked` can pull back the locked `final` plan it previously
/// sent — `recv_messages` only returns unacked *incoming* mail, which cannot
/// surface outbound plans the peer already consumed.
pub fn load_latest_message_content(
    conn: &Connection,
    session_id: &str,
    topic: &str,
) -> Result<Option<String>, MemoryError> {
    let content: Option<String> = conn
        .query_row(
            "SELECT content FROM messages
             WHERE session_id = ?1 AND topic = ?2
             ORDER BY rowid DESC
             LIMIT 1",
            params![session_id, topic],
            |row| row.get(0),
        )
        .optional()?;
    Ok(content)
}

pub fn ack_message(
    conn: &Connection,
    session_id: &str,
    message_id: &str,
) -> Result<(), MemoryError> {
    let updated = conn.execute(
        "UPDATE messages SET status = 'acked' WHERE id = ?1 AND session_id = ?2",
        params![message_id, session_id],
    )?;
    if updated == 0 {
        return Err(MemoryError::NotFound(format!(
            "message {message_id} not found in session {session_id}"
        )));
    }
    Ok(())
}

/// Mark a batch of messages as acked in a single UPDATE. All IDs must belong
/// to `session_id`; any missing ID is silently skipped (idempotent for
/// already-acked messages). Returns the count of rows actually updated.
pub fn ack_messages_many(
    conn: &Connection,
    session_id: &str,
    message_ids: &[String],
) -> Result<usize, MemoryError> {
    if message_ids.is_empty() {
        return Ok(0);
    }
    // Build a parameterised IN list: `(?1, ?2, …)`. The session_id
    // occupies slot ?1, message IDs start at ?2.
    let placeholders: String = (0..message_ids.len())
        .map(|i| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE messages SET status = 'acked' \
         WHERE session_id = ?1 AND id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    // Bind session_id as slot 1, then each message_id starting from slot 2.
    let updated = stmt.execute(rusqlite::params_from_iter(
        std::iter::once(session_id.to_string()).chain(message_ids.iter().cloned()),
    ))?;
    Ok(updated)
}

pub fn register_caps(
    conn: &Connection,
    session_id: &str,
    agent: &str,
    caps: &[Capability],
) -> Result<(), MemoryError> {
    for cap in caps {
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO agent_capabilities (id, session_id, agent, capability, description)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, agent, capability) DO UPDATE SET
                 description = excluded.description,
                 registered_at = datetime('now')",
            params![
                id,
                session_id,
                agent,
                cap.name.as_str(),
                cap.description.as_deref()
            ],
        )?;
    }
    Ok(())
}

pub fn get_caps(
    conn: &Connection,
    session_id: &str,
    agent: Option<&str>,
) -> Result<Vec<Capability>, MemoryError> {
    let sql = if agent.is_some() {
        "SELECT agent, capability, description
         FROM agent_capabilities
         WHERE session_id = ?1 AND agent = ?2
         ORDER BY agent ASC, registered_at ASC, capability ASC"
    } else {
        "SELECT agent, capability, description
         FROM agent_capabilities
         WHERE session_id = ?1
         ORDER BY agent ASC, registered_at ASC, capability ASC"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut caps = Vec::new();

    if let Some(agent) = agent {
        let rows = stmt.query_map(params![session_id, agent], |row| {
            Ok(Capability {
                agent: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
            })
        })?;
        for row in rows {
            caps.push(row?);
        }
    } else {
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(Capability {
                agent: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
            })
        })?;
        for row in rows {
            caps.push(row?);
        }
    }

    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const BASE_SQL: &str = include_str!("../../migrations/001_init.sql");
    const FTS_SQL: &str = include_str!("../../migrations/002_fts.sql");
    const COLLAB_SQL: &str = include_str!("../../migrations/003_collab.sql");
    const COLLAB_V1_SQL: &str = include_str!("../../migrations/004_collab_planning_v1.sql");
    const COLLAB_V2_SQL: &str = include_str!("../../migrations/005_collab_v2.sql");
    const COLLAB_IMPLEMENTER_SQL: &str =
        include_str!("../../migrations/006_collab_implementer.sql");
    const DROP_CURRENT_TASK_INDEX_SQL: &str =
        include_str!("../../migrations/007_drop_current_task_index.sql");
    const COLLAB_PLAN_DRAWERS_SQL: &str =
        include_str!("../../migrations/009_collab_plan_drawers.sql");
    const COLLAB_GENERATION_LEASE_SQL: &str =
        include_str!("../../migrations/010_collab_generation_lease.sql");
    const COLLAB_TASK_LIST_REF_SQL: &str = "ALTER TABLE collab_sessions \
         ADD COLUMN task_list_drawer_id TEXT";
    const COLLAB_RECOVERY_STATE_SQL: &str =
        include_str!("../../migrations/015_collab_recovery_state.sql");
    const COLLAB_MESSAGE_DRAWERS_SQL: &str =
        include_str!("../../migrations/016_collab_message_drawers.sql");
    const COLLAB_PILOT_SQL: &str = include_str!("../../migrations/019_collab_pilot.sql");
    const COLLAB_CHECKPOINTS_SQL: &str =
        include_str!("../../migrations/020_collab_checkpoints.sql");
    const CHECKPOINT_ATTESTATION_CHECK_SQL: &str =
        include_str!("../../migrations/021_checkpoint_attestation_check.sql");
    const QUEUE_TEST_DRAWER_IDS: [&str; 7] = [
        "drawer-123",
        "drawer-a",
        "drawer-b",
        "drawer-first",
        "drawer-second",
        "drawer-third",
        "drawer-x",
    ];

    fn insert_queue_test_drawer(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                "queue test drawer",
                vec![0u8; ironrace_embed::embedder::EMBED_DIM * std::mem::size_of::<f32>()],
                "ironrace-memory",
                "collab-plans",
                "",
                "test",
            ],
        )
        .unwrap();
    }

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BASE_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(COLLAB_SQL).unwrap();
        conn.execute_batch(COLLAB_V1_SQL).unwrap();
        conn.execute_batch(COLLAB_V2_SQL).unwrap();
        conn.execute_batch(COLLAB_IMPLEMENTER_SQL).unwrap();
        conn.execute_batch(DROP_CURRENT_TASK_INDEX_SQL).unwrap();
        conn.execute_batch(COLLAB_PLAN_DRAWERS_SQL).unwrap();
        conn.execute_batch(COLLAB_GENERATION_LEASE_SQL).unwrap();
        conn.execute_batch(COLLAB_TASK_LIST_REF_SQL).unwrap();
        conn.execute_batch(COLLAB_RECOVERY_STATE_SQL).unwrap();
        conn.execute_batch(COLLAB_MESSAGE_DRAWERS_SQL).unwrap();
        conn.execute_batch(COLLAB_PILOT_SQL).unwrap();
        conn.execute_batch(COLLAB_CHECKPOINTS_SQL).unwrap();
        conn.execute_batch(CHECKPOINT_ATTESTATION_CHECK_SQL)
            .unwrap();
        for drawer_id in QUEUE_TEST_DRAWER_IDS {
            insert_queue_test_drawer(&conn, drawer_id);
        }
        conn
    }

    #[test]
    fn test_send_recv_ack_fifo() {
        let db = open();
        create_session(
            &db,
            "sess1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let m1 = send_message(
            &db,
            "sess1",
            "claude",
            "codex",
            "draft",
            "first",
            "drawer-first",
        )
        .unwrap();
        let _m2 = send_message(
            &db,
            "sess1",
            "claude",
            "codex",
            "draft",
            "second",
            "drawer-second",
        )
        .unwrap();

        let received = recv_messages(&db, "sess1", "codex", 10).unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].id, m1);
        assert_eq!(received[0].content, "first");

        ack_message(&db, "sess1", &m1).unwrap();
        let received = recv_messages(&db, "sess1", "codex", 10).unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].content, "second");
    }

    #[test]
    fn test_send_recv_preserves_drawer_id() {
        let db = open();
        create_session(
            &db,
            "sess-drawer",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        send_message(
            &db,
            "sess-drawer",
            "claude",
            "codex",
            "draft",
            "message body",
            "drawer-123",
        )
        .unwrap();

        let received = recv_messages(&db, "sess-drawer", "codex", 1).unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].drawer_id.as_deref(), Some("drawer-123"));
    }

    #[test]
    fn test_ack_idempotent() {
        let db = open();
        create_session(
            &db,
            "sess2",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let message_id =
            send_message(&db, "sess2", "claude", "codex", "draft", "x", "drawer-x").unwrap();
        ack_message(&db, "sess2", &message_id).unwrap();
        let err = ack_message(&db, "wrong-session", &message_id).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_register_caps_upsert() {
        let db = open();
        create_session(
            &db,
            "sess3",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        register_caps(
            &db,
            "sess3",
            "codex",
            &[Capability {
                agent: "codex".to_string(),
                name: "reviewer".to_string(),
                description: Some("v1".to_string()),
            }],
        )
        .unwrap();
        register_caps(
            &db,
            "sess3",
            "codex",
            &[Capability {
                agent: "codex".to_string(),
                name: "reviewer".to_string(),
                description: Some("v2".to_string()),
            }],
        )
        .unwrap();

        let caps = get_caps(&db, "sess3", Some("codex")).unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].description.as_deref(), Some("v2"));
    }

    #[test]
    fn test_get_caps_empty_before_register() {
        let db = open();
        create_session(
            &db,
            "sess4",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let caps = get_caps(&db, "sess4", Some("claude")).unwrap();
        assert!(caps.is_empty());
    }

    #[test]
    fn test_orphan_message_fk_violation() {
        let db = open();
        let err = send_message(
            &db,
            "missing-session",
            "claude",
            "codex",
            "draft",
            "x",
            "drawer-x",
        )
        .unwrap_err();
        assert!(err.to_string().contains("Database error"));
    }

    #[test]
    fn test_task_persists_through_load_session_record() {
        let db = open();
        create_session(
            &db,
            "sess-task",
            "/repo",
            "main",
            Some("build a landing page"),
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let record = load_session_record(&db, "sess-task").unwrap();
        assert_eq!(record.task.as_deref(), Some("build a landing page"));
        assert!(record.ended_at.is_none());
        assert_eq!(record.session.review_round, 0);
    }

    #[test]
    fn test_review_round_persists() {
        let db = open();
        create_session(
            &db,
            "sess-rr",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut session = load_session(&db, "sess-rr").unwrap();
        session.review_round = 2;
        save_session(&db, &session).unwrap();
        let round_trip = load_session(&db, "sess-rr").unwrap();
        assert_eq!(round_trip.review_round, 2);
    }

    #[test]
    fn test_ensure_active_rejects_ended_session() {
        let db = open();
        create_session(
            &db,
            "sess-end",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        ensure_active(&db, "sess-end").unwrap();
        end_session(&db, "sess-end").unwrap();
        let err = ensure_active(&db, "sess-end").unwrap_err();
        assert!(err.to_string().contains("has ended"));
    }

    #[test]
    fn test_end_session_idempotent() {
        let db = open();
        create_session(
            &db,
            "sess-end2",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        end_session(&db, "sess-end2").unwrap();
        // Calling end_session a second time must succeed (idempotent).
        end_session(&db, "sess-end2").unwrap();
    }

    #[test]
    fn test_end_session_missing_returns_not_found() {
        let db = open();
        let err = end_session(&db, "does-not-exist").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_v2_fields_round_trip() {
        let db = open();
        create_session(
            &db,
            "sess-v2",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut session = load_session(&db, "sess-v2").unwrap();
        session.task_list = Some(r#"{"plan_hash":"pf","tasks":[{"id":1},{"id":2}]}"#.to_string());
        session.task_review_round = 1;
        session.global_review_round = 2;
        session.base_sha = Some("abc123".to_string());
        session.last_head_sha = Some("def456".to_string());
        session.pr_url = Some("https://example/pr/42".to_string());
        session.coding_failure = Some("gh_auth: token expired".to_string());
        save_session(&db, &session).unwrap();

        let record = load_session_record(&db, "sess-v2").unwrap();
        let rt = &record.session;
        assert_eq!(rt.task_review_round, 1);
        assert_eq!(rt.global_review_round, 2);
        assert_eq!(rt.base_sha.as_deref(), Some("abc123"));
        assert_eq!(rt.last_head_sha.as_deref(), Some("def456"));
        assert_eq!(rt.pr_url.as_deref(), Some("https://example/pr/42"));
        assert_eq!(rt.coding_failure.as_deref(), Some("gh_auth: token expired"));
        // tasks_count is derived from task_list JSON on demand.
        assert_eq!(rt.tasks_count(), Some(2));
    }

    #[test]
    fn test_v1_defaults_for_fresh_session() {
        let db = open();
        create_session(
            &db,
            "sess-fresh",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let session = load_session(&db, "sess-fresh").unwrap();
        assert!(session.task_list.is_none());
        assert_eq!(session.task_review_round, 0);
        assert_eq!(session.global_review_round, 0);
        assert!(session.base_sha.is_none());
        assert!(session.last_head_sha.is_none());
        assert!(session.pr_url.is_none());
        assert!(session.coding_failure.is_none());
        assert!(session.canonical_plan_drawer_id.is_none());
        assert!(session.final_plan_drawer_id.is_none());
        assert_eq!(session.tasks_count(), None);
        assert_eq!(session.pilot, Agent::Claude);
    }

    // ── pilot field (issue #246 task 2) ──────────────────────────────────────

    #[test]
    fn test_create_session_pilot_and_implementer_defaults_and_non_default() {
        let db = open();
        create_session(
            &db,
            "sess-pilot-default",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let default_session = load_session(&db, "sess-pilot-default").unwrap();
        assert_eq!(default_session.pilot, Agent::Claude);
        assert_eq!(default_session.implementer, Agent::Claude);

        // pilot and implementer are independent knobs: a non-default pilot
        // with the default implementer must persist as given, not coupled.
        create_session(
            &db,
            "sess-pilot-codex",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Codex,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mixed_session = load_session(&db, "sess-pilot-codex").unwrap();
        assert_eq!(mixed_session.pilot, Agent::Codex);
        assert_eq!(mixed_session.implementer, Agent::Claude);
    }

    /// The highest-risk edit in this task: `save_session`'s UPDATE gained a
    /// `pilot = ?27` SET clause, which shifted `WHERE id = ?27` to `?28`. A
    /// mis-ordered `params!` append would silently write the pilot value
    /// into the id predicate instead of the pilot column — a bug that a
    /// round-trip test checking only `pilot` could easily miss (the UPDATE
    /// would just match zero rows and error, OR if it happened to match by
    /// coincidence, only the untested columns would be corrupted). Setting
    /// a non-default value in *every* column and asserting full struct
    /// equality (not just `pilot`) is what actually catches the bind
    /// misalignment.
    #[test]
    fn test_pilot_round_trip_with_every_field_non_default() {
        let db = open();
        create_session(
            &db,
            "sess-pilot-full",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut session = load_session(&db, "sess-pilot-full").unwrap();

        session.phase = Phase::CodeReviewFixGlobalPending;
        session.current_owner = Agent::Codex;
        session.claude_draft_hash = Some("claude-hash".to_string());
        session.codex_draft_hash = Some("codex-hash".to_string());
        session.canonical_plan_hash = Some("canonical-hash".to_string());
        session.final_plan_hash = Some("final-hash".to_string());
        session.canonical_plan_drawer_id = Some("c".repeat(32));
        session.final_plan_drawer_id = Some("f".repeat(32));
        session.codex_review_verdict = Some("approve".to_string());
        session.review_round = 2;
        session.task_list = Some(r#"{"tasks":[{"id":1}]}"#.to_string());
        session.task_list_drawer_id = Some("t".repeat(32));
        session.task_review_round = 1;
        session.global_review_round = 3;
        session.base_sha = Some("base-sha".to_string());
        session.last_head_sha = Some("head-sha".to_string());
        session.pr_url = Some("https://example/pr/9".to_string());
        session.coding_failure = Some("gh_auth: token expired".to_string());
        session.pilot = Agent::Codex;
        session.implementer = Agent::Codex;
        session.pending_failure = Some("git_push_failed: remote rejected".to_string());
        session.failed_from_phase = Some(Phase::CodeImplementPending);
        session.recovery_phase = Some(Phase::CodeReviewFixGlobalPending);
        session.recovery_owner = Some(Agent::Codex);
        session.recovery_origin_owner = Some(Agent::Claude);
        session.recovery_attempts = 3;
        session.total_recovery_attempts = 4;

        save_session(&db, &session).unwrap();

        let round_trip = load_session(&db, "sess-pilot-full").unwrap();
        assert_eq!(
            round_trip, session,
            "every field, including pilot, must round-trip byte-identical"
        );
        assert_eq!(round_trip.pilot, Agent::Codex);
        // The id predicate must still target the original row, not have
        // been overwritten by the pilot bind — reloading by the same id
        // succeeding at all (rather than erroring NotFound) is itself part
        // of that proof, but assert it explicitly too.
        assert_eq!(round_trip.id, "sess-pilot-full");
    }

    /// `set_pilot` must write `pilot` (and, on the with-owner branch,
    /// `current_owner`) and nothing else. `implementer` is seeded to
    /// `Codex`, deliberately *not* equal to the pilot value each branch
    /// writes, because "`pilot` and `implementer` are orthogonal knobs" is
    /// this feature's central design claim and the only way this UPDATE can
    /// break it is by writing `implementer` alongside `pilot`. Seeding
    /// `implementer` equal to the incoming pilot would make a stray
    /// `implementer = ?2` bind write the value that was already there —
    /// invisible. So the with-owner call below drives `pilot = Claude`
    /// against `implementer = Codex`, and the mirror fixture at the end
    /// gives the without-owner branch the same asymmetry.
    #[test]
    fn test_set_pilot_updates_pilot_and_optional_owner() {
        let db = open();
        create_session(
            &db,
            "sess-set-pilot",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Codex,
            },
        )
        .unwrap();
        let seeded = load_session(&db, "sess-set-pilot").unwrap();
        assert_eq!(seeded.implementer, Agent::Codex, "fixture precondition");
        assert_eq!(seeded.pilot, Agent::Claude, "fixture precondition");

        set_pilot(&db, "sess-set-pilot", Agent::Codex, None).unwrap();
        let session = load_session(&db, "sess-set-pilot").unwrap();
        assert_eq!(session.pilot, Agent::Codex);
        assert_eq!(
            session.current_owner,
            Agent::Claude,
            "current_owner must be untouched when None is passed"
        );
        assert_eq!(
            session.implementer,
            Agent::Codex,
            "implementer must be untouched by the without-owner branch"
        );

        set_pilot(&db, "sess-set-pilot", Agent::Claude, Some(Agent::Codex)).unwrap();
        let session = load_session(&db, "sess-set-pilot").unwrap();
        assert_eq!(session.pilot, Agent::Claude);
        assert_eq!(session.current_owner, Agent::Codex);
        assert_eq!(
            session.implementer,
            Agent::Codex,
            "implementer must be untouched by the with-owner branch"
        );

        // Mirror fixture: `implementer = Claude` so the without-owner branch
        // (which writes `pilot = Codex`) also runs against an `implementer`
        // that differs from the value being bound. Without this, only the
        // with-owner branch's stray-write case would be falsifiable.
        create_session(
            &db,
            "sess-set-pilot-mirror",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        set_pilot(&db, "sess-set-pilot-mirror", Agent::Codex, None).unwrap();
        let mirror = load_session(&db, "sess-set-pilot-mirror").unwrap();
        assert_eq!(mirror.pilot, Agent::Codex);
        assert_eq!(
            mirror.implementer,
            Agent::Claude,
            "implementer must be untouched by the without-owner branch"
        );
    }

    #[test]
    fn test_set_pilot_missing_session_returns_not_found() {
        let db = open();
        let err = set_pilot(&db, "does-not-exist", Agent::Codex, None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_recovery_fields_round_trip() {
        let db = open();
        create_session(
            &db,
            "sess-recovery",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut session = load_session(&db, "sess-recovery").unwrap();
        session.pending_failure = Some("git_push_failed: remote rejected".to_string());
        session.failed_from_phase = Some(Phase::CodeImplementPending);
        session.recovery_phase = Some(Phase::CodeReviewFixGlobalPending);
        session.recovery_owner = Some(Agent::Codex);
        session.recovery_origin_owner = Some(Agent::Claude);
        session.recovery_attempts = 3;
        // Distinct from `recovery_attempts` on purpose: the lifetime counter
        // is monotonic while the per-resume budget is reset, so the two
        // diverge in practice and a loader that mapped one column onto the
        // other would still pass if both were 3.
        session.total_recovery_attempts = 4;
        save_session(&db, &session).unwrap();

        let round_trip = load_session(&db, "sess-recovery").unwrap();
        assert_eq!(
            round_trip, session,
            "all seven recovery fields must round-trip byte-identical"
        );
        assert_eq!(
            round_trip.pending_failure.as_deref(),
            Some("git_push_failed: remote rejected")
        );
        assert_eq!(
            round_trip.failed_from_phase,
            Some(Phase::CodeImplementPending)
        );
        assert_eq!(
            round_trip.recovery_phase,
            Some(Phase::CodeReviewFixGlobalPending)
        );
        assert_eq!(round_trip.recovery_owner, Some(Agent::Codex));
        assert_eq!(round_trip.recovery_origin_owner, Some(Agent::Claude));
        assert_eq!(round_trip.recovery_attempts, 3);
        assert_eq!(round_trip.total_recovery_attempts, 4);
    }

    #[test]
    fn test_recovery_fields_null_legacy_row_defaults() {
        // A row that has never been through `save_session` — e.g. a legacy
        // pre-015 row, simulated here by a fresh `create_session` insert,
        // which leaves all seven recovery columns at their NULL column
        // default — must load without error, with every Option field `None`
        // and both attempt counters defaulted to `0` (not propagated as an
        // error or left uninitialized).
        let db = open();
        create_session(
            &db,
            "sess-legacy",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let session = load_session(&db, "sess-legacy").unwrap();
        assert!(session.pending_failure.is_none());
        assert!(session.failed_from_phase.is_none());
        assert!(session.recovery_phase.is_none());
        assert!(session.recovery_owner.is_none());
        assert!(session.recovery_origin_owner.is_none());
        assert_eq!(session.recovery_attempts, 0);
        assert_eq!(session.total_recovery_attempts, 0);
    }

    #[test]
    fn test_plan_drawer_ids_round_trip() {
        let db = open();
        create_session(
            &db,
            "sess-drawers",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        // Fresh session: both drawer ids must be NULL (legacy inline path).
        let session = load_session(&db, "sess-drawers").unwrap();
        assert!(session.canonical_plan_drawer_id.is_none());
        assert!(session.final_plan_drawer_id.is_none());

        // Set both to deterministic 32-char ids and persist.
        let mut session = session;
        session.canonical_plan_drawer_id = Some("c".repeat(32));
        session.final_plan_drawer_id = Some("f".repeat(32));
        save_session(&db, &session).unwrap();

        let round_trip = load_session(&db, "sess-drawers").unwrap();
        assert_eq!(
            round_trip.canonical_plan_drawer_id.as_deref(),
            Some("c".repeat(32).as_str())
        );
        assert_eq!(
            round_trip.final_plan_drawer_id.as_deref(),
            Some("f".repeat(32).as_str())
        );
    }

    // ── ack_messages_many tests ───────────────────────────────────────────────

    #[test]
    fn test_ack_messages_many_marks_all_acked() {
        let db = open();
        create_session(
            &db,
            "amm-1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let m1 = send_message(
            &db, "amm-1", "claude", "codex", "draft", "msg-a", "drawer-a",
        )
        .unwrap();
        let m2 = send_message(
            &db,
            "amm-1",
            "claude",
            "codex",
            "canonical",
            "msg-b",
            "drawer-b",
        )
        .unwrap();

        let count = ack_messages_many(&db, "amm-1", &[m1.clone(), m2.clone()]).unwrap();
        assert_eq!(count, 2, "both messages should be updated");

        // A subsequent recv must return nothing — both messages are acked.
        let remaining = recv_messages(&db, "amm-1", "codex", 10).unwrap();
        assert!(
            remaining.is_empty(),
            "no pending messages should remain after ack_messages_many"
        );
    }

    #[test]
    fn test_ack_messages_many_empty_list_is_noop() {
        let db = open();
        create_session(
            &db,
            "amm-2",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        send_message(
            &db, "amm-2", "claude", "codex", "draft", "msg-a", "drawer-a",
        )
        .unwrap();

        // Acking an empty list must not touch any rows.
        let count = ack_messages_many(&db, "amm-2", &[]).unwrap();
        assert_eq!(count, 0);

        let remaining = recv_messages(&db, "amm-2", "codex", 10).unwrap();
        assert_eq!(remaining.len(), 1, "message must still be pending");
    }

    #[test]
    fn test_ack_messages_many_partial_subset() {
        let db = open();
        create_session(
            &db,
            "amm-3",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let m1 = send_message(
            &db,
            "amm-3",
            "claude",
            "codex",
            "draft",
            "first",
            "drawer-first",
        )
        .unwrap();
        let m2 = send_message(
            &db,
            "amm-3",
            "claude",
            "codex",
            "draft",
            "second",
            "drawer-second",
        )
        .unwrap();
        let m3 = send_message(
            &db,
            "amm-3",
            "claude",
            "codex",
            "draft",
            "third",
            "drawer-third",
        )
        .unwrap();

        // Ack only the first two; the third must remain pending.
        let count = ack_messages_many(&db, "amm-3", &[m1, m2]).unwrap();
        assert_eq!(count, 2);

        let remaining = recv_messages(&db, "amm-3", "codex", 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, m3);
    }

    #[test]
    fn test_ack_messages_many_wrong_session_skipped() {
        let db = open();
        create_session(
            &db,
            "amm-4a",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        create_session(
            &db,
            "amm-4b",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let m1 = send_message(&db, "amm-4a", "claude", "codex", "draft", "x", "drawer-x").unwrap();

        // Passing the correct message ID but the WRONG session_id: zero rows
        // updated (no error, but the message is not acked in the correct session).
        let count = ack_messages_many(&db, "amm-4b", std::slice::from_ref(&m1)).unwrap();
        assert_eq!(count, 0, "cross-session ack must affect zero rows");

        // Message in the correct session remains unacked.
        let remaining = recv_messages(&db, "amm-4a", "codex", 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, m1);
    }

    #[test]
    fn find_active_session_including_terminal_isolates_repo_and_branch() {
        let db = open();
        // /repo-a: one ended (older) + one active session on the same branch.
        create_session(
            &db,
            "a-old",
            "/repo-a",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        end_session(&db, "a-old").unwrap();
        create_session(
            &db,
            "a-active-1",
            "/repo-a",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        create_session(
            &db,
            "a-active-2",
            "/repo-a",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        // `created_at` is second-resolution, so insertion order alone may not
        // disambiguate two same-second rows. Pin both active rows to the SAME
        // instant so the `id DESC` tie-break (not creation timing) is what
        // deterministically selects a-active-2.
        db.execute(
            "UPDATE collab_sessions SET created_at = '2026-01-01T00:00:00Z' \
             WHERE id IN ('a-active-1', 'a-active-2')",
            [],
        )
        .unwrap();
        // A different branch in the same repo, and a different repo, must not leak.
        create_session(
            &db,
            "a-other-branch",
            "/repo-a",
            "feature",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        create_session(
            &db,
            "b-active",
            "/repo-b",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        let found =
            find_active_session_by_repo_branch_including_terminal(&db, "/repo-a", "main").unwrap();
        assert_eq!(found.map(|(id, _)| id), Some("a-active-2".to_string()));

        // Branch with only ended sessions → None, even though the repo has others.
        end_session(&db, "a-active-1").unwrap();
        end_session(&db, "a-active-2").unwrap();
        assert!(
            find_active_session_by_repo_branch_including_terminal(&db, "/repo-a", "main")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            find_active_session_by_repo_branch_including_terminal(&db, "/repo-a", "feature")
                .unwrap()
                .map(|(id, _)| id),
            Some("a-other-branch".to_string()),
            "a sibling branch keeps its own session"
        );

        // Isolation: /repo-b still returns its own active session + a phase string.
        let b = find_active_session_by_repo_branch_including_terminal(&db, "/repo-b", "main")
            .unwrap()
            .unwrap();
        assert_eq!(b.0, "b-active");
        assert!(!b.1.is_empty());
    }

    #[test]
    fn find_active_session_by_repo_branch_releases_only_coding_complete() {
        let db = open();
        create_session(
            &db,
            "terminal-scope",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        assert_eq!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .map(|(id, _)| id),
            Some("terminal-scope".to_string()),
            "planning sessions are active"
        );

        let mut complete = load_session(&db, "terminal-scope").unwrap();
        complete.phase = Phase::CodingComplete;
        save_session(&db, &complete).unwrap();
        assert!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .is_none(),
            "CodingComplete releases the start slot before collab_end — attestation \
             is a human step and must not block the branch"
        );

        create_session(
            &db,
            "coding-scope",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut coding = load_session(&db, "coding-scope").unwrap();
        coding.phase = Phase::CodeImplementPending;
        coding.current_owner = Agent::Claude;
        save_session(&db, &coding).unwrap();
        assert_eq!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .map(|(id, _)| id),
            Some("coding-scope".to_string()),
            "coding sessions remain active"
        );

        coding.phase = Phase::CodingFailed;
        save_session(&db, &coding).unwrap();
        assert_eq!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .map(|(id, _)| id),
            Some("coding-scope".to_string()),
            "CodingFailed KEEPS its start slot: the session stays resumable, and the \
             resume guard refuses a scope owned by a newer session, so releasing it \
             would strand the failed session's plan and recovery state"
        );

        end_session(&db, "coding-scope").unwrap();
        assert!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .is_none(),
            "collab_end releases the slot"
        );
    }

    #[test]
    fn attribution_lookup_still_sees_coding_complete_sessions() {
        let db = open();
        create_session(
            &db,
            "attested",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        let mut complete = load_session(&db, "attested").unwrap();
        complete.phase = Phase::CodingComplete;
        save_session(&db, &complete).unwrap();

        assert!(
            find_active_session_by_repo_branch(&db, "/repo", "main")
                .unwrap()
                .is_none(),
            "start slot is released"
        );
        assert_eq!(
            find_active_session_by_repo_branch_including_terminal(&db, "/repo", "main")
                .unwrap()
                .map(|(id, _)| id),
            Some("attested".to_string()),
            "attribution must still see it: MetricsContext::resolve stamps \
             terminal-but-unended sessions, so the hook path has to agree"
        );

        end_session(&db, "attested").unwrap();
        assert!(
            find_active_session_by_repo_branch_including_terminal(&db, "/repo", "main")
                .unwrap()
                .is_none(),
            "collab_end ends attribution too"
        );
    }

    // ── checkpoint persistence (issue #273 task 3) ────────────────────────────

    fn checkpoint_fixture(session_id: &str, head: &str) -> CollabCheckpoint {
        CollabCheckpoint::from_json(&serde_json::json!({
            "session_id": session_id,
            "task_id": 1,
            "status": "started",
            "head_sha": head,
            "completed_task_ids": "",
        }))
        .unwrap()
    }

    #[test]
    fn checkpoint_round_trips() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        upsert_checkpoint(&db, &checkpoint_fixture("s1", "aaa111")).unwrap();
        let loaded = load_current_checkpoint(&db, "s1").unwrap().unwrap();

        assert_eq!(loaded.head_sha, "aaa111");
        assert_eq!(loaded.status, CheckpointStatus::Started);
        assert_eq!(loaded.attested_by, AttestedBy::Implementer);
        assert!(loaded.updated_at > 0, "server must stamp updated_at");
    }

    #[test]
    fn checkpoint_upsert_replaces_rather_than_accumulates() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        upsert_checkpoint(&db, &checkpoint_fixture("s1", "aaa111")).unwrap();

        // Force the row's stamp far into the past via raw SQL, so the second
        // upsert below — which must hit the ON CONFLICT DO UPDATE branch,
        // since the row already exists — is the only thing that can move it.
        // This isolates the UPDATE branch's `updated_at = excluded.updated_at`
        // clause: without it, a checkpoint could advance status/head_sha/etc.
        // on this branch while its timestamp stayed frozen at `1`, which is
        // #273's exact failure mode (a stale checkpoint presented as current).
        db.execute(
            "UPDATE collab_checkpoints SET updated_at = 1 WHERE session_id = 's1'",
            [],
        )
        .unwrap();

        let mut advanced = checkpoint_fixture("s1", "bbb222");
        advanced.status = CheckpointStatus::Completed;
        advanced.completed_task_ids = vec![1];
        upsert_checkpoint(&db, &advanced).unwrap();

        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM collab_checkpoints WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "one current checkpoint per session");

        let loaded = load_current_checkpoint(&db, "s1").unwrap().unwrap();
        assert_eq!(loaded.head_sha, "bbb222");
        assert_eq!(loaded.completed_task_ids, vec![1]);
        assert_ne!(
            loaded.updated_at, 1,
            "the UPDATE branch must refresh updated_at, not leave the forced-stale value"
        );
    }

    #[test]
    fn load_current_checkpoint_is_none_for_a_session_without_one() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();
        assert!(load_current_checkpoint(&db, "s1").unwrap().is_none());
    }

    /// Every field must survive the round trip — a dropped column here would
    /// silently weaken the `implementation_done` gate downstream.
    ///
    /// "Every" is meant literally: all sixteen fields of `CollabCheckpoint`
    /// are asserted below, `updated_at` as "the server restamped it" rather
    /// than as a value, since `upsert_checkpoint` deliberately overwrites
    /// whatever the caller held. A test whose doc claims total coverage while
    /// leaving a field unasserted is worse than one that claims less: it stops
    /// the next reader looking.
    ///
    /// The fixture is a full struct literal rather than a `from_json` parse
    /// for exactly that reason. `from_json` leaves `attestation_check` `None`
    /// by design — the verdict is server-derived, stamped by the MCP handler
    /// from its own git reads — so a parsed fixture can only ever round-trip
    /// the `None` case, and this layer's persistence of a real verdict would
    /// go untested while the paragraph above claimed otherwise. Naming the
    /// fields is also what makes the count enforceable: a field gained or lost
    /// stops this compiling rather than quietly slipping past the assertions.
    #[test]
    fn checkpoint_round_trips_every_field() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        let full = CollabCheckpoint {
            session_id: "s1".to_string(),
            task_id: Some(4),
            task_title: Some("Wire the gate".to_string()),
            status: CheckpointStatus::BatchComplete,
            head_sha: "ccc333".to_string(),
            commit_sha: Some("ccc333".to_string()),
            completed_task_ids: vec![1, 2, 3, 4],
            next_task_id: Some(5),
            gates_result: "passed".to_string(),
            gates_sha: Some("ccc333".to_string()),
            gates_commands: Some(
                "cargo fmt --all -- --check && cargo test --workspace".to_string(),
            ),
            summary: Some("batch done".to_string()),
            attested_by: AttestedBy::Operator,
            acknowledged_divergence: Some("aaa111..ccc333".to_string()),
            attestation_check: Some(AttestationCheck::Verified),
            updated_at: 0,
        };

        upsert_checkpoint(&db, &full).unwrap();
        let loaded = load_current_checkpoint(&db, "s1").unwrap().unwrap();

        // The row's own key, read back from the column rather than assumed
        // from the lookup argument. Tasks 5-10 compare this against the
        // session being gated, so a loader that dropped or substituted it
        // would gate the wrong session's progress.
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.task_id, Some(4));
        assert_eq!(loaded.task_title.as_deref(), Some("Wire the gate"));
        assert_eq!(loaded.status, CheckpointStatus::BatchComplete);
        assert_eq!(loaded.head_sha, "ccc333");
        assert_eq!(loaded.commit_sha.as_deref(), Some("ccc333"));
        assert_eq!(loaded.completed_task_ids, vec![1, 2, 3, 4]);
        // The resume pointer: a dropped column here would silently strand a
        // resumer with no next task to pick up.
        assert_eq!(loaded.next_task_id, Some(5));
        assert_eq!(loaded.gates_result, "passed");
        assert_eq!(loaded.gates_sha.as_deref(), Some("ccc333"));
        // The exact gate command set: this is what lets a resumer tell a
        // reusable gate proof from one invalidated by a changed gate set.
        assert_eq!(
            loaded.gates_commands.as_deref(),
            Some("cargo fmt --all -- --check && cargo test --workspace")
        );
        assert_eq!(loaded.summary.as_deref(), Some("batch done"));
        assert_eq!(loaded.attested_by, AttestedBy::Operator);
        assert_eq!(
            loaded.acknowledged_divergence.as_deref(),
            Some("aaa111..ccc333")
        );
        // The server's own verdict on that range. It reaches the row only
        // through this column, and `attestation_verdict` renders an operator
        // row whose verdict is missing as `unrecorded` — so a loader that
        // dropped this would quietly downgrade every verified attestation to
        // "unchecked" with no error anywhere.
        assert_eq!(loaded.attestation_check, Some(AttestationCheck::Verified));
        assert!(
            loaded.updated_at > 0,
            "the server stamps updated_at; `full` was built carrying 0"
        );
        assert!(loaded.gates_are_green_at_head());
    }

    /// The obligation Task 2 left this loader: rebuilding a `CollabCheckpoint`
    /// field-by-field from a row bypasses every rule `from_json` enforces
    /// unless `validate()` is called on the reconstructed struct. Migration
    /// 020's CHECK is one-directional and *deliberately* permits
    /// `attested_by = 'operator'` with `acknowledged_divergence` still NULL —
    /// its header calls that combination's exclusion "a tool-layer rule, not
    /// a schema guarantee" — so a raw INSERT of exactly that row must load as
    /// an error, not a checkpoint claiming an unnamed operator escape hatch
    /// from the head-consistency gate.
    #[test]
    fn load_current_checkpoint_rejects_an_operator_row_with_no_acknowledged_divergence() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        // Written with raw SQL, deliberately bypassing upsert_checkpoint (and
        // therefore CollabCheckpoint::validate) entirely — this is the row
        // migration 020's schema permits but the domain rules forbid.
        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, status, head_sha, attested_by, updated_at)
             VALUES ('s1', 'started', 'aaa111', 'operator', 1)",
            [],
        )
        .unwrap();

        let err = load_current_checkpoint(&db, "s1").unwrap_err();
        assert!(
            err.to_string().contains("s1") && err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );
    }

    /// The mirror of Requirement B: a corrupted `completed_task_ids` value
    /// must fail loudly rather than silently drop the unparseable entry. A
    /// `filter_map(...ok())` loader would read `"1,2,X,4"` as `[1, 2, 4]` —
    /// a checkpoint that quietly under-reports progress with no error
    /// anywhere, which matters because `covers_all_tasks` gates
    /// `implementation_done` on this exact field.
    #[test]
    fn load_current_checkpoint_rejects_a_corrupt_completed_task_ids_list() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, status, head_sha, completed_task_ids, updated_at)
             VALUES ('s1', 'started', 'aaa111', '1,2,X,4', 1)",
            [],
        )
        .unwrap();

        let err = load_current_checkpoint(&db, "s1").unwrap_err();
        assert!(
            err.to_string().contains("completed_task_ids") && err.to_string().contains("X"),
            "got: {err}"
        );
    }

    /// The same silent-corruption failure Requirement B refuses for
    /// `completed_task_ids`, one column over: `task_id` has no CHECK in
    /// migration 020, so a raw write can put a negative value in it. A bare
    /// `as u32` cast would wrap that into an unrelated positive task id
    /// instead of failing, which is exactly the kind of quiet
    /// misrepresentation this loader exists to refuse.
    #[test]
    fn load_current_checkpoint_rejects_a_negative_task_id() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, task_id, status, head_sha, updated_at)
             VALUES ('s1', -1, 'started', 'aaa111', 1)",
            [],
        )
        .unwrap();

        let err = load_current_checkpoint(&db, "s1").unwrap_err();
        assert!(
            err.to_string().contains("task_id") && err.to_string().contains("-1"),
            "got: {err}"
        );
    }

    /// `optional_task_id` in `checkpoint.rs` refuses `task_id = 0` on two
    /// grounds — out of range, and zero, task ids being 1-based —
    /// `checked_task_id_column` mirroring only the first would still let a
    /// phantom task id `0` written by a corrupted row reach the
    /// `implementation_done` gate.
    #[test]
    fn load_current_checkpoint_rejects_a_zero_task_id() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, task_id, status, head_sha, updated_at)
             VALUES ('s1', 0, 'started', 'aaa111', 1)",
            [],
        )
        .unwrap();

        let err = load_current_checkpoint(&db, "s1").unwrap_err();
        assert!(
            err.to_string().contains("task_id") && err.to_string().contains('0'),
            "got: {err}"
        );
    }

    /// The `completed_task_ids` mirror of the test above: `checkpoint.rs`'s
    /// parser rejects a `0` entry the same way it rejects a non-numeric one,
    /// so `parse_stored_completed_task_ids` must refuse `"0,1"`, not load it
    /// as `[0, 1]`.
    #[test]
    fn load_current_checkpoint_rejects_a_zero_entry_in_completed_task_ids() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, status, head_sha, completed_task_ids, updated_at)
             VALUES ('s1', 'started', 'aaa111', '0,1', 1)",
            [],
        )
        .unwrap();

        let err = load_current_checkpoint(&db, "s1").unwrap_err();
        assert!(
            err.to_string().contains("completed_task_ids") && err.to_string().contains('0'),
            "got: {err}"
        );
    }

    /// The write-side twin of
    /// `load_current_checkpoint_rejects_an_operator_row_with_no_acknowledged_divergence`.
    /// Every field on `CollabCheckpoint` is `pub`, so a caller can build one
    /// directly — exactly the field-by-field construction
    /// `checkpoint_upsert_replaces_rather_than_accumulates` above uses via
    /// `checkpoint_fixture` plus mutation — and hand `upsert_checkpoint` a
    /// struct that never went through `from_json`'s checks. Without a
    /// `validate()` call at the top of `upsert_checkpoint`, this exact
    /// operator/no-divergence combination would insert cleanly (migration
    /// 020's CHECK is one-directional and permits it) and then permanently
    /// fail every subsequent `load_current_checkpoint` for the session: a
    /// write-succeeds, read-always-fails poison row with no way to read or
    /// fix it back out.
    #[test]
    fn upsert_checkpoint_rejects_an_operator_struct_with_no_acknowledged_divergence() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        let mut poisoned = checkpoint_fixture("s1", "aaa111");
        poisoned.attested_by = AttestedBy::Operator;
        // acknowledged_divergence left None: the combination validate() must
        // refuse.

        let err = upsert_checkpoint(&db, &poisoned).unwrap_err();
        assert!(
            err.to_string().contains("acknowledged_divergence"),
            "got: {err}"
        );

        // And confirm the reject was real, not merely reported: no row was
        // written at all, so there is no poison row to strand the session.
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM collab_checkpoints WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "an invalid checkpoint must not be written at all");
    }

    /// The write half of the required-field rule, and the reason it belongs in
    /// `CollabCheckpoint::validate` rather than in either function that calls
    /// it. `head_sha` is the field this whole issue turns on, and before this
    /// every layer declined to enforce it: `from_json` rejects a blank one but
    /// a struct built field-by-field never goes through `from_json`;
    /// migration 020 has `NOT NULL` on the column and no
    /// `CHECK (head_sha <> '')`. So `cp.head_sha = String::new()` wrote
    /// cleanly and loaded back as `Some("")`. Fail-safe in direction — `""`
    /// can never equal live git HEAD, so the Tasks 5-10 divergence gate blocks
    /// — but it persists a checkpoint whose recorded HEAD is a blank, and the
    /// resulting gate failure is undiagnosable.
    ///
    /// Each value is checked on its own write, against its own empty table, so
    /// no case can be carried by another.
    #[test]
    fn upsert_checkpoint_rejects_a_blank_required_field() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        for blank in ["", "   ", "none"] {
            for field in ["head_sha", "gates_result"] {
                let mut cp = checkpoint_fixture("s1", "aaa111");
                if field == "head_sha" {
                    cp.head_sha = blank.to_string();
                } else {
                    cp.gates_result = blank.to_string();
                }

                let err = match upsert_checkpoint(&db, &cp) {
                    Ok(()) => panic!("{field} = {blank:?} must not be writable"),
                    Err(err) => err.to_string(),
                };
                assert!(err.contains(field) && err.contains("s1"), "got: {err}");

                let count: i64 = db
                    .query_row("SELECT COUNT(*) FROM collab_checkpoints", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(
                    count, 0,
                    "{field} = {blank:?} was rejected but a row was written anyway"
                );
            }
        }
    }

    /// The read half. `upsert_checkpoint` cannot be the only guard: the row
    /// this refuses is one migration 020 permits, so a direct SQL write — or
    /// any row that predates the rule — reaches the loader without ever
    /// passing the writer. Written with raw SQL for exactly that reason.
    #[test]
    fn load_current_checkpoint_rejects_a_blank_required_field() {
        for blank in ["", "   ", "none"] {
            for field in ["head_sha", "gates_result"] {
                let db = open();
                create_session(
                    &db,
                    "s1",
                    "/repo",
                    "main",
                    None,
                    CollabRoles {
                        pilot: Agent::Claude,
                        implementer: Agent::Claude,
                    },
                )
                .unwrap();

                let (head_sha, gates_result) = if field == "head_sha" {
                    (blank, "not_run")
                } else {
                    ("aaa111", blank)
                };
                db.execute(
                    "INSERT INTO collab_checkpoints
                       (session_id, status, head_sha, gates_result, updated_at)
                     VALUES ('s1', 'started', ?1, ?2, 1)",
                    params![head_sha, gates_result],
                )
                .unwrap();

                let err = match load_current_checkpoint(&db, "s1") {
                    Ok(loaded) => panic!("{field} = {blank:?} loaded as {loaded:?}"),
                    Err(err) => err.to_string(),
                };
                assert!(err.contains(field) && err.contains("s1"), "got: {err}");
            }
        }
    }

    /// The write gate must be at least as strict as the read gate, for every
    /// field — otherwise a value the loader refuses can still be written, and
    /// the row is a poison pill: written once, unreadable forever, and
    /// unrepairable through any load-then-write path because the load is what
    /// errors.
    ///
    /// `validate()` does not close this on its own. It covers the three
    /// `String` fields and the attestation correlation and never looks at the
    /// task ids, while `checked_task_id_column` and
    /// `parse_stored_completed_task_ids` both refuse a `0` on load, and
    /// migration 020 has no CHECK on either column. `from_json` refuses these
    /// too, so only a struct built field-by-field reaches them — which is
    /// exactly the construction all-`pub` fields exist for, and what the
    /// loader itself does.
    ///
    /// Asserting the row count is the load-bearing half: an error return that
    /// still wrote the row would leave the session poisoned regardless of what
    /// the caller was told.
    #[test]
    fn upsert_checkpoint_refuses_every_value_the_loader_would_refuse() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        // Each case mutates one field of an otherwise-valid checkpoint to a
        // value `from_json` would have refused, so no case is carried by
        // another.
        for field in ["task_id", "next_task_id", "completed_task_ids"] {
            let mut cp = checkpoint_fixture("s1", "aaa111");
            match field {
                "task_id" => cp.task_id = Some(0),
                "next_task_id" => cp.next_task_id = Some(0),
                _ => cp.completed_task_ids = vec![0, 2],
            }

            let err = match upsert_checkpoint(&db, &cp) {
                Ok(()) => panic!("{field} = 0 must not be writable"),
                Err(err) => err.to_string(),
            };
            assert!(
                err.contains(field) && err.contains("s1"),
                "{field}: got {err}"
            );

            let count: i64 = db
                .query_row("SELECT COUNT(*) FROM collab_checkpoints", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                count, 0,
                "{field} = 0 was rejected but a row was written anyway"
            );
        }

        // The other direction, so the rule above cannot be satisfied by a
        // writer that refuses everything: the same fields at legal values
        // write and load back.
        let mut ok = checkpoint_fixture("s1", "aaa111");
        ok.task_id = Some(1);
        ok.next_task_id = Some(2);
        ok.completed_task_ids = vec![1];
        upsert_checkpoint(&db, &ok).unwrap();
        let loaded = load_current_checkpoint(&db, "s1").unwrap().unwrap();
        assert_eq!(loaded.task_id, Some(1));
        assert_eq!(loaded.next_task_id, Some(2));
        assert_eq!(loaded.completed_task_ids, vec![1]);
    }

    /// The write half of the blank-range rule. Distinct from
    /// `upsert_checkpoint_rejects_an_operator_struct_with_no_acknowledged_divergence`
    /// above, which covers `None`: this covers the state that *passes* a
    /// presence check while naming nothing, and it is the one blank value on
    /// this type that is not fail-safe. A blank `head_sha` can never equal
    /// live git HEAD, so it blocks the Tasks 7-10 divergence gate; a blank
    /// `acknowledged_divergence` is the escape hatch *from* that gate, so it
    /// makes the gate pass on a checkpoint asserting that a human vouched for
    /// no commits at all. Migration 020's CHECK permits the row (the column is
    /// non-NULL and `attested_by` is `operator`), so `validate` is the only
    /// thing standing in its way.
    #[test]
    fn upsert_checkpoint_rejects_a_blank_operator_range() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        for blank in ["", "   ", "none"] {
            let mut cp = checkpoint_fixture("s1", "aaa111");
            cp.attested_by = AttestedBy::Operator;
            cp.acknowledged_divergence = Some(blank.to_string());

            let err = match upsert_checkpoint(&db, &cp) {
                Ok(()) => panic!("an operator range of {blank:?} must not be writable"),
                Err(err) => err.to_string(),
            };
            assert!(
                err.contains("acknowledged_divergence") && err.contains("s1"),
                "got: {err}"
            );

            let count: i64 = db
                .query_row("SELECT COUNT(*) FROM collab_checkpoints", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                count, 0,
                "operator range {blank:?} was rejected but a row was written anyway"
            );
        }
    }

    /// The read half. Raw SQL, because the row this refuses is one migration
    /// 020 permits — its `CHECK` only forbids an *implementer* row carrying a
    /// range — so a direct write reaches the loader without passing the
    /// writer, and a checkpoint claiming an empty operator attestation would
    /// otherwise load clean straight into the gate it exempts.
    #[test]
    fn load_current_checkpoint_rejects_a_blank_operator_range() {
        for blank in ["", "   ", "none"] {
            let db = open();
            create_session(
                &db,
                "s1",
                "/repo",
                "main",
                None,
                CollabRoles {
                    pilot: Agent::Claude,
                    implementer: Agent::Claude,
                },
            )
            .unwrap();

            db.execute(
                "INSERT INTO collab_checkpoints
                   (session_id, status, head_sha, attested_by,
                    acknowledged_divergence, updated_at)
                 VALUES ('s1', 'started', 'aaa111', 'operator', ?1, 1)",
                params![blank],
            )
            .unwrap();

            let err = match load_current_checkpoint(&db, "s1") {
                Ok(loaded) => panic!("an operator range of {blank:?} loaded as {loaded:?}"),
                Err(err) => err.to_string(),
            };
            assert!(
                err.contains("acknowledged_divergence") && err.contains("s1"),
                "got: {err}"
            );
        }
    }

    /// `checked_task_id_column` and `checkpoint.rs`'s `optional_task_id` are
    /// two independent statements of one rule — a task id is 1-based and fits
    /// in `u32` — on the load and the parse path respectively. Nothing else
    /// couples them, so relaxing or tightening either silently stops the
    /// loader mirroring the parser and reopens the gap where a value the tool
    /// path refuses is still readable out of the table.
    ///
    /// Same idiom as `checkpoint.rs`'s
    /// `status_variants_match_migration_020`: feed one candidate set through
    /// both statements and assert they agree. It lives here, not in
    /// `checkpoint.rs`, because the obligation is the loader's — `checkpoint`
    /// is deliberately a pure parse/validate unit that names nothing in the
    /// SQL layer, and the cheaper coupling is to expose the parser helper
    /// `pub(crate)` than to have the parser's tests reach into persistence.
    #[test]
    fn task_id_column_loader_mirrors_the_parser() {
        use crate::collab::checkpoint::optional_task_id;

        for candidate in [
            None,
            Some(0),
            Some(-1),
            Some(i64::from(u32::MAX) + 1),
            Some(1),
            Some(42),
            Some(i64::from(u32::MAX)),
        ] {
            let json = serde_json::json!({ "task_id": candidate });
            let parsed = optional_task_id(&json, "task_id");
            let loaded = checked_task_id_column(candidate, "task_id", "s1");

            assert_eq!(
                parsed.is_ok(),
                loaded.is_ok(),
                "task_id {candidate:?}: parser says {parsed:?}, loader says {loaded:?}"
            );
            if let (Ok(parsed), Ok(loaded)) = (parsed, loaded) {
                assert_eq!(
                    parsed, loaded,
                    "task_id {candidate:?} parses and loads to different values"
                );
            }
        }
    }

    /// `CollabCheckpoint::completed_task_ids`' doc promises that equal
    /// progress is equal data, which is what makes an equality or diff over
    /// stored checkpoints mean anything. `from_json` delivers that with a
    /// `BTreeSet`; the loader has to deliver it separately, because a stored
    /// value can predate the rule or come from a direct SQL write. Without
    /// this the loader's `sort_unstable`/`dedup` can be deleted with the whole
    /// suite still green — `covers_all_tasks` builds its own set and so does
    /// not notice.
    #[test]
    fn load_current_checkpoint_normalizes_a_stored_task_id_list() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        db.execute(
            "INSERT INTO collab_checkpoints
               (session_id, status, head_sha, completed_task_ids, updated_at)
             VALUES ('s1', 'started', 'aaa111', '3,1,2,2', 1)",
            [],
        )
        .unwrap();

        let loaded = load_current_checkpoint(&db, "s1").unwrap().unwrap();
        assert_eq!(loaded.completed_task_ids, vec![1, 2, 3]);
    }

    /// Tasks 5-10 will call `upsert_checkpoint` inside
    /// `Database::with_transaction`, so the write must be an ordinary
    /// participant in its caller's transaction rather than something that
    /// commits on its own. If it opened or committed a transaction of its own,
    /// an abandoned outer transaction would leave a checkpoint behind claiming
    /// progress the surrounding operation rolled back — the same
    /// "recorded progress that did not happen" failure issue #273 is about.
    /// `with_transaction` also replays its closure on `SQLITE_BUSY_SNAPSHOT`,
    /// which this satisfies for free: a pure upsert with no accumulation is
    /// idempotent, and the rollback below is exactly the state a replayed
    /// attempt restarts from.
    #[test]
    fn upsert_checkpoint_inside_a_rolled_back_transaction_leaves_no_checkpoint() {
        let db = open();
        create_session(
            &db,
            "s1",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        {
            let tx = db.unchecked_transaction().unwrap();
            upsert_checkpoint(&tx, &checkpoint_fixture("s1", "aaa111")).unwrap();
            // Visible inside the transaction...
            assert!(load_current_checkpoint(&tx, "s1").unwrap().is_some());
            // ...and dropped without a commit, which rolls it back.
        }

        assert!(
            load_current_checkpoint(&db, "s1").unwrap().is_none(),
            "a rolled-back transaction must leave no checkpoint"
        );
    }
}
