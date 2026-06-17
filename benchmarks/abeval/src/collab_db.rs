//! Read-only reader of the per-task collab session row. The DB is created and
//! migrated by the worker-spawned `ironmem serve`; this module never creates or
//! migrates it — it only reads after bootstrap.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

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
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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

/// The `room` a compose worker stages its plan artifact in via `add_drawer`
/// (`collab-turn-plan-synthesis.md` / `collab-turn-plan-finalize.md`).
pub const COLLAB_DRAFTS_ROOM: &str = "collab-drafts";

/// Newest `collab-drafts` drawer whose `rowid` is strictly greater than
/// `after_rowid`, as `(drawer_id, rowid)`. `after_rowid = i64::MIN` returns the
/// newest such drawer overall; `None` when none exists.
///
/// This lets the driver recover a compose worker's artifact ref from the drawer
/// the worker actually persisted, instead of trusting the model to echo a
/// `ref:` line (which it intermittently omits — drawer-staging flakiness).
pub fn newest_draft_drawer(db_path: &Path, after_rowid: i64) -> Result<Option<(String, i64)>> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening collab db {} read-only", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT id, rowid FROM drawers \
         WHERE room = ?1 AND rowid > ?2 \
         ORDER BY rowid DESC LIMIT 1",
    )?;

    let row = stmt
        .query_row(rusqlite::params![COLLAB_DRAFTS_ROOM, after_rowid], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()
        .with_context(|| format!("querying newest draft drawer in {}", db_path.display()))?;

    Ok(row)
}
