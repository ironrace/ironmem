use crate::collab::queue::{self, Capability, Message, SessionRecord};
use crate::collab::CollabSession;
use crate::db::schema::Database;
use crate::error::MemoryError;

impl Database {
    pub fn collab_create_session(
        &self,
        id: &str,
        repo_path: &str,
        branch: &str,
        task: Option<&str>,
    ) -> Result<(), MemoryError> {
        queue::create_session(&self.conn, id, repo_path, branch, task)
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

    pub fn collab_send_message(
        &self,
        session_id: &str,
        sender: &str,
        receiver: &str,
        topic: &str,
        content: &str,
    ) -> Result<String, MemoryError> {
        queue::send_message(&self.conn, session_id, sender, receiver, topic, content)
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
