//! Per-actor generation lease persistence for `session_handoff` (issue #91).
//!
//! One row per (session_id, agent) in `collab_actor_generations`, created
//! lazily at generation 0. `issue_or_reuse_handoff` sets a pending token +
//! `pending_handoff_generation = generation + 1` without bumping the active
//! `generation`; `claim_handoff_token` advances `generation` to the pending
//! value. See migration 010.

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::Agent;
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorGeneration {
    pub generation: u64,
    pub pending_handoff_token: Option<String>,
    pub pending_handoff_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffIssue {
    pub pending_generation: u64,
    pub token: String,
    pub reused: bool,
}

/// Read the (session, agent) lease row, creating it lazily at generation 0.
pub fn load_or_init_actor_generation(
    conn: &Connection,
    session_id: &str,
    agent: Agent,
) -> Result<ActorGeneration, MemoryError> {
    conn.execute(
        "INSERT OR IGNORE INTO collab_actor_generations (session_id, agent, generation)
         VALUES (?1, ?2, 0)",
        params![session_id, agent.as_str()],
    )?;
    let row = conn
        .query_row(
            "SELECT generation, pending_handoff_token, pending_handoff_generation
             FROM collab_actor_generations WHERE session_id = ?1 AND agent = ?2",
            params![session_id, agent.as_str()],
            |r| {
                let generation: i64 = r.get(0)?;
                let token: Option<String> = r.get(1)?;
                let pending_gen: Option<i64> = r.get(2)?;
                Ok(ActorGeneration {
                    generation: generation.max(0) as u64,
                    pending_handoff_token: token,
                    pending_handoff_generation: pending_gen.map(|g| g.max(0) as u64),
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            MemoryError::NotFound(format!(
                "lease row missing for {session_id}/{}",
                agent.as_str()
            ))
        })?;
    Ok(row)
}

/// Issue a new handoff token (or reuse a pending one). Does NOT bump the active
/// generation — it sets `pending_handoff_generation = generation + 1`. Reuse path
/// returns the same token + pending generation (byte-identical retries before claim).
pub fn issue_or_reuse_handoff(
    conn: &Connection,
    session_id: &str,
    agent: Agent,
) -> Result<HandoffIssue, MemoryError> {
    let current = load_or_init_actor_generation(conn, session_id, agent)?;
    if let (Some(token), Some(pending_generation)) = (
        current.pending_handoff_token.clone(),
        current.pending_handoff_generation,
    ) {
        return Ok(HandoffIssue {
            pending_generation,
            token,
            reused: true,
        });
    }
    let token = Uuid::new_v4().to_string();
    let pending_generation = current.generation + 1;
    conn.execute(
        "UPDATE collab_actor_generations
         SET pending_handoff_token = ?3,
             pending_handoff_generation = ?4,
             pending_handoff_issued_at = datetime('now'),
             pending_handoff_claimed_at = NULL
         WHERE session_id = ?1 AND agent = ?2",
        params![session_id, agent.as_str(), token, pending_generation as i64],
    )?;
    Ok(HandoffIssue {
        pending_generation,
        token,
        reused: false,
    })
}

/// Claim a pending handoff token: advance `generation` to
/// `pending_handoff_generation`, clear pending fields, stamp claimed_at.
/// Returns the new active generation. Errors on mismatch / already-claimed.
pub fn claim_handoff_token(
    conn: &Connection,
    session_id: &str,
    agent: Agent,
    token: &str,
) -> Result<u64, MemoryError> {
    let current = load_or_init_actor_generation(conn, session_id, agent)?;
    let pending_token = current
        .pending_handoff_token
        .ok_or_else(|| MemoryError::Validation("handoff_token already claimed".to_string()))?;
    if pending_token != token {
        return Err(MemoryError::Validation("invalid handoff_token".to_string()));
    }
    let new_generation = current
        .pending_handoff_generation
        .ok_or_else(|| MemoryError::Validation("invalid handoff_token".to_string()))?;
    conn.execute(
        "UPDATE collab_actor_generations
         SET generation = ?3,
             pending_handoff_token = NULL,
             pending_handoff_generation = NULL,
             pending_handoff_claimed_at = datetime('now')
         WHERE session_id = ?1 AND agent = ?2",
        params![session_id, agent.as_str(), new_generation as i64],
    )?;
    Ok(new_generation)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::collab::queue::create_session;

    const BASE_SQL: &str = include_str!("../../migrations/001_init.sql");
    const FTS_SQL: &str = include_str!("../../migrations/002_fts.sql");
    const COLLAB_SQL: &str = include_str!("../../migrations/003_collab.sql");
    const COLLAB_V1_SQL: &str = include_str!("../../migrations/004_collab_planning_v1.sql");
    const COLLAB_V2_SQL: &str = include_str!("../../migrations/005_collab_v2.sql");
    const COLLAB_IMPLEMENTER_SQL: &str =
        include_str!("../../migrations/006_collab_implementer.sql");
    const DROP_CURRENT_TASK_INDEX_SQL: &str =
        include_str!("../../migrations/007_drop_current_task_index.sql");
    const COLLAB_PLAN_DRAWERS_SQL: &str =
        include_str!("../../migrations/009_collab_plan_drawers.sql");
    const COLLAB_GENERATION_LEASE_SQL: &str =
        include_str!("../../migrations/010_collab_generation_lease.sql");

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(BASE_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(COLLAB_SQL).unwrap();
        conn.execute_batch(COLLAB_V1_SQL).unwrap();
        conn.execute_batch(COLLAB_V2_SQL).unwrap();
        conn.execute_batch(COLLAB_IMPLEMENTER_SQL).unwrap();
        conn.execute_batch(DROP_CURRENT_TASK_INDEX_SQL).unwrap();
        conn.execute_batch(COLLAB_PLAN_DRAWERS_SQL).unwrap();
        conn.execute_batch(COLLAB_GENERATION_LEASE_SQL).unwrap();
        conn
    }

    fn seed_session(conn: &Connection, id: &str) {
        create_session(conn, id, "/repo", "main", Some("t"), Agent::Claude).unwrap();
    }

    #[test]
    fn issue_sets_pending_without_bumping_active_generation() {
        let conn = open();
        seed_session(&conn, "s1");
        let issued = issue_or_reuse_handoff(&conn, "s1", Agent::Claude).unwrap();
        assert_eq!(issued.pending_generation, 1);
        assert!(!issued.reused);
        let g = load_or_init_actor_generation(&conn, "s1", Agent::Claude).unwrap();
        assert_eq!(g.generation, 0, "active generation must not bump on issue");
        assert_eq!(g.pending_handoff_generation, Some(1));
        assert_eq!(
            g.pending_handoff_token.as_deref(),
            Some(issued.token.as_str())
        );
    }

    #[test]
    fn reissue_before_claim_is_byte_identical() {
        let conn = open();
        seed_session(&conn, "s1");
        let a = issue_or_reuse_handoff(&conn, "s1", Agent::Claude).unwrap();
        let b = issue_or_reuse_handoff(&conn, "s1", Agent::Claude).unwrap();
        assert_eq!(a.token, b.token);
        assert_eq!(a.pending_generation, b.pending_generation);
        assert!(b.reused);
    }

    #[test]
    fn claim_advances_generation_and_clears_pending() {
        let conn = open();
        seed_session(&conn, "s1");
        let issued = issue_or_reuse_handoff(&conn, "s1", Agent::Claude).unwrap();
        let new_gen = claim_handoff_token(&conn, "s1", Agent::Claude, &issued.token).unwrap();
        assert_eq!(new_gen, 1);
        let g = load_or_init_actor_generation(&conn, "s1", Agent::Claude).unwrap();
        assert_eq!(g.generation, 1);
        assert_eq!(g.pending_handoff_token, None);
        assert_eq!(g.pending_handoff_generation, None);
    }

    #[test]
    fn claim_rejects_wrong_token_and_double_claim() {
        let conn = open();
        seed_session(&conn, "s1");
        let issued = issue_or_reuse_handoff(&conn, "s1", Agent::Claude).unwrap();
        assert!(claim_handoff_token(&conn, "s1", Agent::Claude, "nope").is_err());
        claim_handoff_token(&conn, "s1", Agent::Claude, &issued.token).unwrap();
        assert!(claim_handoff_token(&conn, "s1", Agent::Claude, &issued.token).is_err());
    }

    #[test]
    fn check_constraint_rejects_bad_agent() {
        let conn = open();
        seed_session(&conn, "s1");
        let res = conn.execute(
            "INSERT INTO collab_actor_generations (session_id, agent, generation)
             VALUES ('s1', 'gemini', 0)",
            [],
        );
        assert!(res.is_err());
    }
}
