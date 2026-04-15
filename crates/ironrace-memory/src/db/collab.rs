//! Database helpers for the collab protocol: session CRUD, message queue, capability registry.

use rusqlite::params;

use crate::db::schema::Database;
use crate::error::MemoryError;

// ── Session row ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub phase: String,
    pub current_owner: String,
    pub round: i64,
    pub max_rounds: i64,
    pub repo_path: String,
    pub branch: String,
    pub claude_ok: bool,
    pub codex_ok: bool,
    pub content_hash: Option<String>,
    pub rejected_hashes: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ── Message row ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: String,
    pub session_id: String,
    pub sender: String,
    pub receiver: String,
    pub topic: String,
    pub content: String,
    pub status: String,
    pub created_at: String,
}

// ── Capability row ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CapabilityRow {
    pub name: String,
    pub description: Option<String>,
}

// ── Session update payload ────────────────────────────────────────────────────

pub struct SessionUpdate<'a> {
    pub phase: &'a str,
    pub current_owner: &'a str,
    pub round: i64,
    pub claude_ok: bool,
    pub codex_ok: bool,
    pub content_hash: Option<&'a str>,
    pub rejected_hashes: &'a [String],
}

// ── Database impl ────────────────────────────────────────────────────────────

impl Database {
    // ── Sessions ─────────────────────────────────────────────────────────────

    pub fn collab_session_create(
        &self,
        id: &str,
        repo_path: &str,
        branch: &str,
    ) -> Result<(), MemoryError> {
        self.raw_conn().execute(
            "INSERT INTO collab_sessions (id, repo_path, branch) VALUES (?1, ?2, ?3)",
            params![id, repo_path, branch],
        )?;
        Ok(())
    }

    pub fn collab_session_get(&self, id: &str) -> Result<Option<SessionRow>, MemoryError> {
        let conn = self.raw_conn();
        let mut stmt = conn.prepare(
            "SELECT id, phase, current_owner, round, max_rounds,
                    repo_path, branch, claude_ok, codex_ok, content_hash,
                    rejected_hashes, created_at, updated_at
             FROM collab_sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let rejected_json: String = row.get(10)?;
            let rejected_hashes: Vec<String> =
                serde_json::from_str(&rejected_json).unwrap_or_default();
            Ok(Some(SessionRow {
                id: row.get(0)?,
                phase: row.get(1)?,
                current_owner: row.get(2)?,
                round: row.get(3)?,
                max_rounds: row.get(4)?,
                repo_path: row.get(5)?,
                branch: row.get(6)?,
                claude_ok: row.get::<_, i64>(7)? != 0,
                codex_ok: row.get::<_, i64>(8)? != 0,
                content_hash: row.get(9)?,
                rejected_hashes,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn collab_session_update(
        &self,
        id: &str,
        u: &SessionUpdate<'_>,
    ) -> Result<(), MemoryError> {
        let rejected_json =
            serde_json::to_string(u.rejected_hashes).unwrap_or_else(|_| "[]".to_string());
        let updated = self.raw_conn().execute(
            "UPDATE collab_sessions
             SET phase = ?1, current_owner = ?2, round = ?3,
                 claude_ok = ?4, codex_ok = ?5, content_hash = ?6,
                 rejected_hashes = ?7,
                 updated_at = datetime('now')
             WHERE id = ?8",
            params![
                u.phase,
                u.current_owner,
                u.round,
                u.claude_ok as i64,
                u.codex_ok as i64,
                u.content_hash,
                rejected_json,
                id,
            ],
        )?;
        if updated == 0 {
            return Err(MemoryError::NotFound(format!("session {id} not found")));
        }
        Ok(())
    }

    // ── Messages ──────────────────────────────────────────────────────────────

    pub fn collab_message_send(
        &self,
        id: &str,
        session_id: &str,
        sender: &str,
        receiver: &str,
        topic: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        self.raw_conn().execute(
            "INSERT INTO messages (id, session_id, sender, receiver, topic, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, session_id, sender, receiver, topic, content],
        )?;
        Ok(())
    }

    pub fn collab_message_recv(
        &self,
        session_id: &str,
        receiver: &str,
        limit: usize,
    ) -> Result<Vec<MessageRow>, MemoryError> {
        let conn = self.raw_conn();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, sender, receiver, topic, content, status, created_at
             FROM messages
             WHERE session_id = ?1 AND receiver = ?2 AND status = 'pending'
             ORDER BY created_at ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, receiver, limit as i64], |row| {
            Ok(MessageRow {
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
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Override max_rounds for a session.
    ///
    /// # Warning
    /// This is a test helper exposed only because the E2E tests live in a
    /// separate crate. Production callers **must not** use this method.
    #[doc(hidden)]
    pub fn collab_set_max_rounds(
        &self,
        session_id: &str,
        max_rounds: i64,
    ) -> Result<(), MemoryError> {
        self.raw_conn().execute(
            "UPDATE collab_sessions SET max_rounds = ?1 WHERE id = ?2",
            rusqlite::params![max_rounds, session_id],
        )?;
        Ok(())
    }

    /// Count messages in a session by status.
    pub fn collab_message_count(&self, session_id: &str, status: &str) -> Result<i64, MemoryError> {
        let count: i64 = self.raw_conn().query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND status = ?2",
            rusqlite::params![session_id, status],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Mark a message as acked. Scoped to `session_id` to prevent cross-session acks.
    /// Idempotent — no error if already acked.
    pub fn collab_message_ack(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<(), MemoryError> {
        self.raw_conn().execute(
            "UPDATE messages SET status = 'acked' WHERE id = ?1 AND session_id = ?2",
            params![message_id, session_id],
        )?;
        Ok(())
    }

    // ── Capabilities ──────────────────────────────────────────────────────────

    /// Register capabilities for an agent. Uses INSERT OR REPLACE (upsert by unique key).
    pub fn collab_caps_register(
        &self,
        session_id: &str,
        agent: &str,
        capabilities: &[(String, Option<String>)],
    ) -> Result<usize, MemoryError> {
        let conn = self.raw_conn();
        let mut count = 0usize;
        for (name, description) in capabilities {
            let id = generate_collab_id();
            conn.execute(
                "INSERT INTO agent_capabilities (id, session_id, agent, capability, description)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, agent, capability) DO UPDATE SET
                     description = excluded.description,
                     registered_at = datetime('now')",
                params![id, session_id, agent, name, description],
            )?;
            count += 1;
        }
        Ok(count)
    }

    pub fn collab_caps_get(
        &self,
        session_id: &str,
        agent: &str,
    ) -> Result<Vec<CapabilityRow>, MemoryError> {
        let conn = self.raw_conn();
        let mut stmt = conn.prepare(
            "SELECT capability, description
             FROM agent_capabilities
             WHERE session_id = ?1 AND agent = ?2
             ORDER BY registered_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id, agent], |row| {
            Ok(CapabilityRow {
                name: row.get(0)?,
                description: row.get(1)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

/// Generate a cryptographically random UUID v4 for messages and sessions.
pub fn generate_collab_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::SessionUpdate;
    use crate::db::schema::Database;

    fn open() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn session_create_and_get() {
        let db = open();
        db.collab_session_create("sess1", "/repo", "main").unwrap();
        let row = db.collab_session_get("sess1").unwrap().unwrap();
        assert_eq!(row.id, "sess1");
        assert_eq!(row.phase, "PlanDraft");
        assert_eq!(row.current_owner, "claude");
        assert_eq!(row.round, 0);
        assert!(!row.claude_ok);
        assert!(!row.codex_ok);
        assert!(row.content_hash.is_none());
    }

    #[test]
    fn session_get_missing_returns_none() {
        let db = open();
        let row = db.collab_session_get("nope").unwrap();
        assert!(row.is_none());
    }

    #[test]
    fn session_update_roundtrip() {
        let db = open();
        db.collab_session_create("sess2", "/repo", "main").unwrap();
        db.collab_session_update(
            "sess2",
            &SessionUpdate {
                phase: "PlanReview",
                current_owner: "codex",
                round: 0,
                claude_ok: false,
                codex_ok: false,
                content_hash: Some("abc"),
                rejected_hashes: &[],
            },
        )
        .unwrap();
        let row = db.collab_session_get("sess2").unwrap().unwrap();
        assert_eq!(row.phase, "PlanReview");
        assert_eq!(row.current_owner, "codex");
        assert_eq!(row.content_hash.as_deref(), Some("abc"));
        assert!(row.rejected_hashes.is_empty());
    }

    #[test]
    fn session_update_missing_errors() {
        let db = open();
        let err = db
            .collab_session_update(
                "nope",
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
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn fk_violation_on_orphan_message() {
        let db = open();
        let result = db.collab_message_send("m1", "nosession", "claude", "codex", "plan", "hi");
        assert!(result.is_err(), "orphan message should fail FK constraint");
    }

    #[test]
    fn message_send_recv_ack() {
        let db = open();
        db.collab_session_create("sess3", "/repo", "main").unwrap();
        db.collab_message_send("m1", "sess3", "claude", "codex", "plan", "here is plan")
            .unwrap();
        db.collab_message_send("m2", "sess3", "claude", "codex", "plan", "plan 2")
            .unwrap();

        let msgs = db.collab_message_recv("sess3", "codex", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, "m1"); // FIFO
        assert_eq!(msgs[0].topic, "plan");
        assert_eq!(msgs[0].content, "here is plan");

        db.collab_message_ack("m1", "sess3").unwrap();
        let msgs = db.collab_message_recv("sess3", "codex", 10).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "m2");
    }

    #[test]
    fn message_ack_is_idempotent() {
        let db = open();
        db.collab_session_create("sess4", "/repo", "main").unwrap();
        db.collab_message_send("m1", "sess4", "claude", "codex", "plan", "x")
            .unwrap();
        db.collab_message_ack("m1", "sess4").unwrap();
        db.collab_message_ack("m1", "sess4").unwrap(); // should not error
    }

    #[test]
    fn message_ack_wrong_session_is_no_op() {
        let db = open();
        db.collab_session_create("sess4b", "/repo", "main").unwrap();
        db.collab_message_send("m1", "sess4b", "claude", "codex", "plan", "x")
            .unwrap();
        // Ack with wrong session_id — message should remain pending
        db.collab_message_ack("m1", "other-session").unwrap();
        let msgs = db.collab_message_recv("sess4b", "codex", 10).unwrap();
        assert_eq!(msgs.len(), 1, "message should still be pending");
    }

    #[test]
    fn rejected_hashes_roundtrip() {
        let db = open();
        db.collab_session_create("sess_rh", "/repo", "main")
            .unwrap();
        let hashes = vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()];
        db.collab_session_update(
            "sess_rh",
            &SessionUpdate {
                phase: "PlanFeedback",
                current_owner: "claude",
                round: 1,
                claude_ok: false,
                codex_ok: false,
                content_hash: None,
                rejected_hashes: &hashes,
            },
        )
        .unwrap();
        let row = db.collab_session_get("sess_rh").unwrap().unwrap();
        assert_eq!(row.rejected_hashes, hashes);
    }

    #[test]
    fn recv_with_wrong_receiver_returns_empty() {
        let db = open();
        db.collab_session_create("sess5", "/repo", "main").unwrap();
        db.collab_message_send("m1", "sess5", "claude", "codex", "plan", "x")
            .unwrap();
        let msgs = db.collab_message_recv("sess5", "claude", 10).unwrap(); // wrong receiver
        assert!(msgs.is_empty());
    }

    #[test]
    fn caps_register_and_get_roundtrip() {
        let db = open();
        db.collab_session_create("sess6", "/repo", "main").unwrap();
        let caps = vec![
            (
                "planner".to_string(),
                Some("Implementation planning".to_string()),
            ),
            ("security-reviewer".to_string(), None),
        ];
        let count = db.collab_caps_register("sess6", "claude", &caps).unwrap();
        assert_eq!(count, 2);

        let got = db.collab_caps_get("sess6", "claude").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "planner");
        assert_eq!(got[1].name, "security-reviewer");
        assert!(got[1].description.is_none());
    }

    #[test]
    fn caps_get_unregistered_agent_returns_empty() {
        let db = open();
        db.collab_session_create("sess7", "/repo", "main").unwrap();
        let got = db.collab_caps_get("sess7", "codex").unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn caps_upsert_replaces_on_re_register() {
        let db = open();
        db.collab_session_create("sess8", "/repo", "main").unwrap();
        let caps1 = vec![("planner".to_string(), Some("v1".to_string()))];
        db.collab_caps_register("sess8", "claude", &caps1).unwrap();
        let caps2 = vec![("planner".to_string(), Some("v2".to_string()))];
        db.collab_caps_register("sess8", "claude", &caps2).unwrap();

        let got = db.collab_caps_get("sess8", "claude").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].description.as_deref(), Some("v2"));
    }

    #[test]
    fn existing_tables_are_unchanged_after_migration() {
        let db = open();
        // Verify legacy tables still exist by inserting into them
        db.raw_conn()
            .execute(
                "INSERT INTO schema_version (version) VALUES (999) ON CONFLICT DO NOTHING",
                [],
            )
            .unwrap();
        let ver: i64 = db
            .raw_conn()
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert!(
            ver >= 2,
            "schema_version should have version 2 from collab migration"
        );
    }
}
