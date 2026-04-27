//! SQLite-backed queue and session persistence for the collab protocol.

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::{Agent, CollabSession, Phase};
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub sender: String,
    pub receiver: String,
    pub topic: String,
    pub content: String,
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
    implementer: Agent,
) -> Result<(), MemoryError> {
    // `Agent` is a closed enum so the canonical wire form is guaranteed —
    // no application-layer string validation is needed here. The DB CHECK
    // constraint on the column remains as defense-in-depth against direct
    // SQL writes.
    conn.execute(
        "INSERT INTO collab_sessions (id, repo_path, branch, task, implementer)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, repo_path, branch, task, implementer.as_str()],
    )?;
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
                task_list,
                task_review_round, global_review_round,
                base_sha, last_head_sha, pr_url, coding_failure,
                created_at, updated_at, implementer
         FROM collab_sessions
         WHERE id = ?1",
        params![session_id],
        |row| {
            let phase = parse_text_column::<Phase>(row, "phase")?;
            let current_owner = parse_text_column::<Agent>(row, "current_owner")?;
            let implementer = parse_text_column::<Agent>(row, "implementer")?;
            let review_round_i: i64 = row.get("review_round")?;
            let review_round = review_round_i.clamp(0, u8::MAX as i64) as u8;
            let task_list: Option<String> = row.get("task_list")?;
            let task_review_round_i: i64 = row.get("task_review_round")?;
            let global_review_round_i: i64 = row.get("global_review_round")?;
            Ok(SessionRecord {
                session: CollabSession {
                    id: row.get("id")?,
                    phase,
                    current_owner,
                    claude_draft_hash: row.get("claude_draft_hash")?,
                    codex_draft_hash: row.get("codex_draft_hash")?,
                    canonical_plan_hash: row.get("canonical_plan_hash")?,
                    final_plan_hash: row.get("final_plan_hash")?,
                    codex_review_verdict: row.get("codex_review_verdict")?,
                    review_round,
                    task_list,
                    task_review_round: task_review_round_i.clamp(0, u8::MAX as i64) as u8,
                    global_review_round: global_review_round_i.clamp(0, u8::MAX as i64) as u8,
                    base_sha: row.get("base_sha")?,
                    last_head_sha: row.get("last_head_sha")?,
                    pr_url: row.get("pr_url")?,
                    coding_failure: row.get("coding_failure")?,
                    implementer,
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

pub fn save_session(conn: &Connection, session: &CollabSession) -> Result<(), MemoryError> {
    // `implementer` is set at INSERT time and immutable thereafter; we
    // include it in the UPDATE list defensively so any future rebind of
    // the field stays consistent with the rest of the session, and so a
    // future maintainer doesn't read the absence as a bug to fix.
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
             task_review_round = ?10,
             global_review_round = ?11,
             base_sha = ?12,
             last_head_sha = ?13,
             pr_url = ?14,
             coding_failure = ?15,
             implementer = ?16,
             updated_at = datetime('now')
        WHERE id = ?17",
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
            session.task_review_round as i64,
            session.global_review_round as i64,
            session.base_sha.as_deref(),
            session.last_head_sha.as_deref(),
            session.pr_url.as_deref(),
            session.coding_failure.as_deref(),
            session.implementer.as_str(),
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

pub fn send_message(
    conn: &Connection,
    session_id: &str,
    sender: &str,
    receiver: &str,
    topic: &str,
    content: &str,
) -> Result<String, MemoryError> {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO messages (id, session_id, sender, receiver, topic, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, session_id, sender, receiver, topic, content],
    )?;
    Ok(id)
}

pub fn recv_messages(
    conn: &Connection,
    session_id: &str,
    receiver: &str,
    limit: usize,
) -> Result<Vec<Message>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, sender, receiver, topic, content, status, created_at
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
            status: row.get(6)?,
            created_at: row.get(7)?,
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

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BASE_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(COLLAB_SQL).unwrap();
        conn.execute_batch(COLLAB_V1_SQL).unwrap();
        conn.execute_batch(COLLAB_V2_SQL).unwrap();
        conn.execute_batch(COLLAB_IMPLEMENTER_SQL).unwrap();
        conn.execute_batch(DROP_CURRENT_TASK_INDEX_SQL).unwrap();
        conn
    }

    #[test]
    fn test_send_recv_ack_fifo() {
        let db = open();
        create_session(&db, "sess1", "/repo", "main", None, Agent::Claude).unwrap();
        let m1 = send_message(&db, "sess1", "claude", "codex", "draft", "first").unwrap();
        let _m2 = send_message(&db, "sess1", "claude", "codex", "draft", "second").unwrap();

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
    fn test_ack_idempotent() {
        let db = open();
        create_session(&db, "sess2", "/repo", "main", None, Agent::Claude).unwrap();
        let message_id = send_message(&db, "sess2", "claude", "codex", "draft", "x").unwrap();
        ack_message(&db, "sess2", &message_id).unwrap();
        let err = ack_message(&db, "wrong-session", &message_id).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_register_caps_upsert() {
        let db = open();
        create_session(&db, "sess3", "/repo", "main", None, Agent::Claude).unwrap();
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
        create_session(&db, "sess4", "/repo", "main", None, Agent::Claude).unwrap();
        let caps = get_caps(&db, "sess4", Some("claude")).unwrap();
        assert!(caps.is_empty());
    }

    #[test]
    fn test_orphan_message_fk_violation() {
        let db = open();
        let err =
            send_message(&db, "missing-session", "claude", "codex", "draft", "x").unwrap_err();
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
            Agent::Claude,
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
        create_session(&db, "sess-rr", "/repo", "main", None, Agent::Claude).unwrap();
        let mut session = load_session(&db, "sess-rr").unwrap();
        session.review_round = 2;
        save_session(&db, &session).unwrap();
        let round_trip = load_session(&db, "sess-rr").unwrap();
        assert_eq!(round_trip.review_round, 2);
    }

    #[test]
    fn test_ensure_active_rejects_ended_session() {
        let db = open();
        create_session(&db, "sess-end", "/repo", "main", None, Agent::Claude).unwrap();
        ensure_active(&db, "sess-end").unwrap();
        end_session(&db, "sess-end").unwrap();
        let err = ensure_active(&db, "sess-end").unwrap_err();
        assert!(err.to_string().contains("has ended"));
    }

    #[test]
    fn test_end_session_idempotent() {
        let db = open();
        create_session(&db, "sess-end2", "/repo", "main", None, Agent::Claude).unwrap();
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
        create_session(&db, "sess-v2", "/repo", "main", None, Agent::Claude).unwrap();
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
        create_session(&db, "sess-fresh", "/repo", "main", None, Agent::Claude).unwrap();
        let session = load_session(&db, "sess-fresh").unwrap();
        assert!(session.task_list.is_none());
        assert_eq!(session.task_review_round, 0);
        assert_eq!(session.global_review_round, 0);
        assert!(session.base_sha.is_none());
        assert!(session.last_head_sha.is_none());
        assert!(session.pr_url.is_none());
        assert!(session.coding_failure.is_none());
        assert_eq!(session.tasks_count(), None);
    }

    // ── ack_messages_many tests ───────────────────────────────────────────────

    #[test]
    fn test_ack_messages_many_marks_all_acked() {
        let db = open();
        create_session(&db, "amm-1", "/repo", "main", None, Agent::Claude).unwrap();
        let m1 = send_message(&db, "amm-1", "claude", "codex", "draft", "msg-a").unwrap();
        let m2 = send_message(&db, "amm-1", "claude", "codex", "canonical", "msg-b").unwrap();

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
        create_session(&db, "amm-2", "/repo", "main", None, Agent::Claude).unwrap();
        send_message(&db, "amm-2", "claude", "codex", "draft", "msg-a").unwrap();

        // Acking an empty list must not touch any rows.
        let count = ack_messages_many(&db, "amm-2", &[]).unwrap();
        assert_eq!(count, 0);

        let remaining = recv_messages(&db, "amm-2", "codex", 10).unwrap();
        assert_eq!(remaining.len(), 1, "message must still be pending");
    }

    #[test]
    fn test_ack_messages_many_partial_subset() {
        let db = open();
        create_session(&db, "amm-3", "/repo", "main", None, Agent::Claude).unwrap();
        let m1 = send_message(&db, "amm-3", "claude", "codex", "draft", "first").unwrap();
        let m2 = send_message(&db, "amm-3", "claude", "codex", "draft", "second").unwrap();
        let m3 = send_message(&db, "amm-3", "claude", "codex", "draft", "third").unwrap();

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
        create_session(&db, "amm-4a", "/repo", "main", None, Agent::Claude).unwrap();
        create_session(&db, "amm-4b", "/repo", "main", None, Agent::Claude).unwrap();
        let m1 = send_message(&db, "amm-4a", "claude", "codex", "draft", "x").unwrap();

        // Passing the correct message ID but the WRONG session_id: zero rows
        // updated (no error, but the message is not acked in the correct session).
        let count = ack_messages_many(&db, "amm-4b", std::slice::from_ref(&m1)).unwrap();
        assert_eq!(count, 0, "cross-session ack must affect zero rows");

        // Message in the correct session remains unacked.
        let remaining = recv_messages(&db, "amm-4a", "codex", 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, m1);
    }
}
