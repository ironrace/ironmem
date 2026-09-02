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
/// out by name (line 432) on top of that base shape, and `session_claimed`
/// is a third the resume path needs — see its own doc.
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
    /// Whether `session_uuid` has actually been handed to a `claude`
    /// process yet. `--session-id` may only be used once and `--resume`
    /// only works for a session that exists, so "a drawer exists" is *not*
    /// the same question as "the session exists": a run can persist this
    /// record (crash safety demands it be written before the first launch)
    /// and then stop — on the daily budget, or on repeated launch
    /// failures — without ever starting a process. Resuming such a uuid
    /// would fail every time, forever.
    ///
    /// Defaulted on read so records written before this field existed
    /// deserialize as "not claimed", which is the conservative answer: a
    /// spurious `--session-id` on an existing session fails loudly on the
    /// next dispatch, whereas a spurious `--resume` on a session that was
    /// never opened wedges the issue.
    pub session_claimed: bool,
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
    #[serde(default)]
    session_claimed: bool,
}

/// Logical-key prefix shared by every dispatch-state drawer. One constant
/// because [`all_dispatch_states`] enumerates by it: a prefix spelled
/// differently in the two places would silently enumerate nothing, and
/// "nothing is in flight" is a plausible-looking wrong answer rather than a
/// visible failure.
pub(crate) const DISPATCH_STATE_KEY_PREFIX: &str = "dispatch-state:";

fn dispatch_state_key(issue: &IssueRef) -> String {
    format!("{DISPATCH_STATE_KEY_PREFIX}{}", issue.slug())
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
        session_claimed: state.session_claimed,
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
        session_claimed: body.session_claimed,
    }))
}

/// Every in-flight issue's dispatch-state drawer.
///
/// This is the left-hand column of the spec's *Lead crash-safe state* restart
/// table — "what the Lead knew" — which [`super::supervise::reconcile`] joins
/// against the session registry's "who is alive".
///
/// Scoped by `source_file` prefix rather than read out of the room and
/// filtered afterwards: this room also holds every append-only attempt,
/// review and merge record, and a limit applied across all of them
/// newest-first would let ordinary lineage traffic push the in-flight
/// dispatch states out of the window. A reconciliation that cannot see a
/// dispatch state reports its live IC as an orphan — a wrong answer, not an
/// error. See `Database::get_drawers_by_source_prefix`.
///
/// A drawer whose body no longer deserializes is skipped rather than failing
/// the whole reconciliation: one unreadable record must not take every other
/// in-flight issue's supervision offline with it.
pub fn all_dispatch_states(db: &Database, limit: usize) -> Result<Vec<DispatchState>, MemoryError> {
    let prefix = format!(
        "{}{}",
        crate::mcp::tools::LOGICAL_KEY_SOURCE_PREFIX,
        DISPATCH_STATE_KEY_PREFIX
    );
    let drawers = db.get_drawers_by_source_prefix(super::WING, super::ROOM, &prefix, limit)?;
    let mut states = Vec::new();
    for drawer in drawers {
        let Ok(body) = serde_json::from_str::<DispatchStateBody>(&drawer.content) else {
            continue;
        };
        states.push(DispatchState {
            issue: IssueRef::new(body.repo, body.issue),
            worktree_path: body.worktree_path,
            ic_session_name: body.ic_session_name,
            dispatch_class: body.dispatch_class,
            attempt_n: body.attempt_n,
            state: body.state,
            started_at: body.started_at,
            session_uuid: body.session_uuid,
            turn_n: body.turn_n,
            session_claimed: body.session_claimed,
        });
    }
    Ok(states)
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
            session_claimed: true,
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

    #[test]
    fn all_dispatch_states_returns_every_in_flight_issue() {
        let db = Database::open_in_memory().unwrap();
        let a = IssueRef::new("ironrace/ironmem", 283);
        let b = IssueRef::new("ironrace/other", 7);
        upsert_dispatch_state(&db, &sample(&a, 1)).unwrap();
        upsert_dispatch_state(&db, &sample(&b, 1)).unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.iter().any(|s| s.issue == a));
        assert!(states.iter().any(|s| s.issue == b));

        // Cleared issues drop out.
        clear_dispatch_state(&db, &a).unwrap();
        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, b);
    }

    #[test]
    fn all_dispatch_states_ignores_other_kinds_in_the_same_room() {
        // The room also holds issue-status, gate-config, budget and every
        // append-only lineage record. Only dispatch states are in flight.
        let db = Database::open_in_memory().unwrap();
        let issue = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&issue, 1)).unwrap();
        super::super::write_current(&db, "issue-status:ironrace-ironmem-283", "{}").unwrap();
        super::super::write_current(&db, "budget:2026-09-01", "{}").unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, issue);
    }

    #[test]
    fn an_undeserializable_dispatch_state_is_skipped_not_fatal() {
        // One corrupt record must not take every other in-flight issue's
        // supervision offline with it.
        let db = Database::open_in_memory().unwrap();
        let good = IssueRef::new("ironrace/ironmem", 283);
        upsert_dispatch_state(&db, &sample(&good, 1)).unwrap();
        super::super::write_current(
            &db,
            &format!("{DISPATCH_STATE_KEY_PREFIX}corrupt-1"),
            "not json",
        )
        .unwrap();

        let states = all_dispatch_states(&db, 100).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue, good);
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
