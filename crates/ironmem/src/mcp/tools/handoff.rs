//! `session_handoff` MCP tool + the generation-lease guard (issue #91).

use serde_json::Value;

use crate::collab::{claim_handoff_token, load_or_init_actor_generation, Agent};
use crate::error::MemoryError;
use crate::mcp::app::App;

/// Validate (and on first-touch/claim, bind) this process's generation for
/// (session, agent). Call before any actor-bearing mutating/binding collab op.
/// Must run inside the caller's transaction so a claim is atomic with the op.
pub(super) fn ensure_actor_generation_current(
    app: &App,
    conn: &rusqlite::Connection,
    session_id: &str,
    agent: Agent,
    maybe_token: Option<&str>,
) -> Result<(), MemoryError> {
    if let Some(token) = maybe_token {
        let claimed = claim_handoff_token(conn, session_id, agent, token)?;
        app.set_cached_generation(session_id, agent, claimed);
        return Ok(());
    }
    let db_active = load_or_init_actor_generation(conn, session_id, agent)?.generation;
    if let Some(cached) = app.cached_generation(session_id, agent) {
        if cached == db_active {
            return Ok(());
        }
        return Err(MemoryError::Validation(format!(
            "stale collab generation for {}: local={cached} current={db_active}; \
             obtain a session_handoff token in a fresh process",
            agent.as_str()
        )));
    }
    if db_active == 0 {
        app.set_cached_generation(session_id, agent, 0);
        return Ok(());
    }
    Err(MemoryError::Validation(format!(
        "this session has been handed off (generation {db_active}); \
         present a session_handoff token to claim it"
    )))
}

/// Read an optional non-empty `handoff_token` string arg.
pub(super) fn opt_handoff_token(args: &Value) -> Option<String> {
    args.get("handoff_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::{issue_or_reuse_handoff, Agent};
    use std::sync::Arc;

    fn test_app_with_db_path(
        db_path: std::path::PathBuf,
        root: &std::path::Path,
    ) -> Arc<crate::mcp::app::App> {
        use crate::config::{Config, EmbedMode, McpAccessMode};
        let config = Config {
            db_path,
            model_dir: root.join("model"),
            model_dir_explicit: true,
            state_dir: root.join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(crate::mcp::app::App::new(config).unwrap())
    }

    /// Gen-0 path: a fresh session with no issued handoff lets a process bind at
    /// generation 0 without any token.
    #[test]
    fn gen0_fresh_session_binds_without_token() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app_with_db_path(dir.path().join("mem.sqlite3"), dir.path());
        let session_id = "test-session-gen0";

        // Seed the session row (needed for the FK constraint in the generation table).
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    session_id,
                    "/repo",
                    "main",
                    Some("t"),
                    Agent::Claude,
                )
            })
            .unwrap();

        // First call: no cached gen, DB gen = 0, must succeed and cache 0.
        app.db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&app, tx, session_id, Agent::Claude, None)
            })
            .unwrap();

        assert_eq!(app.cached_generation(session_id, Agent::Claude), Some(0));
    }

    /// Stale predecessor: after a handoff is claimed (generation advances to 1),
    /// a process still cached at 0 must be rejected with "stale collab generation".
    #[test]
    fn stale_predecessor_rejected_after_claim() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        // Predecessor app – gets bound at gen 0.
        let pred = test_app_with_db_path(db_path.clone(), dir.path());
        // Successor app – shares the same DB file.
        let succ = test_app_with_db_path(db_path, dir.path());

        let session_id = "test-stale-pred";

        // Seed session in predecessor.
        pred.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    session_id,
                    "/repo",
                    "main",
                    Some("t"),
                    Agent::Claude,
                )
            })
            .unwrap();

        // Predecessor binds at generation 0.
        pred.db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&pred, tx, session_id, Agent::Claude, None)
            })
            .unwrap();
        assert_eq!(
            pred.cached_generation(session_id, Agent::Claude),
            Some(0),
            "predecessor must be cached at gen 0"
        );

        // Issue a handoff token (via predecessor's DB connection).
        let token = pred
            .db
            .with_transaction(|tx| issue_or_reuse_handoff(tx, session_id, Agent::Claude))
            .unwrap()
            .token;

        // Successor claims the handoff token — advances DB generation to 1.
        succ.db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&succ, tx, session_id, Agent::Claude, Some(&token))
            })
            .unwrap();
        assert_eq!(
            succ.cached_generation(session_id, Agent::Claude),
            Some(1),
            "successor must be cached at gen 1 after claim"
        );

        // Predecessor tries to act again — cached gen 0, DB gen 1 → stale error.
        let err = pred
            .db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&pred, tx, session_id, Agent::Claude, None)
            })
            .unwrap_err();

        assert!(
            err.to_string().contains("stale collab generation"),
            "expected stale collab generation error, got: {err}"
        );
    }
}
