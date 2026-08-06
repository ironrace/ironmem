use abeval::collab_db::{read_session_state, SessionState};
use std::path::Path;

/// Build a minimal collab_sessions table + one row in a temp sqlite file,
/// then read it back through the read-only reader.
fn seed_db(
    dir: &Path,
    session_id: &str,
    phase: &str,
    owner: &str,
    plan_review_round: i64,
    global_review_round: i64,
) -> std::path::PathBuf {
    let db = dir.join("collab.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE collab_sessions (
            id TEXT PRIMARY KEY,
            phase TEXT NOT NULL,
            current_owner TEXT NOT NULL,
            implementer TEXT NOT NULL DEFAULT 'claude',
            review_round INTEGER NOT NULL DEFAULT 0,
            task_review_round INTEGER NOT NULL DEFAULT 0,
            global_review_round INTEGER NOT NULL DEFAULT 0,
            last_head_sha TEXT,
            pr_url TEXT,
            -- migration 015 recovery columns (nullable; NULL = no failure in flight)
            pending_failure TEXT,
            recovery_phase TEXT,
            recovery_owner TEXT,
            -- migration 019: which agent LEADS the session
            pilot TEXT NOT NULL DEFAULT 'claude' CHECK (pilot IN ('claude', 'codex'))
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO collab_sessions
            (id, phase, current_owner, implementer, review_round, task_review_round, global_review_round, last_head_sha, pr_url)
         VALUES (?1, ?2, ?3, 'claude', ?4, 0, ?5, 'abc123', NULL)",
        rusqlite::params![session_id, phase, owner, plan_review_round, global_review_round],
    )
    .unwrap();
    db
}

#[test]
fn reads_existing_session_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(
        dir.path(),
        "sess-1",
        "CodeReviewFixGlobalPending",
        "codex",
        1,
        2,
    );
    let st = read_session_state(&db, "sess-1").unwrap();
    // Spelled out field by field on purpose — NOT `SessionState::fixture`. This
    // is the reader's exhaustive contract: a new column must break this literal
    // so whoever adds it has to state what the reader should return for it.
    assert_eq!(
        st,
        SessionState {
            phase: "CodeReviewFixGlobalPending".into(),
            current_owner: "codex".into(),
            implementer: "claude".into(),
            pr_url: None,
            review_round: 1,
            global_review_round: 2,
            task_review_round: 0,
            last_head_sha: Some("abc123".into()),
            pending_failure: None,
            recovery_phase: None,
            recovery_owner: None,
            pilot: "claude".into(),
        }
    );
    assert!(!st.is_terminal());
}

/// The driver refuses any pilot but `claude`, so the poll has to surface the
/// column's real value rather than assuming the default.
#[test]
fn reads_a_non_default_pilot() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(
        dir.path(),
        "sess-pilot",
        "PlanSynthesisPending",
        "codex",
        0,
        0,
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE collab_sessions SET pilot = 'codex' WHERE id = 'sess-pilot'",
        [],
    )
    .unwrap();
    assert_eq!(
        read_session_state(&db, "sess-pilot").unwrap().pilot,
        "codex"
    );
}

/// The real pre-019 shape has no `pilot` column at all — the COALESCE cannot
/// save that case, `prepare` fails on it. The error has to name migration 019
/// so an operator sees an un-migrated DB rather than a bare SQLite message.
#[test]
fn pre_019_db_without_a_pilot_column_names_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pre019.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE collab_sessions (
            id TEXT PRIMARY KEY,
            phase TEXT NOT NULL,
            current_owner TEXT NOT NULL,
            implementer TEXT NOT NULL DEFAULT 'claude',
            review_round INTEGER NOT NULL DEFAULT 0,
            task_review_round INTEGER NOT NULL DEFAULT 0,
            global_review_round INTEGER NOT NULL DEFAULT 0,
            last_head_sha TEXT,
            pr_url TEXT,
            pending_failure TEXT,
            recovery_phase TEXT,
            recovery_owner TEXT
        );
        INSERT INTO collab_sessions (id, phase, current_owner)
            VALUES ('sess-old', 'PlanParallelDrafts', 'claude');",
    )
    .unwrap();
    let err = read_session_state(&db, "sess-old").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("migration 019"),
        "error points at the un-migrated DB: {msg}"
    );
}

/// A row that omits `pilot` entirely takes the migration's `'claude'` default,
/// i.e. the pre-019 lead, so an un-flagged session drives unchanged.
#[test]
fn omitted_pilot_reads_as_claude() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(
        dir.path(),
        "sess-default",
        "PlanParallelDrafts",
        "claude",
        0,
        0,
    );
    assert_eq!(
        read_session_state(&db, "sess-default").unwrap().pilot,
        "claude"
    );
}

/// A session with a recoverable tooling failure in flight must surface all
/// three recovery columns to the driver — the dispatcher cannot tell a
/// recovery-flipped `(phase, owner)` pair from a genuine anomaly without them.
#[test]
fn reads_recovery_state_when_a_tooling_failure_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(
        dir.path(),
        "sess-rec",
        "CodeReviewFixGlobalPending",
        "claude",
        0,
        1,
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE collab_sessions SET pending_failure = ?1, recovery_phase = ?2, \
         recovery_owner = ?3 WHERE id = 'sess-rec'",
        rusqlite::params![
            "git_push_failed: remote rejected",
            "CodeReviewFixGlobalPending",
            "claude"
        ],
    )
    .unwrap();
    let st = read_session_state(&db, "sess-rec").unwrap();
    assert_eq!(
        st.pending_failure.as_deref(),
        Some("git_push_failed: remote rejected")
    );
    assert_eq!(
        st.recovery_phase.as_deref(),
        Some("CodeReviewFixGlobalPending")
    );
    assert_eq!(st.recovery_owner.as_deref(), Some("claude"));
}

#[test]
fn missing_session_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(dir.path(), "sess-1", "PlanParallelDrafts", "claude", 0, 0);
    let err = read_session_state(&db, "nope").unwrap_err();
    assert!(
        err.to_string().contains("nope"),
        "error names the missing session: {err}"
    );
}

#[test]
fn terminal_phases_detected() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed_db(dir.path(), "done", "CodingComplete", "claude", 1, 1);
    assert!(read_session_state(&db, "done").unwrap().is_terminal());
}
