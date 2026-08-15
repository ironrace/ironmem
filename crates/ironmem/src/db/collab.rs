use crate::collab::queue::{self, Capability, Message, SessionRecord};
#[cfg(test)]
use crate::collab::Agent;
use crate::collab::{CollabCheckpoint, CollabRoles, CollabSession};
use crate::db::schema::Database;
use crate::error::MemoryError;

impl Database {
    pub fn collab_create_session(
        &self,
        id: &str,
        repo_path: &str,
        branch: &str,
        task: Option<&str>,
        roles: CollabRoles,
    ) -> Result<(), MemoryError> {
        queue::create_session(&self.conn, id, repo_path, branch, task, roles)
    }

    pub fn collab_end_session(&self, session_id: &str) -> Result<(), MemoryError> {
        queue::end_session(&self.conn, session_id)
    }

    pub fn collab_load_session(&self, session_id: &str) -> Result<CollabSession, MemoryError> {
        queue::load_session(&self.conn, session_id)
    }

    pub fn collab_load_session_record(
        &self,
        session_id: &str,
    ) -> Result<SessionRecord, MemoryError> {
        queue::load_session_record(&self.conn, session_id)
    }

    pub fn collab_save_session(&self, session: &CollabSession) -> Result<(), MemoryError> {
        queue::save_session(&self.conn, session)
    }

    pub fn collab_upsert_checkpoint(
        &self,
        checkpoint: &CollabCheckpoint,
    ) -> Result<(), MemoryError> {
        queue::upsert_checkpoint(&self.conn, checkpoint)
    }

    pub fn collab_load_current_checkpoint(
        &self,
        session_id: &str,
    ) -> Result<Option<CollabCheckpoint>, MemoryError> {
        queue::load_current_checkpoint(&self.conn, session_id)
    }

    pub fn collab_send_message(
        &self,
        session_id: &str,
        sender: &str,
        receiver: &str,
        topic: &str,
        content: &str,
        drawer_id: &str,
    ) -> Result<String, MemoryError> {
        let drawer = self.get_drawer(drawer_id)?.ok_or_else(|| {
            MemoryError::Validation(format!(
                "drawer_id {drawer_id:?} does not reference an existing drawer"
            ))
        })?;
        if drawer.content != content {
            return Err(MemoryError::Validation(
                "drawer_id content does not match collab message content".to_string(),
            ));
        }
        queue::send_message(
            &self.conn, session_id, sender, receiver, topic, content, drawer_id,
        )
    }

    pub fn collab_recv_messages(
        &self,
        session_id: &str,
        receiver: &str,
        limit: usize,
    ) -> Result<Vec<Message>, MemoryError> {
        queue::recv_messages(&self.conn, session_id, receiver, limit)
    }

    pub fn collab_latest_message_content(
        &self,
        session_id: &str,
        topic: &str,
    ) -> Result<Option<String>, MemoryError> {
        queue::load_latest_message_content(&self.conn, session_id, topic)
    }

    pub fn collab_ack_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<(), MemoryError> {
        queue::ack_message(&self.conn, session_id, message_id)
    }

    pub fn collab_ack_messages_many(
        &self,
        session_id: &str,
        message_ids: &[String],
    ) -> Result<usize, MemoryError> {
        queue::ack_messages_many(&self.conn, session_id, message_ids)
    }

    pub fn collab_register_caps(
        &self,
        session_id: &str,
        agent: &str,
        caps: &[Capability],
    ) -> Result<(), MemoryError> {
        queue::register_caps(&self.conn, session_id, agent, caps)
    }

    pub fn collab_get_caps(
        &self,
        session_id: &str,
        agent: Option<&str>,
    ) -> Result<Vec<Capability>, MemoryError> {
        queue::get_caps(&self.conn, session_id, agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collab_send_message_rejects_a_dangling_drawer_ref() {
        let db = Database::open_in_memory().unwrap();
        db.collab_create_session(
            "session",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        let err = db
            .collab_send_message(
                "session",
                "claude",
                "codex",
                "draft",
                "message body",
                "missing-drawer",
            )
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("does not reference an existing drawer"));
    }

    /// Wires `collab_upsert_checkpoint` / `collab_load_current_checkpoint`
    /// through `Database::open_in_memory`'s full migration chain (not
    /// `queue::tests::open`'s hand-picked subset), so a future migration that
    /// changes migration 020's shape but not `queue.rs`'s test fixture would
    /// still be caught here.
    #[test]
    fn collab_checkpoint_accessors_round_trip_through_database() {
        let db = Database::open_in_memory().unwrap();
        db.collab_create_session(
            "session",
            "/repo",
            "main",
            None,
            CollabRoles {
                pilot: Agent::Claude,
                implementer: Agent::Claude,
            },
        )
        .unwrap();

        assert!(
            db.collab_load_current_checkpoint("session")
                .unwrap()
                .is_none(),
            "no checkpoint written yet"
        );

        let checkpoint = CollabCheckpoint::from_json(&serde_json::json!({
            "session_id": "session",
            "task_id": 2,
            "status": "started",
            "head_sha": "abc123",
        }))
        .unwrap();
        db.collab_upsert_checkpoint(&checkpoint).unwrap();

        let loaded = db
            .collab_load_current_checkpoint("session")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.head_sha, "abc123");
        assert_eq!(loaded.task_id, Some(2));
    }
}
