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

    // There is deliberately no `collab_end_session` accessor, for the reason
    // the `collab_load_current_checkpoint` note below spells out about its
    // missing `collab_upsert_checkpoint` sibling: ending a session is not a
    // bare row write. `handle_collab_end` reads endedness first so a repeat
    // call stays a no-op, takes the generation lease, enforces the phase
    // allowlist, writes the WAL row, attests the metrics outcome, and clears
    // the attribution cell — and the abandon arm additionally stamps the
    // `abandoned:` epitaph that `queue::ensure_active` echoes on every later
    // refusal. A non-transactional one-liner here would skip all of it while
    // looking like the supported way to end a session. It existed with zero
    // callers until #297 Task 3 removed it; reach for `queue::end_session`
    // inside a transaction that does the rest.
    //
    // The removal is a breaking change to a `pub` method on a `pub` type, and
    // it ships without a `#[deprecated]` shim on purpose: `ironmem` is not
    // published for out-of-tree use, so its only consumers are this
    // repository's own binaries, MCP server, and tests, all updated in the
    // same change. `CHANGELOG.md` records it (along with `end_session`'s new
    // `SessionEndOutcome` return) so the decision is written down rather than
    // inferred from a compile error. If the crate is ever published, that
    // calculus changes and this is the note to revisit.

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

    /// There is deliberately no `collab_upsert_checkpoint` sibling to this
    /// reader. A checkpoint write is not a bare row write: the
    /// `collab_checkpoint` tool handler takes the generation lease, runs the
    /// `HeadCheck`, and stamps `attestation_check` from
    /// `verify_acknowledged_range`'s own git reads before it ever reaches
    /// `queue::upsert_checkpoint`. A convenience accessor here would offer a
    /// one-liner that skips all three — `validate()` accepts an operator row
    /// with a plausible-looking range — so what landed would be an unleased,
    /// unresolved operator attestation that every reader surface then renders
    /// as `unrecorded`. Writers go through the tool handler; the rare test
    /// that needs the raw primitive names `queue::upsert_checkpoint` through
    /// `with_connection`, so the bypass is visible at the call site.
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

    /// Wires `queue::upsert_checkpoint` / `collab_load_current_checkpoint`
    /// through `Database::open_in_memory`'s full migration chain (not
    /// `queue::tests::open`'s hand-picked subset), so a future migration that
    /// changes migration 020's shape but not `queue.rs`'s test fixture would
    /// still be caught here.
    ///
    /// The write side reaches for `queue::upsert_checkpoint` through
    /// `with_connection` rather than a `Database` accessor because no such
    /// accessor exists — see the note on `collab_load_current_checkpoint` for
    /// why the write path is the tool handler's alone.
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
        db.with_connection(|c| queue::upsert_checkpoint(c, &checkpoint))
            .unwrap();

        let loaded = db
            .collab_load_current_checkpoint("session")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.head_sha, "abc123");
        assert_eq!(loaded.task_id, Some(2));
    }
}
