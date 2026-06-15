//! `session_handoff` MCP tool + the generation-lease guard (issue #91).
//!
//! `ensure_actor_generation_current` validates (and on first-touch/claim,
//! binds) this process's generation for (session, agent). Call before any
//! actor-bearing mutating/binding collab op. When `maybe_token` is `Some`, the
//! guard must run inside the caller's write transaction so the claim is atomic
//! with the op; the no-token validation path may run in its own transaction (as
//! `collab_wait_my_turn` does).
//!
//! `handle_session_handoff` issues (or byte-identically reuses) a one-time
//! handoff token and renders a deterministic, model-free session handoff block
//! for an unplanned successor. The token is returned top-level in the JSON
//! response — NOT embedded inside the fenced block.

use std::fmt::Write as _;

use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use crate::collab::queue::SessionRecord;
use crate::collab::{claim_handoff_token, read_actor_generation, Agent};
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
        if !app.config.mcp_access_mode.allows_writes() {
            return Err(MemoryError::Permission(
                "claiming a session_handoff token requires write access (IRONMEM_MCP_MODE=trusted)"
                    .to_string(),
            ));
        }
        let claimed = claim_handoff_token(conn, session_id, agent, token)?;
        // The cache is advisory; the DB is authoritative. If the enclosing
        // transaction rolls back after this claim, the process cache may be one
        // generation ahead of the DB. The guard treats that as a (fail-safe)
        // stale condition on subsequent calls rather than silently accepting it.
        app.set_cached_generation(session_id, agent, claimed);
        return Ok(());
    }
    let db_active = read_actor_generation(conn, session_id, agent)?
        .map(|a| a.generation)
        .unwrap_or(0);
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

fn task_list_str_field(raw: Option<&str>, key: &str) -> Option<String> {
    let raw = raw?;
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get(key)?
        .as_str()
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
    // Intentionally runs as a separate read after the session-snapshot transaction
    // (render-only; can't interleave under the single-request MCP dispatch model).
    db.with_connection(|conn| {
        // Wrap the needle in sentinel newlines so `session_id: <id>` matches only
        // as a complete line, avoiding substring collisions (e.g. "test-sid" inside
        // "test-sid-extra") or cross-session matches. Concatenating char(10) on both
        // sides of `content` ensures first-line and last-line entries also match.
        let needle = format!("\nsession_id: {session_id}\n");
        let content: Option<String> = conn
            .query_row(
                "SELECT content FROM drawers
                 WHERE wing = ?1 AND room = ?2
                   AND (char(10) || content || char(10)) LIKE '%' || ?3 || '%'
                 -- drawer ids are content-hash based; inserts upsert-in-place (rowid stable
                 -- on conflict), so rowid DESC = newest first-seen checkpoint content.
                 -- A future switch to INSERT OR REPLACE would break this ordering.
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

/// Pure deterministic render of session state + checkpoint (no clock,
/// no randomness, no timestamps). Key order in the fenced block is stable
/// across calls. `pending_generation` is the **to-be-claimed** value
/// (= `active_generation + 1`), not the caller's current active generation.
/// `agent` is the agent role whose session context is being transferred (the
/// vacating actor).
pub(super) fn compose_handoff_block(
    record: &SessionRecord,
    agent: Agent,
    pending_generation: u64,
    checkpoint: Option<Checkpoint>,
) -> String {
    let s = &record.session;
    let cp = checkpoint.unwrap_or_default();
    let cp_present = cp != Checkpoint::default();
    let plan_file_path = task_list_str_field(s.task_list.as_deref(), "plan_file_path");
    let execution_mode = task_list_str_field(s.task_list.as_deref(), "execution_mode");
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
    let _ = writeln!(
        out,
        "task_list.plan_file_path: {}",
        opt(plan_file_path.as_deref())
    );
    let _ = writeln!(
        out,
        "task_list.execution_mode: {}",
        opt(execution_mode.as_deref())
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

    // Resurrection guard + active-session snapshot + issue, atomic in one transaction.
    let (record, issued) = app.db.with_transaction(|tx| {
        ensure_actor_generation_current(
            app,
            tx,
            session_id,
            agent,
            opt_handoff_token(args).as_deref(),
        )?;
        crate::collab::queue::ensure_active(tx, session_id)?;
        let record = crate::collab::queue::load_session_record(tx, session_id)?;
        let issued = crate::collab::issue_or_reuse_handoff(tx, session_id, agent)?;
        Ok((record, issued))
    })?;

    // Best-effort handoff counter: keyed on session_id (the repo's task_tag
    // convention for collab rows, matching increment_task_review_rounds call sites).
    // Counted only on a *fresh* token issue (`!issued.reused`) so one logical
    // handoff counts once: a pre-claim retry of session_handoff is byte-identical
    // and reuses the pending token, and must not double-bump the counter. It is
    // still counted at issue time (not claim time) so it reflects handoff intent
    // even if the spawned successor never claims the lease. Warn-and-continue: a
    // metrics error must never fail the session_handoff response.
    if !issued.reused {
        if let Err(e) = app.db.increment_task_handoffs(session_id) {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "metrics: increment_task_handoffs failed — handoff count may be under-counted"
            );
        }
    }

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
        assert!(a.contains("task_list.plan_file_path: \u{2014}"));
        assert!(a.contains("task_list.execution_mode: \u{2014}"));
        assert!(a.contains("handoff.agent: claude"));
        assert!(a.contains("handoff.generation: 1"));
    }

    fn test_handoff_app() -> (Arc<crate::mcp::app::App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.sqlite3");
        let root = dir.path().to_path_buf();
        let app = test_app_with_db_path(path, &root);
        (app, dir)
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
        let (app, _dir) = test_handoff_app();
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

    /// Verify that `parse_checkpoint` extracts all expected fields from a realistic
    /// multi-line checkpoint string, and that `compose_handoff_block` with that
    /// checkpoint renders the populated fields (checkpoint: present, gates: passed,
    /// checkpoint.status: completed, etc.).
    #[test]
    fn parse_checkpoint_and_compose_block_with_populated_checkpoint() {
        let checkpoint_body = "\
collab_checkpoint\n\
session_id: test-sid\n\
phase: CodeImplementPending\n\
status: completed\n\
task_id: 2\n\
completed_task_ids: 1,2\n\
next_task_id: 3\n\
gates: passed\n";

        let cp = parse_checkpoint(checkpoint_body);
        assert_eq!(cp.status.as_deref(), Some("completed"), "status");
        assert_eq!(cp.task_id.as_deref(), Some("2"), "task_id");
        assert_eq!(
            cp.completed_task_ids.as_deref(),
            Some("1,2"),
            "completed_task_ids"
        );
        assert_eq!(cp.next_task_id.as_deref(), Some("3"), "next_task_id");
        assert_eq!(cp.gates.as_deref(), Some("passed"), "gates");

        // compose_handoff_block must render the populated checkpoint correctly.
        let r = sample_record(Phase::CodeImplementPending);
        let block = compose_handoff_block(&r, Agent::Codex, 2, Some(cp));

        assert!(
            block.contains("checkpoint: present"),
            "checkpoint must be present"
        );
        assert!(
            block.contains("checkpoint.status: completed"),
            "checkpoint.status must be rendered"
        );
        assert!(
            block.contains("checkpoint.task_id: 2"),
            "checkpoint.task_id must be rendered"
        );
        assert!(
            block.contains("checkpoint.completed_task_ids: 1,2"),
            "checkpoint.completed_task_ids must be rendered"
        );
        assert!(
            block.contains("checkpoint.next_task_id: 3"),
            "checkpoint.next_task_id must be rendered"
        );
        assert!(
            block.contains("gates: passed"),
            "gates must be rendered from checkpoint"
        );
        assert!(
            block.contains("handoff.agent: codex"),
            "handoff.agent must be rendered"
        );
        assert!(
            block.contains("handoff.generation: 2"),
            "handoff.generation must be rendered"
        );
    }

    // ── Task 4 tests ─────────────────────────────────────────────────────────

    /// For every relevant phase, verify the handoff block:
    ///   (a) contains `phase: <Name>`,
    ///   (b) two renders are byte-identical (determinism),
    ///   (c) contains no timestamp field substrings.
    #[test]
    fn golden_block_per_phase() {
        use crate::collab::Phase::*;
        for phase in [
            PlanParallelDrafts,
            PlanCodexReviewPending,
            PlanLocked,
            CodeImplementPending,
            CodeReviewFixGlobalPending,
            CodeReviewLocalPending,
            CodeReviewFinalPending,
            CodingComplete,
            CodingFailed,
        ] {
            let r = sample_record(phase);
            let b1 = compose_handoff_block(&r, Agent::Claude, 1, None);
            let b2 = compose_handoff_block(&r, Agent::Claude, 1, None);
            assert_eq!(b1, b2, "phase {phase} must render identically");
            assert!(
                b1.contains(&format!("phase: {phase}")),
                "missing phase line for {phase}"
            );
            for ts in ["created_at", "updated_at", "ended_at"] {
                assert!(
                    !b1.contains(ts),
                    "block must not contain {ts} (phase {phase})"
                );
            }
        }
    }

    /// Verify that plan drawer IDs/hashes and checkpoint gates all render
    /// correctly when populated.
    #[test]
    fn golden_block_with_plan_drawers_and_checkpoint_gates() {
        let mut r = sample_record(crate::collab::Phase::CodeImplementPending);
        r.session.canonical_plan_drawer_id = Some("abc123".into());
        r.session.canonical_plan_hash = Some("def456".into());
        r.session.final_plan_drawer_id = Some("fff999".into());
        r.session.final_plan_hash = Some("aaa111".into());
        r.session.task_list = Some(
            json!({
                "plan_file_path": "docs/superpowers/plans/handoff.md",
                "execution_mode": "mechanical_direct",
                "tasks": [{"id": 1}]
            })
            .to_string(),
        );
        let cp = Checkpoint {
            status: Some("completed".into()),
            task_id: Some("2".into()),
            completed_task_ids: Some("1,2".into()),
            next_task_id: Some("3".into()),
            gates: Some("passed".into()),
        };
        let block = compose_handoff_block(&r, Agent::Codex, 2, Some(cp));
        assert!(block.contains("plan.canonical.drawer_id: abc123"));
        assert!(block.contains("plan.canonical.hash: def456"));
        assert!(block.contains("plan.final.drawer_id: fff999"));
        assert!(block.contains("task_list.plan_file_path: docs/superpowers/plans/handoff.md"));
        assert!(block.contains("task_list.execution_mode: mechanical_direct"));
        assert!(block.contains("gates: passed"));
        assert!(block.contains("checkpoint: present"));
        assert!(block.contains("checkpoint.status: completed"));
        assert!(block.contains("handoff.agent: codex"));
        assert!(block.contains("handoff.generation: 2"));
    }

    /// Calling session_handoff twice before the token is claimed must return
    /// byte-identical handoff_block, handoff_token, and generation values.
    #[test]
    fn session_handoff_twice_before_claim_is_byte_identical() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);
        let a =
            handle_session_handoff(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let b =
            handle_session_handoff(&app, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        assert_eq!(a["handoff_block"], b["handoff_block"]);
        assert_eq!(a["handoff_token"], b["handoff_token"]);
        assert_eq!(a["generation"], b["generation"]);
    }

    /// Predecessor cannot mint a new handoff after the successor has claimed the
    /// previous one (two App instances over the same DB file).
    #[test]
    fn stale_predecessor_cannot_mint_after_claim() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        let pred = test_app_with_db_path(db_path.clone(), dir.path());
        let succ = test_app_with_db_path(db_path, dir.path());

        let sid = seed_active_session(&pred);

        // Predecessor issues the handoff (binds at gen 0, pending gen becomes 1).
        let issued =
            handle_session_handoff(&pred, &json!({"session_id": sid, "agent": "claude"})).unwrap();
        let token = issued["handoff_token"].as_str().unwrap().to_string();

        // Successor claims the token — advances DB generation to 1.
        handle_session_handoff(
            &succ,
            &json!({"session_id": sid, "agent": "claude", "handoff_token": token}),
        )
        .unwrap();

        // Predecessor tries to mint a new handoff — must be rejected (stale gen).
        let res = handle_session_handoff(&pred, &json!({"session_id": sid, "agent": "claude"}));
        assert!(
            res.is_err(),
            "stale predecessor must not mint a new handoff"
        );
    }

    /// A token-claim attempted through a ReadOnly-mode App must be rejected with a
    /// Permission error before any DB write occurs. The token itself remains valid
    /// (trusted-mode claim still succeeds after the rejection).
    #[test]
    fn token_claim_rejected_in_read_only_mode() {
        use crate::config::{Config, EmbedMode, McpAccessMode};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        // Trusted app — issues the session and the handoff token.
        let trusted_app = test_app_with_db_path(db_path.clone(), dir.path());
        let sid = seed_active_session(&trusted_app);

        // Issue the token via the trusted app.
        let token = trusted_app
            .db
            .with_transaction(|tx| issue_or_reuse_handoff(tx, &sid, Agent::Claude))
            .unwrap()
            .token;

        // Build a ReadOnly-mode App over the same DB.
        let ro_config = Config {
            db_path: db_path.clone(),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        };
        #[allow(clippy::arc_with_non_send_sync)]
        let ro_app = std::sync::Arc::new(crate::mcp::app::App::new(ro_config).unwrap());

        // Claim attempt through the read-only app must fail with a Permission error.
        let err = ro_app
            .db
            .with_connection(|conn| {
                ensure_actor_generation_current(&ro_app, conn, &sid, Agent::Claude, Some(&token))
            })
            .unwrap_err();

        assert!(
            matches!(err, MemoryError::Permission(_)),
            "expected Permission error, got: {err:?}"
        );
        assert!(
            err.to_string().contains("write access"),
            "error must mention write access, got: {err}"
        );

        // The token must still be claimable by a trusted-mode caller (no DB mutation occurred).
        trusted_app
            .db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&trusted_app, tx, &sid, Agent::Claude, Some(&token))
            })
            .unwrap();
    }

    /// The no-token path of `ensure_actor_generation_current` must not create a
    /// lease row in `collab_actor_generations`. This proves that read-only/restricted
    /// tools that call this guard (e.g. `collab_recv`, `collab_wait_my_turn`) do not
    /// write a DB row on the no-token path.
    #[test]
    fn guard_no_token_does_not_create_lease_row() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);
        app.db
            .with_connection(|conn| {
                ensure_actor_generation_current(&app, conn, &sid, Agent::Claude, None)
            })
            .unwrap();
        let n: i64 = app
            .db
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM collab_actor_generations \
                     WHERE session_id = ?1 AND agent = 'claude'",
                    rusqlite::params![sid],
                    |r| r.get(0),
                )
                .map_err(crate::error::MemoryError::from)
            })
            .unwrap();
        assert_eq!(n, 0, "no-token guard path must not create a lease row");
    }

    /// `session_handoff` on an ended session must return `Err`.
    #[test]
    fn session_handoff_on_ended_session_is_rejected() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);

        // End the session directly via the queue layer.
        app.db
            .with_transaction(|tx| crate::collab::queue::end_session(tx, &sid))
            .unwrap();

        // Handoff on an ended session must fail (ensure_active rejects it).
        let result = handle_session_handoff(&app, &json!({"session_id": sid, "agent": "claude"}));
        assert!(
            result.is_err(),
            "session_handoff on an ended session must return Err"
        );
    }

    /// Calling the no-token guard twice for the same (app, session, agent) in
    /// steady-state (db gen == cached gen == 0) must succeed both times.
    #[test]
    fn guard_cached_equal_db_is_ok_reentrant() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);

        // First call: binds the cache at gen 0.
        app.db
            .with_connection(|conn| {
                ensure_actor_generation_current(&app, conn, &sid, Agent::Claude, None)
            })
            .unwrap();

        // Second call: cached == db (both 0) → must still be Ok.
        app.db
            .with_connection(|conn| {
                ensure_actor_generation_current(&app, conn, &sid, Agent::Claude, None)
            })
            .unwrap();
    }

    /// A fresh process (empty cache) calling the no-token guard when the DB
    /// generation is already > 0 must be rejected with an error mentioning
    /// "handed off".
    #[test]
    fn guard_rejects_tokenless_fresh_process_when_gen_gt_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");

        // Predecessor: issues the handoff.
        let pred = test_app_with_db_path(db_path.clone(), dir.path());
        let sid = {
            let sid = uuid::Uuid::new_v4().to_string();
            pred.db
                .with_transaction(|tx| {
                    crate::collab::queue::create_session(
                        tx,
                        &sid,
                        "/repo",
                        "main",
                        Some("t"),
                        Agent::Claude,
                    )
                })
                .unwrap();
            sid
        };

        // Issue and claim (pred → succ) to advance DB to gen 1.
        let token = pred
            .db
            .with_transaction(|tx| issue_or_reuse_handoff(tx, &sid, Agent::Claude))
            .unwrap()
            .token;

        let succ = test_app_with_db_path(db_path.clone(), dir.path());
        succ.db
            .with_transaction(|tx| {
                ensure_actor_generation_current(&succ, tx, &sid, Agent::Claude, Some(&token))
            })
            .unwrap();

        // Third fresh App: empty cache, DB gen = 1, no token → must be rejected.
        let fresh = test_app_with_db_path(db_path, dir.path());
        let err = fresh
            .db
            .with_connection(|conn| {
                ensure_actor_generation_current(&fresh, conn, &sid, Agent::Claude, None)
            })
            .unwrap_err();

        assert!(
            err.to_string().contains("handed off"),
            "expected 'handed off' in error, got: {err}"
        );
    }

    /// `handle_session_handoff` bumps `task_outcomes.handoffs` by 1 for a
    /// session whose row exists (keyed on session_id). A metrics failure or
    /// absent row must still return the normal handoff JSON.
    #[test]
    fn handle_session_handoff_bumps_handoffs_counter() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);

        // Seed a task_outcomes row with task_tag = session_id (the repo convention).
        app.db
            .upsert_task_outcome(&crate::db::metrics::TaskOutcome {
                task_tag: sid.clone(),
                collab_session_id: Some(sid.clone()),
                started_at: Some("2026-06-15T00:00:00Z".to_string()),
                done_at: None,
                outcome: None,
                review_rounds: 0,
                fix_commits: 0,
                handoffs: 0,
                pr_url: None,
            })
            .unwrap();

        let resp =
            handle_session_handoff(&app, &json!({ "session_id": sid, "agent": "claude" })).unwrap();

        // Response must carry the normal handoff fields.
        assert!(
            resp.get("handoff_token").is_some(),
            "handoff_token must be top-level"
        );
        assert!(
            resp.get("handoff_block").is_some(),
            "handoff_block must be present"
        );

        // Counter incremented exactly once.
        let got = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(got.handoffs, 1, "handoffs must be 1 after one handoff call");
    }

    /// A pre-claim retry of `session_handoff` reuses the pending token (it is
    /// byte-identical, see `session_handoff_twice_before_claim_is_byte_identical`)
    /// and must NOT double-bump the handoffs counter: one logical handoff = one
    /// increment, gated on `!issued.reused`.
    #[test]
    fn handle_session_handoff_retry_before_claim_counts_once() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);

        app.db
            .upsert_task_outcome(&crate::db::metrics::TaskOutcome {
                task_tag: sid.clone(),
                collab_session_id: Some(sid.clone()),
                started_at: Some("2026-06-15T00:00:00Z".to_string()),
                done_at: None,
                outcome: None,
                review_rounds: 0,
                fix_commits: 0,
                handoffs: 0,
                pr_url: None,
            })
            .unwrap();

        // Two issues before any claim: second reuses the pending token.
        let first =
            handle_session_handoff(&app, &json!({ "session_id": sid, "agent": "claude" })).unwrap();
        let second =
            handle_session_handoff(&app, &json!({ "session_id": sid, "agent": "claude" })).unwrap();
        assert_eq!(
            first.get("handoff_token"),
            second.get("handoff_token"),
            "pre-claim retry must reuse the same token (byte-identical)"
        );

        let got = app.db.get_task_outcome(&sid).unwrap().unwrap();
        assert_eq!(
            got.handoffs, 1,
            "two pre-claim issues are one logical handoff — counter must be 1, not 2"
        );
    }

    /// Absent task_outcomes row: increment is a no-op; response is still normal.
    #[test]
    fn handle_session_handoff_absent_row_still_returns_normal_response() {
        let (app, _dir) = test_handoff_app();
        let sid = seed_active_session(&app);
        // Deliberately do NOT seed a task_outcomes row.

        let resp =
            handle_session_handoff(&app, &json!({ "session_id": sid, "agent": "claude" })).unwrap();

        assert!(
            resp.get("handoff_token").is_some(),
            "handoff_token must be top-level"
        );
        assert!(
            resp.get("handoff_block").is_some(),
            "handoff_block must be present"
        );
        // No row created by the increment.
        assert!(
            app.db.get_task_outcome(&sid).unwrap().is_none(),
            "absent row must remain absent after best-effort increment"
        );
    }

    /// `opt_handoff_token` must treat an empty string as `None` and a non-empty
    /// value as `Some`.
    #[test]
    fn opt_handoff_token_treats_empty_string_as_none() {
        assert_eq!(
            opt_handoff_token(&json!({"handoff_token": ""})),
            None,
            "empty string must yield None"
        );
        assert_eq!(
            opt_handoff_token(&json!({"handoff_token": "abc-token"})),
            Some("abc-token".to_string()),
            "non-empty string must yield Some"
        );
        assert_eq!(
            opt_handoff_token(&json!({})),
            None,
            "missing key must yield None"
        );
    }
}
