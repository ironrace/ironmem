use serde_json::{json, Value};

use crate::collab::queue::Capability;
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

use super::shared::{require_agent, require_str, MAX_COLLAB_CAP_FIELD_CHARS};

pub(super) fn handle_collab_register_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let capabilities = args
        .get("capabilities")
        .and_then(|value| value.as_array())
        .ok_or_else(|| MemoryError::Validation("capabilities must be an array".to_string()))?;

    let mut parsed = Vec::new();
    for capability in capabilities {
        let name = capability
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| MemoryError::Validation("capability name is required".to_string()))?;
        let name = sanitize::sanitize_content(name, MAX_COLLAB_CAP_FIELD_CHARS)?.to_string();
        let description = capability
            .get("description")
            .and_then(|value| value.as_str())
            .map(|value| sanitize::sanitize_content(value, MAX_COLLAB_CAP_FIELD_CHARS))
            .transpose()?
            .map(ToString::to_string);
        parsed.push(Capability {
            agent: agent.to_string(),
            name,
            description,
        });
    }

    let count = parsed.len();
    let claim = app.db.with_transaction(|tx| {
        let claim = super::handoff::ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            super::handoff::opt_handoff_token(args).as_deref(),
        )?;
        crate::collab::queue::ensure_active(tx, session_id)?;
        crate::collab::queue::register_caps(tx, session_id, agent.as_str(), &parsed)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_register_caps",
            &json!({
                "session_id": session_id,
                "agent": agent.as_str(),
                "count": count,
            }),
            Some(&json!({ "success": true, "count": count })),
        )?;
        Ok(claim)
    })?;
    claim.publish(app);

    Ok(json!({ "success": true, "count": count }))
}

pub(super) fn handle_collab_get_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = args
        .get("agent")
        .and_then(|value| value.as_str())
        .map(require_agent)
        .transpose()?;
    let capabilities = app
        .db
        .collab_get_caps(session_id, agent.as_ref().map(|a| a.as_str()))?
        .into_iter()
        .map(|capability| {
            json!({
                "agent": capability.agent,
                "name": capability.name,
                "description": capability.description,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "capabilities": capabilities }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::{Agent, CollabRoles};
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

    /// A **production** closure — the one inside
    /// [`handle_collab_register_caps`] — must survive a replayed
    /// `SQLITE_BUSY_SNAPSHOT` attempt and commit its effects exactly once.
    ///
    /// The schema-level tests in `db::schema` prove the retry *wrapper*
    /// behaves; they run a synthetic closure the test itself wrote. This test
    /// proves a real caller of that wrapper is actually safe under it, driving
    /// the real MCP tool handler end to end.
    ///
    /// `handle_collab_register_caps` is representative of the collab write
    /// paths that stayed on retrying `with_transaction`: its closure has the
    /// exact read-then-write shape that a busy snapshot can strike — it reads
    /// (`ensure_actor_generation_current`'s generation lookup, then
    /// `ensure_active`'s session lookup) before it writes — and its effects are
    /// directly countable, including an append-only `wal_log` row that would
    /// show up **twice** if the retry ever double-committed.
    ///
    /// Contention is injected by `Database::arm_busy_snapshot_once` (the Task
    /// 5/6 recipe, driven from one thread with no sleeps): a contending
    /// connection commits between the closure's read and its first write, so
    /// attempt 1 fails the read→write upgrade with extended code 517 and
    /// attempt 2 — the replay — commits.
    #[test]
    fn collab_register_caps_commits_exactly_once_under_busy_snapshot_replay() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let app = test_app_with_db_path(db_path.clone(), dir.path());
        let session_id = "caps-exactly-once";

        // Seed the session row the capability FK requires. This runs *before*
        // arming, so it is not the transaction the fixture interferes with.
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    session_id,
                    "/repo",
                    "main",
                    Some("task"),
                    CollabRoles {
                        pilot: Agent::Claude,
                        implementer: Agent::Claude,
                    },
                )
            })
            .unwrap();

        let probe = app.db.arm_busy_snapshot_once(&db_path).unwrap();

        let response = handle_collab_register_caps(
            &app,
            &json!({
                "session_id": session_id,
                "agent": "claude",
                "capabilities": [{ "name": "rust", "description": "writes rust" }],
            }),
        )
        .expect("the replayed attempt must succeed, not surface the busy snapshot to the caller");
        assert_eq!(response, json!({ "success": true, "count": 1 }));

        // The fixture must actually have fired, and the production call must
        // actually have been replayed — otherwise the counts below would pass
        // vacuously against a call that never hit contention at all.
        assert_eq!(
            probe.contentions_injected(),
            1,
            "the contending commit never fired — this test proved nothing"
        );
        assert_eq!(
            probe.transactions_begun(),
            2,
            "expected one busy-snapshot failure plus one replay; \
             1 means the closure never hit contention"
        );

        // Exactly-once, read back over a fresh connection so only *committed*
        // state is observed.
        let verify = rusqlite::Connection::open(&db_path).unwrap();
        let caps: i64 = verify
            .query_row(
                "SELECT COUNT(*) FROM agent_capabilities
                 WHERE session_id = ?1 AND agent = 'claude' AND capability = 'rust'",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(caps, 1, "the capability must be registered exactly once");

        // `wal_log` is append-only with no upsert, so a double-committed
        // replay would leave 2 rows here where the upserted capability above
        // would still read as 1. This is the assertion that would catch it.
        let logged: i64 = verify
            .query_row(
                "SELECT COUNT(*) FROM wal_log WHERE operation = 'collab_register_caps'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            logged, 1,
            "the rolled-back attempt must leave no audit row behind"
        );
    }
}
