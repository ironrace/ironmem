//! Dispatch-state drawer — the Lead's crash-safe memory (spec's *Lead
//! crash-safe state* section).
//!
//! `logical_key` per in-flight issue, so a restarted Lead can reconcile this
//! set against `ListAgents`/`claude agents --json` (present → adopt, present
//! but dead → restart from checkpoint, absent but alive → flag as an
//! orphan). Rung 1 only stores and retrieves this record; the reconciliation
//! logic itself is rung 7's (*supervision + crash safety*).

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{read_current, validate_repo, write_current, IssueRef};

/// One in-flight IC dispatch. Field names and the literal-shape fields
/// (`issue`, `repo`, `worktree_path`, `ic_session_name`, `dispatch_class`,
/// `attempt_n`, `state`, `started_at`) mirror the spec's storage table
/// exactly; `session_uuid` and `turn_n` are the two additions the spec calls
/// out by name (line 432) on top of that base shape.
///
/// Unlike [`super::lineage::AttemptRecord`], the spec's shape here lists
/// `issue` and `repo` as *separate* sibling fields — so `issue` in the
/// persisted body is the bare issue number, not the `repo#number` canonical
/// string `AttemptRecord` uses. [`IssueRef`] still carries both; only the
/// wire shape differs, to track each kind's literal spec shape faithfully.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchState {
    pub issue: IssueRef,
    pub worktree_path: String,
    pub ic_session_name: String,
    pub dispatch_class: String,
    pub attempt_n: u32,
    pub state: String,
    pub started_at: String,
    pub session_uuid: String,
    pub turn_n: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchStateBody {
    issue: u64,
    repo: String,
    worktree_path: String,
    ic_session_name: String,
    dispatch_class: String,
    attempt_n: u32,
    state: String,
    started_at: String,
    session_uuid: String,
    turn_n: u32,
}

fn dispatch_state_key(issue: &IssueRef) -> String {
    format!("dispatch-state:{}", issue.slug())
}

/// Write (overwrite) the dispatch-state drawer for `state.issue`. Called at
/// dispatch time and at every state transition, per the spec.
pub fn upsert_dispatch_state(db: &Database, state: &DispatchState) -> Result<String, MemoryError> {
    validate_repo(&state.issue.repo)?;
    let body = DispatchStateBody {
        issue: state.issue.number,
        repo: state.issue.repo.clone(),
        worktree_path: state.worktree_path.clone(),
        ic_session_name: state.ic_session_name.clone(),
        dispatch_class: state.dispatch_class.clone(),
        attempt_n: state.attempt_n,
        state: state.state.clone(),
        started_at: state.started_at.clone(),
        session_uuid: state.session_uuid.clone(),
        turn_n: state.turn_n,
    };
    let content = serde_json::to_string(&body)?;
    write_current(db, &dispatch_state_key(&state.issue), &content)
}

/// Read the dispatch-state drawer for an issue, if one is currently in
/// flight.
pub fn get_dispatch_state(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<DispatchState>, MemoryError> {
    let Some(drawer) = read_current(db, &dispatch_state_key(issue))? else {
        return Ok(None);
    };
    let body: DispatchStateBody = serde_json::from_str(&drawer.content)?;
    Ok(Some(DispatchState {
        issue: IssueRef::new(body.repo, body.issue),
        worktree_path: body.worktree_path,
        ic_session_name: body.ic_session_name,
        dispatch_class: body.dispatch_class,
        attempt_n: body.attempt_n,
        state: body.state,
        started_at: body.started_at,
        session_uuid: body.session_uuid,
        turn_n: body.turn_n,
    }))
}

/// Clear an issue's dispatch-state drawer once it's no longer in flight
/// (terminal success, exhaustion, or hand-off to a human). Returns `true` if
/// a drawer was actually deleted.
///
/// Wrapped in a transaction with a `wal_log_tx` entry, like every other
/// mutating call in this module ([`upsert_dispatch_state`] via
/// [`write_current`], [`super::lineage::record_attempt`]): without it, a
/// dispatch's crash-safe record could vanish with no audit trail explaining
/// when or why, which is exactly the kind of gap rung 7's reconciliation
/// logic would need to debug around.
pub fn clear_dispatch_state(db: &Database, issue: &IssueRef) -> Result<bool, MemoryError> {
    let id = super::logical_drawer_id(&dispatch_state_key(issue));
    db.with_transaction(|tx| {
        let deleted = Database::delete_drawer_tx(tx, &id)?;
        Database::wal_log_tx(
            tx,
            "autopilot_clear_dispatch_state",
            &json!({
                "drawer_id": &id,
                "issue": issue.canonical(),
                "deleted": deleted,
            }),
            None,
        )?;
        Ok(deleted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(issue: &IssueRef, turn_n: u32) -> DispatchState {
        DispatchState {
            issue: issue.clone(),
            worktree_path: "/tmp/worktrees/ironmem-283".into(),
            ic_session_name: "ic-ironmem-283".into(),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "in_progress".into(),
            started_at: "2026-08-25T00:00:00Z".into(),
            session_uuid: "11111111-1111-1111-1111-111111111111".into(),
            turn_n,
        }
    }

    #[test]
    fn dispatch_state_round_trips_and_overwrites() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);

        let id_first = upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        let id_second = upsert_dispatch_state(&db, &sample(&issue, 2)).unwrap();
        assert_eq!(
            id_first, id_second,
            "same issue must resolve to the same dispatch-state drawer"
        );

        let current = get_dispatch_state(&db, &issue).unwrap().unwrap();
        assert_eq!(current.turn_n, 2);
        assert_eq!(current.session_uuid, "11111111-1111-1111-1111-111111111111");
        assert_eq!(current.issue, issue);
    }

    #[test]
    fn missing_dispatch_state_is_none_not_an_error() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 9999);
        assert_eq!(get_dispatch_state(&db, &issue).unwrap(), None);
    }

    #[test]
    fn clear_dispatch_state_removes_it() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();

        assert!(clear_dispatch_state(&db, &issue).unwrap());
        assert_eq!(get_dispatch_state(&db, &issue).unwrap(), None);
        // Idempotent: clearing an already-cleared issue is not an error.
        assert!(!clear_dispatch_state(&db, &issue).unwrap());
    }

    // ── Regression: clearing must leave the same wal-log audit trail every
    // other mutation in this module leaves. ─────────────────────────────────
    #[test]
    fn clear_dispatch_state_writes_a_wal_log_entry() {
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();

        assert!(clear_dispatch_state(&db, &issue).unwrap());

        let count: i64 = db
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM wal_log WHERE operation = ?1",
                    rusqlite::params!["autopilot_clear_dispatch_state"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "clearing a dispatch-state drawer must log exactly one wal entry"
        );

        // Clearing an already-cleared issue still logs (deleted: false), so
        // the audit trail records the no-op attempt too.
        assert!(!clear_dispatch_state(&db, &issue).unwrap());
        let count_after_noop: i64 = db
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM wal_log WHERE operation = ?1",
                    rusqlite::params!["autopilot_clear_dispatch_state"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count_after_noop, 2);
    }
}
