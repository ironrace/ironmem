//! Read-only projection helpers for the dashboard server.
//!
//! All functions open no write transactions, call no migrations, and reach no
//! write paths. SQL queries are confined to this module so HTTP handlers stay
//! free of raw SQL.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::db::CodeMap;
use crate::error::MemoryError;
use crate::report::{run_report, Report, ReportOptions};

/// Maximum content length for a drawer in list views (truncated beyond this).
const LIST_CONTENT_LIMIT: usize = 200;

// ────────────────────────────────────────────────────────────────────────────
// Memory summary projection (Task 2)
// ────────────────────────────────────────────────────────────────────────────

/// A truncated drawer entry safe for list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawerSummary {
    pub id: String,
    /// Content truncated to [`LIST_CONTENT_LIMIT`] bytes.
    pub content_preview: String,
    pub wing: String,
    pub room: String,
    pub source_file: String,
    pub added_by: String,
    pub filed_at: String,
}

/// Memory taxonomy and recent-drawers summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySummary {
    pub total_drawers: usize,
    /// Wing → total drawer count.
    pub wing_counts: HashMap<String, usize>,
    /// Wing → room → drawer count.
    pub taxonomy: HashMap<String, HashMap<String, usize>>,
    pub kg_stats: serde_json::Value,
    /// Most recent drawers (truncated content, optional wing/room filters).
    pub recent_drawers: Vec<DrawerSummary>,
}

/// Query parameters for the memory summary endpoint.
#[derive(Debug, Clone, Default)]
pub struct MemoryParams {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub limit: usize,
}

/// Project the memory summary from a read-only [`Database`].
///
/// Delegates entirely to existing read-only DB helpers; never calls `open` or
/// `migrate`. `KnowledgeGraph::new(&db).stats()` is the approved read-only
/// path for KG counts — it never calls `App::new`.
pub fn memory_summary(db: &Database, params: &MemoryParams) -> Result<MemorySummary, MemoryError> {
    let limit = params.limit.clamp(1, 500);

    let total_drawers = db.count_drawers(params.wing.as_deref())?;

    let wing_counts: HashMap<String, usize> = db.wing_counts()?.into_iter().collect();

    let raw_taxonomy = db.taxonomy()?;
    let taxonomy: HashMap<String, HashMap<String, usize>> = raw_taxonomy
        .into_iter()
        .map(|(wing, rooms)| (wing, rooms.into_iter().collect()))
        .collect();

    let kg_stats = KnowledgeGraph::new(db).stats()?;

    let drawers = db.get_drawers(params.wing.as_deref(), params.room.as_deref(), limit)?;

    let recent_drawers = drawers
        .into_iter()
        .map(|d| {
            let preview = truncate_content(&d.content, LIST_CONTENT_LIMIT);
            DrawerSummary {
                id: d.id,
                content_preview: preview,
                wing: d.wing,
                room: d.room,
                source_file: d.source_file,
                added_by: d.added_by,
                filed_at: d.filed_at,
            }
        })
        .collect();

    Ok(MemorySummary {
        total_drawers,
        wing_counts,
        taxonomy,
        kg_stats,
        recent_drawers,
    })
}

fn truncate_content(content: &str, limit: usize) -> String {
    if content.len() <= limit {
        content.to_string()
    } else {
        // Truncate at a char boundary to avoid splitting multi-byte characters.
        let mut end = limit;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &content[..end])
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Report projection (Task 2)
// ────────────────────────────────────────────────────────────────────────────

/// Thin wrapper: delegates to [`run_report`] so CLI and dashboard semantics
/// are always identical. No reimplementation.
pub fn report_projection(
    db: &Database,
    task: Option<String>,
    since: Option<String>,
) -> Result<Report, MemoryError> {
    let opts = ReportOptions { task, since };
    run_report(db, &opts)
}

// ────────────────────────────────────────────────────────────────────────────
// Code-map list (Task 3)
// ────────────────────────────────────────────────────────────────────────────

/// Query parameters for code-map listing.
#[derive(Debug, Clone, Default)]
pub struct CodeMapParams {
    pub repo: Option<String>,
    pub area: Option<String>,
}

/// List code-map rows with optional repo/area filters.
///
/// Raw SQL is confined here; the HTTP handler stays SQL-free.
pub fn list_code_maps(db: &Database, params: &CodeMapParams) -> Result<Vec<CodeMap>, MemoryError> {
    db.with_connection(|conn| list_code_maps_conn(conn, params))
}

pub(crate) fn list_code_maps_conn(
    conn: &Connection,
    params: &CodeMapParams,
) -> Result<Vec<CodeMap>, MemoryError> {
    let sql = match (&params.repo, &params.area) {
        (Some(_), Some(_)) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE repo = ?1 AND area = ?2 ORDER BY built_at DESC"
        }
        (Some(_), None) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE repo = ?1 ORDER BY built_at DESC"
        }
        (None, Some(_)) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE area = ?1 ORDER BY built_at DESC"
        }
        (None, None) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps ORDER BY built_at DESC"
        }
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = match (&params.repo, &params.area) {
        (Some(repo), Some(area)) => stmt
            .query_map(params![repo, area], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (Some(repo), None) => stmt
            .query_map(params![repo], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, Some(area)) => stmt
            .query_map(params![area], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, None) => stmt
            .query_map([], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

fn map_code_map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodeMap> {
    let source_files_json: String = row.get(4)?;
    let source_files: Vec<String> = serde_json::from_str(&source_files_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(CodeMap {
        repo: row.get(0)?,
        area: row.get(1)?,
        drawer_id: row.get(2)?,
        head_sha: row.get(3)?,
        source_files,
        built_by: row.get(5)?,
        built_at: row.get(6)?,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Collab session list (Task 3)
// ────────────────────────────────────────────────────────────────────────────

/// Compact collab session summary for dashboard list views.
///
/// Never inlines plan/message bodies — returns drawer refs only per issue #90.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollabSessionSummary {
    pub id: String,
    pub task: Option<String>,
    pub repo_path: String,
    pub branch: String,
    pub phase: String,
    pub current_owner: String,
    pub implementer: String,
    pub base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub ended_at: Option<String>,
    pub coding_failure: Option<String>,
    pub pr_url: Option<String>,
    pub tasks_count: Option<u32>,
    /// Drawer ref for canonical plan — never the full body.
    pub canonical_plan_drawer_id: Option<String>,
    pub canonical_plan_hash: Option<String>,
    /// Drawer ref for final plan — never the full body.
    pub final_plan_drawer_id: Option<String>,
    pub final_plan_hash: Option<String>,
}

/// List compact collab session summaries (most recently updated first).
///
/// Plan/message bodies are never returned — only drawer refs so the #90
/// by-reference contract is respected.
pub fn list_sessions(db: &Database) -> Result<Vec<CollabSessionSummary>, MemoryError> {
    db.with_connection(list_sessions_conn)
}

pub(crate) fn list_sessions_conn(
    conn: &Connection,
) -> Result<Vec<CollabSessionSummary>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT id, task, repo_path, branch, phase, current_owner, implementer,
                base_sha, last_head_sha, created_at, updated_at, ended_at,
                coding_failure, pr_url, task_list,
                canonical_plan_drawer_id, canonical_plan_hash,
                final_plan_drawer_id, final_plan_hash
         FROM collab_sessions
         ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        let task_list: Option<String> = row.get(14)?;
        let tasks_count = crate::collab::tasks_count_from_list(task_list.as_deref());
        Ok(CollabSessionSummary {
            id: row.get(0)?,
            task: row.get(1)?,
            repo_path: row.get(2)?,
            branch: row.get(3)?,
            phase: row.get(4)?,
            current_owner: row.get(5)?,
            implementer: row.get(6)?,
            base_sha: row.get(7)?,
            head_sha: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            ended_at: row.get(11)?,
            coding_failure: row.get(12)?,
            pr_url: row.get(13)?,
            tasks_count,
            canonical_plan_drawer_id: row.get(15)?,
            canonical_plan_hash: row.get(16)?,
            final_plan_drawer_id: row.get(17)?,
            final_plan_hash: row.get(18)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Fetch a single session summary by id. Returns `None` when not found.
pub fn get_session(
    db: &Database,
    session_id: &str,
) -> Result<Option<CollabSessionSummary>, MemoryError> {
    db.with_connection(|conn| {
        conn.query_row(
            "SELECT id, task, repo_path, branch, phase, current_owner, implementer,
                base_sha, last_head_sha, created_at, updated_at, ended_at,
                coding_failure, pr_url, task_list,
                canonical_plan_drawer_id, canonical_plan_hash,
                final_plan_drawer_id, final_plan_hash
         FROM collab_sessions WHERE id = ?1",
            params![session_id],
            |row: &rusqlite::Row<'_>| {
                let task_list: Option<String> = row.get(14)?;
                let tasks_count = crate::collab::tasks_count_from_list(task_list.as_deref());
                Ok(CollabSessionSummary {
                    id: row.get(0)?,
                    task: row.get(1)?,
                    repo_path: row.get(2)?,
                    branch: row.get(3)?,
                    phase: row.get(4)?,
                    current_owner: row.get(5)?,
                    implementer: row.get(6)?,
                    base_sha: row.get(7)?,
                    head_sha: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                    ended_at: row.get(11)?,
                    coding_failure: row.get(12)?,
                    pr_url: row.get(13)?,
                    tasks_count,
                    canonical_plan_drawer_id: row.get(15)?,
                    canonical_plan_hash: row.get(16)?,
                    final_plan_drawer_id: row.get(17)?,
                    final_plan_hash: row.get(18)?,
                })
            },
        )
        .optional()
        .map_err(MemoryError::from)
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;

    fn open_fresh() -> Database {
        Database::open_in_memory().unwrap()
    }

    // ── memory_summary ──────────────────────────────────────────────────────

    #[test]
    fn memory_summary_empty_db() {
        let db = open_fresh();
        let params = MemoryParams {
            wing: None,
            room: None,
            limit: 10,
        };
        let summary = memory_summary(&db, &params).unwrap();
        assert_eq!(summary.total_drawers, 0);
        assert!(summary.recent_drawers.is_empty());
        assert!(summary.wing_counts.is_empty());
        assert!(summary.taxonomy.is_empty());
    }

    #[test]
    fn memory_summary_with_drawers_and_filters() {
        let db = open_fresh();
        // Insert two drawers in different wings.
        let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        db.insert_drawer(
            &crate::db::drawers::generate_id("alpha content", "alpha", "general"),
            "alpha content",
            &emb,
            "alpha",
            "general",
            "src/a.rs",
            "test",
        )
        .unwrap();
        db.insert_drawer(
            &crate::db::drawers::generate_id("beta content", "beta", "notes"),
            "beta content",
            &emb,
            "beta",
            "notes",
            "src/b.rs",
            "test",
        )
        .unwrap();

        // Full summary.
        let params = MemoryParams {
            wing: None,
            room: None,
            limit: 10,
        };
        let summary = memory_summary(&db, &params).unwrap();
        assert_eq!(summary.total_drawers, 2);
        assert_eq!(summary.recent_drawers.len(), 2);
        assert_eq!(*summary.wing_counts.get("alpha").unwrap(), 1);
        assert_eq!(*summary.wing_counts.get("beta").unwrap(), 1);

        // Wing-filtered.
        let params_alpha = MemoryParams {
            wing: Some("alpha".to_string()),
            room: None,
            limit: 10,
        };
        let alpha = memory_summary(&db, &params_alpha).unwrap();
        assert_eq!(alpha.total_drawers, 1);
        assert_eq!(alpha.recent_drawers.len(), 1);
        assert_eq!(alpha.recent_drawers[0].wing, "alpha");
    }

    #[test]
    fn drawer_content_is_truncated_in_list_views() {
        let db = open_fresh();
        let long_content = "x".repeat(500);
        let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        db.insert_drawer(
            &crate::db::drawers::generate_id(&long_content, "w", "r"),
            &long_content,
            &emb,
            "w",
            "r",
            "",
            "test",
        )
        .unwrap();

        let params = MemoryParams {
            wing: None,
            room: None,
            limit: 10,
        };
        let summary = memory_summary(&db, &params).unwrap();
        assert_eq!(summary.recent_drawers.len(), 1);
        let preview = &summary.recent_drawers[0].content_preview;
        // Preview must be shorter than original.
        assert!(
            preview.len() <= LIST_CONTENT_LIMIT + 4,
            "preview too long: {}",
            preview.len()
        );
    }

    // ── report_projection ───────────────────────────────────────────────────

    #[test]
    fn report_projection_returns_report_struct() {
        let db = open_fresh();
        let report = report_projection(&db, None, None).unwrap();
        // An empty db yields an empty report (no tasks, no cost).
        // We just verify the type round-trips without error.
        assert!(report.tasks.is_empty());
    }

    // ── list_code_maps ──────────────────────────────────────────────────────

    #[test]
    fn list_code_maps_empty() {
        let db = open_fresh();
        let maps = list_code_maps(&db, &CodeMapParams::default()).unwrap();
        assert!(maps.is_empty());
    }

    #[test]
    fn list_code_maps_with_filter() {
        let db = open_fresh();
        let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        // Insert a drawer to satisfy the code_maps FK.
        let drawer_id = crate::db::drawers::generate_id("map content", "code-maps", "code-maps");
        db.insert_drawer(
            &drawer_id,
            "map content",
            &emb,
            "code-maps",
            "code-maps",
            "",
            "test",
        )
        .unwrap();
        db.upsert_code_map(
            "my-repo",
            "core",
            &drawer_id,
            "aabbccdd1122334455667788aabbccdd11223344",
            &["src/lib.rs".to_string()],
            "test-agent",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        // No filter — should return the row.
        let all = list_code_maps(&db, &CodeMapParams::default()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].repo, "my-repo");
        assert_eq!(all[0].area, "core");

        // Repo filter match.
        let filtered = list_code_maps(
            &db,
            &CodeMapParams {
                repo: Some("my-repo".to_string()),
                area: None,
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);

        // Repo filter no match.
        let none = list_code_maps(
            &db,
            &CodeMapParams {
                repo: Some("other-repo".to_string()),
                area: None,
            },
        )
        .unwrap();
        assert!(none.is_empty());
    }

    // ── list_sessions ───────────────────────────────────────────────────────

    #[test]
    fn list_sessions_empty() {
        let db = open_fresh();
        let sessions = list_sessions(&db).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_does_not_include_plan_bodies() {
        let db = open_fresh();
        db.with_connection(|conn| {
            crate::collab::queue::create_session(
                conn,
                "test-session-id-001",
                "/repo",
                "main",
                Some("test task"),
                crate::collab::Agent::Claude,
            )
        })
        .unwrap();

        let sessions = list_sessions(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "test-session-id-001");
        assert_eq!(s.task.as_deref(), Some("test task"));
        // Plan bodies must NOT be present — only refs (which are NULL at creation).
        // The struct has no plan_body field by design.
        assert!(s.canonical_plan_drawer_id.is_none());
        assert!(s.final_plan_drawer_id.is_none());
    }
}
