//! MCP tool definitions and dispatch.

use serde_json::{json, Value};

use super::app::App;
use crate::config::McpAccessMode;
use crate::error::MemoryError;

mod collab_caps;
mod collab_events;
mod collab_session;
mod diary;
mod drawers;
mod kg;
mod shared;

use collab_caps::{handle_collab_get_caps, handle_collab_register_caps};
use collab_session::{
    handle_collab_ack, handle_collab_approve, handle_collab_end, handle_collab_recv,
    handle_collab_send, handle_collab_start, handle_collab_status, handle_collab_wait_my_turn,
};
use diary::{handle_diary_read, handle_diary_write};
use drawers::{
    handle_add_drawer, handle_delete_drawer, handle_get_taxonomy, handle_list_rooms,
    handle_list_wings, handle_search, handle_status,
};
use kg::{
    handle_find_tunnels, handle_graph_stats, handle_kg_add, handle_kg_invalidate, handle_kg_query,
    handle_kg_stats, handle_kg_timeline, handle_traverse,
};

/// Return tool definitions for tools/list.
pub fn tool_definitions(app: &App) -> Vec<Value> {
    let tools = vec![
        json!({
            "name": "status",
            "description": "Memory overview — total drawers, wing and room counts",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "search",
            "description": "Semantic search with KG-boosted ranking. Returns bounded content excerpts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "default": 10 },
                    "wing": { "type": "string" },
                    "room": { "type": "string" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "add_drawer",
            "description": "File verbatim content into a wing/room",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "wing": { "type": "string" },
                    "room": { "type": "string", "default": "general" }
                },
                "required": ["content", "wing"]
            }
        }),
        json!({
            "name": "delete_drawer",
            "description": "Remove a drawer by ID",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "list_wings",
            "description": "All wings with drawer counts",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_rooms",
            "description": "Rooms within a wing (or all rooms)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wing": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "get_taxonomy",
            "description": "Full wing → room → count tree",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "kg_add",
            "description": "Add an entity relationship triple to the knowledge graph",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string" },
                    "subject_type": { "type": "string", "default": "unknown" },
                    "predicate": { "type": "string" },
                    "object": { "type": "string" },
                    "object_type": { "type": "string", "default": "unknown" },
                    "valid_from": { "type": "string" },
                    "confidence": { "type": "number", "default": 1.0 }
                },
                "required": ["subject", "predicate", "object"]
            }
        }),
        json!({
            "name": "kg_query",
            "description": "Query knowledge graph for an entity's relationships",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entity": { "type": "string" },
                    "entity_type": { "type": "string" }
                },
                "required": ["entity"]
            }
        }),
        json!({
            "name": "kg_invalidate",
            "description": "Mark a triple as no longer valid",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "triple_id": { "type": "string" },
                    "valid_to": { "type": "string" }
                },
                "required": ["triple_id"]
            }
        }),
        json!({
            "name": "kg_timeline",
            "description": "Chronological fact history for an entity",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entity": { "type": "string" },
                    "entity_type": { "type": "string" }
                },
                "required": ["entity"]
            }
        }),
        json!({
            "name": "kg_stats",
            "description": "Knowledge graph summary statistics",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "traverse",
            "description": "BFS traversal from a room to find related rooms",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room": { "type": "string" },
                    "max_depth": { "type": "integer", "default": 3 }
                },
                "required": ["room"]
            }
        }),
        json!({
            "name": "find_tunnels",
            "description": "Find rooms that span multiple wings",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "graph_stats",
            "description": "Memory graph summary — rooms, wings, tunnels, edges",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "diary_write",
            "description": "Write a timestamped diary entry",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "wing": { "type": "string", "default": "diary" }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "diary_read",
            "description": "Read recent diary entries",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wing": { "type": "string", "default": "diary" },
                    "limit": { "type": "integer", "default": 20 }
                }
            }
        }),
        json!({
            "name": "collab_start",
            "description": "Create a bounded Claude↔Codex planning session. Optional `task` describes the planning goal and is returned in collab_status so the counterpart agent can fetch it without a manual paste.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "initiator": { "type": "string", "enum": ["claude", "codex"] },
                    "task": { "type": "string" }
                },
                "required": ["repo_path", "branch", "initiator"]
            }
        }),
        json!({
            "name": "collab_send",
            "description": "Send a collab message and advance the bounded state machine. v1 planning topics: draft, canonical, review, final. v3 coding topics: task_list, implement, review_fix, final, review_local, review_fix_global, final_review, failure_report. Topic final is phase-dispatched (v1 plan finalize vs v3 per-task final chosen by current phase).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "sender": { "type": "string", "enum": ["claude", "codex"] },
                    "topic": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["session_id", "sender", "topic", "content"]
            }
        }),
        json!({
            "name": "collab_recv",
            "description": "Read pending collab messages for one agent",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "receiver": { "type": "string", "enum": ["claude", "codex"] },
                    "limit": { "type": "integer", "default": 10 }
                },
                "required": ["session_id", "receiver"]
            }
        }),
        json!({
            "name": "collab_ack",
            "description": "Mark a collab message as consumed",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string" },
                    "session_id": { "type": "string" }
                },
                "required": ["message_id", "session_id"]
            }
        }),
        json!({
            "name": "collab_status",
            "description": "Return the full collab session state",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "collab_approve",
            "description": "Codex-only shortcut for submitting an approve review",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["codex"] },
                    "content_hash": { "type": "string" }
                },
                "required": ["session_id", "agent", "content_hash"]
            }
        }),
        json!({
            "name": "collab_register_caps",
            "description": "Register available sub-agents/tools for a collab participant",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "capabilities": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["name"]
                        }
                    }
                },
                "required": ["session_id", "agent", "capabilities"]
            }
        }),
        json!({
            "name": "collab_get_caps",
            "description": "Read registered capabilities for one or all collab participants",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "collab_wait_my_turn",
            "description": "Long-poll: block until current_owner == agent or the timeout elapses. Returns {is_my_turn, phase, current_owner, session_ended}. Default timeout 30s, max 60s.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "timeout_secs": { "type": "integer", "default": 30 }
                },
                "required": ["session_id", "agent"]
            }
        }),
        json!({
            "name": "collab_end",
            "description": "End a collab session. Valid only from PlanLocked (pre-task_list), CodingComplete, or CodingFailed; rejected in any active planning phase (PlanParallelDrafts through PlanClaudeFinalizePending) or coding-active phase (CodeImplementPending through PrReadyPending). Idempotent once allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] }
                },
                "required": ["session_id", "agent"]
            }
        }),
    ];

    tools
        .into_iter()
        .filter(|tool| {
            tool.get("name")
                .and_then(|value| value.as_str())
                .map(|name| tool_allowed_in_mode(app.config.mcp_access_mode, name))
                .unwrap_or(false)
        })
        .collect()
}

/// Dispatch a tool call to the appropriate handler.
pub fn call_tool(app: &App, name: &str, args: &Value) -> Result<Value, MemoryError> {
    if !tool_known(name) {
        return Err(MemoryError::NotFound(format!("Unknown tool: {name}")));
    }
    ensure_tool_allowed(app, name)?;
    match name {
        "status" => handle_status(app),
        "search" => handle_search(app, args),
        "add_drawer" => handle_add_drawer(app, args),
        "delete_drawer" => handle_delete_drawer(app, args),
        "list_wings" => handle_list_wings(app),
        "list_rooms" => handle_list_rooms(app, args),
        "get_taxonomy" => handle_get_taxonomy(app),
        "kg_add" => handle_kg_add(app, args),
        "kg_query" => handle_kg_query(app, args),
        "kg_invalidate" => handle_kg_invalidate(app, args),
        "kg_timeline" => handle_kg_timeline(app, args),
        "kg_stats" => handle_kg_stats(app),
        "traverse" => handle_traverse(app, args),
        "find_tunnels" => handle_find_tunnels(app),
        "graph_stats" => handle_graph_stats(app),
        "diary_write" => handle_diary_write(app, args),
        "diary_read" => handle_diary_read(app, args),
        "collab_start" => handle_collab_start(app, args),
        "collab_send" => handle_collab_send(app, args),
        "collab_recv" => handle_collab_recv(app, args),
        "collab_ack" => handle_collab_ack(app, args),
        "collab_status" => handle_collab_status(app, args),
        "collab_approve" => handle_collab_approve(app, args),
        "collab_register_caps" => handle_collab_register_caps(app, args),
        "collab_get_caps" => handle_collab_get_caps(app, args),
        "collab_wait_my_turn" => handle_collab_wait_my_turn(app, args),
        "collab_end" => handle_collab_end(app, args),
        _ => Err(MemoryError::Permission(format!(
            "Tool '{name}' is not available in the current MCP mode"
        ))),
    }
}

// ── Mode-gating helpers ──────────────────────────────────────────────────────

fn tool_known(name: &str) -> bool {
    matches!(
        name,
        "status"
            | "search"
            | "add_drawer"
            | "delete_drawer"
            | "list_wings"
            | "list_rooms"
            | "get_taxonomy"
            | "kg_add"
            | "kg_query"
            | "kg_invalidate"
            | "kg_timeline"
            | "kg_stats"
            | "traverse"
            | "find_tunnels"
            | "graph_stats"
            | "diary_write"
            | "diary_read"
            | "collab_start"
            | "collab_send"
            | "collab_recv"
            | "collab_ack"
            | "collab_status"
            | "collab_approve"
            | "collab_register_caps"
            | "collab_get_caps"
            | "collab_wait_my_turn"
            | "collab_end"
    )
}

fn tool_allowed_in_mode(mode: McpAccessMode, name: &str) -> bool {
    if !tool_known(name) {
        return false;
    }
    mode.allows_writes()
        || !matches!(
            name,
            "add_drawer"
                | "delete_drawer"
                | "kg_add"
                | "kg_invalidate"
                | "diary_write"
                | "collab_start"
                | "collab_send"
                | "collab_ack"
                | "collab_approve"
                | "collab_register_caps"
                | "collab_end"
        )
}

fn ensure_tool_allowed(app: &App, name: &str) -> Result<(), MemoryError> {
    if tool_allowed_in_mode(app.config.mcp_access_mode, name) {
        Ok(())
    } else {
        Err(MemoryError::Permission(format!(
            "Tool '{name}' is disabled when IRONMEM_MCP_MODE={}",
            match app.config.mcp_access_mode {
                McpAccessMode::Trusted => "trusted",
                McpAccessMode::ReadOnly => "read-only",
                McpAccessMode::Restricted => "restricted",
            }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::shared::render_sensitive_text;
    use super::*;

    #[test]
    fn test_tool_access_modes_disable_writes_outside_trusted_mode() {
        assert!(tool_allowed_in_mode(McpAccessMode::Trusted, "add_drawer"));
        assert!(!tool_allowed_in_mode(McpAccessMode::ReadOnly, "add_drawer"));
        assert!(!tool_allowed_in_mode(McpAccessMode::Restricted, "kg_add"));
        assert!(tool_allowed_in_mode(McpAccessMode::Restricted, "search"));
    }

    #[test]
    fn confidence_validation_rejects_out_of_range() {
        use crate::config::{Config, EmbedMode, McpAccessMode};
        use std::sync::Arc;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        std::env::set_var("IRONMEM_DISABLE_MIGRATION", "1");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(crate::mcp::app::App::new(config).unwrap());

        // over 1.0
        let args = serde_json::json!({
            "subject": "foo", "predicate": "knows", "object": "bar",
            "subject_type": "entity", "object_type": "entity",
            "confidence": 1.5
        });
        let result = handle_kg_add(&app, &args);
        assert!(result.is_err(), "confidence > 1.0 should fail");

        // under 0.0
        let args = serde_json::json!({
            "subject": "foo", "predicate": "knows", "object": "bar",
            "subject_type": "entity", "object_type": "entity",
            "confidence": -0.1
        });
        let result = handle_kg_add(&app, &args);
        assert!(result.is_err(), "confidence < 0.0 should fail");

        // valid
        let args = serde_json::json!({
            "subject": "foo", "predicate": "knows", "object": "bar",
            "subject_type": "entity", "object_type": "entity",
            "confidence": 0.8
        });
        let result = handle_kg_add(&app, &args);
        assert!(result.is_ok(), "confidence 0.8 should succeed");

        std::env::remove_var("IRONMEM_DISABLE_MIGRATION");
    }

    #[test]
    fn test_render_sensitive_text_truncates_and_redacts() {
        let (excerpt, truncated, redacted, consumed) = render_sensitive_text("abcdef", 3, false);
        assert_eq!(excerpt, Value::String("abc".into()));
        assert!(truncated);
        assert!(!redacted);
        assert_eq!(consumed, 3);

        let (excerpt, truncated, redacted, consumed) = render_sensitive_text("abcdef", 10, true);
        assert_eq!(excerpt, Value::Null);
        assert!(!truncated);
        assert!(redacted);
        assert_eq!(consumed, 0);
    }
}
