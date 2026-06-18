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
            pr_url TEXT
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
        }
    );
    assert!(!st.is_terminal());
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
