//! MCP tool definitions and dispatch.

use serde_json::{json, Value};

use super::app::App;
use crate::config::McpAccessMode;
use crate::error::MemoryError;

mod code_maps;
mod collab_caps;
mod collab_events;
mod collab_session;
mod diary;
mod drawers;
mod handoff;
mod kg;
mod shared;
mod symbol_graph;

use code_maps::{handle_code_map_load, handle_code_map_status, handle_code_map_write};
use collab_caps::{handle_collab_get_caps, handle_collab_register_caps};
use collab_session::{
    handle_collab_ack, handle_collab_approve, handle_collab_end, handle_collab_recv,
    handle_collab_send, handle_collab_set_implementer, handle_collab_start,
    handle_collab_start_code_review, handle_collab_status, handle_collab_wait_my_turn,
};
use diary::{handle_diary_read, handle_diary_write};
use drawers::{
    handle_add_drawer, handle_delete_drawer, handle_get_drawer, handle_get_taxonomy,
    handle_list_rooms, handle_list_wings, handle_search, handle_status,
};
use handoff::handle_session_handoff;
use kg::{
    handle_find_tunnels, handle_graph_stats, handle_kg_add, handle_kg_invalidate, handle_kg_query,
    handle_kg_stats, handle_kg_timeline, handle_traverse,
};
use symbol_graph::{
    handle_symbol_graph_imports, handle_symbol_graph_index, handle_symbol_graph_lookup,
    handle_symbol_graph_neighbors,
};

/// Return tool definitions for tools/list.
pub fn tool_definitions(app: &App) -> Vec<Value> {
    let tools = vec![
        json!({
            "name": "status",
            "description": "Memory overview — total drawers, wing and room counts, knowledge graph summary, and one-line metrics summary. Optional set_task_tag/clear_task_tag manage the explicit metrics task tag for non-collab work (METRICS_SPEC §2.3).",
            "inputSchema": { "type": "object", "properties": {
                "set_task_tag": { "type": "string", "description": "Set the explicit metrics task tag for subsequent token_usage rows. Process-local and ephemeral (cleared on server restart); shadowed while an active collab session is attributing (METRICS_SPEC §2.3 gives the collab session id priority)." },
                "clear_task_tag": { "type": "boolean", "description": "Clear the explicit metrics task tag" }
            } }
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
            "description": "File verbatim content into a wing/room. Omit logical_key for append/content-addressed durable notes; pass logical_key to overwrite a current-context drawer for that wing/room instead of accumulating stale copies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "wing": { "type": "string" },
                    "room": { "type": "string", "default": "general" },
                    "logical_key": {
                        "type": "string",
                        "description": "Optional stable key for replaceable current context, e.g. 'current-context' or 'task-state'. Same wing/room/logical_key rewrites the same drawer id."
                    }
                },
                "required": ["content", "wing"]
            }
        }),
        json!({
            "name": "get_drawer",
            "description": "Fetch a single drawer by exact ID. By default returns full content; pass include_content:false for metadata only, max_chars for a bounded excerpt, or hash_only:true for a content hash without body. Restricted mode redacts content_hash.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "include_content": {
                        "type": "boolean",
                        "description": "When false, omit the content body. Default true."
                    },
                    "max_chars": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum content characters to return when include_content is true. Capped at the server max."
                    },
                    "hash_only": {
                        "type": "boolean",
                        "description": "When true, omit content and return content_hash unless restricted mode redacts it. Overrides include_content. Default false."
                    }
                },
                "required": ["id"]
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
                    "entity_type": { "type": "string" },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max triples to return (default 50)."
                    }
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
            "description": "Create a bounded Claude↔Codex planning session. Optional `task` describes the planning goal and is returned in collab_status so the counterpart agent can fetch it without a manual paste. Optional `implementer` (default 'claude') selects which agent runs the v3 batch implementation phase; 'codex' routes CodeImplementPending to Codex so it drives its own subagent-driven-development end-to-end.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "initiator": { "type": "string", "enum": ["claude", "codex"] },
                    "task": { "type": "string" },
                    "implementer": { "type": "string", "enum": ["claude", "codex"] }
                },
                "required": ["repo_path", "branch", "initiator"]
            }
        }),
        json!({
            "name": "collab_start_code_review",
            "description": "Create a bounded Claude↔Codex review-only session positioned directly at the v3 global-review stage. Codex owns the first turn; initiator must be claude.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "base_sha": { "type": "string" },
                    "head_sha": { "type": "string" },
                    "initiator": { "type": "string", "enum": ["claude"] },
                    "task": { "type": "string" }
                },
                "required": ["repo_path", "branch", "base_sha", "head_sha", "initiator", "task"]
            }
        }),
        json!({
            "name": "collab_set_implementer",
            "description": "Select or reassign which agent owns the v3 batch implementation phase. Valid during planning and during CodeImplementPending; in CodeImplementPending this also moves current_owner to the selected implementer so /collab join --implementer=... can resume from the last ironmem checkpoint.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "implementer": { "type": "string", "enum": ["claude", "codex"] },
                    "handoff_token": { "type": "string" }
                },
                "required": ["session_id", "agent", "implementer"]
            }
        }),
        json!({
            "name": "collab_send",
            "description": "Send a collab message and advance the bounded state machine. v1 planning topics: draft, canonical, review, final. v3 coding topics: task_list, implementation_done, review_local, review_fix_global, final_review, failure_report.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "sender": { "type": "string", "enum": ["claude", "codex"] },
                    "topic": { "type": "string" },
                    "content": { "type": "string" },
                    "handoff_token": { "type": "string" }
                },
                "required": ["session_id", "sender", "topic", "content"]
            }
        }),
        json!({
            "name": "collab_recv",
            "description": "Read pending collab messages for one agent. When auto_ack is true, atomically marks all returned messages as acked in the same transaction, eliminating one round-trip compared to calling collab_ack separately for each message. Default false preserves the existing two-step recv+ack flow.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "receiver": { "type": "string", "enum": ["claude", "codex"] },
                    "limit": { "type": "integer", "default": 10 },
                    "auto_ack": { "type": "boolean", "default": false },
                    "handoff_token": { "type": "string" }
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
                    "session_id": { "type": "string" },
                    "handoff_token": { "type": "string" }
                },
                "required": ["message_id", "session_id"]
            }
        }),
        json!({
            "name": "collab_status",
            "description": "Return collab session state. Accepted plans and task lists are returned by reference by default — `canonical_plan_ref`/`final_plan_ref`/`task_list_ref` = {drawer_id, hash, first_200_chars}; pass verbose:true to additionally inline full canonical/final plans, and include_task_list:true to inline full task_list. (Legacy pre-009 plans inline the full body and emit no *_plan_ref.)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "verbose": {
                        "type": "boolean",
                        "description": "When true, include the full canonical/final plan body alongside the compact reference. Default false (compact reference only)."
                    },
                    "include_task_list": {
                        "type": "boolean",
                        "description": "When true, include the full task_list JSON alongside task_list_ref. Default false (compact reference only)."
                    }
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
                    "content_hash": { "type": "string" },
                    "handoff_token": { "type": "string" }
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
                    },
                    "handoff_token": { "type": "string" }
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
                    "timeout_secs": { "type": "integer", "default": 30 },
                    "handoff_token": { "type": "string" }
                },
                "required": ["session_id", "agent"]
            }
        }),
        json!({
            "name": "collab_end",
            "description": "End a collab session. Valid only from PlanLocked (pre-task_list), CodingComplete, or CodingFailed; rejected in any active planning phase (PlanParallelDrafts through PlanClaudeFinalizePending) or coding-active phase (CodeImplementPending through CodeReviewFinalPending). Idempotent once allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "handoff_token": { "type": "string" }
                },
                "required": ["session_id", "agent"]
            }
        }),
        json!({
            "name": "code_map_write",
            "description": "Write or refresh a per-area code map. Embeds summary, stores the drawer in room 'code-maps', and records the sidecar row keyed (repo, area). Write-mode only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Canonical repo path (trailing slash stripped)" },
                    "area": { "type": "string", "description": "Named sub-area of the repo (e.g. 'core', 'auth')" },
                    "summary": { "type": "string", "description": "Code-map body to embed and store" },
                    "head_sha": { "type": "string", "description": "Git SHA at which the map was built" },
                    "source_files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Repo-relative paths of source files this map covers"
                    },
                    "built_by": { "type": "string", "description": "Agent or user that built this map" },
                    "turn_id": { "type": "string", "description": "Optional collab turn ID for metrics attribution" }
                },
                "required": ["repo", "area", "summary", "head_sha", "source_files", "built_by"]
            }
        }),
        json!({
            "name": "code_map_load",
            "description": "Load a per-area code map and classify its freshness against the current HEAD.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string" },
                    "area": { "type": "string" },
                    "turn_id": { "type": "string", "description": "Optional collab turn ID" }
                },
                "required": ["repo", "area"]
            }
        }),
        json!({
            "name": "code_map_status",
            "description": "Lightweight freshness check for a per-area code map. Returns verdict only, no drawer body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string" },
                    "area": { "type": "string" }
                },
                "required": ["repo", "area"]
            }
        }),
        json!({
            "name": "symbol_graph_index",
            "description": "Index all Rust and Python source files in a git repository into the local symbol/import graph (migration 012). Only supported extensions (.rs, .py) are indexed; others are skipped. Incremental by content-hash: unchanged files are skipped unless --force. Write-mode only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Path to a git worktree root" },
                    "force": { "type": "boolean", "default": false, "description": "Re-index even unchanged files" }
                },
                "required": ["repo"]
            }
        }),
        json!({
            "name": "symbol_lookup",
            "description": "Look up symbol declarations (functions, structs, classes, etc.) in the indexed symbol graph by name or qualified name. Returns bounded metadata — no full source bodies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Path to the indexed git worktree root" },
                    "query": { "type": "string", "description": "Name or qualified-name prefix to search for" },
                    "kind": { "type": "string", "description": "Optional filter by kind (fn, struct, enum, class, trait, …)" },
                    "limit": { "type": "integer", "default": 50, "description": "Max results (capped at 100)" }
                },
                "required": ["repo", "query"]
            }
        }),
        json!({
            "name": "symbol_imports",
            "description": "Look up import statements in the indexed symbol graph by file path (repo-relative) or module name prefix. Returns bounded metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Path to the indexed git worktree root" },
                    "query": { "type": "string", "description": "Repo-relative file path or module name prefix" },
                    "limit": { "type": "integer", "default": 50, "description": "Max results (capped at 100)" }
                },
                "required": ["repo", "query"]
            }
        }),
        json!({
            "name": "symbol_neighbors",
            "description": "Look up symbol-graph edges (import or contains) by symbol id, name, qualified name, or file path. v0 edge scope: import (file→module) and contains (symbol→parent symbol).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Path to the indexed git worktree root" },
                    "query": { "type": "string", "description": "Symbol id, symbol/qualified name, or repo-relative file path prefix" },
                    "limit": { "type": "integer", "default": 50, "description": "Max results (capped at 100)" }
                },
                "required": ["repo", "query"]
            }
        }),
        json!({
            "name": "session_handoff",
            "description": "Issue (or byte-identically reuse) a one-time handoff token plus a deterministic, model-free session handoff block for an unplanned successor. Sets the pending generation; the successor presents handoff_token on its first mutating collab call to claim it, making this predecessor inert. The token is returned top-level (NOT inside the block) — the successor needs both.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "handoff_token": {
                        "type": "string",
                        "description": "Required when the session's active generation > 0 (a prior handoff has been claimed); omit only on a generation-0 session. On the first mutating collab call a successor presents this to claim the new generation."
                    }
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
        "status" => handle_status(app, args),
        "search" => handle_search(app, args),
        "add_drawer" => handle_add_drawer(app, args),
        "get_drawer" => handle_get_drawer(app, args),
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
        "collab_start_code_review" => handle_collab_start_code_review(app, args),
        "collab_set_implementer" => handle_collab_set_implementer(app, args),
        "collab_send" => handle_collab_send(app, args),
        "collab_recv" => handle_collab_recv(app, args),
        "collab_ack" => handle_collab_ack(app, args),
        "collab_status" => handle_collab_status(app, args),
        "collab_approve" => handle_collab_approve(app, args),
        "collab_register_caps" => handle_collab_register_caps(app, args),
        "collab_get_caps" => handle_collab_get_caps(app, args),
        "collab_wait_my_turn" => handle_collab_wait_my_turn(app, args),
        "collab_end" => handle_collab_end(app, args),
        "session_handoff" => handle_session_handoff(app, args),
        "code_map_write" => handle_code_map_write(app, args),
        "code_map_load" => handle_code_map_load(app, args),
        "code_map_status" => handle_code_map_status(app, args),
        "symbol_graph_index" => handle_symbol_graph_index(app, args),
        "symbol_lookup" | "symbol_graph_lookup" => handle_symbol_graph_lookup(app, args),
        "symbol_imports" | "symbol_graph_imports" => handle_symbol_graph_imports(app, args),
        "symbol_neighbors" | "symbol_graph_neighbors" => handle_symbol_graph_neighbors(app, args),
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
            | "get_drawer"
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
            | "collab_start_code_review"
            | "collab_set_implementer"
            | "collab_send"
            | "collab_recv"
            | "collab_ack"
            | "collab_status"
            | "collab_approve"
            | "collab_register_caps"
            | "collab_get_caps"
            | "collab_wait_my_turn"
            | "collab_end"
            | "session_handoff"
            | "code_map_write"
            | "code_map_load"
            | "code_map_status"
            | "symbol_graph_index"
            | "symbol_lookup"
            | "symbol_imports"
            | "symbol_neighbors"
            | "symbol_graph_lookup"
            | "symbol_graph_imports"
            | "symbol_graph_neighbors"
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
                | "collab_start_code_review"
                | "collab_set_implementer"
                | "collab_send"
                | "collab_ack"
                | "collab_approve"
                | "collab_register_caps"
                | "collab_end"
                | "session_handoff"
                | "code_map_write"
                | "symbol_graph_index"
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

        // symbol_graph_index is write-gated; read tools are always allowed.
        assert!(tool_allowed_in_mode(
            McpAccessMode::Trusted,
            "symbol_graph_index"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "symbol_graph_index"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::Restricted,
            "symbol_graph_index"
        ));
        assert!(tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "symbol_lookup"
        ));
        assert!(tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "symbol_imports"
        ));
        assert!(tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "symbol_neighbors"
        ));
    }

    #[test]
    fn get_drawer_is_known_read_tool_allowed_in_all_modes() {
        assert!(tool_known("get_drawer"));
        // get_drawer is a pure read: never write-gated.
        assert!(tool_allowed_in_mode(McpAccessMode::Trusted, "get_drawer"));
        assert!(tool_allowed_in_mode(McpAccessMode::ReadOnly, "get_drawer"));
        assert!(tool_allowed_in_mode(
            McpAccessMode::Restricted,
            "get_drawer"
        ));
    }

    #[test]
    fn session_handoff_is_write_gated_and_known() {
        assert!(tool_known("session_handoff"));
        assert!(tool_allowed_in_mode(
            McpAccessMode::Trusted,
            "session_handoff"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "session_handoff"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::Restricted,
            "session_handoff"
        ));
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
