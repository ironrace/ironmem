//! MCP tool definitions and dispatch.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::app::App;
use crate::bootstrap::MEMORY_PROTOCOL;
use crate::collab::queue::{Capability, SessionRecord};
use crate::collab::{apply_event, CollabError, CollabEvent, Phase};
use crate::config::McpAccessMode;
use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::SearchFilters;
use crate::diary;
use crate::error::MemoryError;
use crate::sanitize;
use crate::search;

/// Maximum allowed value for search `limit`.
const MAX_SEARCH_LIMIT: usize = 25;
/// Maximum allowed value for list/read `limit` parameters.
const MAX_READ_LIMIT: usize = 100;
/// Maximum allowed BFS traversal depth.
const MAX_DEPTH: usize = 10;
/// Maximum characters returned per sensitive text field.
const MAX_SENSITIVE_FIELD_CHARS: usize = 4_000;
/// Maximum aggregate characters returned across search results.
const MAX_SEARCH_RESPONSE_CHARS: usize = 32_000;
/// Maximum content length accepted by collab queue messages.
const MAX_COLLAB_CONTENT_CHARS: usize = 32_000;
/// Maximum capability field length.
const MAX_COLLAB_CAP_FIELD_CHARS: usize = 512;

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
            "description": "Send a collab message and advance the bounded state machine. v1 planning topics: draft, canonical, review, final. v2 coding topics: task_list, implement, review, verdict, comment, final, review_local, review_global, verdict_global, comment_global, final_review, pr_opened, failure_report. Topics review and final are phase-dispatched (v1 vs v2 semantics chosen by current phase).",
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

// ── Tool handlers ────────────────────────────────────────────────────────────

fn handle_status(app: &App) -> Result<Value, MemoryError> {
    let total = app.db.count_drawers(None)?;
    let wings = app.db.wing_counts()?;
    let kg = KnowledgeGraph::new(&app.db);
    let kg_stats = kg.stats()?;

    Ok(json!({
        "total_drawers": total,
        "wings": wings.into_iter().collect::<std::collections::HashMap<_, _>>(),
        "knowledge_graph": kg_stats,
        "memory_protocol": MEMORY_PROTOCOL,
        "warming_up": app.is_warming_up(),
    }))
}

fn handle_search(app: &App, args: &Value) -> Result<Value, MemoryError> {
    if app.is_warming_up() {
        return Ok(json!({
            "warming_up": true,
            "message": "Memory server is initializing. Search will be available shortly.",
            "results": [],
        }));
    }
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("query is required".into()))?;

    let filters = SearchFilters {
        wing: args.get("wing").and_then(|v| v.as_str()).map(String::from),
        room: args.get("room").and_then(|v| v.as_str()).map(String::from),
        limit: (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize)
            .min(MAX_SEARCH_LIMIT),
    };

    let result = search::pipeline::search(app, query, &filters)?;

    let mut remaining_content_budget = MAX_SEARCH_RESPONSE_CHARS;
    let redact_content = app.config.mcp_access_mode.redacts_sensitive_content();

    let results: Vec<Value> = result
        .results
        .iter()
        .map(|sd| {
            let (content, truncated, redacted, consumed_chars) = render_sensitive_text(
                &sd.drawer.content,
                remaining_content_budget.min(MAX_SENSITIVE_FIELD_CHARS),
                redact_content,
            );
            remaining_content_budget = remaining_content_budget.saturating_sub(consumed_chars);
            json!({
                "id": sd.drawer.id,
                "content": content,
                "content_truncated": truncated,
                "content_redacted": redacted,
                "wing": sd.drawer.wing,
                "room": sd.drawer.room,
                "score": sd.score,
                "date": sd.drawer.date,
            })
        })
        .collect();

    Ok(json!({
        "results": results,
        "total_candidates": result.total_candidates,
        "query_sanitized": result.sanitizer_info.was_sanitized,
        "sanitizer_method": result.sanitizer_info.method,
    }))
}

fn handle_add_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    if app.is_warming_up() {
        return Ok(json!({
            "warming_up": true,
            "message": "Memory server is initializing. Please retry in a moment.",
        }));
    }
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("content is required".into()))?;
    let wing = args
        .get("wing")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("wing is required".into()))?;
    let room = args
        .get("room")
        .and_then(|v| v.as_str())
        .unwrap_or("general");

    let content = sanitize::sanitize_content(content, 100_000)?;
    let wing = sanitize::sanitize_name(wing, "wing")?;
    let room = sanitize::sanitize_name(room, "room")?;

    let id = crate::db::drawers::generate_id(content, &wing, &room);

    // Ensure real embedder is loaded before embedding (no-op after first call).
    app.ensure_embedder_ready()?;

    let embedding = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        emb.embed_one(content).map_err(MemoryError::Embed)?
    };

    app.db.with_transaction(|tx| {
        crate::db::schema::Database::insert_drawer_tx(
            tx, &id, content, &embedding, &wing, &room, "", "mcp",
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "add_drawer",
            &json!({"id": &id, "wing": &wing, "room": &room}),
            None,
        )?;
        Ok(())
    })?;

    app.insert_into_index(&id, &embedding)?;

    Ok(json!({
        "success": true,
        "id": id,
        "wing": wing,
        "room": room,
    }))
}

fn handle_delete_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("id is required".into()))?;
    validate_hex_id(id, "id")?;

    let deleted = app.db.with_transaction(|tx| {
        let deleted = crate::db::schema::Database::delete_drawer_tx(tx, id)?;
        crate::db::schema::Database::wal_log_tx(tx, "delete_drawer", &json!({"id": id}), None)?;
        Ok(deleted)
    })?;

    if deleted {
        app.mark_dirty();
    }

    Ok(json!({ "success": deleted, "id": id }))
}

fn handle_list_wings(app: &App) -> Result<Value, MemoryError> {
    let wings = app.db.wing_counts()?;
    Ok(json!({
        "wings": wings.into_iter().collect::<std::collections::HashMap<_, _>>()
    }))
}

fn handle_list_rooms(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let wing = match args.get("wing").and_then(|v| v.as_str()) {
        Some(w) => Some(sanitize::sanitize_name(w, "wing")?),
        None => None,
    };
    let rooms = app.db.room_counts(wing.as_deref())?;
    Ok(json!({
        "wing": wing.as_deref().unwrap_or("all"),
        "rooms": rooms.into_iter().collect::<std::collections::HashMap<_, _>>()
    }))
}

fn handle_get_taxonomy(app: &App) -> Result<Value, MemoryError> {
    let taxonomy = app.db.taxonomy()?;
    Ok(json!({ "taxonomy": taxonomy }))
}

fn handle_kg_add(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("subject is required".into()))?;
    let predicate = args
        .get("predicate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("predicate is required".into()))?;
    let object = args
        .get("object")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("object is required".into()))?;

    let subject = sanitize::sanitize_name(subject, "subject")?;
    let predicate = sanitize::sanitize_name(predicate, "predicate")?;
    let object = sanitize::sanitize_name(object, "object")?;

    let subject_type_raw = args
        .get("subject_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let object_type_raw = args
        .get("object_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let subject_type = sanitize::sanitize_name(subject_type_raw, "subject_type")?;
    let object_type = sanitize::sanitize_name(object_type_raw, "object_type")?;

    let valid_from = args.get("valid_from").and_then(|v| v.as_str());
    if let Some(vf) = valid_from {
        validate_date_format(vf, "valid_from")?;
    }
    let confidence = match args.get("confidence").and_then(|v| v.as_f64()) {
        None => 1.0,
        Some(c) if c.is_finite() && (0.0..=1.0).contains(&c) => c,
        Some(bad) => {
            return Err(MemoryError::Validation(format!(
                "confidence must be a finite number between 0.0 and 1.0, got {bad}"
            )))
        }
    };

    let source_closet = match args.get("source_closet").and_then(|v| v.as_str()) {
        Some(sc) => Some(sanitize::sanitize_name(sc, "source_closet")?),
        None => None,
    };

    let id = app.db.with_transaction(|tx| {
        let triple_id = KnowledgeGraph::add_triple_tx(
            tx,
            &subject,
            &subject_type,
            &predicate,
            &object,
            &object_type,
            valid_from,
            confidence,
            source_closet.as_deref(),
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "kg_add",
            &json!({
                "triple_id": &triple_id,
                "subject": &subject,
                "subject_type": &subject_type,
                "predicate": &predicate,
                "object": &object,
                "object_type": &object_type
            }),
            None,
        )?;
        Ok(triple_id)
    })?;

    Ok(json!({ "success": true, "triple_id": id }))
}

fn handle_kg_query(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let entity_name = args
        .get("entity")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("entity is required".into()))?;
    let entity_name = sanitize::sanitize_name(entity_name, "entity")?;
    let entity_type = args
        .get("entity_type")
        .and_then(|v| v.as_str())
        .map(|value| sanitize::sanitize_name(value, "entity_type"))
        .transpose()?;

    let kg = KnowledgeGraph::new(&app.db);
    let entity = kg.resolve_entity(&entity_name, entity_type.as_deref())?;
    let triples = kg.query_entity_current(&entity.id)?;

    Ok(json!({
        "entity": {
            "id": entity.id,
            "name": entity.name,
            "entity_type": entity.entity_type,
        },
        "triples": triples,
    }))
}

fn handle_kg_invalidate(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let triple_id = args
        .get("triple_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("triple_id is required".into()))?;
    validate_hex_id(triple_id, "triple_id")?;
    let now_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let valid_to = args
        .get("valid_to")
        .and_then(|v| v.as_str())
        .unwrap_or(&now_str);
    validate_date_format(valid_to, "valid_to")?;

    let invalidated = app.db.with_transaction(|tx| {
        let updated = KnowledgeGraph::invalidate_triple_tx(tx, triple_id, valid_to)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "kg_invalidate",
            &json!({"triple_id": triple_id, "valid_to": valid_to, "success": updated}),
            None,
        )?;
        Ok(updated)
    })?;

    Ok(json!({ "success": invalidated, "triple_id": triple_id }))
}

fn handle_kg_timeline(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let entity = args
        .get("entity")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("entity is required".into()))?;
    let entity = sanitize::sanitize_name(entity, "entity")?;
    let entity_type = args
        .get("entity_type")
        .and_then(|v| v.as_str())
        .map(|value| sanitize::sanitize_name(value, "entity_type"))
        .transpose()?;

    let kg = KnowledgeGraph::new(&app.db);
    let resolved = kg.resolve_entity(&entity, entity_type.as_deref())?;
    let timeline = kg.timeline_for_entity_id(&resolved.id)?;

    Ok(json!({
        "entity": {
            "id": resolved.id,
            "name": resolved.name,
            "entity_type": resolved.entity_type,
        },
        "timeline": timeline
    }))
}

fn handle_kg_stats(app: &App) -> Result<Value, MemoryError> {
    let kg = KnowledgeGraph::new(&app.db);
    kg.stats()
}

fn handle_traverse(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let room = args
        .get("room")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("room is required".into()))?;
    let room = sanitize::sanitize_name(room, "room")?;
    let max_depth =
        (args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize).min(MAX_DEPTH);

    let result = search::graph::traverse(app, &room, max_depth)?;
    Ok(serde_json::to_value(result)?)
}

fn handle_find_tunnels(app: &App) -> Result<Value, MemoryError> {
    let tunnels = search::graph::find_tunnels(app)?;
    Ok(json!({ "tunnels": tunnels }))
}

fn handle_graph_stats(app: &App) -> Result<Value, MemoryError> {
    let stats = search::graph::graph_stats(app)?;
    Ok(serde_json::to_value(stats)?)
}

fn handle_diary_write(app: &App, args: &Value) -> Result<Value, MemoryError> {
    if app.is_warming_up() {
        return Ok(json!({
            "warming_up": true,
            "message": "Memory server is initializing. Please retry in a moment.",
        }));
    }
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("content is required".into()))?;
    let wing = args.get("wing").and_then(|v| v.as_str()).unwrap_or("diary");
    app.ensure_embedder_ready()?;
    let entry = diary::write_entry(app, content, wing, "diary", 100_000)?;
    app.db.wal_log(
        "diary_write",
        &json!({"id": &entry.id, "wing": &entry.wing}),
        None,
    )?;

    Ok(json!({ "success": true, "id": entry.id, "wing": entry.wing }))
}

fn handle_diary_read(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let wing_raw = args.get("wing").and_then(|v| v.as_str()).unwrap_or("diary");
    let wing = sanitize::sanitize_name(wing_raw, "wing")?;
    let limit =
        (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize).min(MAX_READ_LIMIT);
    let redact_content = app.config.mcp_access_mode.redacts_sensitive_content();

    let drawers = app
        .db
        .get_drawers(Some(&wing), Some(diary::DIARY_ROOM), limit)?;

    let entries: Vec<Value> = drawers
        .iter()
        .map(|d| {
            let (content, truncated, redacted, _) =
                render_sensitive_text(&d.content, MAX_SENSITIVE_FIELD_CHARS, redact_content);
            json!({
                "id": d.id,
                "content": content,
                "content_truncated": truncated,
                "content_redacted": redacted,
                "filed_at": d.filed_at,
                "date": d.date,
            })
        })
        .collect();

    Ok(json!({ "entries": entries, "count": entries.len() }))
}

// ── Collab protocol handlers ─────────────────────────────────────────────────

fn handle_collab_start(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_path = require_str(args, "repo_path")?;
    let branch = require_str(args, "branch")?;
    let initiator = require_agent(require_str(args, "initiator")?)?;
    let task_owned = args
        .get("task")
        .and_then(Value::as_str)
        .map(|value| sanitize::sanitize_content(value, MAX_COLLAB_CONTENT_CHARS))
        .transpose()?
        .map(ToString::to_string);
    let task = task_owned.as_deref();
    let session_id = uuid::Uuid::new_v4().to_string();

    app.db.with_transaction(|tx| {
        crate::collab::queue::create_session(tx, &session_id, repo_path, branch, task)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_start",
            &json!({
                "session_id": session_id,
                "repo_path": repo_path,
                "branch": branch,
                "initiator": initiator,
                "has_task": task.is_some(),
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "session_id": session_id, "task": task }))
}

fn handle_collab_send(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let sender = require_agent(require_str(args, "sender")?)?;
    let topic = require_str(args, "topic")?;
    let content =
        sanitize::sanitize_content(require_str(args, "content")?, MAX_COLLAB_CONTENT_CHARS)?;
    if !is_known_collab_topic(topic) {
        return Err(MemoryError::Validation(format!(
            "unknown collab topic: {topic}"
        )));
    }

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let mut session = crate::collab::queue::load_session_record(tx, session_id)?.session;
        let phase_before = session.phase.to_string();

        // Upstream turn gate: reject sends from the non-owner before any
        // payload parsing or event dispatch. Two carve-outs:
        //   1. `PlanParallelDrafts` — both agents submit drafts
        //      independently; current_owner there is a "next-expected" hint
        //      and the state-machine arm uses its own "already-submitted"
        //      guard.
        //   2. `failure_report` with a `branch_drift:` prefix — either agent
        //      must be able to abort the session when they detect branch
        //      drift, even if it is not their turn. The deeper check in
        //      `apply_event` validates the prefix and rejects generic
        //      off-turn failure reports as NotYourTurn.
        let turn_exempt = matches!(session.phase, crate::collab::Phase::PlanParallelDrafts)
            || (topic == "failure_report"
                && sender != session.current_owner
                && failure_report_is_branch_drift(content));
        if !turn_exempt && sender != session.current_owner {
            return Err(MemoryError::Validation(format!(
                "not your turn: phase {} expects sender '{}', got '{}'",
                session.phase, session.current_owner, sender
            )));
        }

        let event = build_collab_event(topic, content, &session)?;

        session = apply_event(&session, sender, &event).map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;

        let message_id = crate::collab::queue::send_message(
            tx,
            session_id,
            sender,
            other_agent(sender),
            topic,
            content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_send",
            &json!({
                "session_id": session_id,
                "sender": sender,
                "topic": topic,
                "phase_before": phase_before,
            }),
            Some(&json!({
                "message_id": message_id,
                "phase": session.phase.to_string(),
            })),
        )?;

        Ok(json!({
            "message_id": message_id,
            "phase": session.phase.to_string(),
        }))
    })
}

fn handle_collab_recv(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let receiver = require_agent(require_str(args, "receiver")?)?;
    let limit = (args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize).min(50);

    // Blind-drafts invariant: during PlanParallelDrafts, an agent must not see
    // the counterpart's draft until it has submitted its own. This enforces
    // the "parallel" in parallel drafts at the server boundary so the protocol
    // doesn't rely on agent-side discipline alone.
    let session = app.db.collab_load_session(session_id)?;
    let suppress_drafts = matches!(session.phase, crate::collab::Phase::PlanParallelDrafts)
        && match receiver {
            "claude" => session.claude_draft_hash.is_none(),
            "codex" => session.codex_draft_hash.is_none(),
            _ => false,
        };

    let messages = app.db.collab_recv_messages(session_id, receiver, limit)?;
    let filtered: Vec<Value> = messages
        .into_iter()
        .filter(|message| !(suppress_drafts && message.topic == "draft"))
        .map(|message| {
            json!({
                "id": message.id,
                "sender": message.sender,
                "topic": message.topic,
                "content": message.content,
                "created_at": message.created_at,
            })
        })
        .collect();
    Ok(json!({ "messages": filtered }))
}

fn handle_collab_ack(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let message_id = require_str(args, "message_id")?;
    let session_id = require_str(args, "session_id")?;
    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        crate::collab::queue::ack_message(tx, session_id, message_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_ack",
            &json!({
                "session_id": session_id,
                "message_id": message_id,
            }),
            Some(&json!({ "ok": true })),
        )?;
        Ok(())
    })?;
    Ok(json!({ "ok": true }))
}

fn handle_collab_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let record = app.db.collab_load_session_record(session_id)?;
    Ok(session_record_json(&record))
}

fn handle_collab_approve(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    if agent != "codex" {
        return Err(MemoryError::Validation(
            "agent must be 'codex' for collab_approve".to_string(),
        ));
    }
    let content_hash = require_str(args, "content_hash")?;
    let review_content = json!({
        "verdict": "approve",
        "content_hash": content_hash,
    })
    .to_string();

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let session = crate::collab::queue::load_session(tx, session_id)?;
        let expected_hash = session
            .canonical_plan_hash
            .as_deref()
            .ok_or_else(|| MemoryError::Validation("canonical_plan_hash is not set".to_string()))?;
        if content_hash != expected_hash {
            return Err(MemoryError::Validation(
                "content_hash does not match canonical_plan_hash".to_string(),
            ));
        }
        let session = apply_event(
            &session,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: "approve".to_string(),
            },
        )
        .map_err(collab_error_to_memory_error)?;
        crate::collab::queue::save_session(tx, &session)?;
        let _ = crate::collab::queue::send_message(
            tx,
            session_id,
            "codex",
            "claude",
            "review",
            &review_content,
        )?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_approve",
            &json!({
                "session_id": session_id,
                "agent": agent,
                "content_hash": content_hash,
            }),
            Some(&json!({ "phase": session.phase.to_string() })),
        )?;
        Ok(json!({ "phase": session.phase.to_string() }))
    })
}

fn handle_collab_register_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
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
    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        crate::collab::queue::register_caps(tx, session_id, agent, &parsed)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_register_caps",
            &json!({
                "session_id": session_id,
                "agent": agent,
                "count": count,
            }),
            Some(&json!({ "success": true, "count": count })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "success": true, "count": count }))
}

fn handle_collab_get_caps(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = args
        .get("agent")
        .and_then(|value| value.as_str())
        .map(require_agent)
        .transpose()?;
    let capabilities = app
        .db
        .collab_get_caps(session_id, agent)?
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

/// Polling cadence for `collab_wait_my_turn`. Short enough that
/// turn transitions feel immediate, long enough that idle waits don't
/// hammer SQLite.
const WAIT_MY_TURN_POLL_MS: u64 = 500;
/// Default timeout (seconds) applied when the caller omits `timeout_secs`.
const WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard cap on `timeout_secs` — clients that want longer should re-poll.
const WAIT_MY_TURN_MAX_TIMEOUT_SECS: u64 = 60;

/// Snapshot of session state read by `wait_my_turn` on each poll tick. Taken
/// in one `load_session_record` call so `task_list_submitted` and `phase` are
/// always from the same row — a concurrent `collab_send(task_list)` commit
/// cannot interleave into this view and produce an inconsistent terminal-set
/// decision. The returned status is stale-but-consistent: the next tick picks
/// up the new phase.
struct WaitTurnSnapshot {
    is_my_turn: bool,
    phase: String,
    current_owner: String,
    ended: bool,
    phase_is_terminal: bool,
}

fn wait_turn_snapshot(record: &SessionRecord, agent: &str) -> WaitTurnSnapshot {
    let ended = record.ended_at.is_some();
    // Dynamic terminal set, evaluated on a single snapshot: pre-task_list,
    // PlanLocked is terminal so v1 agents can exit cleanly after the plan
    // locks. Post-task_list the v2 coding phase is underway and the terminal
    // set switches to `{CodingComplete, CodingFailed}`.
    let task_list_submitted = record.session.task_list.is_some();
    let phase_is_terminal = if task_list_submitted {
        record.session.phase.is_terminal_v2()
    } else {
        matches!(record.session.phase, crate::collab::Phase::PlanLocked)
            || record.session.phase.is_terminal_v2()
    };
    let is_my_turn = !ended && !phase_is_terminal && record.session.current_owner == agent;
    WaitTurnSnapshot {
        is_my_turn,
        phase: record.session.phase.to_string(),
        current_owner: record.session.current_owner.clone(),
        ended,
        phase_is_terminal,
    }
}

fn handle_collab_wait_my_turn(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS)
        .clamp(1, WAIT_MY_TURN_MAX_TIMEOUT_SECS);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_millis(WAIT_MY_TURN_POLL_MS);

    loop {
        let record = app.db.collab_load_session_record(session_id)?;
        let snap = wait_turn_snapshot(&record, agent);

        if snap.is_my_turn
            || snap.ended
            || snap.phase_is_terminal
            || std::time::Instant::now() >= deadline
        {
            return Ok(json!({
                "is_my_turn": snap.is_my_turn,
                "phase": snap.phase,
                "current_owner": snap.current_owner,
                "session_ended": snap.ended,
            }));
        }

        std::thread::sleep(poll_interval);
    }
}

fn handle_collab_end(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    app.db.with_transaction(|tx| {
        // collab_end is valid only from PlanLocked (pre-task_list), or from
        // the two v2 terminal phases. Rejecting during any active planning
        // or coding phase prevents either agent from killing a session the
        // counterpart is still working in.
        let session = crate::collab::queue::load_session(tx, session_id)?;
        let allowed = matches!(
            session.phase,
            Phase::PlanLocked | Phase::CodingComplete | Phase::CodingFailed
        );
        if !allowed {
            return Err(MemoryError::Validation(format!(
                "collab_end rejected in active phase {}; end is only valid from PlanLocked (pre-task_list), CodingComplete, or CodingFailed",
                session.phase
            )));
        }
        crate::collab::queue::end_session(tx, session_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_end",
            &json!({
                "session_id": session_id,
                "agent": agent,
                "phase": session.phase.to_string(),
            }),
            Some(&json!({ "ok": true })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "ok": true, "session_id": session_id }))
}

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

fn render_sensitive_text(
    content: &str,
    max_chars: usize,
    redact: bool,
) -> (Value, bool, bool, usize) {
    if redact {
        return (Value::Null, false, true, 0);
    }

    let excerpt: String = content.chars().take(max_chars).collect();
    let excerpt_chars = excerpt.chars().count();
    let content_chars = content.chars().count();
    let truncated = excerpt_chars < content_chars;

    (Value::String(excerpt), truncated, false, excerpt_chars)
}

/// Validate that a date string matches YYYY-MM-DD format.
fn validate_date_format(value: &str, field_name: &str) -> Result<(), MemoryError> {
    if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be in YYYY-MM-DD format, got: {value}"
        )));
    }
    Ok(())
}

/// Validate that an ID is a 16 or 32-character hex string (SHA-256 truncated).
/// Accepts both lengths for backwards compatibility with existing data.
fn validate_hex_id(value: &str, field_name: &str) -> Result<(), MemoryError> {
    if !(value.len() == 16 || value.len() == 32) || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be a 16 or 32-character hex string"
        )));
    }
    Ok(())
}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, MemoryError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MemoryError::Validation(format!("{key} is required")))
}

fn require_agent(value: &str) -> Result<&str, MemoryError> {
    if matches!(value, "claude" | "codex") {
        Ok(value)
    } else {
        Err(MemoryError::Validation(
            "agent must be 'claude' or 'codex'".to_string(),
        ))
    }
}

fn other_agent(agent: &str) -> &'static str {
    if agent == "claude" {
        "codex"
    } else {
        "claude"
    }
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

/// True for every topic the collab_send handler accepts — v1 planning
/// vocabulary plus the v2 coding vocabulary. The topic strings `review` and
/// `final` are intentionally reused across versions; dispatch happens on the
/// current phase inside `build_collab_event`.
fn is_known_collab_topic(topic: &str) -> bool {
    matches!(
        topic,
        "draft"
            | "canonical"
            | "review"
            | "final"
            | "task_list"
            | "implement"
            | "verdict"
            | "comment"
            | "review_local"
            | "review_global"
            | "verdict_global"
            | "comment_global"
            | "final_review"
            | "pr_opened"
            | "failure_report"
    )
}

/// Maximum length (chars) for `coding_failure` on a failure_report. Matches
/// the CHECK constraint in migration 005 so the DB and MCP layer agree. The
/// outer `content` cap (MAX_COLLAB_CONTENT_CHARS) is larger — this per-field
/// cap prevents a caller from filling the whole content budget with one
/// unbounded string.
const MAX_CODING_FAILURE_CHARS: usize = 2048;

/// Maximum length (chars) for `pr_url` on a pr_opened event. Matches the
/// CHECK constraint in migration 005.
const MAX_PR_URL_CHARS: usize = 2048;

/// Translate a `(topic, content)` send into a `CollabEvent` using the session's
/// current phase to disambiguate v1/v2-overloaded topics (`review`, `final`).
/// Dispatch is split into v1 planning, phase-overloaded, and v2 coding groups
/// so each sub-function stays under the file's 50-line function guideline.
fn build_collab_event(
    topic: &str,
    content: &str,
    session: &crate::collab::CollabSession,
) -> Result<CollabEvent, MemoryError> {
    match topic {
        "draft" | "canonical" => build_v1_plan_event(topic, content),
        "review" | "final" => build_overloaded_event(topic, content, &session.phase),
        "task_list" | "implement" | "verdict" | "comment" | "review_local" | "review_global"
        | "verdict_global" | "comment_global" | "final_review" | "pr_opened" | "failure_report" => {
            build_v2_coding_event(topic, content)
        }
        other => Err(MemoryError::Validation(format!(
            "unknown collab topic: {other}"
        ))),
    }
}

/// v1 planning topics with no phase overloading. `draft` and `canonical` hash
/// the raw content and carry no structured payload.
fn build_v1_plan_event(topic: &str, content: &str) -> Result<CollabEvent, MemoryError> {
    match topic {
        "draft" => Ok(CollabEvent::SubmitDraft {
            content_hash: sha256_hex(content),
        }),
        "canonical" => Ok(CollabEvent::PublishCanonical {
            content_hash: sha256_hex(content),
        }),
        _ => unreachable!("build_v1_plan_event called with non-v1 topic: {topic}"),
    }
}

/// Phase-overloaded topics (`review`, `final`) are shared across v1 planning
/// and v2 per-task coding. An explicit phase whitelist picks the right event
/// variant — anything outside the whitelist is rejected here so the caller
/// gets a clean `WrongPhase` instead of a cryptic JSON parse error downstream.
fn build_overloaded_event(
    topic: &str,
    content: &str,
    phase: &Phase,
) -> Result<CollabEvent, MemoryError> {
    match (topic, phase) {
        ("review", Phase::PlanCodexReviewPending) => Ok(CollabEvent::SubmitReview {
            verdict: parse_review_verdict(content)?,
        }),
        ("review", Phase::CodeReviewPending) => {
            let head_sha = parse_required_head_sha(content, "review")?;
            Ok(CollabEvent::CodeReview { head_sha })
        }
        ("final", Phase::PlanClaudeFinalizePending) => {
            let plan = parse_final_payload(content)?;
            Ok(CollabEvent::PublishFinal {
                content_hash: sha256_hex(&plan),
            })
        }
        ("final", Phase::CodeFinalPending) => {
            let head_sha = parse_required_head_sha(content, "final")?;
            Ok(CollabEvent::CodeFinal { head_sha })
        }
        (topic, phase) => Err(MemoryError::Validation(format!(
            "topic '{topic}' is not accepted in phase {phase}; v1 expects it in PlanCodexReviewPending/PlanClaudeFinalizePending and v2 expects it in CodeReviewPending/CodeFinalPending",
        ))),
    }
}

/// v2 coding topics. Each payload is parsed once and required fields are
/// extracted in a single pass so `verdict_global` / `review_global` don't
/// double-parse their JSON for head_sha and verdict.
fn build_v2_coding_event(topic: &str, content: &str) -> Result<CollabEvent, MemoryError> {
    match topic {
        "task_list" => parse_task_list_event(content),
        "implement" => Ok(CollabEvent::CodeImplement {
            head_sha: parse_required_head_sha(content, "implement")?,
        }),
        "verdict" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "verdict")?;
            Ok(CollabEvent::CodeVerdict { verdict, head_sha })
        }
        "comment" => Ok(CollabEvent::CodeComment {
            head_sha: parse_required_head_sha(content, "comment")?,
        }),
        "review_local" => Ok(CollabEvent::ReviewLocal {
            head_sha: parse_required_head_sha(content, "review_local")?,
        }),
        "review_global" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "review_global")?;
            Ok(CollabEvent::ReviewGlobal { verdict, head_sha })
        }
        "verdict_global" => {
            let (head_sha, verdict) = parse_head_sha_and_verdict(content, "verdict_global")?;
            Ok(CollabEvent::VerdictGlobal { verdict, head_sha })
        }
        "comment_global" => Ok(CollabEvent::CommentGlobal {
            head_sha: parse_required_head_sha(content, "comment_global")?,
        }),
        "final_review" => Ok(CollabEvent::FinalReview {
            head_sha: parse_required_head_sha(content, "final_review")?,
        }),
        "pr_opened" => parse_pr_opened_event(content),
        "failure_report" => parse_failure_report_event(content),
        _ => unreachable!("build_v2_coding_event called with non-v2 topic: {topic}"),
    }
}

fn parse_pr_opened_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("pr_opened content must be JSON: {e}")))?;
    let head_sha = extract_required_str(&payload, "head_sha", "pr_opened")?;
    let pr_url = extract_required_str(&payload, "pr_url", "pr_opened")?;
    if pr_url.chars().count() > MAX_PR_URL_CHARS {
        return Err(MemoryError::Validation(format!(
            "pr_opened pr_url exceeds {MAX_PR_URL_CHARS} chars",
        )));
    }
    // Only https URLs are accepted — a javascript:/file:// URL here could
    // become an open-redirect or SSRF if any downstream consumer renders it.
    if !pr_url.starts_with("https://") {
        return Err(MemoryError::Validation(
            "pr_opened pr_url must start with https://".to_string(),
        ));
    }
    Ok(CollabEvent::PrOpened { pr_url, head_sha })
}

fn parse_failure_report_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!("failure_report content must be JSON: {e}"))
    })?;
    let coding_failure = payload
        .get("coding_failure")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            MemoryError::Validation(
                "failure_report content must include a non-empty \"coding_failure\" field"
                    .to_string(),
            )
        })?
        .to_string();
    if coding_failure.chars().count() > MAX_CODING_FAILURE_CHARS {
        return Err(MemoryError::Validation(format!(
            "failure_report coding_failure exceeds {MAX_CODING_FAILURE_CHARS} chars",
        )));
    }
    Ok(CollabEvent::FailureReport { coding_failure })
}

/// Best-effort check for the `branch_drift:` prefix used by the upstream
/// turn gate. Returns false on any JSON parse failure so malformed payloads
/// still fall through to the main `parse_failure_report_event` validation.
fn failure_report_is_branch_drift(content: &str) -> bool {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|v| {
            v.get("coding_failure")
                .and_then(Value::as_str)
                .map(|s| s.starts_with(crate::collab::BRANCH_DRIFT_PREFIX))
        })
        .unwrap_or(false)
}

/// Parse the JSON payload once and extract both `head_sha` and `verdict` in a
/// single pass. Used by `verdict` / `review_global` / `verdict_global` which
/// all need both fields.
fn parse_head_sha_and_verdict(content: &str, topic: &str) -> Result<(String, String), MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("{topic} content must be JSON: {e}")))?;
    let head_sha = extract_required_str(&payload, "head_sha", topic)?;
    let verdict = extract_required_str(&payload, "verdict", topic)?;
    Ok((head_sha, verdict))
}

/// Parse and validate the task_list payload shape. Fails fast on missing
/// fields, empty task array, missing acceptance criteria, or non-array tasks.
/// The state machine re-checks plan_hash, base_sha presence, and task count.
fn parse_task_list_event(content: &str) -> Result<CollabEvent, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "task_list content must be JSON shaped like {{\"plan_hash\":\"…\",\"base_sha\":\"…\",\"head_sha\":\"…\",\"tasks\":[{{\"id\":1,\"title\":\"…\",\"acceptance\":[\"…\"]}}]}} (parse error: {e})"
        ))
    })?;
    let plan_hash = payload
        .get("plan_hash")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            MemoryError::Validation("task_list missing non-empty plan_hash".to_string())
        })?
        .to_string();
    let base_sha = payload
        .get("base_sha")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty base_sha".to_string()))?
        .to_string();
    let head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| MemoryError::Validation("task_list missing non-empty head_sha".to_string()))?
        .to_string();
    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| MemoryError::Validation("task_list missing \"tasks\" array".to_string()))?;
    if tasks.is_empty() {
        return Err(MemoryError::Validation(
            "task_list must contain at least one task".to_string(),
        ));
    }
    let mut last_id: Option<i64> = None;
    for (idx, task) in tasks.iter().enumerate() {
        let task_id = task.get("id").and_then(Value::as_i64).ok_or_else(|| {
            MemoryError::Validation(format!("task_list task[{idx}] missing integer \"id\""))
        })?;
        if let Some(prev) = last_id {
            if task_id <= prev {
                return Err(MemoryError::Validation(format!(
                    "task_list tasks must be strictly ordered by id (task[{idx}].id={task_id} follows {prev})"
                )));
            }
        }
        last_id = Some(task_id);
        let acceptance = task
            .get("acceptance")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                MemoryError::Validation(format!(
                    "task_list task[{idx}] missing \"acceptance\" array"
                ))
            })?;
        if acceptance.is_empty() {
            return Err(MemoryError::Validation(format!(
                "task_list task[{idx}] must include at least one acceptance criterion"
            )));
        }
    }
    let tasks_count = u32::try_from(tasks.len())
        .map_err(|_| MemoryError::Validation("task_list contains too many tasks".to_string()))?;
    // Canonicalize the task_list JSON we store on the session so downstream
    // readers see a normalized form regardless of incoming whitespace.
    let task_list_json = serde_json::to_string(&payload)
        .map_err(|e| MemoryError::Validation(format!("task_list serialize error: {e}")))?;
    Ok(CollabEvent::SubmitTaskList {
        plan_hash,
        base_sha,
        task_list_json,
        tasks_count,
        head_sha,
    })
}

fn parse_required_head_sha(content: &str, topic: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content)
        .map_err(|e| MemoryError::Validation(format!("{topic} content must be JSON: {e}")))?;
    extract_required_str(&payload, "head_sha", topic)
}

/// Pull a non-empty string field out of a parsed JSON payload with a uniform
/// validation error.
fn extract_required_str(payload: &Value, field: &str, topic: &str) -> Result<String, MemoryError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            MemoryError::Validation(format!(
                "{topic} content must include a non-empty \"{field}\" field"
            ))
        })
}

fn parse_review_verdict(content: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "review content must be JSON shaped like {{\"verdict\":\"approve|approve_with_minor_edits|request_changes\",\"notes\":[\"...\"]}} (parse error: {e})"
        ))
    })?;
    let verdict = payload
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MemoryError::Validation(
                "review content must include a \"verdict\" string field".to_string(),
            )
        })?;
    Ok(verdict.to_string())
}

fn parse_final_payload(content: &str) -> Result<String, MemoryError> {
    let payload: Value = serde_json::from_str(content).map_err(|e| {
        MemoryError::Validation(format!(
            "final content must be JSON shaped like {{\"plan\":\"<full plan text>\"}} (parse error: {e})"
        ))
    })?;
    let plan = payload.get("plan").and_then(Value::as_str).ok_or_else(|| {
        MemoryError::Validation("final content must include a \"plan\" string field".to_string())
    })?;
    Ok(plan.to_string())
}

fn collab_error_to_memory_error(error: CollabError) -> MemoryError {
    MemoryError::Validation(error.to_string())
}

fn session_record_json(record: &SessionRecord) -> Value {
    json!({
        "id": record.session.id.as_str(),
        "phase": record.session.phase.to_string(),
        "current_owner": record.session.current_owner.as_str(),
        "repo_path": record.repo_path.as_str(),
        "branch": record.branch.as_str(),
        "task": record.task.as_deref(),
        "claude_draft_hash": record.session.claude_draft_hash.as_deref(),
        "codex_draft_hash": record.session.codex_draft_hash.as_deref(),
        "canonical_plan_hash": record.session.canonical_plan_hash.as_deref(),
        "final_plan_hash": record.session.final_plan_hash.as_deref(),
        "codex_review_verdict": record.session.codex_review_verdict.as_deref(),
        "review_round": record.session.review_round,
        "task_list": record.session.task_list.as_deref(),
        "tasks_count": record.session.tasks_count(),
        "current_task_index": record.session.current_task_index,
        "task_review_round": record.session.task_review_round,
        "global_review_round": record.session.global_review_round,
        "base_sha": record.session.base_sha.as_deref(),
        "last_head_sha": record.session.last_head_sha.as_deref(),
        "pr_url": record.session.pr_url.as_deref(),
        "coding_failure": record.session.coding_failure.as_deref(),
        "ended_at": record.ended_at.as_deref(),
        "created_at": record.created_at.as_str(),
        "updated_at": record.updated_at.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_required_str_pins_error_format() {
        let payload = json!({ "head_sha": "abc123", "empty": "", "n": 3 });
        assert_eq!(
            extract_required_str(&payload, "head_sha", "implement").unwrap(),
            "abc123"
        );
        let missing = extract_required_str(&payload, "pr_url", "pr_opened").unwrap_err();
        assert_eq!(
            missing.to_string(),
            "Validation error: pr_opened content must include a non-empty \"pr_url\" field"
        );
        let empty = extract_required_str(&payload, "empty", "verdict").unwrap_err();
        assert!(empty.to_string().contains("non-empty \"empty\" field"));
        let wrong_type = extract_required_str(&payload, "n", "verdict").unwrap_err();
        assert!(wrong_type.to_string().contains("non-empty \"n\" field"));
    }

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
