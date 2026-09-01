//! Attempt lineage and per-issue current state.
//!
//! Two of the module doc's five drawer kinds live here:
//!
//! 1. [`AttemptRecord`] — one append-only drawer per attempt, **never**
//!    written with a `logical_key`. Every attempt appends a record,
//!    successes included (spec line 421: an earlier design draft that only
//!    wrote on failure was called out as a bug, because it would leave
//!    successful approaches unrecorded and therefore re-derivable).
//! 2. [`IssueStatus`] — the per-issue current-state drawer, overwritten via
//!    `logical_key` on every update.
//!
//! Exact issue→attempt traversal goes through the knowledge graph
//! (`issue-<n> --has_attempt--> <attempt_id>`), not semantic search — per the
//! spec, `search` alone can't reliably enumerate every attempt on an issue.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::error::MemoryError;

use super::scrub::scrub_and_bound;
use super::{validate_repo, write_current, zero_embedding, IssueRef, ADDED_BY, ROOM, WING};

/// Per-field bound applied to `approach` and `why_failed` before they are
/// persisted. Chosen to comfortably hold a real diagnostic (a failing test
/// name, an assertion, a few lines of stderr) while keeping a single attempt
/// record from ballooning to the size of a raw CI log.
pub const MAX_LINEAGE_FIELD_CHARS: usize = 4_000;

use super::{ISSUE_ENTITY_TYPE, MAX_ISSUE_EDGES};

const ATTEMPT_ENTITY_TYPE: &str = "attempt";
const HAS_ATTEMPT_PREDICATE: &str = "has_attempt";

/// Outcome of one dispatch's work on an issue. Distinct from the goal
/// evaluator's `met`/`not_met`/`impossible` verdict (that's a rung-2
/// concept, judged transcript-only, mid-dispatch) — this is the lineage
/// writer's own append-time classification of what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Success,
    Failed,
}

/// One attempt at an issue, as the caller (the rung-2 dispatch runner)
/// supplies it. `approach` and `why_failed` are scrubbed and length-bounded
/// by [`record_attempt`] before they are persisted — this struct carries the
/// caller's raw values.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    pub issue: IssueRef,
    pub attempt_n: u32,
    pub approach: String,
    pub verdict: AttemptOutcome,
    /// Populated for a failed attempt; `None` for a success.
    pub why_failed: Option<String>,
    /// Present in the shape for both outcomes (spec line 421 — the field is
    /// always part of the record), but only ever populated with a real value
    /// when the attempt produced a commit.
    pub commit_sha: Option<String>,
}

/// The JSON actually written to the drawer. Kept separate from
/// [`AttemptRecord`] because the persisted shape carries bookkeeping
/// (`record_id`, `recorded_at`, scrub flags) the caller doesn't supply and
/// the spec's literal shape (`{issue, attempt_n, approach, verdict,
/// why_failed, commit_sha}`) doesn't name a `repo` sibling the way the
/// dispatch-state shape does — so here `issue` is the `repo#number`
/// canonical string, not a bare number.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttemptBody {
    issue: String,
    repo: String,
    issue_number: u64,
    attempt_n: u32,
    approach: String,
    verdict: AttemptOutcome,
    why_failed: Option<String>,
    commit_sha: Option<String>,
    /// Guarantees this record's content — and therefore its content-derived
    /// drawer id — is unique even if every other field is identical to a
    /// prior attempt (e.g. two attempts that both fail for the same reason
    /// with no commit). Without this, two such attempts would hash to the
    /// same id and the second would silently overwrite the first via
    /// `insert_drawer`'s `ON CONFLICT(id) DO UPDATE` — the exact
    /// history-destroying failure mode the spec's `logical_key` hazard warns
    /// about, reachable here even without ever touching `logical_key`.
    record_id: String,
    recorded_at: String,
    approach_redacted: bool,
    approach_truncated: bool,
    why_failed_redacted: bool,
    why_failed_truncated: bool,
}

/// What [`record_attempt`] persisted, for a caller that wants to know
/// whether scrubbing actually changed anything (useful for tests and for a
/// future Lead-side "this attempt's output was redacted" surface).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAttempt {
    pub drawer_id: String,
    pub redacted: bool,
    pub truncated: bool,
}

/// Append a new attempt lineage record. **Never** overwrites a prior
/// attempt — see the module and crate docs' hazard note. `approach` and
/// `why_failed` are scrubbed for credential-shaped substrings and bounded to
/// [`MAX_LINEAGE_FIELD_CHARS`] before anything is persisted.
pub fn record_attempt(
    db: &Database,
    record: &AttemptRecord,
) -> Result<RecordedAttempt, MemoryError> {
    validate_repo(&record.issue.repo)?;

    let approach_scrub = scrub_and_bound(&record.approach, MAX_LINEAGE_FIELD_CHARS);
    let why_failed_scrub = record
        .why_failed
        .as_deref()
        .map(|text| scrub_and_bound(text, MAX_LINEAGE_FIELD_CHARS));

    let body = AttemptBody {
        issue: record.issue.canonical(),
        repo: record.issue.repo.clone(),
        issue_number: record.issue.number,
        attempt_n: record.attempt_n,
        approach: approach_scrub.text,
        verdict: record.verdict,
        why_failed: why_failed_scrub.as_ref().map(|o| o.text.clone()),
        commit_sha: record.commit_sha.clone(),
        record_id: uuid::Uuid::new_v4().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
        approach_redacted: approach_scrub.redacted,
        approach_truncated: approach_scrub.truncated,
        why_failed_redacted: why_failed_scrub.as_ref().is_some_and(|o| o.redacted),
        why_failed_truncated: why_failed_scrub.as_ref().is_some_and(|o| o.truncated),
    };

    let content = serde_json::to_string(&body)?;
    let drawer_id = crate::db::drawers::generate_id(&content, WING, ROOM);
    let issue_entity = record.issue.entity_name();
    let embedding = zero_embedding();

    // Drawer + knowledge-graph edge are written in one transaction so a
    // crash between the two can never leave an attempt drawer with no edge
    // (or vice versa) for a restarted Lead to trip over.
    db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx, &drawer_id, &content, &embedding, WING, ROOM, "", ADDED_BY,
        )?;
        KnowledgeGraph::add_triple_tx(
            tx,
            &issue_entity,
            ISSUE_ENTITY_TYPE,
            HAS_ATTEMPT_PREDICATE,
            &drawer_id,
            ATTEMPT_ENTITY_TYPE,
            None,
            1.0,
            None,
        )?;
        Database::wal_log_tx(
            tx,
            "autopilot_record_attempt",
            &json!({
                "drawer_id": &drawer_id,
                "issue": &body.issue,
                "attempt_n": body.attempt_n,
            }),
            None,
        )?;
        Ok(())
    })?;

    Ok(RecordedAttempt {
        drawer_id,
        redacted: body.approach_redacted || body.why_failed_redacted,
        truncated: body.approach_truncated || body.why_failed_truncated,
    })
}

/// All recorded attempts for an issue, oldest (`attempt_n`) first. Returns an
/// empty vec (not an error) for an issue with no attempts yet — that's a
/// normal state (an issue the Lead hasn't dispatched), not a fault.
///
/// This is the "exact issue→attempt traversal" the spec calls for: it
/// resolves the issue's knowledge-graph entity, walks its current
/// `has_attempt` edges, and fetches each edge's target drawer directly by
/// id — the same guarantee `mcp__ironmem__kg_query` would give a caller
/// going through the MCP surface instead.
pub fn attempts_for_issue(
    db: &Database,
    issue: &IssueRef,
) -> Result<Vec<AttemptRecord>, MemoryError> {
    let kg = KnowledgeGraph::new(db);
    let entity = match kg.resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE)) {
        Ok(entity) => entity,
        Err(MemoryError::NotFound(_)) => return Ok(Vec::new()),
        Err(other) => return Err(other),
    };

    let triples = kg.query_entity_current(&entity.id, MAX_ISSUE_EDGES)?;
    let mut records = Vec::new();
    for triple in triples {
        if triple.predicate != HAS_ATTEMPT_PREDICATE {
            continue;
        }
        // `triple.object` is the *entity* id (`entity_id(name, type)`'s
        // hash), not the drawer id we stored as that entity's `name` — the
        // `triples` table's `subject`/`object` columns hold entity ids, per
        // `KnowledgeGraph::add_triple_conn`. Resolve the entity first to get
        // back the actual attempt drawer id.
        let Some(object_entity) = kg.get_entity(&triple.object)? else {
            // A dangling edge (entity somehow gc'd out from under it) is a
            // retention concern, not a traversal bug; skip rather than fail
            // the whole query.
            continue;
        };
        let Some(drawer) = db.get_drawer(&object_entity.name)? else {
            continue;
        };
        let body: AttemptBody = serde_json::from_str(&drawer.content)?;
        records.push(AttemptRecord {
            issue: IssueRef::new(body.repo, body.issue_number),
            attempt_n: body.attempt_n,
            approach: body.approach,
            verdict: body.verdict,
            why_failed: body.why_failed,
            commit_sha: body.commit_sha,
        });
    }
    records.sort_by_key(|record| record.attempt_n);
    Ok(records)
}

/// Best-so-far state for one issue, persisted via `logical_key` so each
/// update overwrites rather than accumulates. This module only stores
/// whatever the caller computes as "current" — deciding what counts as
/// "best" (e.g. first success wins, or a future AVO score comparison) is a
/// rung-2 policy question, not a storage-layer one.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueStatus {
    pub issue: IssueRef,
    pub best_verdict: Option<AttemptOutcome>,
    pub best_commit_sha: Option<String>,
    /// Cumulative attempt count across *all* dispatches ever made against
    /// this issue — the counter the spec's *Cross-dispatch stagnation
    /// control* section persists across dispatches, independent of any
    /// single dispatch's own `attempt_n`.
    pub cumulative_attempt_n: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueStatusBody {
    issue: String,
    repo: String,
    issue_number: u64,
    best_verdict: Option<AttemptOutcome>,
    best_commit_sha: Option<String>,
    cumulative_attempt_n: u32,
    updated_at: String,
}

fn issue_status_key(issue: &IssueRef) -> String {
    format!("issue-status:{}", issue.slug())
}

/// Write (overwrite) an issue's current-state drawer.
pub fn upsert_issue_status(db: &Database, status: &IssueStatus) -> Result<String, MemoryError> {
    validate_repo(&status.issue.repo)?;
    let body = IssueStatusBody {
        issue: status.issue.canonical(),
        repo: status.issue.repo.clone(),
        issue_number: status.issue.number,
        best_verdict: status.best_verdict,
        best_commit_sha: status.best_commit_sha.clone(),
        cumulative_attempt_n: status.cumulative_attempt_n,
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let content = serde_json::to_string(&body)?;
    write_current(db, &issue_status_key(&status.issue), &content)
}

/// Read an issue's current-state drawer, if one has been written yet.
pub fn get_issue_status(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<IssueStatus>, MemoryError> {
    let Some(drawer) = super::read_current(db, &issue_status_key(issue))? else {
        return Ok(None);
    };
    let body: IssueStatusBody = serde_json::from_str(&drawer.content)?;
    Ok(Some(IssueStatus {
        issue: IssueRef::new(body.repo, body.issue_number),
        best_verdict: body.best_verdict,
        best_commit_sha: body.best_commit_sha,
        cumulative_attempt_n: body.cumulative_attempt_n,
    }))
}

/// Every drawer this module has written in `ROOM` (any wing), for tests
/// that need to distinguish "many distinct rows" from "one row, repeatedly
/// overwritten" without depending on knowledge of specific ids.
#[cfg(test)]
fn all_lineage_drawers(db: &Database) -> Vec<crate::db::Drawer> {
    db.get_drawers(Some(WING), Some(ROOM), usize::MAX).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(
        issue: &IssueRef,
        attempt_n: u32,
        verdict: AttemptOutcome,
        why_failed: Option<&str>,
    ) -> AttemptRecord {
        AttemptRecord {
            issue: issue.clone(),
            attempt_n,
            approach: format!("tried approach #{attempt_n}"),
            verdict,
            why_failed: why_failed.map(|s| s.to_string()),
            commit_sha: None,
        }
    }

    // ── Required test 1: N failed attempts produce N distinct drawers ──────
    // Regression guard against the `logical_key` hazard: if `record_attempt`
    // were ever refactored to write attempts through `write_current` (the
    // logical_key path) instead of `write_append_only`, every attempt after
    // the first would silently overwrite its predecessor and this test would
    // start failing (three attempts, one surviving drawer).
    #[test]
    fn n_failed_attempts_produce_n_distinct_drawers() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);

        let mut ids = Vec::new();
        for n in 1..=3u32 {
            let record = attempt(
                &issue,
                n,
                AttemptOutcome::Failed,
                Some("same failure every time"),
            );
            ids.push(record_attempt(&db, &record).unwrap().drawer_id);
        }

        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 3, "each attempt must get its own drawer id");

        for id in &ids {
            assert!(
                db.get_drawer(id).unwrap().is_some(),
                "every attempt drawer must still be readable"
            );
        }
        assert_eq!(
            all_lineage_drawers(&db).len(),
            3,
            "three attempts must persist as three rows, not one repeatedly overwritten row"
        );
    }

    // ── Required test: a successful attempt also produces a lineage record ─
    #[test]
    fn a_successful_attempt_also_produces_a_lineage_record() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 42);

        let record = AttemptRecord {
            issue: issue.clone(),
            attempt_n: 1,
            approach: "implemented the fix directly".into(),
            verdict: AttemptOutcome::Success,
            why_failed: None,
            commit_sha: Some("deadbeefcafef00d".into()),
        };
        let recorded = record_attempt(&db, &record).unwrap();

        let drawer = db.get_drawer(&recorded.drawer_id).unwrap().unwrap();
        let body: serde_json::Value = serde_json::from_str(&drawer.content).unwrap();
        assert_eq!(body["verdict"], "success");
        assert_eq!(body["commit_sha"], "deadbeefcafef00d");
        // The shape always carries `commit_sha` and `why_failed` fields (spec
        // line 421) even though only one is populated for a success.
        assert!(body.get("why_failed").is_some());
        assert!(body["why_failed"].is_null());

        let attempts = attempts_for_issue(&db, &issue).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].verdict, AttemptOutcome::Success);
        assert_eq!(attempts[0].commit_sha.as_deref(), Some("deadbeefcafef00d"));
    }

    // ── Required test 2: per-issue status drawer overwrites ────────────────
    #[test]
    fn per_issue_status_drawer_overwrites_rather_than_accumulating() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 99);

        let id_first = upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue.clone(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();
        let id_second = upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue.clone(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: Some("abc123".into()),
                cumulative_attempt_n: 2,
            },
        )
        .unwrap();

        assert_eq!(
            id_first, id_second,
            "logical_key must resolve to the same drawer"
        );
        assert_eq!(
            db.get_drawers(Some(WING), Some(ROOM), usize::MAX)
                .unwrap()
                .len(),
            1,
            "an overwrite must not leave the earlier version as a separate row"
        );

        let current = get_issue_status(&db, &issue).unwrap().unwrap();
        assert_eq!(current.cumulative_attempt_n, 2);
        assert_eq!(current.best_verdict, Some(AttemptOutcome::Success));
        assert_eq!(current.best_commit_sha.as_deref(), Some("abc123"));
    }

    // ── Required test 3: kg traversal returns all attempts on an issue ─────
    #[test]
    fn kg_query_on_an_issue_returns_all_its_attempts() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 7);
        let other_issue = IssueRef::new("ironmem", 8);

        record_attempt(
            &db,
            &attempt(&issue, 1, AttemptOutcome::Failed, Some("approach A failed")),
        )
        .unwrap();
        record_attempt(
            &db,
            &attempt(&issue, 2, AttemptOutcome::Failed, Some("approach B failed")),
        )
        .unwrap();
        record_attempt(&db, &attempt(&issue, 3, AttemptOutcome::Success, None)).unwrap();
        // A distractor on a different issue must not leak into the first
        // issue's traversal.
        record_attempt(
            &db,
            &attempt(&other_issue, 1, AttemptOutcome::Failed, Some("unrelated")),
        )
        .unwrap();

        // Via this module's own traversal helper...
        let attempts = attempts_for_issue(&db, &issue).unwrap();
        assert_eq!(attempts.len(), 3);
        assert_eq!(
            attempts.iter().map(|a| a.attempt_n).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // ...and via the raw KG primitive directly, the same one
        // `mcp__ironmem__kg_query` calls (`KnowledgeGraph::query_entity_current`),
        // to prove the traversal isn't hiding behind this module's own
        // convenience wrapper.
        let kg = KnowledgeGraph::new(&db);
        let entity = kg
            .resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE))
            .unwrap();
        let triples = kg.query_entity_current(&entity.id, 50).unwrap();
        let has_attempt_edges = triples
            .iter()
            .filter(|t| t.predicate == HAS_ATTEMPT_PREDICATE)
            .count();
        assert_eq!(has_attempt_edges, 3);
    }

    #[test]
    fn attempts_for_issue_is_empty_not_an_error_when_none_recorded() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 12345);
        assert_eq!(attempts_for_issue(&db, &issue).unwrap(), Vec::new());
    }

    // ── Required test 4: secret in gate output is scrubbed before it's ever
    // written to the drawer (checked by reading the raw persisted row back,
    // not just the writer's return value). ──────────────────────────────────
    #[test]
    fn gate_output_containing_a_token_is_scrubbed_before_the_drawer_write_persists_it() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 500);
        let secret = "ghp_thisIsAFakeTokenButShapedLikeOne123";
        let why_failed = format!("push rejected: remote said token {secret} was invalid");

        let record = AttemptRecord {
            issue: issue.clone(),
            attempt_n: 1,
            approach: "attempted git push".into(),
            verdict: AttemptOutcome::Failed,
            why_failed: Some(why_failed),
            commit_sha: None,
        };
        let recorded = record_attempt(&db, &record).unwrap();
        assert!(recorded.redacted);

        // The whole point: read the raw row back from the database, not the
        // in-memory value handed back by the writer.
        let drawer = db.get_drawer(&recorded.drawer_id).unwrap().unwrap();
        assert!(
            !drawer.content.contains(secret),
            "the persisted drawer content must not contain the raw secret"
        );
        assert!(drawer.content.contains("[REDACTED]"));
    }

    #[test]
    fn issue_status_key_avoids_reserved_characters_for_cross_repo_issues() {
        let issue = IssueRef::new("ironrace/ironmem", 1);
        let key = issue_status_key(&issue);
        assert_eq!(key, "issue-status:ironrace-ironmem-1");
    }
}
