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
    /// Which agent LEADS the session — synthesizes/finalizes the plan and audits
    /// the copilot's commits (migration 019). Orthogonal to `implementer`.
    /// `claude` for every pre-019 row and every session that omits the flag.
    /// The driver only knows how to drive `claude`; see
    /// `collab_driver::ensure_supported_pilot`.
    pub pilot: String,
    pub pr_url: Option<String>,
    pub review_round: u32,
    pub global_review_round: u32,
    pub task_review_round: u32,
    pub last_head_sha: Option<String>,
    /// Recoverable ("tooling") failure currently in flight, e.g.
    /// `git_push_failed: …` (migration 015). `Some` means the state machine
    /// kept the session in `recovery_phase` and flipped `current_owner` to
    /// `recovery_owner`, who completes the interrupted turn via the
    /// delegated-completion override. `None` in the common case.
    pub pending_failure: Option<String>,
    /// The phase the interrupted turn was in, as a phase name (same encoding
    /// as `phase`). Only meaningful while `pending_failure` is `Some`.
    pub recovery_phase: Option<String>,
    /// The agent recovery handed control to (`claude`/`codex`). Only
    /// meaningful while `pending_failure` is `Some`.
    pub recovery_owner: Option<String>,
}

impl SessionState {
    /// `{CodingComplete, CodingFailed}` is the terminal set (mirrors
    /// `crates/ironmem/src/collab/phase.rs::is_terminal`).
    pub fn is_terminal(&self) -> bool {
        // keep in sync with collab_driver::PHASE_CODING_{COMPLETE,FAILED}
        matches!(self.phase.as_str(), "CodingComplete" | "CodingFailed")
    }

    /// Fixture row for tests, carrying the three fields that decide what the
    /// dispatcher does — `phase`, `current_owner`, `pilot` — as required
    /// arguments; everything else takes an inert placeholder that callers
    /// override with struct-update syntax (`..SessionState::fixture(..)`).
    ///
    /// `pilot` is deliberately an argument rather than a defaulted field, and
    /// there is deliberately no `Default` impl: a `SessionState` that can be
    /// built without saying who leads the session is the exact silent default
    /// `collab_driver::ensure_supported_pilot` exists to eliminate — a
    /// codex-piloted session read as `claude` misclassifies every lead turn.
    /// Every other field may gain a placeholder here precisely because none of
    /// them changes which agent the driver believes is in charge.
    ///
    /// Not a production path. `read_session_state` below populates all eleven
    /// fields from the row and must keep doing so — no read may reach a
    /// placeholder.
    pub fn fixture(phase: &str, current_owner: &str, pilot: &str) -> Self {
        Self {
            phase: phase.to_string(),
            current_owner: current_owner.to_string(),
            implementer: "claude".to_string(),
            pilot: pilot.to_string(),
            pr_url: None,
            review_round: 0,
            global_review_round: 0,
            task_review_round: 0,
            last_head_sha: None,
            pending_failure: None,
            recovery_phase: None,
            recovery_owner: None,
        }
    }
}

/// Open the per-task collab DB read-only and select the single session row.
/// A missing row is a loud error (the caller polls a session it just started).
pub fn read_session_state(db_path: &Path, session_id: &str) -> Result<SessionState> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening collab db {} read-only", db_path.display()))?;

    // Migration 019 adds `pilot` as `NOT NULL DEFAULT 'claude'`, and the ALTER
    // backfills every pre-019 row with `'claude'`, so no DB the product can
    // produce yields a NULL here: the COALESCE never fires on a real row and
    // only covers a hand-patched schema that dropped the NOT NULL. It does NOT
    // cover a genuinely pre-019 DB — there the column is absent entirely and
    // `prepare` fails first, hence the context on that call.
    let mut stmt = conn
        .prepare(
            "SELECT phase, current_owner, implementer, pr_url, \
                review_round, global_review_round, task_review_round, last_head_sha, \
                pending_failure, recovery_phase, recovery_owner, \
                COALESCE(pilot, 'claude') \
         FROM collab_sessions WHERE id = ?1",
        )
        .with_context(|| {
            format!(
                "preparing the collab_sessions poll against {} \
                 (a `no such column: pilot` here means the DB predates migration 019)",
                db_path.display()
            )
        })?;

    stmt.query_row([session_id], |row| {
        Ok(SessionState {
            phase: row.get(0)?,
            current_owner: row.get(1)?,
            implementer: row.get(2)?,
            pr_url: row.get(3)?,
            review_round: row.get::<_, i64>(4)?.max(0) as u32,
            global_review_round: row.get::<_, i64>(5)?.max(0) as u32,
            task_review_round: row.get::<_, i64>(6)?.max(0) as u32,
            last_head_sha: row.get(7)?,
            pending_failure: row.get(8)?,
            recovery_phase: row.get(9)?,
            recovery_owner: row.get(10)?,
            pilot: row.get(11)?,
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
