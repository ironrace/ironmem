//! Conservative memory-retention helpers for operational drawers.

use chrono::{Duration, Utc};
use rusqlite::{params, Transaction};
use serde::Serialize;
use serde_json::json;

use super::schema::Database;
use crate::error::MemoryError;

pub const DEFAULT_COLLAB_CHECKPOINT_RETENTION_DAYS: i64 = 60;
pub const DEFAULT_COLLAB_ARTIFACT_RETENTION_DAYS: i64 = 180;
pub const DEFAULT_GC_LIMIT: usize = 500;

const MAX_GC_LIMIT: usize = 10_000;
const COLLAB_WING: &str = "ironrace-memory";
const CHECKPOINT_ROOM: &str = "collab-checkpoints";
const PLAN_ROOM: &str = "collab-plans";
const TASK_LIST_ROOM: &str = "collab-task-lists";

#[derive(Debug, Clone)]
pub struct MemoryGcOptions {
    pub apply: bool,
    pub collab_checkpoint_days: i64,
    pub collab_artifact_days: i64,
    pub limit: usize,
}

impl Default for MemoryGcOptions {
    fn default() -> Self {
        Self {
            apply: false,
            collab_checkpoint_days: DEFAULT_COLLAB_CHECKPOINT_RETENTION_DAYS,
            collab_artifact_days: DEFAULT_COLLAB_ARTIFACT_RETENTION_DAYS,
            limit: DEFAULT_GC_LIMIT,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemoryGcReport {
    pub applied: bool,
    pub collab_checkpoint_retention_days: i64,
    pub collab_artifact_retention_days: i64,
    pub limit: usize,
    pub candidates: Vec<MemoryGcCandidate>,
    pub delete_candidates: usize,
    pub skipped_candidates: usize,
    pub deleted: usize,
    pub deleted_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGcAction {
    Delete,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryGcCandidate {
    pub id: String,
    pub wing: String,
    pub room: String,
    pub filed_at: String,
    pub content_bytes: usize,
    pub action: MemoryGcAction,
    pub reason: String,
}

#[derive(Debug)]
struct RawCandidate {
    id: String,
    wing: String,
    room: String,
    filed_at: String,
    content_bytes: usize,
}

pub fn run_memory_gc(
    db: &Database,
    options: MemoryGcOptions,
) -> Result<MemoryGcReport, MemoryError> {
    let collab_checkpoint_days =
        validate_days(options.collab_checkpoint_days, "collab-checkpoint-days")?;
    let collab_artifact_days = validate_days(options.collab_artifact_days, "collab-artifact-days")?;
    let limit = validate_limit(options.limit)?;

    let checkpoint_cutoff = cutoff_datetime(collab_checkpoint_days);
    let artifact_cutoff = cutoff_datetime(collab_artifact_days);

    let mut candidates = Vec::new();
    collect_room_candidates(
        db,
        CHECKPOINT_ROOM,
        &checkpoint_cutoff,
        limit,
        false,
        &mut candidates,
    )?;
    collect_room_candidates(
        db,
        PLAN_ROOM,
        &artifact_cutoff,
        limit,
        true,
        &mut candidates,
    )?;
    collect_room_candidates(
        db,
        TASK_LIST_ROOM,
        &artifact_cutoff,
        limit,
        true,
        &mut candidates,
    )?;

    let delete_candidates = candidates
        .iter()
        .filter(|c| c.action == MemoryGcAction::Delete)
        .count();
    let skipped_candidates = candidates.len().saturating_sub(delete_candidates);

    let mut deleted = 0;
    let mut deleted_bytes = 0;
    if options.apply {
        for candidate in candidates
            .iter()
            .filter(|c| c.action == MemoryGcAction::Delete)
        {
            let was_deleted = db.with_transaction(|tx| {
                let was_deleted = Database::delete_drawer_tx(tx, &candidate.id)?;
                if was_deleted {
                    Database::wal_log_tx(
                        tx,
                        "memory_gc",
                        &json!({
                            "id": candidate.id,
                            "wing": candidate.wing,
                            "room": candidate.room,
                            "reason": candidate.reason,
                        }),
                        None,
                    )?;
                }
                Ok(was_deleted)
            })?;
            if was_deleted {
                deleted += 1;
                deleted_bytes += candidate.content_bytes;
            }
        }
    }

    Ok(MemoryGcReport {
        applied: options.apply,
        collab_checkpoint_retention_days: collab_checkpoint_days,
        collab_artifact_retention_days: collab_artifact_days,
        limit,
        candidates,
        delete_candidates,
        skipped_candidates,
        deleted,
        deleted_bytes,
    })
}

pub fn render_memory_gc_report(report: &MemoryGcReport) -> String {
    let mode = if report.applied { "applied" } else { "dry-run" };
    let mut out = String::new();
    out.push_str(&format!(
        "memory gc {mode}: {} delete candidate(s), {} skipped, {} deleted, {} bytes freed\n",
        report.delete_candidates, report.skipped_candidates, report.deleted, report.deleted_bytes
    ));
    out.push_str(&format!(
        "retention: collab-checkpoints={}d, collab-plans/task-lists={}d, limit={}\n",
        report.collab_checkpoint_retention_days,
        report.collab_artifact_retention_days,
        report.limit
    ));
    if !report.applied {
        out.push_str("dry-run only: pass --apply to delete listed delete candidates\n");
    }
    for candidate in &report.candidates {
        out.push_str(&format!(
            "- {:?} {} {}/{} filed_at={} bytes={} reason={}\n",
            candidate.action,
            candidate.id,
            candidate.wing,
            candidate.room,
            candidate.filed_at,
            candidate.content_bytes,
            candidate.reason
        ));
    }
    out
}

fn collect_room_candidates(
    db: &Database,
    room: &str,
    cutoff: &str,
    limit: usize,
    skip_linked_collab_refs: bool,
    candidates: &mut Vec<MemoryGcCandidate>,
) -> Result<(), MemoryError> {
    let remaining = limit.saturating_sub(candidates.len());
    if remaining == 0 {
        return Ok(());
    }
    let rows = db.old_drawers_in_room(COLLAB_WING, room, cutoff, remaining)?;
    for row in rows {
        if skip_linked_collab_refs && db.is_referenced_collab_drawer(&row.id)? {
            candidates.push(MemoryGcCandidate {
                id: row.id,
                wing: row.wing,
                room: row.room,
                filed_at: row.filed_at,
                content_bytes: row.content_bytes,
                action: MemoryGcAction::Skip,
                reason: "referenced by collab state".to_string(),
            });
        } else {
            candidates.push(MemoryGcCandidate {
                id: row.id,
                wing: row.wing,
                room: row.room.clone(),
                filed_at: row.filed_at,
                content_bytes: row.content_bytes,
                action: MemoryGcAction::Delete,
                reason: format!("stale {room} drawer older than {cutoff} UTC"),
            });
        }
    }
    Ok(())
}

fn validate_days(days: i64, label: &str) -> Result<i64, MemoryError> {
    if !(1..=3650).contains(&days) {
        return Err(MemoryError::Validation(format!(
            "{label} must be between 1 and 3650 days"
        )));
    }
    Ok(days)
}

fn validate_limit(limit: usize) -> Result<usize, MemoryError> {
    if limit == 0 || limit > MAX_GC_LIMIT {
        return Err(MemoryError::Validation(format!(
            "limit must be between 1 and {MAX_GC_LIMIT}"
        )));
    }
    Ok(limit)
}

fn cutoff_datetime(days: i64) -> String {
    (Utc::now() - Duration::days(days))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

impl Database {
    fn old_drawers_in_room(
        &self,
        wing: &str,
        room: &str,
        cutoff: &str,
        limit: usize,
    ) -> Result<Vec<RawCandidate>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, wing, room, filed_at, length(content)
             FROM drawers
             WHERE wing = ?1
               AND room = ?2
               AND datetime(filed_at) < datetime(?3)
             ORDER BY datetime(filed_at) ASC, id ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![wing, room, cutoff, limit as i64], |row| {
            let len: i64 = row.get(4)?;
            Ok(RawCandidate {
                id: row.get(0)?,
                wing: row.get(1)?,
                room: row.get(2)?,
                filed_at: row.get(3)?,
                content_bytes: len.max(0) as usize,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn is_referenced_collab_drawer(&self, drawer_id: &str) -> Result<bool, MemoryError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM (
                SELECT 1
                FROM collab_sessions
                WHERE canonical_plan_drawer_id = ?1
                   OR final_plan_drawer_id = ?1
                   OR task_list_drawer_id = ?1
                UNION ALL
                SELECT 1
                FROM messages
                WHERE drawer_id IS NOT NULL
                  AND drawer_id = ?1
             )",
            params![drawer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub(crate) fn is_referenced_collab_drawer_tx(
        tx: &Transaction<'_>,
        drawer_id: &str,
    ) -> Result<bool, MemoryError> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*)
             FROM (
                SELECT 1
                FROM collab_sessions
                WHERE canonical_plan_drawer_id = ?1
                   OR final_plan_drawer_id = ?1
                   OR task_list_drawer_id = ?1
                UNION ALL
                SELECT 1
                FROM messages
                WHERE drawer_id IS NOT NULL
                  AND drawer_id = ?1
             )",
            params![drawer_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::drawers::generate_id;

    fn emb() -> Vec<f32> {
        vec![0.1; ironrace_embed::embedder::EMBED_DIM]
    }

    fn insert_old(db: &Database, content: &str, room: &str) -> String {
        let id = generate_id(content, COLLAB_WING, room);
        db.insert_drawer(&id, content, &emb(), COLLAB_WING, room, "", "test")
            .unwrap();
        db.exec_raw(&format!(
            "UPDATE drawers
             SET filed_at = '2000-01-01 00:00:00', date = '2000-01-01'
             WHERE id = '{id}'"
        ))
        .unwrap();
        id
    }

    #[test]
    fn dry_run_reports_deletions_without_deleting() {
        let db = Database::open_in_memory().unwrap();
        let checkpoint_id = insert_old(&db, "old checkpoint", CHECKPOINT_ROOM);
        let task_id = insert_old(&db, "old task list", TASK_LIST_ROOM);
        let plan_id = insert_old(&db, "linked plan", PLAN_ROOM);
        db.exec_raw(&format!(
            "INSERT INTO collab_sessions
                (id, repo_path, branch, canonical_plan_drawer_id)
             VALUES
                ('s1', '/tmp/repo', 'main', '{plan_id}')"
        ))
        .unwrap();

        let report = run_memory_gc(
            &db,
            MemoryGcOptions {
                apply: false,
                collab_checkpoint_days: 1,
                collab_artifact_days: 1,
                limit: 10,
            },
        )
        .unwrap();

        assert_eq!(report.delete_candidates, 2);
        assert_eq!(report.skipped_candidates, 1);
        assert_eq!(report.deleted, 0);
        assert!(db.get_drawer(&checkpoint_id).unwrap().is_some());
        assert!(db.get_drawer(&task_id).unwrap().is_some());
        assert!(db.get_drawer(&plan_id).unwrap().is_some());
    }

    #[test]
    fn apply_deletes_unlinked_stale_operational_drawers() {
        let db = Database::open_in_memory().unwrap();
        let checkpoint_id = insert_old(&db, "old checkpoint", CHECKPOINT_ROOM);
        let task_id = insert_old(&db, "old task list", TASK_LIST_ROOM);
        let plan_id = insert_old(&db, "linked plan", PLAN_ROOM);
        db.exec_raw(&format!(
            "INSERT INTO collab_sessions
                (id, repo_path, branch, final_plan_drawer_id)
             VALUES
                ('s1', '/tmp/repo', 'main', '{plan_id}')"
        ))
        .unwrap();

        let report = run_memory_gc(
            &db,
            MemoryGcOptions {
                apply: true,
                collab_checkpoint_days: 1,
                collab_artifact_days: 1,
                limit: 10,
            },
        )
        .unwrap();

        assert_eq!(report.deleted, 2);
        assert!(db.get_drawer(&checkpoint_id).unwrap().is_none());
        assert!(db.get_drawer(&task_id).unwrap().is_none());
        assert!(db.get_drawer(&plan_id).unwrap().is_some());
    }

    #[test]
    fn message_transport_drawers_are_not_gc_candidates() {
        let db = Database::open_in_memory().unwrap();
        let message_drawer_id = insert_old(&db, "message drawer", "collab-messages");
        db.exec_raw(&format!(
            "INSERT INTO collab_sessions (id, repo_path, branch)
             VALUES ('message-session', '/tmp/repo', 'main');
             INSERT INTO messages
                (id, session_id, sender, receiver, topic, content, drawer_id)
             VALUES
                ('message-1', 'message-session', 'claude', 'codex', 'draft',
                 'message body', '{message_drawer_id}')"
        ))
        .unwrap();

        let report = run_memory_gc(
            &db,
            MemoryGcOptions {
                apply: true,
                collab_checkpoint_days: 1,
                collab_artifact_days: 1,
                limit: 10,
            },
        )
        .unwrap();

        assert_eq!(report.delete_candidates, 0);
        assert_eq!(report.skipped_candidates, 0);
        assert_eq!(report.deleted, 0);
        assert!(report.candidates.is_empty());
        assert!(db.get_drawer(&message_drawer_id).unwrap().is_some());
    }

    #[test]
    fn rejects_unsafe_limits() {
        let db = Database::open_in_memory().unwrap();
        let err = run_memory_gc(
            &db,
            MemoryGcOptions {
                limit: 0,
                ..MemoryGcOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("limit must be between"));
    }
}
