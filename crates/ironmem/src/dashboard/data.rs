//! Read-only projection helpers for the dashboard server.
//!
//! All functions open no write transactions, call no migrations, and reach no
//! write paths. SQL queries are confined to this module so HTTP handlers stay
//! free of raw SQL.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::code_maps::freshness::{classify_with_diff, diff_against_head, Freshness};
use crate::db::drawers::Drawer;
use crate::db::CodeMap;
use crate::db::ReadOnlyDb;
use crate::error::MemoryError;
use crate::report::{Report, ReportOptions};
use ironrace_embed::embedder::ModelStatus;

/// Maximum content length for a drawer in list views (truncated beyond this).
const LIST_CONTENT_LIMIT: usize = 200;

/// Single source of truth for the dashboard `limit` bounds. Both the data layer
/// (clamping) and the HTTP layer (`routes.rs`) reference these so the cap and
/// default can never drift apart.
pub(crate) const MAX_DASHBOARD_LIMIT: usize = 500;
/// Default `limit` when a request omits it.
pub(crate) const DEFAULT_LIMIT: usize = 50;

// ────────────────────────────────────────────────────────────────────────────
// Warming status (GAP 1: embed-model cache readiness)
// ────────────────────────────────────────────────────────────────────────────

/// Embed-model cache warming status, surfaced on `/api/summary`.
///
/// IMPORTANT semantics: this reports whether the cache *can embed* (model files
/// present and intact), NOT whether memory is populated. The summary surfaces
/// this alongside `total_drawers` so warming readiness is never misread as
/// content readiness.
///
/// This is a closed four-value set (mirroring [`AgeBucket`]/[`FreshnessBadge`]
/// in this module) so the wire contract is compile-time enforced. It carries no
/// payload: the [`ModelStatus::Unreadable`] detail string is deliberately
/// dropped here so no filesystem/permission detail can leak into an HTTP
/// response — callers that want the detail must log it server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarmingStatus {
    /// Model files present and checksums match — embeddings can run.
    Ready,
    /// One or both model files are absent.
    Missing,
    /// Files present but a checksum mismatch (corrupt or tampered).
    Corrupt,
    /// Files present but could not be read (permissions, I/O error).
    Unreadable,
}

impl From<&ModelStatus> for WarmingStatus {
    fn from(status: &ModelStatus) -> Self {
        match status {
            ModelStatus::Ready => WarmingStatus::Ready,
            ModelStatus::Missing => WarmingStatus::Missing,
            ModelStatus::Corrupt => WarmingStatus::Corrupt,
            ModelStatus::Unreadable(_) => WarmingStatus::Unreadable,
        }
    }
}

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
    /// Room → total drawer count.
    pub room_counts: HashMap<String, usize>,
    /// Wing → room → drawer count.
    pub taxonomy: HashMap<String, HashMap<String, usize>>,
    pub kg_stats: serde_json::Value,
    /// Most recent drawers (truncated content, optional wing/room filters).
    pub recent_drawers: Vec<DrawerSummary>,
}

/// Query parameters for the memory summary endpoint.
#[derive(Debug, Clone)]
pub struct MemoryParams {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub limit: usize,
}

impl Default for MemoryParams {
    fn default() -> Self {
        Self {
            wing: None,
            room: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// Project the memory summary from a read-only [`ReadOnlyDb`].
///
/// Delegates entirely to existing read-only DB helpers; never calls `open` or
/// `migrate`. `db.kg_stats()` is the approved read-only path for KG counts.
pub fn memory_summary(
    db: &ReadOnlyDb,
    params: &MemoryParams,
) -> Result<MemorySummary, MemoryError> {
    let limit = params.limit.clamp(1, MAX_DASHBOARD_LIMIT);

    let total_drawers = db.count_drawers(params.wing.as_deref())?;

    let wing_counts: HashMap<String, usize> = db.wing_counts()?.into_iter().collect();
    let room_counts: HashMap<String, usize> = db
        .room_counts(params.wing.as_deref())?
        .into_iter()
        .collect();

    let raw_taxonomy = db.taxonomy()?;
    let taxonomy: HashMap<String, HashMap<String, usize>> = raw_taxonomy
        .into_iter()
        .map(|(wing, rooms)| (wing, rooms.into_iter().collect()))
        .collect();

    let kg_stats = db.kg_stats()?;

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
        room_counts,
        taxonomy,
        kg_stats,
        recent_drawers,
    })
}

/// Exact drawer detail response. Full content is only exposed through this
/// exact-id lookup path, never through list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawerDetail {
    pub id: String,
    pub content: String,
    pub wing: String,
    pub room: String,
    pub source_file: String,
    pub added_by: String,
    pub filed_at: String,
    pub date: String,
}

impl From<Drawer> for DrawerDetail {
    fn from(drawer: Drawer) -> Self {
        Self {
            id: drawer.id,
            content: drawer.content,
            wing: drawer.wing,
            room: drawer.room,
            source_file: drawer.source_file,
            added_by: drawer.added_by,
            filed_at: drawer.filed_at,
            date: drawer.date,
        }
    }
}

pub fn drawer_detail(db: &ReadOnlyDb, id: &str) -> Result<Option<DrawerDetail>, MemoryError> {
    Ok(db.get_drawer(id)?.map(DrawerDetail::from))
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
    db: &ReadOnlyDb,
    task: Option<String>,
    since: Option<String>,
) -> Result<Report, MemoryError> {
    let opts = ReportOptions { task, since };
    db.run_report(&opts)
}

// ────────────────────────────────────────────────────────────────────────────
// Code-map list (Task 3)
// ────────────────────────────────────────────────────────────────────────────

/// Query parameters for code-map listing.
#[derive(Debug, Clone)]
pub struct CodeMapParams {
    pub repo: Option<String>,
    pub area: Option<String>,
    pub limit: usize,
}

impl Default for CodeMapParams {
    fn default() -> Self {
        Self {
            repo: None,
            area: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// List code-map rows with optional repo/area filters.
///
/// Raw SQL is confined here; the HTTP handler stays SQL-free.
pub fn list_code_maps(
    db: &ReadOnlyDb,
    params: &CodeMapParams,
) -> Result<Vec<CodeMap>, MemoryError> {
    db.with_connection(|conn| list_code_maps_conn(conn, params))
}

pub(crate) fn list_code_maps_conn(
    conn: &Connection,
    params: &CodeMapParams,
) -> Result<Vec<CodeMap>, MemoryError> {
    let limit = params.limit.clamp(1, MAX_DASHBOARD_LIMIT) as i64;
    let sql = match (&params.repo, &params.area) {
        (Some(_), Some(_)) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE repo = ?1 AND area = ?2 ORDER BY built_at DESC LIMIT ?3"
        }
        (Some(_), None) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE repo = ?1 ORDER BY built_at DESC LIMIT ?2"
        }
        (None, Some(_)) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps WHERE area = ?1 ORDER BY built_at DESC LIMIT ?2"
        }
        (None, None) => {
            "SELECT repo, area, drawer_id, head_sha, source_files, built_by, built_at
             FROM code_maps ORDER BY built_at DESC LIMIT ?1"
        }
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = match (&params.repo, &params.area) {
        (Some(repo), Some(area)) => stmt
            .query_map(params![repo, area, limit], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (Some(repo), None) => stmt
            .query_map(params![repo, limit], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, Some(area)) => stmt
            .query_map(params![area, limit], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        (None, None) => stmt
            .query_map(params![limit], map_code_map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

// ────────────────────────────────────────────────────────────────────────────
// Code-map freshness (GAP 2: per-row freshness badge — hybrid)
// ────────────────────────────────────────────────────────────────────────────

/// A built-age bucket, used only when a map's `repo` path cannot be resolved to
/// a real git worktree (so the real `classify` engine cannot run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeBucket {
    /// Built within [`AGE_FRESH_DAYS`].
    Fresh,
    /// Built within [`AGE_AGING_DAYS`].
    Aging,
    /// Older than [`AGE_AGING_DAYS`].
    Stale,
    /// `built_at` could not be parsed as a timestamp.
    Unknown,
}

/// Per-row freshness badge. When the map's `repo` path resolves to a git
/// worktree we report the real [`Freshness`] outcome; otherwise we fall back to
/// a coarse build-age bucket so the row is never left without a signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FreshnessBadge {
    /// Real engine: no tracked source file changed since the map was built.
    Fresh,
    /// Real engine: tracked source files changed since the map was built.
    Stale { changed_files: usize },
    /// Real engine: the map cannot be trusted; a full re-scout is required.
    Rescout { reason: String },
    /// Fallback: `repo` path is not a resolvable git worktree — age-only signal.
    Age { bucket: AgeBucket },
}

/// A code-map row enriched with a freshness badge for the dashboard. The
/// original [`CodeMap`] fields are flattened so existing UI/consumers keep
/// reading `repo`/`area`/`head_sha`/… unchanged; only `freshness` is added.
#[derive(Debug, Clone, Serialize)]
pub struct CodeMapView {
    #[serde(flatten)]
    pub map: CodeMap,
    pub freshness: FreshnessBadge,
}

/// Newer than this (in days) is [`AgeBucket::Fresh`].
const AGE_FRESH_DAYS: i64 = 7;
/// Newer than this (in days) is [`AgeBucket::Aging`]; older is [`AgeBucket::Stale`].
const AGE_AGING_DAYS: i64 = 30;

// Guard the bucket ordering at compile time: a swap would make `Aging`
// unreachable and silently mis-bucket every map.
const _: () = assert!(
    AGE_FRESH_DAYS < AGE_AGING_DAYS,
    "AGE_FRESH_DAYS must be < AGE_AGING_DAYS"
);

/// Bucket a map's `built_at` timestamp by age relative to `now`. An
/// unparseable timestamp is [`AgeBucket::Unknown`] (never silently treated as
/// fresh) and is logged server-side: `built_at` is system-written, so a value
/// that will not parse signals an upstream data-integrity bug, not user input.
pub fn age_bucket(built_at: &str, now: DateTime<Utc>) -> AgeBucket {
    let Ok(built) = DateTime::parse_from_rfc3339(built_at) else {
        eprintln!("dashboard: code-map built_at is not RFC3339: {built_at:?}");
        return AgeBucket::Unknown;
    };
    let age_days = (now - built.with_timezone(&Utc)).num_days();
    if age_days < AGE_FRESH_DAYS {
        AgeBucket::Fresh
    } else if age_days < AGE_AGING_DAYS {
        AgeBucket::Aging
    } else {
        AgeBucket::Stale
    }
}

/// Map the freshness engine outcome to a badge (drops the `Stale` path list
/// down to a count for the wire — the UI only needs the number).
fn badge_from_freshness(freshness: Freshness) -> FreshnessBadge {
    match freshness {
        Freshness::Fresh => FreshnessBadge::Fresh,
        Freshness::Stale { changed_files } => FreshnessBadge::Stale {
            changed_files: changed_files.len(),
        },
        Freshness::RescoutRequired { reason } => FreshnessBadge::Rescout { reason },
    }
}

/// Test-only single-row convenience over [`row_freshness_cached`]. Production
/// always goes through [`enrich_code_maps`], which shares one diff cache across
/// the batch; this wrapper exists so single-row behavior can be asserted in
/// isolation.
#[cfg(test)]
fn row_freshness(map: &CodeMap, now: DateTime<Utc>) -> FreshnessBadge {
    let mut diff_cache = HashMap::new();
    row_freshness_cached(map, now, &mut diff_cache)
}

/// Enrich code-map rows with freshness badges. Pure transform — builds new
/// [`CodeMapView`] values and never mutates the inputs.
///
/// The git `diff` is memoized by `(repo, head_sha)`: many areas in one repo
/// share the same worktree and build SHA, so a request listing N rows runs at
/// most one `git diff` per distinct `(repo, head_sha)` instead of one per row,
/// bounding the subprocess fan-out well under the row-limit clamp.
pub fn enrich_code_maps(maps: Vec<CodeMap>, now: DateTime<Utc>) -> Vec<CodeMapView> {
    let mut diff_cache: HashMap<(String, String), Result<Vec<String>, String>> = HashMap::new();
    maps.into_iter()
        .map(|map| {
            let freshness = row_freshness_cached(&map, now, &mut diff_cache);
            CodeMapView { map, freshness }
        })
        .collect()
}

/// Classify a single code-map row (hybrid), sharing a git-diff cache across a
/// batch:
/// - If `map.repo` resolves to a real directory, run the real freshness engine
///   (`git diff` against HEAD, memoized by `(repo, head_sha)`) — the canonical
///   absolute worktree path is stored at write time.
/// - If the path does not exist (or is not a directory), fall back to a
///   build-age bucket derived from `built_at` — the worktree simply is not
///   checked out here.
/// - If the path cannot be `stat`-ed for any OTHER reason (permission, transient
///   I/O), freshness genuinely could not be determined: log server-side and emit
///   a distinct `Rescout` badge rather than masquerading the unknown as a
///   confident age signal (`Path::is_dir()` would silently collapse this into
///   the age fallback).
///
/// `now` is injected so the age path is deterministic and testable.
fn row_freshness_cached(
    map: &CodeMap,
    now: DateTime<Utc>,
    diff_cache: &mut HashMap<(String, String), Result<Vec<String>, String>>,
) -> FreshnessBadge {
    match std::fs::metadata(&map.repo) {
        Ok(meta) if meta.is_dir() => {
            let key = (map.repo.clone(), map.head_sha.clone());
            let diff = diff_cache
                .entry(key)
                .or_insert_with(|| diff_against_head(Path::new(&map.repo), &map.head_sha))
                .clone();
            badge_from_freshness(classify_with_diff(map, diff))
        }
        Ok(_) => FreshnessBadge::Age {
            bucket: age_bucket(&map.built_at, now),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FreshnessBadge::Age {
            bucket: age_bucket(&map.built_at, now),
        },
        Err(e) => {
            eprintln!("dashboard: cannot stat code-map repo {:?}: {e}", map.repo);
            FreshnessBadge::Rescout {
                reason: "repo path unreadable; re-scout required".to_string(),
            }
        }
    }
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

#[derive(Debug, Clone)]
pub struct SessionParams {
    pub limit: usize,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
        }
    }
}

/// List compact collab session summaries (most recently updated first).
///
/// Plan/message bodies are never returned — only drawer refs so the #90
/// by-reference contract is respected.
pub fn list_sessions(
    db: &ReadOnlyDb,
    params: &SessionParams,
) -> Result<Vec<CollabSessionSummary>, MemoryError> {
    db.with_connection(|conn| list_sessions_conn(conn, params))
}

pub(crate) fn list_sessions_conn(
    conn: &Connection,
    params: &SessionParams,
) -> Result<Vec<CollabSessionSummary>, MemoryError> {
    let limit = params.limit.clamp(1, MAX_DASHBOARD_LIMIT) as i64;
    let mut stmt = conn.prepare(
        "SELECT id, task, repo_path, branch, phase, current_owner, implementer,
                base_sha, last_head_sha, created_at, updated_at, ended_at,
                coding_failure, pr_url, task_list,
                canonical_plan_drawer_id, canonical_plan_hash,
                final_plan_drawer_id, final_plan_hash
         FROM collab_sessions
         ORDER BY updated_at DESC
         LIMIT ?1",
    )?;

    let rows = stmt.query_map(params![limit], |row| {
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

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;

    // ── freshness (GAP 2: per-code-map-row freshness) ─────────────────────────

    fn ts(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    }

    /// Build a git repo, commit `path`, return (TempDir, root, head_sha).
    fn git_repo_with(path: &str, content: &str) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "T"]);
        std::fs::write(root.join(path), content).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "c1"]);
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (dir, root, sha)
    }

    fn map_with(repo: &str, head_sha: &str, files: Vec<String>, built_at: &str) -> CodeMap {
        CodeMap {
            repo: repo.to_string(),
            area: "core".to_string(),
            drawer_id: "drawer-1".to_string(),
            head_sha: head_sha.to_string(),
            source_files: files,
            built_by: "tester".to_string(),
            built_at: built_at.to_string(),
        }
    }

    #[test]
    fn age_bucket_thresholds_and_unparseable() {
        let now = ts("2026-06-22T00:00:00Z");
        assert_eq!(age_bucket("2026-06-20T00:00:00Z", now), AgeBucket::Fresh); // 2d
        assert_eq!(age_bucket("2026-06-14T00:00:00Z", now), AgeBucket::Aging); // 8d
        assert_eq!(age_bucket("2026-01-01T00:00:00Z", now), AgeBucket::Stale); // ~172d
        assert_eq!(age_bucket("not-a-timestamp", now), AgeBucket::Unknown);
    }

    #[test]
    fn row_freshness_missing_repo_path_falls_back_to_age_badge() {
        let now = ts("2026-06-22T00:00:00Z");
        let map = map_with(
            "/nonexistent/repo/path-xyz",
            "aabbccdd1122334455667788aabbccdd11223344",
            vec!["src/lib.rs".to_string()],
            "2026-06-20T00:00:00Z",
        );
        assert_eq!(
            row_freshness(&map, now),
            FreshnessBadge::Age {
                bucket: AgeBucket::Fresh
            }
        );
    }

    #[test]
    fn row_freshness_resolvable_repo_uses_git_classify_stale() {
        let (_dir, root, build_sha) = git_repo_with("a.rs", "// v1");
        // Change the tracked source file after the build SHA.
        std::fs::write(root.join("a.rs"), "// v2").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "c2"]);

        let map = map_with(
            &root.to_string_lossy(),
            &build_sha,
            vec!["a.rs".to_string()],
            "2026-06-20T00:00:00Z",
        );
        // `now` is irrelevant on the git branch (path resolves).
        let now = ts("2026-06-22T00:00:00Z");
        assert_eq!(
            row_freshness(&map, now),
            FreshnessBadge::Stale { changed_files: 1 }
        );
    }

    #[test]
    fn row_freshness_resolvable_repo_uses_git_classify_fresh() {
        let (_dir, root, build_sha) = git_repo_with("a.rs", "// v1");
        // Change a DIFFERENT file — the tracked source file is unchanged.
        std::fs::write(root.join("b.rs"), "// b").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "c2"]);

        let map = map_with(
            &root.to_string_lossy(),
            &build_sha,
            vec!["a.rs".to_string()],
            "2026-06-20T00:00:00Z",
        );
        let now = ts("2026-06-22T00:00:00Z");
        assert_eq!(row_freshness(&map, now), FreshnessBadge::Fresh);
    }

    #[test]
    fn row_freshness_resolvable_repo_malformed_sha_is_rescout() {
        // Resolvable worktree but a non-hex head_sha: the engine's shape guard
        // rejects it → RescoutRequired → Rescout badge (fail-safe, never Fresh).
        let (_dir, root, _build_sha) = git_repo_with("a.rs", "// v1");
        let map = map_with(
            &root.to_string_lossy(),
            "HEAD", // not a hex object name
            vec!["a.rs".to_string()],
            "2026-06-20T00:00:00Z",
        );
        let now = ts("2026-06-22T00:00:00Z");
        assert!(
            matches!(row_freshness(&map, now), FreshnessBadge::Rescout { .. }),
            "non-hex head_sha on a real repo must classify as Rescout"
        );
    }

    #[test]
    fn row_freshness_unreadable_repo_path_is_rescout_not_age() {
        // A `repo` whose parent is a FILE yields a non-NotFound stat error
        // (ENOTDIR). That must surface as Rescout (freshness unknown), never be
        // silently collapsed into a confident age badge.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let repo = file.join("child"); // stat() -> ENOTDIR (not NotFound)
        let map = map_with(
            &repo.to_string_lossy(),
            "aabbccdd1122334455667788aabbccdd11223344",
            vec!["a.rs".to_string()],
            "2026-06-20T00:00:00Z",
        );
        let now = ts("2026-06-22T00:00:00Z");
        assert!(
            matches!(row_freshness(&map, now), FreshnessBadge::Rescout { .. }),
            "an unreadable (non-NotFound) repo path must be Rescout, not an age badge"
        );
    }

    #[test]
    fn freshness_badge_serializes_stale_and_rescout_shapes() {
        // Lock the wire contract the UI's freshnessLabel() reads.
        let stale = serde_json::to_value(FreshnessBadge::Stale { changed_files: 3 }).unwrap();
        assert_eq!(stale["kind"], "stale");
        assert_eq!(stale["changed_files"], 3);

        let rescout = serde_json::to_value(FreshnessBadge::Rescout {
            reason: "map cannot be verified".to_string(),
        })
        .unwrap();
        assert_eq!(rescout["kind"], "rescout");
        assert_eq!(rescout["reason"], "map cannot be verified");

        let fresh = serde_json::to_value(FreshnessBadge::Fresh).unwrap();
        assert_eq!(fresh["kind"], "fresh");
    }

    #[test]
    fn age_bucket_exact_threshold_boundaries() {
        let now = ts("2026-06-22T00:00:00Z");
        // num_days truncates; strict `<` means day 7 is Aging, day 30 is Stale.
        assert_eq!(age_bucket("2026-06-16T00:00:00Z", now), AgeBucket::Fresh); // 6d
        assert_eq!(age_bucket("2026-06-15T00:00:00Z", now), AgeBucket::Aging); // 7d
        assert_eq!(age_bucket("2026-05-24T00:00:00Z", now), AgeBucket::Aging); // 29d
        assert_eq!(age_bucket("2026-05-23T00:00:00Z", now), AgeBucket::Stale); // 30d
    }

    #[test]
    fn enrich_code_maps_handles_empty_input() {
        let now = ts("2026-06-22T00:00:00Z");
        assert!(enrich_code_maps(vec![], now).is_empty());
    }

    #[test]
    fn code_map_view_serializes_flattened_fields_plus_freshness() {
        let map = map_with(
            "repo-a",
            "aabbccdd1122334455667788aabbccdd11223344",
            vec!["src/lib.rs".to_string()],
            "2026-01-01T00:00:00Z",
        );
        let now = ts("2026-06-22T00:00:00Z"); // ~172d → age:stale (path unresolved)
        let views = enrich_code_maps(vec![map.clone()], now);
        let json = serde_json::to_value(&views).unwrap();
        let row = &json[0];
        // Original fields are flattened (UI keeps reading them directly).
        assert_eq!(row["repo"], "repo-a");
        assert_eq!(row["head_sha"], map.head_sha);
        assert_eq!(row["area"], "core");
        // Freshness badge is tagged and, for an unresolved path, an age bucket.
        assert_eq!(row["freshness"]["kind"], "age");
        assert_eq!(row["freshness"]["bucket"], "stale");
    }

    // ── WarmingStatus (GAP 1: warming status) ─────────────────────────────────

    #[test]
    fn warming_status_maps_all_model_status_variants() {
        use ironrace_embed::embedder::ModelStatus;
        assert_eq!(
            WarmingStatus::from(&ModelStatus::Ready),
            WarmingStatus::Ready
        );
        assert_eq!(
            WarmingStatus::from(&ModelStatus::Missing),
            WarmingStatus::Missing
        );
        assert_eq!(
            WarmingStatus::from(&ModelStatus::Corrupt),
            WarmingStatus::Corrupt
        );
        // The underlying I/O detail is never carried — the enum has no payload.
        assert_eq!(
            WarmingStatus::from(&ModelStatus::Unreadable("permission denied".into())),
            WarmingStatus::Unreadable
        );
    }

    #[test]
    fn warming_status_serializes_to_stable_snake_case_labels() {
        // The wire contract consumed by the UI must stay these exact strings.
        assert_eq!(serde_json::to_value(WarmingStatus::Ready).unwrap(), "ready");
        assert_eq!(
            serde_json::to_value(WarmingStatus::Missing).unwrap(),
            "missing"
        );
        assert_eq!(
            serde_json::to_value(WarmingStatus::Corrupt).unwrap(),
            "corrupt"
        );
        assert_eq!(
            serde_json::to_value(WarmingStatus::Unreadable).unwrap(),
            "unreadable"
        );
    }

    #[test]
    fn warming_status_missing_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let status = ironrace_embed::embedder::model_status(dir.path());
        assert_eq!(WarmingStatus::from(&status), WarmingStatus::Missing);
    }

    /// File-backed fixture: the projection functions take a `ReadOnlyDb`, which
    /// cannot share an in-memory connection with a writer. Setup writes through a
    /// `Database`; reads go through a fresh `ReadOnlyDb` opened on the same file.
    struct Fixture {
        _dir: tempfile::TempDir,
        db: Database,
        path: std::path::PathBuf,
    }

    impl Fixture {
        fn reader(&self) -> crate::db::ReadOnlyDb {
            Database::open_read_only(&self.path).unwrap()
        }
    }

    fn open_fresh() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.sqlite3");
        let db = Database::open(&path).unwrap();
        db.migrate().unwrap();
        Fixture {
            _dir: dir,
            db,
            path,
        }
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
        let summary = memory_summary(&db.reader(), &params).unwrap();
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
        db.db
            .insert_drawer(
                &crate::db::drawers::generate_id("alpha content", "alpha", "general"),
                "alpha content",
                &emb,
                "alpha",
                "general",
                "src/a.rs",
                "test",
            )
            .unwrap();
        db.db
            .insert_drawer(
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
        let summary = memory_summary(&db.reader(), &params).unwrap();
        assert_eq!(summary.total_drawers, 2);
        assert_eq!(summary.recent_drawers.len(), 2);
        assert_eq!(*summary.wing_counts.get("alpha").unwrap(), 1);
        assert_eq!(*summary.wing_counts.get("beta").unwrap(), 1);
        assert_eq!(*summary.room_counts.get("general").unwrap(), 1);
        assert_eq!(*summary.room_counts.get("notes").unwrap(), 1);

        // Wing-filtered.
        let params_alpha = MemoryParams {
            wing: Some("alpha".to_string()),
            room: None,
            limit: 10,
        };
        let alpha = memory_summary(&db.reader(), &params_alpha).unwrap();
        assert_eq!(alpha.total_drawers, 1);
        assert_eq!(alpha.recent_drawers.len(), 1);
        assert_eq!(alpha.recent_drawers[0].wing, "alpha");
        assert_eq!(*alpha.room_counts.get("general").unwrap(), 1);
        assert!(!alpha.room_counts.contains_key("notes"));
    }

    #[test]
    fn drawer_content_is_truncated_in_list_views_but_full_by_exact_id() {
        let db = open_fresh();
        let long_content = "x".repeat(500);
        let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        let drawer_id = crate::db::drawers::generate_id(&long_content, "w", "r");
        db.db
            .insert_drawer(&drawer_id, &long_content, &emb, "w", "r", "", "test")
            .unwrap();

        let params = MemoryParams {
            wing: None,
            room: None,
            limit: 10,
        };
        let reader = db.reader();
        let summary = memory_summary(&reader, &params).unwrap();
        assert_eq!(summary.recent_drawers.len(), 1);
        let preview = &summary.recent_drawers[0].content_preview;
        // Preview must be shorter than original.
        assert!(
            preview.len() <= LIST_CONTENT_LIMIT + 4,
            "preview too long: {}",
            preview.len()
        );
        assert_ne!(preview, &long_content);

        let detail = drawer_detail(&reader, &drawer_id).unwrap().unwrap();
        assert_eq!(detail.content, long_content);
    }

    // ── report_projection ───────────────────────────────────────────────────

    #[test]
    fn report_projection_matches_run_report_for_same_filters() {
        use crate::db::metrics::NewTokenUsage;

        let db = open_fresh();
        db.db
            .insert_token_usage(&NewTokenUsage {
                ts: "2026-01-02T00:00:00Z".to_string(),
                source: "transcript".to_string(),
                harness: "claude".to_string(),
                model: Some("claude-opus-4-8".to_string()),
                tool_name: None,
                session_id: None,
                collab_session_id: None,
                collab_phase: Some("impl".to_string()),
                task_tag: Some("dashboard-test".to_string()),
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                estimated: false,
                chars: 0,
                cost_usd: None,
                map_status: None,
                turn_id: None,
                area: None,
                original_response_bytes: None,
                compacted_response_bytes: None,
            })
            .unwrap();

        let task = Some("dashboard-test".to_string());
        let since = Some("2026-01-01".to_string());
        let projected = report_projection(&db.reader(), task.clone(), since.clone()).unwrap();
        let direct =
            crate::report::run_report(&db.db, &crate::report::ReportOptions { task, since })
                .unwrap();
        assert_eq!(projected, direct);
    }

    // ── list_code_maps ──────────────────────────────────────────────────────

    #[test]
    fn list_code_maps_empty() {
        let db = open_fresh();
        let maps = list_code_maps(&db.reader(), &CodeMapParams::default()).unwrap();
        assert!(maps.is_empty());
    }

    #[test]
    fn list_code_maps_with_filter() {
        let db = open_fresh();
        let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
        // Insert a drawer to satisfy the code_maps FK.
        let drawer_id = crate::db::drawers::generate_id("map content", "code-maps", "code-maps");
        db.db
            .insert_drawer(
                &drawer_id,
                "map content",
                &emb,
                "code-maps",
                "code-maps",
                "",
                "test",
            )
            .unwrap();
        db.db
            .upsert_code_map(
                "my-repo",
                "core",
                &drawer_id,
                "aabbccdd1122334455667788aabbccdd11223344",
                &["src/lib.rs".to_string()],
                "test-agent",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let drawer_id_2 =
            crate::db::drawers::generate_id("map content 2", "code-maps", "code-maps");
        db.db
            .insert_drawer(
                &drawer_id_2,
                "map content 2",
                &emb,
                "code-maps",
                "code-maps",
                "",
                "test",
            )
            .unwrap();
        db.db
            .upsert_code_map(
                "my-repo",
                "docs",
                &drawer_id_2,
                "bbbbccdd1122334455667788aabbccdd11223344",
                &["docs/readme.md".to_string()],
                "test-agent",
                "2026-01-02T00:00:00Z",
            )
            .unwrap();

        let reader = db.reader();

        // No filter — should return the row.
        let all = list_code_maps(&reader, &CodeMapParams::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|row| row.repo == "my-repo"));

        // Repo filter match.
        let filtered = list_code_maps(
            &reader,
            &CodeMapParams {
                repo: Some("my-repo".to_string()),
                area: None,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 2);

        // Area-only filter match.
        let area_only = list_code_maps(
            &reader,
            &CodeMapParams {
                repo: None,
                area: Some("docs".to_string()),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(area_only.len(), 1);
        assert_eq!(area_only[0].area, "docs");

        // Repo + area combined filter.
        let combined = list_code_maps(
            &reader,
            &CodeMapParams {
                repo: Some("my-repo".to_string()),
                area: Some("core".to_string()),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].area, "core");

        // SQL limit is enforced.
        let limited = list_code_maps(
            &reader,
            &CodeMapParams {
                repo: Some("my-repo".to_string()),
                area: None,
                limit: 1,
            },
        )
        .unwrap();
        assert_eq!(limited.len(), 1);

        // Repo filter no match.
        let none = list_code_maps(
            &reader,
            &CodeMapParams {
                repo: Some("other-repo".to_string()),
                area: None,
                limit: 10,
            },
        )
        .unwrap();
        assert!(none.is_empty());
    }

    // ── list_sessions ───────────────────────────────────────────────────────

    #[test]
    fn list_sessions_empty() {
        let db = open_fresh();
        let sessions = list_sessions(&db.reader(), &SessionParams::default()).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_does_not_include_plan_bodies() {
        let db = open_fresh();
        db.db
            .with_connection(|conn| {
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
        db.db
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE collab_sessions
                 SET canonical_plan_drawer_id = ?1,
                     canonical_plan_hash = ?2,
                     final_plan_drawer_id = ?3,
                     final_plan_hash = ?4,
                     task_list = ?5
                 WHERE id = ?6",
                    rusqlite::params![
                        "canonical-drawer",
                        "canonical-hash",
                        "final-drawer",
                        "final-hash",
                        r#"{"tasks":[{"id":1},{"id":2}]}"#,
                        "test-session-id-001",
                    ],
                )?;
                Ok(())
            })
            .unwrap();

        let sessions = list_sessions(&db.reader(), &SessionParams::default()).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "test-session-id-001");
        assert_eq!(s.task.as_deref(), Some("test task"));
        assert_eq!(s.tasks_count, Some(2));
        assert_eq!(
            s.canonical_plan_drawer_id.as_deref(),
            Some("canonical-drawer")
        );
        assert_eq!(s.canonical_plan_hash.as_deref(), Some("canonical-hash"));
        assert_eq!(s.final_plan_drawer_id.as_deref(), Some("final-drawer"));
        assert_eq!(s.final_plan_hash.as_deref(), Some("final-hash"));
    }
}
