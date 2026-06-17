//! Read-only reader of the per-task collab session row. The DB is created and
//! migrated by the worker-spawned `ironmem serve`; this module never creates or
//! migrates it — it only reads after bootstrap.

use std::path::Path;

use anyhow::{Context, Result};

/// The subset of `collab_sessions` the driver polls. Field names mirror the
/// schema columns (003/005/006 migrations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub phase: String,
    pub current_owner: String,
    pub implementer: String,
    pub pr_url: Option<String>,
    pub global_review_round: u32,
    pub task_review_round: u32,
    pub last_head_sha: Option<String>,
}

impl SessionState {
    /// `{CodingComplete, CodingFailed}` is the terminal set (mirrors
    /// `crates/ironmem/src/collab/phase.rs::is_terminal`).
    pub fn is_terminal(&self) -> bool {
        // keep in sync with collab_driver::PHASE_CODING_{COMPLETE,FAILED}
        matches!(self.phase.as_str(), "CodingComplete" | "CodingFailed")
    }
}

/// Open the per-task collab DB read-only and select the single session row.
/// A missing row is a loud error (the caller polls a session it just started).
pub fn read_session_state(db_path: &Path, session_id: &str) -> Result<SessionState> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("opening collab db {} read-only", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT phase, current_owner, implementer, pr_url, \
                global_review_round, task_review_round, last_head_sha \
         FROM collab_sessions WHERE id = ?1",
    )?;

    stmt.query_row([session_id], |row| {
        Ok(SessionState {
            phase: row.get(0)?,
            current_owner: row.get(1)?,
            implementer: row.get(2)?,
            pr_url: row.get(3)?,
            global_review_round: row.get::<_, i64>(4)?.max(0) as u32,
            task_review_round: row.get::<_, i64>(5)?.max(0) as u32,
            last_head_sha: row.get(6)?,
        })
    })
    .with_context(|| format!("no collab_sessions row for session {session_id}"))
}
