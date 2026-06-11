//! Metrics storage layer — insert/query API for the four counter tables
//! introduced in migration 008 (`token_usage`, `occupancy_samples`,
//! `session_summary`, `task_outcomes`).
//!
//! This module is storage-only: no write call-sites exist in this PR.
//! Enum column values are stringly-typed here; the DB CHECK constraints
//! (see `migrations/008_metrics.sql`) enforce domain correctness so a
//! malformed direct write cannot land an out-of-domain value.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::schema::Database;
use crate::error::MemoryError;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// Input for a new `token_usage` row (id is auto-assigned by SQLite).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTokenUsage {
    pub ts: String,
    pub source: String,
    pub harness: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub task_tag: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub estimated: bool,
    pub chars: i64,
    pub cost_usd: Option<f64>,
}

/// A stored `token_usage` row including its auto-assigned `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub id: i64,
    pub ts: String,
    pub source: String,
    pub harness: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub task_tag: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub estimated: bool,
    pub chars: i64,
    pub cost_usd: Option<f64>,
}

/// Query filters for `token_usage`. All fields are optional; unset fields
/// match every row. Results are ordered by `(ts, id)` then optionally limited.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsageQuery {
    pub task_tag: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub limit: Option<usize>,
}

/// Input for a new `occupancy_samples` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewOccupancySample {
    pub ts: String,
    pub harness: String,
    pub session_id: Option<String>,
    pub workspace_root: Option<String>,
    pub hook_event: Option<String>,
    pub input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub context_window: i64,
    pub occupancy_pct: Option<f64>,
}

/// A stored `occupancy_samples` row including its auto-assigned `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OccupancySample {
    pub id: i64,
    pub ts: String,
    pub harness: String,
    pub session_id: Option<String>,
    pub workspace_root: Option<String>,
    pub hook_event: Option<String>,
    pub input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub context_window: i64,
    pub occupancy_pct: Option<f64>,
}

/// A `session_summary` row. `session_id` is the primary key and is
/// caller-supplied (not auto-assigned). This one struct is both the
/// `upsert_session_summary` input and the `get_session_summary` result — the
/// `New*`/stored split used for `token_usage`/`occupancy_samples` is
/// intentionally collapsed here because the PK is caller-supplied (there is no
/// auto-assigned `id` to omit on insert).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub harness: String,
    pub workspace_root: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub peak_occupancy_pct: Option<f64>,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub mcp_chars_served: i64,
    pub compactions: i64,
}

/// A `task_outcomes` row. `task_tag` is the unique caller-supplied key
/// (METRICS_SPEC §5.4); `id` is not exposed — callers query by `task_tag`.
/// This one struct is both the `upsert_task_outcome` input and the
/// `get_task_outcome` / `task_outcomes_for_collab` result (same rationale as
/// [`SessionSummary`]: the key is caller-supplied, so no `New*` split is
/// needed). Note: `task_tag` is `NOT NULL` in the schema, so a writer keying a
/// task by collab session (METRICS_SPEC §2.3, `task_key =
/// COALESCE(collab_session_id, task_tag)`) must derive a non-null `task_tag`
/// before upserting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub task_tag: String,
    pub collab_session_id: Option<String>,
    pub started_at: Option<String>,
    pub done_at: Option<String>,
    pub outcome: Option<String>,
    pub review_rounds: i64,
    pub fix_commits: i64,
    pub handoffs: i64,
    pub pr_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Row mappers (private free functions)
// ---------------------------------------------------------------------------

fn map_token_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenUsage> {
    let estimated_int: i64 = row.get(13)?;
    Ok(TokenUsage {
        id: row.get(0)?,
        ts: row.get(1)?,
        source: row.get(2)?,
        harness: row.get(3)?,
        model: row.get(4)?,
        session_id: row.get(5)?,
        collab_session_id: row.get(6)?,
        collab_phase: row.get(7)?,
        task_tag: row.get(8)?,
        input_tokens: row.get(9)?,
        output_tokens: row.get(10)?,
        cache_creation_input_tokens: row.get(11)?,
        cache_read_input_tokens: row.get(12)?,
        estimated: estimated_int != 0,
        chars: row.get(14)?,
        cost_usd: row.get(15)?,
    })
}

fn map_occupancy_sample(row: &rusqlite::Row<'_>) -> rusqlite::Result<OccupancySample> {
    Ok(OccupancySample {
        id: row.get(0)?,
        ts: row.get(1)?,
        harness: row.get(2)?,
        session_id: row.get(3)?,
        workspace_root: row.get(4)?,
        hook_event: row.get(5)?,
        input_tokens: row.get(6)?,
        cache_read_input_tokens: row.get(7)?,
        context_window: row.get(8)?,
        occupancy_pct: row.get(9)?,
    })
}

fn map_session_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        session_id: row.get(0)?,
        harness: row.get(1)?,
        workspace_root: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        peak_occupancy_pct: row.get(5)?,
        total_input_tokens: row.get(6)?,
        total_output_tokens: row.get(7)?,
        mcp_chars_served: row.get(8)?,
        compactions: row.get(9)?,
    })
}

fn map_task_outcome(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskOutcome> {
    Ok(TaskOutcome {
        task_tag: row.get(0)?,
        collab_session_id: row.get(1)?,
        started_at: row.get(2)?,
        done_at: row.get(3)?,
        outcome: row.get(4)?,
        review_rounds: row.get(5)?,
        fix_commits: row.get(6)?,
        handoffs: row.get(7)?,
        pr_url: row.get(8)?,
    })
}

// ---------------------------------------------------------------------------
// impl Database
// ---------------------------------------------------------------------------

impl Database {
    /// Insert a `token_usage` row and return its auto-assigned rowid.
    pub fn insert_token_usage(&self, row: &NewTokenUsage) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO token_usage (
                ts, source, harness, model, session_id, collab_session_id, collab_phase,
                task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
                cache_read_input_tokens, estimated, chars, cost_usd
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                row.ts,
                row.source,
                row.harness,
                row.model,
                row.session_id,
                row.collab_session_id,
                row.collab_phase,
                row.task_tag,
                row.input_tokens,
                row.output_tokens,
                row.cache_creation_input_tokens,
                row.cache_read_input_tokens,
                row.estimated as i64,
                row.chars,
                row.cost_usd,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Query `token_usage` rows with optional filters, ordered by `(ts, id)`.
    ///
    /// Uses the `(?N IS NULL OR col = ?N)` idiom so a single fixed SQL string
    /// handles all filter combinations without dynamic query building.
    /// `limit` of `None` maps to `-1` which SQLite treats as unlimited.
    pub fn query_token_usage(&self, q: &TokenUsageQuery) -> Result<Vec<TokenUsage>, MemoryError> {
        let limit = q.limit.map(|l| l as i64).unwrap_or(-1);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, source, harness, model, session_id, collab_session_id, collab_phase,
                    task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
                    cache_read_input_tokens, estimated, chars, cost_usd
             FROM token_usage
             WHERE (?1 IS NULL OR task_tag = ?1)
               AND (?2 IS NULL OR collab_session_id = ?2)
               AND (?3 IS NULL OR collab_phase = ?3)
             ORDER BY ts, id
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![q.task_tag, q.collab_session_id, q.collab_phase, limit],
            map_token_usage,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// Insert an `occupancy_samples` row and return its auto-assigned rowid.
    pub fn insert_occupancy_sample(&self, row: &NewOccupancySample) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO occupancy_samples (
                ts, harness, session_id, workspace_root, hook_event,
                input_tokens, cache_read_input_tokens, context_window, occupancy_pct
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                row.ts,
                row.harness,
                row.session_id,
                row.workspace_root,
                row.hook_event,
                row.input_tokens,
                row.cache_read_input_tokens,
                row.context_window,
                row.occupancy_pct,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Return up to `limit` occupancy samples for `session_id`, ordered by
    /// `(ts, id)` ascending.
    pub fn occupancy_samples_for_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<OccupancySample>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, harness, session_id, workspace_root, hook_event,
                    input_tokens, cache_read_input_tokens, context_window, occupancy_pct
             FROM occupancy_samples
             WHERE session_id = ?1
             ORDER BY ts, id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], map_occupancy_sample)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// Insert or replace a `session_summary` row. On conflict the non-key
    /// columns are updated to the new values (full-row upsert).
    pub fn upsert_session_summary(&self, row: &SessionSummary) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO session_summary (
                session_id, harness, workspace_root, started_at, ended_at,
                peak_occupancy_pct, total_input_tokens, total_output_tokens,
                mcp_chars_served, compactions
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
            ON CONFLICT(session_id) DO UPDATE SET
                harness             = excluded.harness,
                workspace_root      = excluded.workspace_root,
                started_at          = excluded.started_at,
                ended_at            = excluded.ended_at,
                peak_occupancy_pct  = excluded.peak_occupancy_pct,
                total_input_tokens  = excluded.total_input_tokens,
                total_output_tokens = excluded.total_output_tokens,
                mcp_chars_served    = excluded.mcp_chars_served,
                compactions         = excluded.compactions",
            params![
                row.session_id,
                row.harness,
                row.workspace_root,
                row.started_at,
                row.ended_at,
                row.peak_occupancy_pct,
                row.total_input_tokens,
                row.total_output_tokens,
                row.mcp_chars_served,
                row.compactions,
            ],
        )?;
        Ok(())
    }

    /// Fetch a `session_summary` by `session_id`. Returns `None` if not found.
    pub fn get_session_summary(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionSummary>, MemoryError> {
        self.conn
            .query_row(
                "SELECT session_id, harness, workspace_root, started_at, ended_at,
                        peak_occupancy_pct, total_input_tokens, total_output_tokens,
                        mcp_chars_served, compactions
                 FROM session_summary
                 WHERE session_id = ?1",
                params![session_id],
                map_session_summary,
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// Insert or replace a `task_outcomes` row. On conflict the non-key
    /// columns are updated to the new values (full-row upsert).
    pub fn upsert_task_outcome(&self, row: &TaskOutcome) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO task_outcomes (
                task_tag, collab_session_id, started_at, done_at, outcome,
                review_rounds, fix_commits, handoffs, pr_url
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ON CONFLICT(task_tag) DO UPDATE SET
                collab_session_id = excluded.collab_session_id,
                started_at        = excluded.started_at,
                done_at           = excluded.done_at,
                outcome           = excluded.outcome,
                review_rounds     = excluded.review_rounds,
                fix_commits       = excluded.fix_commits,
                handoffs          = excluded.handoffs,
                pr_url            = excluded.pr_url",
            params![
                row.task_tag,
                row.collab_session_id,
                row.started_at,
                row.done_at,
                row.outcome,
                row.review_rounds,
                row.fix_commits,
                row.handoffs,
                row.pr_url,
            ],
        )?;
        Ok(())
    }

    /// Fetch a `task_outcomes` row by `task_tag`. Returns `None` if not found.
    pub fn get_task_outcome(&self, task_tag: &str) -> Result<Option<TaskOutcome>, MemoryError> {
        self.conn
            .query_row(
                "SELECT task_tag, collab_session_id, started_at, done_at, outcome,
                        review_rounds, fix_commits, handoffs, pr_url
                 FROM task_outcomes
                 WHERE task_tag = ?1",
                params![task_tag],
                map_task_outcome,
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// Return all `task_outcomes` rows for a given `collab_session_id`,
    /// ordered by `(started_at, id)` ascending.
    pub fn task_outcomes_for_collab(
        &self,
        collab_session_id: &str,
    ) -> Result<Vec<TaskOutcome>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_tag, collab_session_id, started_at, done_at, outcome,
                    review_rounds, fix_commits, handoffs, pr_url
             FROM task_outcomes
             WHERE collab_session_id = ?1
             ORDER BY started_at, id",
        )?;
        let rows = stmt.query_map(params![collab_session_id], map_task_outcome)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;
    use rusqlite::OptionalExtension;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn sample_token_usage() -> NewTokenUsage {
        NewTokenUsage {
            ts: "2026-06-11T00:00:00Z".into(),
            source: "mcp_response".into(),
            harness: "claude".into(),
            model: Some("claude-opus-4-8".into()),
            session_id: Some("sess-1".into()),
            collab_session_id: Some("collab-1".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("issue-80".into()),
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 7,
            estimated: true,
            chars: 480,
            cost_usd: Some(0.0123),
        }
    }

    fn sample_occupancy_sample(ts: &str) -> NewOccupancySample {
        NewOccupancySample {
            ts: ts.into(),
            harness: "claude".into(),
            session_id: Some("sess-1".into()),
            workspace_root: Some("/repo".into()),
            hook_event: Some("precompact".into()),
            input_tokens: 150_000,
            cache_read_input_tokens: 90_000,
            context_window: 200_000,
            occupancy_pct: Some(1.2),
        }
    }

    fn sample_task_outcome(task_tag: &str, started_at: &str) -> TaskOutcome {
        TaskOutcome {
            task_tag: task_tag.into(),
            collab_session_id: Some("collab-1".into()),
            started_at: Some(started_at.into()),
            done_at: None,
            outcome: None,
            review_rounds: 0,
            fix_commits: 0,
            handoffs: 0,
            pr_url: None,
        }
    }

    #[test]
    fn token_usage_round_trip() {
        let db = db();
        let id = db.insert_token_usage(&sample_token_usage()).unwrap();
        assert!(id > 0);

        let by_tag = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 1);
        let row = &by_tag[0];
        assert_eq!(row.id, id);
        assert_eq!(row.source, "mcp_response");
        assert_eq!(row.input_tokens, 100);
        assert!(row.estimated);
        assert!((row.cost_usd.unwrap() - 0.0123).abs() < 1e-9);

        let by_collab = db
            .query_token_usage(&TokenUsageQuery {
                collab_session_id: Some("collab-1".into()),
                collab_phase: Some("impl".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_collab.len(), 1);
    }

    #[test]
    fn token_usage_round_trip_preserves_nullable_fields() {
        let db = db();
        let mut row = sample_token_usage();
        row.model = None;
        row.session_id = None;
        row.collab_session_id = None;
        row.collab_phase = None;
        row.task_tag = Some("issue-null".into());
        row.cost_usd = None;

        db.insert_token_usage(&row).unwrap();

        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-null".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].model.is_none());
        assert!(rows[0].session_id.is_none());
        assert!(rows[0].collab_session_id.is_none());
        assert!(rows[0].collab_phase.is_none());
        assert!(rows[0].cost_usd.is_none());
    }

    #[test]
    fn token_usage_query_is_ordered_and_limited() {
        let db = db();
        for ts in [
            "2026-06-11T00:00:03Z",
            "2026-06-11T00:00:01Z",
            "2026-06-11T00:00:02Z",
        ] {
            let mut r = sample_token_usage();
            r.ts = ts.into();
            db.insert_token_usage(&r).unwrap();
        }
        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, "2026-06-11T00:00:01Z");
        assert_eq!(rows[1].ts, "2026-06-11T00:00:02Z");
    }

    #[test]
    fn token_usage_soft_fk_survives_unknown_collab_session() {
        let db = db();
        let mut r = sample_token_usage();
        r.collab_session_id = Some("does-not-exist".into());
        assert!(db.insert_token_usage(&r).is_ok());
    }

    #[test]
    fn token_usage_rejects_bad_enums_and_negatives() {
        let db = db();
        let mutations: [fn(&mut NewTokenUsage); 9] = [
            |r| r.source = "bogus".into(),
            |r| r.harness = "bogus".into(),
            |r| r.collab_phase = Some("bogus".into()),
            |r| r.input_tokens = -1,
            |r| r.output_tokens = -1,
            |r| r.cache_creation_input_tokens = -1,
            |r| r.cache_read_input_tokens = -1,
            |r| r.chars = -1,
            |r| r.cost_usd = Some(-0.01),
        ];
        for mutate in mutations {
            let mut r = sample_token_usage();
            mutate(&mut r);
            assert!(db.insert_token_usage(&r).is_err());
        }
    }

    #[test]
    fn occupancy_round_trip_allows_over_one() {
        let db = db();
        let id = db
            .insert_occupancy_sample(&sample_occupancy_sample("2026-06-11T00:00:00Z"))
            .unwrap();
        assert!(id > 0);
        let rows = db.occupancy_samples_for_session("sess-1", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].occupancy_pct.unwrap() - 1.2).abs() < 1e-9);
    }

    #[test]
    fn occupancy_round_trip_preserves_nullable_fields() {
        let db = db();
        let mut row = sample_occupancy_sample("2026-06-11T00:00:00Z");
        row.workspace_root = None;
        row.hook_event = None;
        row.occupancy_pct = None;

        db.insert_occupancy_sample(&row).unwrap();

        let rows = db.occupancy_samples_for_session("sess-1", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].workspace_root.is_none());
        assert!(rows[0].hook_event.is_none());
        assert!(rows[0].occupancy_pct.is_none());
    }

    #[test]
    fn occupancy_samples_are_ordered_and_limited() {
        let db = db();
        for ts in [
            "2026-06-11T00:00:03Z",
            "2026-06-11T00:00:01Z",
            "2026-06-11T00:00:02Z",
        ] {
            db.insert_occupancy_sample(&sample_occupancy_sample(ts))
                .unwrap();
        }

        let rows = db.occupancy_samples_for_session("sess-1", 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, "2026-06-11T00:00:01Z");
        assert_eq!(rows[1].ts, "2026-06-11T00:00:02Z");
    }

    #[test]
    fn occupancy_rejects_bad_hook_event() {
        let db = db();
        let mut r = sample_occupancy_sample("2026-06-11T00:00:00Z");
        r.hook_event = Some("bogus".into());
        assert!(db.insert_occupancy_sample(&r).is_err());
    }

    #[test]
    fn occupancy_rejects_bad_harness_and_negatives() {
        let db = db();
        let mutations: [fn(&mut NewOccupancySample); 5] = [
            |r| r.harness = "bogus".into(),
            |r| r.input_tokens = -1,
            |r| r.cache_read_input_tokens = -1,
            |r| r.context_window = 0,
            |r| r.occupancy_pct = Some(-0.01),
        ];
        for mutate in mutations {
            let mut r = sample_occupancy_sample("2026-06-11T00:00:00Z");
            mutate(&mut r);
            assert!(db.insert_occupancy_sample(&r).is_err());
        }
    }

    #[test]
    fn session_summary_upsert_updates() {
        let db = db();
        let mut s = SessionSummary {
            session_id: "sess-1".into(),
            harness: "claude".into(),
            workspace_root: Some("/repo".into()),
            started_at: Some("2026-06-11T00:00:00Z".into()),
            ended_at: None,
            peak_occupancy_pct: Some(0.4),
            total_input_tokens: 10,
            total_output_tokens: 2,
            mcp_chars_served: 100,
            compactions: 0,
        };
        db.upsert_session_summary(&s).unwrap();
        s.ended_at = Some("2026-06-11T01:00:00Z".into());
        s.peak_occupancy_pct = Some(0.8);
        s.total_input_tokens = 50;
        s.compactions = 1;
        db.upsert_session_summary(&s).unwrap();

        let got = db.get_session_summary("sess-1").unwrap().unwrap();
        assert_eq!(got.ended_at.as_deref(), Some("2026-06-11T01:00:00Z"));
        assert_eq!(got.total_input_tokens, 50);
        assert_eq!(got.compactions, 1);
        assert!((got.peak_occupancy_pct.unwrap() - 0.8).abs() < 1e-9);
        assert!(db.get_session_summary("missing").unwrap().is_none());
    }

    #[test]
    fn session_summary_rejects_bad_harness() {
        let db = db();
        let s = SessionSummary {
            session_id: "sess-1".into(),
            harness: "bogus".into(),
            workspace_root: None,
            started_at: None,
            ended_at: None,
            peak_occupancy_pct: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            mcp_chars_served: 0,
            compactions: 0,
        };
        assert!(db.upsert_session_summary(&s).is_err());
    }

    #[test]
    fn session_summary_rejects_negative_counters() {
        let db = db();
        let mutations: [fn(&mut SessionSummary); 5] = [
            |s| s.peak_occupancy_pct = Some(-0.01),
            |s| s.total_input_tokens = -1,
            |s| s.total_output_tokens = -1,
            |s| s.mcp_chars_served = -1,
            |s| s.compactions = -1,
        ];
        for mutate in mutations {
            let mut s = SessionSummary {
                session_id: "sess-1".into(),
                harness: "claude".into(),
                workspace_root: None,
                started_at: None,
                ended_at: None,
                peak_occupancy_pct: None,
                total_input_tokens: 0,
                total_output_tokens: 0,
                mcp_chars_served: 0,
                compactions: 0,
            };
            mutate(&mut s);
            assert!(db.upsert_session_summary(&s).is_err());
        }
    }

    #[test]
    fn task_outcome_upsert_and_query() {
        let db = db();
        let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
        db.upsert_task_outcome(&t).unwrap();
        t.done_at = Some("2026-06-11T02:00:00Z".into());
        t.outcome = Some("merged".into());
        t.review_rounds = 2;
        t.pr_url = Some("https://example/pr/1".into());
        db.upsert_task_outcome(&t).unwrap();

        let got = db.get_task_outcome("issue-80").unwrap().unwrap();
        assert_eq!(got.outcome.as_deref(), Some("merged"));
        assert_eq!(got.review_rounds, 2);

        let for_collab = db.task_outcomes_for_collab("collab-1").unwrap();
        assert_eq!(for_collab.len(), 1);
        assert!(db.get_task_outcome("missing").unwrap().is_none());
    }

    #[test]
    fn task_outcomes_for_collab_is_ordered_by_started_at_then_id() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("task-b", "2026-06-11T00:00:02Z"))
            .unwrap();
        db.upsert_task_outcome(&sample_task_outcome("task-a", "2026-06-11T00:00:01Z"))
            .unwrap();
        db.upsert_task_outcome(&sample_task_outcome("task-c", "2026-06-11T00:00:01Z"))
            .unwrap();

        let rows = db.task_outcomes_for_collab("collab-1").unwrap();
        let task_tags: Vec<_> = rows.iter().map(|row| row.task_tag.as_str()).collect();
        assert_eq!(task_tags, ["task-a", "task-c", "task-b"]);
    }

    #[test]
    fn task_outcome_rejects_bad_outcome() {
        let db = db();
        let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
        t.outcome = Some("bogus".into());
        assert!(db.upsert_task_outcome(&t).is_err());
    }

    #[test]
    fn task_outcome_rejects_negative_counters() {
        let db = db();
        let mutations: [fn(&mut TaskOutcome); 3] = [
            |t| t.review_rounds = -1,
            |t| t.fix_commits = -1,
            |t| t.handoffs = -1,
        ];
        for mutate in mutations {
            let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
            mutate(&mut t);
            assert!(db.upsert_task_outcome(&t).is_err());
        }
    }

    #[test]
    fn real_db_migrates_cleanly() {
        let Some(src) = real_db_path() else {
            eprintln!("skip real_db_migrates_cleanly: no readable real DB");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("copy.sqlite3");
        if std::fs::copy(&src, &dst).is_err() {
            eprintln!("skip real_db_migrates_cleanly: copy failed");
            return;
        }
        let db = Database::open(&dst).unwrap();
        db.migrate().unwrap();
        let exists: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='token_usage'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert!(exists);
    }

    fn real_db_path() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("IRONMEM_DB_PATH") {
            let pb = std::path::PathBuf::from(p);
            if pb.is_file() {
                return Some(pb);
            }
        }
        let pb = dirs::home_dir()?
            .join(".ironrace-memory")
            .join("memory.sqlite3");
        pb.is_file().then_some(pb)
    }
}
