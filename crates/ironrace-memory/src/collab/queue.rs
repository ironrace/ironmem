//! SQLite-backed queue and session persistence for the collab protocol.

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::{CollabSession, Phase};
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
) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO collab_sessions (id, repo_path, branch, task) VALUES (?1, ?2, ?3, ?4)",
        params![id, repo_path, branch, task],
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
    conn.query_row(
        "SELECT id, phase, current_owner, repo_path, branch,
                claude_draft_hash, codex_draft_hash, canonical_plan_hash,
                final_plan_hash, codex_review_verdict,
                review_round, task, ended_at,
                created_at, updated_at
         FROM collab_sessions
         WHERE id = ?1",
        params![session_id],
        |row| {
            let phase: String = row.get(1)?;
            let phase = Phase::try_from(phase.as_str()).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
                )
            })?;
            let review_round_i: i64 = row.get(10)?;
            let review_round = review_round_i.clamp(0, u8::MAX as i64) as u8;
            Ok(SessionRecord {
                session: CollabSession {
                    id: row.get(0)?,
                    phase,
                    current_owner: row.get(2)?,
                    claude_draft_hash: row.get(5)?,
                    codex_draft_hash: row.get(6)?,
                    canonical_plan_hash: row.get(7)?,
                    final_plan_hash: row.get(8)?,
                    codex_review_verdict: row.get(9)?,
                    review_round,
                },
                repo_path: row.get(3)?,
                branch: row.get(4)?,
                task: row.get(11)?,
                ended_at: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| MemoryError::NotFound(format!("session {session_id} not found")))
}

pub fn save_session(conn: &Connection, session: &CollabSession) -> Result<(), MemoryError> {
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
             updated_at = datetime('now')
        WHERE id = ?9",
        params![
            session.phase.to_string(),
            session.current_owner.as_str(),
            session.claude_draft_hash.as_deref(),
            session.codex_draft_hash.as_deref(),
            session.canonical_plan_hash.as_deref(),
            session.final_plan_hash.as_deref(),
            session.codex_review_verdict.as_deref(),
            session.review_round as i64,
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

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(BASE_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(COLLAB_SQL).unwrap();
        conn.execute_batch(COLLAB_V1_SQL).unwrap();
        conn
    }

    #[test]
    fn test_send_recv_ack_fifo() {
        let db = open();
        create_session(&db, "sess1", "/repo", "main", None).unwrap();
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
        create_session(&db, "sess2", "/repo", "main", None).unwrap();
        let message_id = send_message(&db, "sess2", "claude", "codex", "draft", "x").unwrap();
        ack_message(&db, "sess2", &message_id).unwrap();
        let err = ack_message(&db, "wrong-session", &message_id).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_register_caps_upsert() {
        let db = open();
        create_session(&db, "sess3", "/repo", "main", None).unwrap();
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
        create_session(&db, "sess4", "/repo", "main", None).unwrap();
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
        create_session(&db, "sess-rr", "/repo", "main", None).unwrap();
        let mut session = load_session(&db, "sess-rr").unwrap();
        session.review_round = 2;
        save_session(&db, &session).unwrap();
        let round_trip = load_session(&db, "sess-rr").unwrap();
        assert_eq!(round_trip.review_round, 2);
    }

    #[test]
    fn test_ensure_active_rejects_ended_session() {
        let db = open();
        create_session(&db, "sess-end", "/repo", "main", None).unwrap();
        ensure_active(&db, "sess-end").unwrap();
        end_session(&db, "sess-end").unwrap();
        let err = ensure_active(&db, "sess-end").unwrap_err();
        assert!(err.to_string().contains("has ended"));
    }

    #[test]
    fn test_end_session_idempotent() {
        let db = open();
        create_session(&db, "sess-end2", "/repo", "main", None).unwrap();
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
}
