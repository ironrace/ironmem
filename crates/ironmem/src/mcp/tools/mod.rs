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
    handle_collab_resume, handle_collab_send, handle_collab_set_implementer, handle_collab_start,
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
            "description": "Memory, graph, and metrics overview. task-tag fields scope non-collab metrics.",
            "inputSchema": { "type": "object", "properties": {
                "set_task_tag": { "type": "string", "description": "Process-local metrics task tag; active collab takes priority." },
                "clear_task_tag": { "type": "boolean", "description": "Clear the explicit metrics task tag" }
            } }
        }),
        json!({
            "name": "search",
            "description": "Semantic search. Returns bounded excerpts and stable IDs; use get_drawer for a complete body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "default": 10 },
                    "wing": { "type": "string" },
                    "room": { "type": "string" },
                    "full": {
                        "type": "boolean",
                        "default": false,
                        "description": "Return bounded full content; use get_drawer for the complete body."
                    },
                    "include_superseded": {
                        "type": "boolean",
                        "default": false,
                        "description": "true includes retained superseded history; false hides it."
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "add_drawer",
            "description": "Store content in a wing/room. logical_key overwrites replaceable current context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "wing": { "type": "string" },
                    "room": { "type": "string", "default": "general" },
                    "logical_key": {
                        "type": "string",
                        "description": "Stable key that rewrites this wing/room drawer."
                    },
                    "supersedes": {
                        "type": "string",
                        "description": "ID of a current drawer in this wing/room to retire; it is retained and retrievable by ID."
                    }
                },
                "required": ["content", "wing"]
            }
        }),
        json!({
            "name": "get_drawer",
            "description": "Fetch a drawer by ID or its wing/room/logical_key; supports metadata, bounded content, or content hash.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "wing": { "type": "string", "description": "Required with logical_key." },
                    "room": { "type": "string", "default": "general" },
                    "logical_key": {
                        "type": "string",
                        "description": "Stable key used with wing and room to resolve a replaceable drawer."
                    },
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
                        "default": false,
                        "description": "Omit content and return content_hash unless restricted mode redacts it. Overrides include_content."
                    }
                },
                "anyOf": [
                    { "required": ["id"] },
                    { "required": ["wing", "logical_key"] }
                ]
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
            "description": "Create a bounded Claude↔Codex planning session. task is visible in collab_status; pilot picks the planning lead (default claude); implementer defaults to pilot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "initiator": { "type": "string", "enum": ["claude", "codex"] },
                    "task": { "type": "string" },
                    "pilot": { "type": "string", "enum": ["claude", "codex"] },
                    "implementer": { "type": "string", "enum": ["claude", "codex"] }
                },
                "required": ["repo_path", "branch", "initiator"]
            }
        }),
        json!({
            "name": "collab_start_code_review",
            "description": "Create a Claude↔Codex review-only session at global review. Initiator must be claude.",
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
            "description": "Select the coding owner during planning or CodeImplementPending.",
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
            "description": "Send a collab message and advance its state machine.",
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
            "description": "Read pending collab messages. Default returns references; full includes content and auto_ack consumes returned messages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "receiver": { "type": "string", "enum": ["claude", "codex"] },
                    "limit": { "type": "integer", "default": 10 },
                    "auto_ack": { "type": "boolean", "default": false },
                    "full": {
                        "type": "boolean",
                        "default": false,
                        "description": "Include inline content with each drawer reference."
                    },
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
            "description": "Return collab state and compact plan/task-list references. Optional fields inline their bodies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "verbose": {
                        "type": "boolean",
                        "default": false,
                        "description": "Include the full canonical/final plan body beside the compact reference."
                    },
                    "include_task_list": {
                        "type": "boolean",
                        "default": false,
                        "description": "Include the full task_list JSON beside task_list_ref."
                    }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "collab_approve",
            // "Copilot" rather than "Codex": the approver is
            // `copilot(session)` — the agent that is not the session's pilot,
            // so Codex under the default `pilot=claude` and Claude under
            // `pilot=codex`. Kept to one line: the listing is under a hard
            // whole-listing token budget (see
            // `tool_listing_stays_within_prompt_cache_schema_budget`).
            "description": "Copilot-only shortcut for submitting an approve review",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    // Both agents are advertised because the accepted one is
                    // session-dependent (`copilot(session)`), which a static
                    // schema cannot express. `handle_collab_approve` does the
                    // real narrowing; a one-value enum here would block the
                    // legitimate pilot=codex caller.
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
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
            "description": "Long-poll until the agent's turn or a post-claim phase, owner, terminal, ended, or recovery-state change. Timeout returns {unchanged:true}; otherwise returns {is_my_turn, phase, current_owner, session_ended}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "timeout_secs": { "type": "integer", "default": 30, "maximum": 60 },
                    "handoff_token": { "type": "string" }
                },
                "required": ["session_id", "agent"]
            }
        }),
        json!({
            "name": "collab_end",
            "description": "End an eligible collab session; rejected during active planning or coding.",
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
            "name": "collab_resume",
            "description": "Resume a tooling-failed CodingFailed session; semantic failures remain rejected.",
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
            "description": "Write or refresh a per-area code map. Write-mode only.",
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
            "description": "Index Rust and Python into the symbol/import graph. Incremental unless force. Write-mode only.",
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
            "description": "Look up symbol declarations by name or qualified name; returns bounded metadata.",
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
            "description": "Look up import or contains symbol-graph edges by symbol or file path.",
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
            "description": "Issue or reuse a one-time handoff token and deterministic successor block. The successor claims the token on its first mutating collab call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "agent": { "type": "string", "enum": ["claude", "codex"] },
                    "handoff_token": {
                        "type": "string",
                        "description": "Required after a prior handoff claim; successor uses it to claim this generation."
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
    ensure_tool_allowed(app, name, args)?;
    // The single readiness wait for every write-shaped tool, driven off
    // `WRITE_SHAPED_TOOLS` rather than opted into by each handler. Handlers
    // used to call `wait_for_write_ready` individually, which meant a new
    // write tool could be given the wait without being added to the list the
    // framing loop dispatches on — and would then take this SYNCHRONOUS wait
    // on the thread that owns the `App`, freezing every connection for the
    // whole timeout. One list, one wait site, so that cannot be split.
    //
    // Normally a no-op: `server::dispatch_request` already awaited readiness
    // asynchronously before calling this, so the gate is resolved and this
    // returns via the fast path. It still matters for callers that reach
    // `call_tool`/`dispatch` directly.
    if is_write_shaped_tool(name) {
        // Validate first, so a malformed write is rejected outright instead of
        // waiting out the readiness timeout only to be rejected anyway.
        precheck_write_request(app, name, args)?;
        app.wait_for_write_ready()?;
    }
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
        "collab_resume" => handle_collab_resume(app, args),
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

/// The tools that must not run against a not-yet-ready embedder, and so block
/// on `ReadinessGate` instead of returning a soft `warming_up` body.
///
/// THE single source of truth for "is this a write?". Everything derives from
/// this list: the framing loop (`server::dispatch_request`) parks exactly
/// these on the gate asynchronously, `call_tool` performs the synchronous
/// fallback wait for exactly these, and `precheck_write_request` validates
/// exactly these. Adding a write-shaped tool is one edit here — no handler
/// opts itself in, so a tool cannot end up taking the synchronous wait while
/// the framing loop is unaware of it and freeze every connection for the whole
/// readiness timeout.
///
/// `write_shaped_tools_are_covered_end_to_end` pins that each entry is a known
/// tool with its own `precheck_write_request` arm;
/// `write_shaped_tools_are_a_subset_of_mutating_tools` pins the relationship to
/// `MUTATING_TOOLS` from the independent mode-gating side.
pub(crate) const WRITE_SHAPED_TOOLS: &[&str] = &["add_drawer", "diary_write", "code_map_write"];

/// Whether `name` is one of [`WRITE_SHAPED_TOOLS`].
pub(crate) fn is_write_shaped_tool(name: &str) -> bool {
    WRITE_SHAPED_TOOLS.contains(&name)
}

/// Everything about a write-shaped `tools/call` that can be rejected without
/// the server being ready: the tool existing, mode gating, and argument
/// validation.
///
/// The framing loop runs this before parking a write on the readiness gate, so
/// a malformed or forbidden call fails immediately instead of serving out the
/// whole `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` window (90s by default) and
/// only then being rejected. `call_tool` still performs all of these checks
/// itself — this is a pre-pass, never the sole enforcement point, and it
/// delegates to the same validators the handlers use so the two cannot drift
/// apart.
///
/// Deliberately runs only the *structural* half of `code_map_write`'s
/// validation: the full check shells out to `git`, and `call_tool` repeats it
/// on the far side of the gate regardless, so doing it here would fork `git`
/// twice per request — once on the pre-wait path — to reach the same verdict.
pub(crate) fn precheck_write_request(
    app: &App,
    name: &str,
    args: &Value,
) -> Result<(), MemoryError> {
    if !tool_known(name) {
        return Err(MemoryError::NotFound(format!("Unknown tool: {name}")));
    }
    ensure_tool_allowed(app, name, args)?;
    match name {
        "add_drawer" => drawers::validate_add_drawer_args(args).map(drop),
        "diary_write" => diary::validate_diary_write_args(args).map(drop),
        "code_map_write" => code_maps::precheck_code_map_write_args(args),
        _ => Ok(()),
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
            | "collab_resume"
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

/// Every tool that PERSISTS state — the superset of [`WRITE_SHAPED_TOOLS`].
///
/// Two different questions get asked about a tool and they must not be
/// confused:
///
/// - "does it persist anything?" → this list. Drives read-only mode gating and
///   the framing loop's per-connection ordering barrier.
/// - "does it need the embedder?" → [`WRITE_SHAPED_TOOLS`], a strict subset.
///   Only those have to block on the readiness gate.
///
/// The distinction matters because a mutating tool that is NOT embedder-
/// dependent skips the gate entirely. If such a tool were also allowed to skip
/// the ordering barrier, it could execute and commit while an earlier write on
/// the same connection was still parked — e.g. `delete_drawer` landing before
/// the `add_drawer` it was meant to follow, so the parked add then re-creates
/// the row the client just deleted.
///
/// `write_shaped_tools_are_a_subset_of_mutating_tools` pins the subset
/// relationship, so the two lists cannot drift apart.
pub(crate) const MUTATING_TOOLS: &[&str] = &[
    "add_drawer",
    "delete_drawer",
    "kg_add",
    "kg_invalidate",
    "diary_write",
    "collab_start",
    "collab_start_code_review",
    "collab_set_implementer",
    "collab_send",
    "collab_ack",
    "collab_approve",
    "collab_register_caps",
    "collab_end",
    "collab_resume",
    "session_handoff",
    "code_map_write",
    "symbol_graph_index",
];

/// A tool that persists state for SOME arguments and not others.
///
/// `collab_recv` is the motivating case: it is a read, but with
/// `auto_ack: true` it acks every message it returns in the same transaction
/// (`collab_session::handle_collab_recv`). Classifying it by NAME alone gets
/// one of the two cases wrong — as "read" it slipped through read-only mode
/// and past the framing loop's ordering barrier while genuinely writing; as
/// "mutating" it would stop plain `collab_recv` working in read-only mode at
/// all, which is how review agents read collab traffic without acking it.
///
/// So classification is per-CALL, not per-tool: see [`is_mutating_call`].
pub(crate) struct ConditionalMutation {
    /// The tool this describes.
    pub name: &'static str,
    /// Whether THIS call persists state. Must be a PURE function of the
    /// request: the framing loop asks it once to enqueue a mutation and again
    /// to release the barrier, and a predicate that answered differently the
    /// second time would leave `mutation_in_flight` set forever, hanging every
    /// later write on that connection with no error and no log.
    pub mutates: fn(&Value) -> bool,
    /// Named in the read-only rejection so a client learns which argument to
    /// drop, rather than being told the whole tool is disabled.
    pub trigger: &'static str,
    /// Arguments that MUST classify as a write, and as a read.
    /// `conditionally_mutating_tools_actually_flip` runs every witness through
    /// `mutates`, so an entry whose argument was renamed — or whose handler
    /// stopped writing — fails loudly instead of sitting here decoratively
    /// while `is_mutating_call` quietly returns false for every call.
    #[cfg_attr(not(test), allow(dead_code))]
    pub mutating_witnesses: &'static [&'static str],
    #[cfg_attr(not(test), allow(dead_code))]
    pub read_witnesses: &'static [&'static str],
}

/// Presence of a non-empty `handoff_token`, which makes an otherwise-read
/// collab call claim the generation lease (`handoff::claim_handoff_token`).
///
/// Delegates to `handoff::opt_handoff_token` — the same function the handlers
/// pass to `ensure_actor_generation_current` — so "is this a write?" cannot
/// drift from "does this actually claim?".
fn claims_handoff_token(args: &Value) -> bool {
    handoff::opt_handoff_token(args).is_some()
}

/// `collab_recv` writes two different ways: `auto_ack` acks every message it
/// returns, and `handoff_token` claims the generation lease.
fn collab_recv_mutates(args: &Value) -> bool {
    args.get("auto_ack")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || claims_handoff_token(args)
}

/// Every tool that writes only for certain arguments.
///
/// Both entries are collab reads that take `handoff_token`; `collab_recv` adds
/// `auto_ack`. Every OTHER `ensure_actor_generation_current` caller
/// (`collab_send`, `collab_ack`, `collab_approve`, `collab_end`,
/// `collab_set_implementer`, `collab_register_caps`) is already in
/// [`MUTATING_TOOLS`], so this list plus that one covers the whole guard.
pub(crate) const CONDITIONALLY_MUTATING_TOOLS: &[ConditionalMutation] = &[
    ConditionalMutation {
        name: "collab_recv",
        mutates: collab_recv_mutates,
        trigger: "auto_ack (acks the messages it returns) or handoff_token \
                  (claims the generation lease)",
        mutating_witnesses: &[
            r#"{"auto_ack": true}"#,
            r#"{"handoff_token": "tok"}"#,
            r#"{"auto_ack": false, "handoff_token": "tok"}"#,
        ],
        read_witnesses: &[
            "{}",
            r#"{"auto_ack": false}"#,
            r#"{"handoff_token": ""}"#,
            r#"{"handoff_token": null}"#,
        ],
    },
    ConditionalMutation {
        name: "collab_wait_my_turn",
        mutates: claims_handoff_token,
        trigger: "handoff_token (claims the generation lease)",
        mutating_witnesses: &[r#"{"handoff_token": "tok"}"#],
        // `auto_ack` is meaningless here, so it must NOT flip this tool — a
        // witness that would catch the two predicates being wired up swapped.
        read_witnesses: &["{}", r#"{"auto_ack": true}"#, r#"{"handoff_token": ""}"#],
    },
];

/// Every advertised tool that does NOT persist state under ANY arguments.
///
/// Exists solely so `write_shaped_tools_are_a_subset_of_mutating_tools` can
/// require every advertised tool to be explicitly classified. Without it a
/// newly added tool defaults to "read" by omission — permitted in read-only
/// mode and free to overtake a parked write in the framing loop's ordering
/// barrier — with nothing to catch it.
///
/// A tool that writes only for certain arguments belongs in
/// [`CONDITIONALLY_MUTATING_TOOLS`], not here.
#[cfg(test)]
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "status",
    "search",
    "get_drawer",
    "list_wings",
    "list_rooms",
    "get_taxonomy",
    "kg_query",
    "kg_timeline",
    "kg_stats",
    "traverse",
    "find_tunnels",
    "graph_stats",
    "diary_read",
    "collab_status",
    "collab_get_caps",
    "code_map_load",
    "code_map_status",
    "symbol_lookup",
    "symbol_imports",
    "symbol_neighbors",
    "symbol_graph_lookup",
    "symbol_graph_imports",
    "symbol_graph_neighbors",
];

/// Whether `name` persists state for EVERY call — see [`MUTATING_TOOLS`].
///
/// Prefer [`is_mutating_call`] anywhere the arguments are in hand: this
/// answers "is this tool always a write?", which is False for a
/// conditionally-mutating tool even on the call where it does write.
pub(crate) fn is_mutating_tool(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// The conditional-mutation entry for `name`, if it has one.
fn conditional_mutation(name: &str) -> Option<&'static ConditionalMutation> {
    CONDITIONALLY_MUTATING_TOOLS
        .iter()
        .find(|entry| entry.name == name)
}

/// Whether THIS CALL persists state: always-mutating tools, plus
/// conditionally-mutating ones whose write-triggering argument is set.
///
/// This — not [`is_mutating_tool`] — is what read-only mode gating and the
/// framing loop's ordering barrier ask, so a write cannot escape either by
/// hiding behind a read's tool name.
pub(crate) fn is_mutating_call(name: &str, args: &Value) -> bool {
    is_mutating_tool(name) || conditional_mutation(name).is_some_and(|entry| (entry.mutates)(args))
}

/// Gap between `collab_wait_my_turn` snapshot reads, for the async long-poll
/// driver in `server`.
pub(crate) const WAIT_MY_TURN_POLL_INTERVAL: std::time::Duration =
    collab_session::WAIT_MY_TURN_POLL_INTERVAL;

/// Distinct wrappers for the two `Instant`s `wait_my_turn_deadline` consumes,
/// so the adjacent same-typed arguments cannot be swapped silently.
pub(crate) use collab_session::{ArrivedAt, ClaimCommittedAt, WaitTurnBaseline};

/// Deadline for the `collab_wait_my_turn` poll loop — see
/// [`collab_session::wait_my_turn_deadline`].
pub(crate) fn wait_my_turn_deadline(
    arrived_at: ArrivedAt,
    claim_committed_at: ClaimCommittedAt,
    args: &Value,
) -> std::time::Instant {
    collab_session::wait_my_turn_deadline(arrived_at, claim_committed_at, args)
}

/// Mode-gate, validate, and settle the generation for a `collab_wait_my_turn`
/// call, once, before polling starts.
///
/// Carries the mode gating that `call_tool` would otherwise apply: the async
/// long-poll path in `server` bypasses `call_tool`, so without this a
/// `handoff_token` claim would skip read-only enforcement entirely.
pub(crate) fn wait_my_turn_begin(app: &App, args: &Value) -> Result<WaitTurnBaseline, MemoryError> {
    ensure_tool_allowed(app, "collab_wait_my_turn", args)?;
    collab_session::wait_my_turn_begin(app, args)
}

/// One non-blocking `collab_wait_my_turn` snapshot — see
/// [`collab_session::wait_my_turn_poll`].
pub(crate) fn wait_my_turn_poll(
    app: &App,
    args: &Value,
    baseline: &WaitTurnBaseline,
) -> Result<(Value, bool), MemoryError> {
    collab_session::wait_my_turn_poll(app, args, baseline)
}

/// Mode gating for a call whose arguments are known. `tool_allowed_in_mode` is
/// the name-only form used for ADVERTISING, where no arguments exist yet.
fn call_allowed_in_mode(mode: McpAccessMode, name: &str, args: &Value) -> bool {
    if !tool_known(name) {
        return false;
    }
    mode.allows_writes() || !is_mutating_call(name, args)
}

/// Whether `name` may be advertised at all in `mode`.
///
/// Deliberately argument-blind, and deliberately permissive for a
/// conditionally-mutating tool: `collab_recv` stays advertised in read-only
/// mode because plain `collab_recv` genuinely works there. The write-triggering
/// argument is rejected per call by [`call_allowed_in_mode`].
fn tool_allowed_in_mode(mode: McpAccessMode, name: &str) -> bool {
    if !tool_known(name) {
        return false;
    }
    mode.allows_writes() || !is_mutating_tool(name)
}

fn ensure_tool_allowed(app: &App, name: &str, args: &Value) -> Result<(), MemoryError> {
    if call_allowed_in_mode(app.config.mcp_access_mode, name, args) {
        return Ok(());
    }
    // A conditionally-mutating tool rejected for its ARGUMENT gets a message
    // that says so: "collab_recv is disabled" would be actively misleading when
    // dropping one argument makes the same call succeed.
    if !is_mutating_tool(name) {
        if let Some(entry) = conditional_mutation(name) {
            return Err(MemoryError::Permission(format!(
                "Tool '{name}' persists state for this call — {} — which is disabled \
                 when IRONMEM_MCP_MODE={}; call it without that argument to read \
                 without writing",
                entry.trigger,
                mode_label(app.config.mcp_access_mode),
            )));
        }
    }
    Err(MemoryError::Permission(format!(
        "Tool '{name}' is disabled when IRONMEM_MCP_MODE={}",
        mode_label(app.config.mcp_access_mode)
    )))
}

fn mode_label(mode: McpAccessMode) -> &'static str {
    match mode {
        McpAccessMode::Trusted => "trusted",
        McpAccessMode::ReadOnly => "read-only",
        McpAccessMode::Restricted => "restricted",
    }
}

#[cfg(test)]
mod tests {
    use super::shared::{
        centered_excerpt_bounds, render_search_excerpt, render_sensitive_text,
        MAX_SEARCH_EXCERPT_CHARS,
    };
    use super::*;

    /// The write-tool set used to be spelled out in three unlinked places —
    /// the framing loop's dispatch check, this module's precheck match, and
    /// the handlers themselves. Adding a fourth write tool and updating only
    /// some of them compiles cleanly and passes every other test, while the
    /// tool silently takes the SYNCHRONOUS wait inside its handler and freezes
    /// the single-owner dispatcher — and therefore every connection — for the
    /// whole readiness timeout.
    ///
    /// This pins each obligation that [`WRITE_SHAPED_TOOLS`] now carries, so a
    /// new entry cannot be added half-wired.
    /// The converse of the test below, and the direction that actually bites.
    ///
    /// `write_shaped_tools_are_covered_end_to_end` iterates
    /// `WRITE_SHAPED_TOOLS`, so it can only ever check entries that are already
    /// there — it is blind to the dangerous case, a tool that SHOULD be in the
    /// list and is not. This checks the relationship from the independent
    /// `MUTATING_TOOLS` side, which is also what read-only mode gating uses, so
    /// the two lists cannot drift into disagreeing about what a write is.
    ///
    /// The remaining risk — a mutating tool that needs the embedder but is
    /// absent from `WRITE_SHAPED_TOOLS` — is no longer silent: since
    /// `App::ensure_embedder_ready` fails closed, such a tool errors during
    /// warm-up instead of persisting an all-zero vector.
    #[test]
    fn write_shaped_tools_are_a_subset_of_mutating_tools() {
        for name in WRITE_SHAPED_TOOLS {
            assert!(
                is_mutating_tool(name),
                "{name} blocks on the readiness gate but is not in MUTATING_TOOLS, \
                 so read-only mode would let it through and the framing loop would \
                 not hold it in connection order"
            );
        }
        for name in MUTATING_TOOLS {
            assert!(
                tool_known(name),
                "{name} is listed as mutating but is not a known tool"
            );
        }

        // The direction that actually bites: enumerate every tool the server
        // ADVERTISES — an source independent of both constants — and require
        // each one to be explicitly classified. A newly added tool that nobody
        // classified fails here instead of silently defaulting to "read", which
        // would let it through read-only mode AND let it overtake a parked
        // write in the framing loop's ordering barrier.
        //
        // `tool_allowed_in_mode` is NOT usable for this: it now derives from
        // `is_mutating_tool`, so asserting against it would be the code's own
        // output fed back to itself.
        let app = App::open_for_test().unwrap();
        for tool in tool_definitions(&app) {
            let name = tool["name"].as_str().expect("advertised tool needs a name");
            let classifications = [
                is_mutating_tool(name),
                conditional_mutation(name).is_some(),
                READ_ONLY_TOOLS.contains(&name),
            ];
            let count = classifications.iter().filter(|held| **held).count();
            assert_eq!(
                count, 1,
                "{name} is advertised but has {count} classifications; it must be in \
                 EXACTLY one of MUTATING_TOOLS, CONDITIONALLY_MUTATING_TOOLS, or \
                 READ_ONLY_TOOLS (got mutating={}, conditional={}, read-only={})",
                classifications[0], classifications[1], classifications[2]
            );
        }
    }

    #[test]
    fn search_tool_metadata_declares_excerpt_default_and_full_paths() {
        let app = App::open_for_test().unwrap();
        let search = tool_definitions(&app)
            .into_iter()
            .find(|tool| tool["name"] == "search")
            .expect("search tool must be advertised");

        let full = &search["inputSchema"]["properties"]["full"];
        assert_eq!(full["type"], "boolean");
        assert_eq!(full["default"], false);

        let include_superseded = &search["inputSchema"]["properties"]["include_superseded"];
        assert_eq!(include_superseded["type"], "boolean");
        assert_eq!(include_superseded["default"], false);
        let include_superseded_description = include_superseded["description"]
            .as_str()
            .expect("include_superseded property needs a description");
        assert!(include_superseded_description.contains("false"));
        assert!(include_superseded_description.contains("superseded"));
        assert!(include_superseded_description.contains("history"));
        assert!(include_superseded_description.contains("true"));

        let description = search["description"]
            .as_str()
            .expect("search tool needs a description");
        assert!(description.contains("Returns bounded excerpts and stable IDs"));
        assert!(description.contains("use get_drawer for a complete body"));

        let full_description = full["description"]
            .as_str()
            .expect("full property needs a description");
        assert!(full_description.contains("Return bounded full content"));
        assert!(full_description.contains("use get_drawer for the complete body"));
    }

    /// Raised 3_500 -> 3_550 for the v17 supersession surface (#211): `search`
    /// gained `include_superseded` and `add_drawer` gained `supersedes`, which
    /// cost ~140 bytes more than the previous ceiling left spare.
    ///
    /// The budget is deliberately a whole-listing ceiling with no per-tool
    /// allocation, so the cheapest way to land a new field is to delete prose
    /// from whichever unrelated tool happens to be wordiest. That trade is not
    /// acceptable: raise this constant when new capability genuinely needs the
    /// room, and pay for it by moving prose into real schema keys (`default`,
    /// `maximum`) — not by silently degrading a neighbour's description.
    #[test]
    fn tool_listing_stays_within_prompt_cache_schema_budget() {
        let app = App::open_for_test().unwrap();
        let bytes = serde_json::to_vec(&tool_definitions(&app)).unwrap().len();
        let estimated_tokens = bytes.div_ceil(4);
        assert!(
            estimated_tokens <= 3_550,
            "tool listing is ~{estimated_tokens} tokens ({bytes} bytes); trim descriptions that duplicate their schemas"
        );
    }

    /// A conditionally-mutating entry is only worth anything if the argument it
    /// names actually flips the classification. A stale entry — the argument
    /// renamed, or the handler's write removed — would otherwise sit here
    /// looking like enforcement while `is_mutating_call` returned false for
    /// every call, silently restoring the read-only bypass this list exists to
    /// close.
    #[test]
    fn conditionally_mutating_tools_actually_flip() {
        for entry in CONDITIONALLY_MUTATING_TOOLS {
            let name = entry.name;
            assert!(
                tool_known(name),
                "{name} is listed as conditionally mutating but is not a known tool"
            );
            assert!(
                !is_mutating_tool(name),
                "{name} is in CONDITIONALLY_MUTATING_TOOLS and MUTATING_TOOLS; the \
                 conditional entry is then dead — it is already always a write"
            );
            assert!(
                !entry.mutating_witnesses.is_empty() && !entry.read_witnesses.is_empty(),
                "{name} needs witnesses in BOTH directions, or this test cannot tell \
                 a working predicate from one stuck at a constant"
            );

            for witness in entry.mutating_witnesses {
                let args: Value = serde_json::from_str(witness)
                    .unwrap_or_else(|e| panic!("{name} witness {witness} must parse: {e}"));
                assert!(
                    is_mutating_call(name, &args),
                    "{name}{witness} must classify as a WRITE, or it bypasses read-only \
                     mode and the framing loop's ordering barrier while genuinely writing"
                );
            }

            for witness in entry.read_witnesses {
                let args: Value = serde_json::from_str(witness)
                    .unwrap_or_else(|e| panic!("{name} witness {witness} must parse: {e}"));
                assert!(
                    !is_mutating_call(name, &args),
                    "{name}{witness} must classify as a READ, or plain calls stop \
                     working in read-only mode and queue behind the write barrier"
                );
            }
        }
    }

    /// The generation-lease claim is a write, and it hides behind two tools
    /// whose names read as pure queries. Pinned separately from the witness
    /// loop because the danger is a tool being absent from
    /// `CONDITIONALLY_MUTATING_TOOLS` entirely — which the loop, iterating that
    /// same list, is structurally blind to.
    #[test]
    fn handoff_token_makes_every_read_shaped_collab_tool_a_write() {
        let claiming = json!({"handoff_token": "tok"});
        for name in ["collab_recv", "collab_wait_my_turn"] {
            assert!(
                is_mutating_call(name, &claiming),
                "{name} with a handoff_token claims the generation lease \
                 (handoff::claim_handoff_token) — it must classify as a write"
            );
            assert!(
                !is_mutating_call(name, &json!({})),
                "{name} without a handoff_token is a read"
            );
        }
    }

    /// Builds a ReadOnly-mode `App` over a fresh temporary DB.
    fn read_only_app() -> (App, tempfile::TempDir) {
        use crate::config::{Config, EmbedMode};
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        };
        (App::new(config).unwrap(), dir)
    }

    /// The bypass this classification change exists to close, asserted through
    /// the ENFORCEMENT path.
    ///
    /// The predicate test below is NOT sufficient on its own, and that is the
    /// whole reason this test exists: `call_allowed_in_mode` is a pure
    /// function, so an `ensure_tool_allowed` that ignores its `args` and gates
    /// on the tool name alone — precisely the pre-fix behavior, and precisely
    /// the bypass — leaves a predicate-only test green. This drives
    /// `call_tool`, the path a real MCP request actually takes.
    #[test]
    fn read_only_mode_refuses_auto_ack_through_the_enforcement_path() {
        let (app, _dir) = read_only_app();

        let refused = call_tool(
            &app,
            "collab_recv",
            &json!({"session_id": "s", "receiver": "claude", "auto_ack": true}),
        )
        .expect_err("auto_ack acks the messages it returns; read-only must refuse it");
        assert!(
            matches!(refused, MemoryError::Permission(_)),
            "expected a Permission refusal, got {refused:?}"
        );
        assert!(
            refused.to_string().contains("auto_ack"),
            "the refusal must name the offending argument so a client knows what to \
             drop; got {refused}"
        );

        // The control: a plain recv must get PAST mode gating. It may still fail
        // on the session not existing — what matters is that the failure is not a
        // Permission refusal, since that is what proves the gate let it through.
        let plain = call_tool(
            &app,
            "collab_recv",
            &json!({"session_id": "s", "receiver": "claude"}),
        );
        assert!(
            !matches!(plain, Err(MemoryError::Permission(_))),
            "a plain collab_recv is a read and must pass read-only gating; got {plain:?}"
        );

        // The async long-poll path in `server` bypasses `call_tool` entirely and
        // therefore carries its own gating call. Pin it here, or the daemon path
        // can lose read-only enforcement while `call_tool` still has it.
        let via_long_poll = wait_my_turn_begin(
            &app,
            &json!({"session_id": "s", "agent": "claude", "handoff_token": "tok"}),
        );
        assert!(
            matches!(via_long_poll, Err(MemoryError::Permission(_))),
            "the async long-poll path must apply the same read-only gating as \
             call_tool; got {via_long_poll:?}"
        );
    }

    /// Predicate-level companion to the enforcement test above. Kept because it
    /// pins the classification itself, but it is deliberately NOT the only
    /// coverage — see that test for why.
    #[test]
    fn read_only_mode_refuses_auto_ack_but_allows_plain_recv() {
        assert!(
            call_allowed_in_mode(McpAccessMode::ReadOnly, "collab_recv", &json!({})),
            "plain collab_recv is a read and must stay available in read-only mode"
        );
        assert!(
            call_allowed_in_mode(
                McpAccessMode::ReadOnly,
                "collab_recv",
                &json!({"auto_ack": false})
            ),
            "collab_recv with auto_ack:false is a read"
        );
        assert!(
            !call_allowed_in_mode(
                McpAccessMode::ReadOnly,
                "collab_recv",
                &json!({"auto_ack": true})
            ),
            "collab_recv with auto_ack:true acks messages — read-only mode must refuse it"
        );
        assert!(
            call_allowed_in_mode(
                McpAccessMode::Trusted,
                "collab_recv",
                &json!({"auto_ack": true})
            ),
            "trusted mode allows writes, so auto_ack is fine there"
        );

        // Still ADVERTISED in read-only mode: the tool is usable there, just not
        // with that argument. Dropping it from the list would hide the read.
        assert!(
            tool_allowed_in_mode(McpAccessMode::ReadOnly, "collab_recv"),
            "collab_recv must remain advertised in read-only mode"
        );
    }

    /// Each entry is a real tool with its own `precheck_write_request` arm, so
    /// a malformed write fails fast instead of waiting out the readiness
    /// timeout only to be rejected anyway. The complementary direction — a
    /// tool that should be classified and is not — is
    /// `write_shaped_tools_are_a_subset_of_mutating_tools`.
    #[test]
    fn write_shaped_tools_are_covered_end_to_end() {
        let app = App::open_for_test().unwrap();

        for name in WRITE_SHAPED_TOOLS {
            assert!(
                tool_known(name),
                "{name} is listed as write-shaped but is not a known tool"
            );
            // A dedicated precheck arm, not the `_ => Ok(())` fallthrough:
            // empty arguments must be rejected on their own merits, so a
            // malformed write fails fast instead of waiting out the gate.
            let precheck = precheck_write_request(&app, name, &json!({}));
            assert!(
                matches!(precheck, Err(MemoryError::Validation(_))),
                "{name} needs its own arm in precheck_write_request; empty args \
                 produced {precheck:?} instead of a validation error"
            );
        }

        // The converse: a read-shaped tool must NOT be parked on the gate,
        // or `search` would block during warm-up instead of returning its
        // soft body.
        for name in ["search", "status", "get_drawer", "code_map_load"] {
            assert!(
                !is_write_shaped_tool(name),
                "{name} is read-shaped and must not block on readiness"
            );
        }
    }

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
    fn add_drawer_schema_documents_temporal_supersession() {
        let app = App::open_for_test().unwrap();
        let add_drawer = tool_definitions(&app)
            .into_iter()
            .find(|tool| tool["name"] == "add_drawer")
            .expect("add_drawer must be advertised");
        let supersedes = &add_drawer["inputSchema"]["properties"]["supersedes"];

        assert_eq!(supersedes["type"].as_str(), Some("string"));
        let description = supersedes["description"]
            .as_str()
            .expect("supersedes must have a description");
        assert!(
            description.contains("retained") && description.contains("retrievable by ID"),
            "schema must make clear that supersession retains temporal history"
        );
        assert!(
            !description.contains("Task"),
            "public schema must describe current behavior, not an internal roadmap"
        );
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

    #[test]
    fn render_search_excerpt_redacts_with_sensitive_text_parity() {
        let result = render_search_excerpt("abcdef", "abc", MAX_SEARCH_EXCERPT_CHARS, true);

        assert_eq!(result, (Value::Null, false, true, 0));
    }

    #[test]
    fn render_search_excerpt_empty_query_returns_truncated_prefix() {
        let content = "a".repeat(MAX_SEARCH_EXCERPT_CHARS * 2);

        let (excerpt, truncated, redacted, consumed) =
            render_search_excerpt(&content, "", MAX_SEARCH_EXCERPT_CHARS, false);

        assert_eq!(excerpt, Value::String("a".repeat(MAX_SEARCH_EXCERPT_CHARS)));
        assert!(truncated);
        assert!(!redacted);
        assert_eq!(consumed, MAX_SEARCH_EXCERPT_CHARS);
    }

    #[test]
    fn render_search_excerpt_short_body_is_not_truncated() {
        let (excerpt, truncated, redacted, consumed) =
            render_search_excerpt("short body", "", MAX_SEARCH_EXCERPT_CHARS, false);

        assert_eq!(excerpt, Value::String("short body".into()));
        assert!(!truncated);
        assert!(!redacted);
        assert_eq!(consumed, "short body".chars().count());
    }

    #[test]
    fn render_search_excerpt_centers_on_late_case_insensitive_match() {
        let content = format!("{}needle {}", "prefix ".repeat(430), "suffix ".repeat(430));
        let result = render_search_excerpt(&content, "the NEEDLE", 96, false);

        assert_eq!(
            result,
            render_search_excerpt(&content, "the NEEDLE", 96, false)
        );
        let excerpt = result.0.as_str().expect("excerpt should be a string");
        assert!(excerpt.contains("needle"));
        assert!(excerpt.starts_with('…'));
        assert!(excerpt.ends_with('…'));
        assert!(result.1);
        assert!(!result.2);
        assert!(excerpt.chars().count() <= 96);
        assert_eq!(result.3, excerpt.chars().count());
    }

    #[test]
    fn render_search_excerpt_handles_utf8_match_near_the_end() {
        let content = format!("{}目标{}", "前🙂".repeat(180), "尾😀".repeat(40));
        let result = render_search_excerpt(&content, "目标", 32, false);

        let excerpt = result.0.as_str().expect("excerpt should be a string");
        assert!(excerpt.contains("目标"));
        assert_eq!(excerpt, excerpt.chars().collect::<String>());
        assert!(excerpt.chars().count() <= 32);
        assert_eq!(result.3, excerpt.chars().count());
    }

    #[test]
    fn render_search_excerpt_markers_stay_within_every_small_budget() {
        let content = "prefix target suffix";

        for max_chars in 0..=20 {
            let (excerpt, _, _, consumed) =
                render_search_excerpt(content, "target", max_chars, false);
            let excerpt = excerpt.as_str().expect("excerpt should be a string");
            assert!(excerpt.chars().count() <= max_chars);
            assert_eq!(consumed, excerpt.chars().count());
        }
    }

    #[test]
    fn centered_excerpt_bounds_anchors_an_interior_single_char_budget() {
        let chars: Vec<char> = "prefix target suffix".chars().collect();
        let match_start = "prefix ".chars().count();
        let match_end = match_start + "target".chars().count();
        let midpoint = match_start + (match_end - match_start) / 2;

        assert_eq!(
            centered_excerpt_bounds(&chars, match_start, match_end, 1),
            (midpoint, midpoint)
        );

        let (excerpt, truncated, redacted, consumed) =
            render_search_excerpt("prefix target suffix", "target", 1, false);
        assert_eq!(excerpt, Value::String("…".into()));
        assert!(truncated);
        assert!(!redacted);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn render_search_excerpt_centers_partial_long_match() {
        let long_query = "abcdefghijklmnop";
        let content = format!("{}{}{}", "x".repeat(40), long_query, "y".repeat(40));
        let (excerpt, truncated, redacted, consumed) =
            render_search_excerpt(&content, long_query, 12, false);

        assert_eq!(excerpt, Value::String("…defghijklm…".into()));
        assert!(truncated);
        assert!(!redacted);
        assert_eq!(consumed, 12);
    }

    #[test]
    fn render_search_excerpt_normalizes_punctuation_bearing_query_tokens() {
        let content = format!("{}needle{}", "x".repeat(400), "y".repeat(400));
        let (excerpt, truncated, redacted, consumed) =
            render_search_excerpt(&content, "\"Needle?,\"", 48, false);
        let excerpt = excerpt.as_str().expect("excerpt should be a string");

        assert!(excerpt.contains("needle"));
        assert!(excerpt.starts_with('…'));
        assert!(excerpt.ends_with('…'));
        assert!(truncated);
        assert!(!redacted);
        assert_eq!(consumed, excerpt.chars().count());
    }

    #[test]
    fn render_search_excerpt_whitespace_snap_stops_at_fifteen_chars() {
        let prefix_at_fifteen = format!("{} {}", "x".repeat(100), "x".repeat(31));
        let content_at_fifteen = format!("{prefix_at_fifteen}needle{}", "y".repeat(100));
        let (excerpt_at_fifteen, _, _, _) =
            render_search_excerpt(&content_at_fifteen, "needle", 42, false);
        let excerpt_at_fifteen = excerpt_at_fifteen
            .as_str()
            .expect("excerpt should be a string");
        assert!(excerpt_at_fifteen.starts_with("… "));

        let prefix_at_sixteen = format!("{} {}", "x".repeat(100), "x".repeat(32));
        let content_at_sixteen = format!("{prefix_at_sixteen}needle{}", "y".repeat(100));
        let (excerpt_at_sixteen, _, _, _) =
            render_search_excerpt(&content_at_sixteen, "needle", 42, false);
        let excerpt_at_sixteen = excerpt_at_sixteen
            .as_str()
            .expect("excerpt should be a string");
        assert!(excerpt_at_sixteen.starts_with("…x"));
    }

    /// `compact::COMPACTABLE_TOOLS` opts tools into response compaction
    /// independently of this module's mutating/read-only classification, but
    /// it still names TOOLS — an entry that is not a real, advertised tool
    /// would be silently inert (`should_compact` would gate on a name
    /// `tools/call` can never receive) instead of failing loudly at the one
    /// place that would catch a typo or a renamed tool.
    #[test]
    fn compactable_tools_are_known_tools() {
        for name in crate::mcp::compact::COMPACTABLE_TOOLS {
            assert!(
                tool_known(name),
                "{name} is listed in COMPACTABLE_TOOLS but is not a known tool"
            );
        }
    }
}
