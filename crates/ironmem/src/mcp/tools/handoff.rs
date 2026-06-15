//! `session_handoff` MCP tool + the generation-lease guard (issue #91).
//!
//! `ensure_actor_generation_current` validates (and on first-touch/claim,
//! binds) this process's generation for (session, agent). Call before any
//! actor-bearing mutating/binding collab op; must run inside the caller's
//! transaction so a claim is atomic with the op.
//!
//! `handle_session_handoff` issues (or byte-identically reuses) a one-time
//! handoff token and renders a deterministic, model-free session handoff block
//! for an unplanned successor. The token is returned top-level in the JSON
//! response — NOT embedded inside the fenced block.

use std::fmt::Write as _;

use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use crate::collab::queue::SessionRecord;
use crate::collab::{claim_handoff_token, load_or_init_actor_generation, Agent};
use crate::error::MemoryError;
use crate::mcp::app::App;

use super::shared::{require_agent, require_str};

// ── Checkpoint constants ─────────────────────────────────────────────────────

const HANDOFF_FENCE: &str = "ironrace-session-handoff";
const CHECKPOINT_WING: &str = "ironrace-memory";
const CHECKPOINT_ROOM: &str = "collab-checkpoints";

// ── Generation-lease guard ───────────────────────────────────────────────────

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

// ── Checkpoint reader ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Checkpoint {
    pub status: Option<String>,
    pub task_id: Option<String>,
    pub completed_task_ids: Option<String>,
    pub next_task_id: Option<String>,
    pub gates: Option<String>,
}

fn opt(s: Option<&str>) -> &str {
    match s {
        Some(v) if !v.is_empty() => v,
        _ => "\u{2014}", // em dash
    }
}

/// Newest collab checkpoint drawer for this session, parsed from the compact
/// KV format. Deterministic SQL (exact wing/room + content match, newest by
/// rowid) — never semantic search.
pub(super) fn latest_checkpoint(
    db: &crate::db::schema::Database,
    session_id: &str,
) -> Result<Option<Checkpoint>, MemoryError> {
    db.with_connection(|conn| {
        let needle = format!("session_id: {session_id}");
        let content: Option<String> = conn
            .query_row(
                "SELECT content FROM drawers
                 WHERE wing = ?1 AND room = ?2 AND content LIKE '%' || ?3 || '%'
                 ORDER BY rowid DESC LIMIT 1",
                rusqlite::params![CHECKPOINT_WING, CHECKPOINT_ROOM, needle],
                |r| r.get(0),
            )
            .optional()?;
        Ok(content.map(|c| parse_checkpoint(&c)))
    })
}

fn parse_checkpoint(content: &str) -> Checkpoint {
    let mut cp = Checkpoint::default();
    for line in content.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "status" => cp.status = Some(v),
            "task_id" => cp.task_id = Some(v),
            "completed_task_ids" => cp.completed_task_ids = Some(v),
            "next_task_id" => cp.next_task_id = Some(v),
            "gates" => cp.gates = Some(v),
            _ => {}
        }
    }
    cp
}

// ── Handoff block renderer ───────────────────────────────────────────────────

/// Pure deterministic render of session state + checkpoint. No timestamps,
/// no clock, no randomness. `pending_generation` is the to-be-claimed value;
/// `agent` is the actor being handed off to.
pub(super) fn compose_handoff_block(
    record: &SessionRecord,
    agent: Agent,
    pending_generation: u64,
    checkpoint: Option<Checkpoint>,
) -> String {
    let s = &record.session;
    let cp = checkpoint.unwrap_or_default();
    let cp_present = cp != Checkpoint::default();
    let mut out = String::new();
    let _ = writeln!(out, "```{HANDOFF_FENCE}");
    let _ = writeln!(out, "session_id: {}", s.id);
    let _ = writeln!(out, "phase: {}", s.phase);
    let _ = writeln!(out, "current_owner: {}", s.current_owner.as_str());
    let _ = writeln!(out, "implementer: {}", s.implementer.as_str());
    let _ = writeln!(out, "repo_path: {}", record.repo_path);
    let _ = writeln!(out, "branch: {}", record.branch);
    let _ = writeln!(out, "base_sha: {}", opt(s.base_sha.as_deref()));
    let _ = writeln!(out, "last_head_sha: {}", opt(s.last_head_sha.as_deref()));
    let _ = writeln!(
        out,
        "plan.canonical.drawer_id: {}",
        opt(s.canonical_plan_drawer_id.as_deref())
    );
    let _ = writeln!(
        out,
        "plan.canonical.hash: {}",
        opt(s.canonical_plan_hash.as_deref())
    );
    let _ = writeln!(
        out,
        "plan.final.drawer_id: {}",
        opt(s.final_plan_drawer_id.as_deref())
    );
    let _ = writeln!(
        out,
        "plan.final.hash: {}",
        opt(s.final_plan_hash.as_deref())
    );
    let _ = writeln!(out, "task_list.present: {}", s.task_list.is_some());
    let _ = writeln!(
        out,
        "tasks_count: {}",
        s.tasks_count()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "\u{2014}".into())
    );
    let _ = writeln!(out, "review_round: {}", s.review_round);
    let _ = writeln!(out, "task_review_round: {}", s.task_review_round);
    let _ = writeln!(out, "global_review_round: {}", s.global_review_round);
    let _ = writeln!(out, "coding_failure: {}", opt(s.coding_failure.as_deref()));
    let _ = writeln!(out, "pr_url: {}", opt(s.pr_url.as_deref()));
    let _ = writeln!(out, "expected_next_event: {}", s.phase.expected_event());
    let _ = writeln!(
        out,
        "checkpoint: {}",
        if cp_present { "present" } else { "none" }
    );
    let _ = writeln!(out, "checkpoint.status: {}", opt(cp.status.as_deref()));
    let _ = writeln!(out, "checkpoint.task_id: {}", opt(cp.task_id.as_deref()));
    let _ = writeln!(
        out,
        "checkpoint.completed_task_ids: {}",
        opt(cp.completed_task_ids.as_deref())
    );
    let _ = writeln!(
        out,
        "checkpoint.next_task_id: {}",
        opt(cp.next_task_id.as_deref())
    );
    let _ = writeln!(
        out,
        "gates: {}",
        cp.gates
            .as_deref()
            .filter(|g| !g.is_empty())
            .unwrap_or("not_recorded")
    );
    let _ = writeln!(out, "handoff.agent: {}", agent.as_str());
    let _ = writeln!(out, "handoff.generation: {pending_generation}");
    out.push_str("```");
    out
}

// ── Tool handler ─────────────────────────────────────────────────────────────

pub(super) fn handle_session_handoff(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    let record = app.db.collab_load_session_record(session_id)?;
    if record.ended_at.is_some() {
        return Err(MemoryError::Validation(format!(
            "session {session_id} has ended; cannot issue a handoff"
        )));
    }

    // Resurrection guard + issue, atomic in one transaction.
    let issued = app.db.with_transaction(|tx| {
        ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            opt_handoff_token(args).as_deref(),
        )?;
        crate::collab::issue_or_reuse_handoff(tx, session_id, agent)
    })?;

    let checkpoint = latest_checkpoint(&app.db, session_id)?;
    let block = compose_handoff_block(&record, agent, issued.pending_generation, checkpoint);

    Ok(json!({
        "session_id": session_id,
        "agent": agent.as_str(),
        "generation": issued.pending_generation,
        "handoff_token": issued.token,
        "handoff_block": block,
    }))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::queue::{create_session, SessionRecord};
    use crate::collab::{issue_or_reuse_handoff, Agent, Phase};
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

    fn sample_record(phase: Phase) -> SessionRecord {
        use crate::collab::CollabSession;
        let mut s = CollabSession::new("test-sid-sample");
        s.phase = phase;
        SessionRecord {
            session: s,
            repo_path: "/r".into(),
            branch: "b".into(),
            task: None,
            ended_at: None,
            created_at: "".into(),
            updated_at: "".into(),
        }
    }

    #[test]
    fn compose_block_is_deterministic_and_has_no_timestamps() {
        let r = sample_record(Phase::CodeImplementPending);
        let a = compose_handoff_block(&r, Agent::Claude, 1, None);
        let b = compose_handoff_block(&r, Agent::Claude, 1, None);
        assert_eq!(a, b);
        assert!(a.starts_with("```ironrace-session-handoff\n"));
        assert!(a.trim_end().ends_with("```"));
        assert!(!a.contains("created_at") && !a.contains("updated_at") && !a.contains("ended_at"));
        assert!(a.contains("phase: CodeImplementPending"));
        assert!(a.contains("checkpoint: none"));
        assert!(a.contains("gates: not_recorded"));
        assert!(a.contains("handoff.agent: claude"));
        assert!(a.contains("handoff.generation: 1"));
    }

    fn test_handoff_app() -> Arc<crate::mcp::app::App> {
        let dir = tempfile::tempdir().unwrap();
        // Keep dir alive by leaking it — test is short-lived.
        let path = dir.path().join("mem.sqlite3");
        let root = dir.path().to_path_buf();
        // Leak tempdir so the path stays valid for the app lifetime.
        std::mem::forget(dir);
        test_app_with_db_path(path, &root)
    }

    fn seed_active_session(app: &crate::mcp::app::App) -> String {
        let sid = uuid::Uuid::new_v4().to_string();
        app.db
            .with_transaction(|tx| {
                create_session(tx, &sid, "/repo", "main", Some("task"), Agent::Claude)
            })
            .unwrap();
        sid
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

    #[test]
    fn session_handoff_returns_token_and_block_without_embedding_token_in_block() {
        let app = test_handoff_app();
        let sid = seed_active_session(&app);
        let out =
            handle_session_handoff(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let token = out["handoff_token"].as_str().unwrap();
        assert!(!token.is_empty());
        let block = out["handoff_block"].as_str().unwrap();
        assert!(
            !block.contains(token),
            "token must NOT appear inside the fenced block"
        );
        assert_eq!(out["generation"], json!(1));
    }
}
