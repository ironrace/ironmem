//! MCP tool definitions and dispatch.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::app::App;
use crate::bootstrap::MEMORY_PROTOCOL;
use crate::collab::queue::{Capability, SessionRecord};
use crate::collab::{apply_event, CollabError, CollabEvent};
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
            "name": "ironmem_status",
            "description": "Memory overview — total drawers, wing and room counts",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_search",
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
            "name": "ironmem_add_drawer",
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
            "name": "ironmem_delete_drawer",
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
            "name": "ironmem_list_wings",
            "description": "All wings with drawer counts",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_list_rooms",
            "description": "Rooms within a wing (or all rooms)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wing": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "ironmem_get_taxonomy",
            "description": "Full wing → room → count tree",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_kg_add",
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
            "name": "ironmem_kg_query",
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
            "name": "ironmem_kg_invalidate",
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
            "name": "ironmem_kg_timeline",
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
            "name": "ironmem_kg_stats",
            "description": "Knowledge graph summary statistics",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_traverse",
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
            "name": "ironmem_find_tunnels",
            "description": "Find rooms that span multiple wings",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_graph_stats",
            "description": "Memory graph summary — rooms, wings, tunnels, edges",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "ironmem_diary_write",
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
            "name": "ironmem_diary_read",
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
            "name": "ironmem_collab_start",
            "description": "Create a bounded Claude↔Codex planning session. Optional `task` describes the planning goal and is returned in ironmem_collab_status so the counterpart agent can fetch it without a manual paste.",
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
            "name": "ironmem_collab_send",
            "description": "Send a collab message and advance the bounded planning state machine when applicable",
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
            "name": "ironmem_collab_recv",
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
            "name": "ironmem_collab_ack",
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
            "name": "ironmem_collab_status",
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
            "name": "ironmem_collab_approve",
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
            "name": "ironmem_collab_register_caps",
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
            "name": "ironmem_collab_get_caps",
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
            "name": "ironmem_collab_wait_my_turn",
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
            "name": "ironmem_collab_end",
            "description": "End a collab session. Reserved for the v2 coding phase — DO NOT call during planning. Idempotent; blocks subsequent send/approve/wait_my_turn writes.",
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
        "ironmem_status" => handle_status(app),
        "ironmem_search" => handle_search(app, args),
        "ironmem_add_drawer" => handle_add_drawer(app, args),
        "ironmem_delete_drawer" => handle_delete_drawer(app, args),
        "ironmem_list_wings" => handle_list_wings(app),
        "ironmem_list_rooms" => handle_list_rooms(app, args),
        "ironmem_get_taxonomy" => handle_get_taxonomy(app),
        "ironmem_kg_add" => handle_kg_add(app, args),
        "ironmem_kg_query" => handle_kg_query(app, args),
        "ironmem_kg_invalidate" => handle_kg_invalidate(app, args),
        "ironmem_kg_timeline" => handle_kg_timeline(app, args),
        "ironmem_kg_stats" => handle_kg_stats(app),
        "ironmem_traverse" => handle_traverse(app, args),
        "ironmem_find_tunnels" => handle_find_tunnels(app),
        "ironmem_graph_stats" => handle_graph_stats(app),
        "ironmem_diary_write" => handle_diary_write(app, args),
        "ironmem_diary_read" => handle_diary_read(app, args),
        "ironmem_collab_start" => handle_collab_start(app, args),
        "ironmem_collab_send" => handle_collab_send(app, args),
        "ironmem_collab_recv" => handle_collab_recv(app, args),
        "ironmem_collab_ack" => handle_collab_ack(app, args),
        "ironmem_collab_status" => handle_collab_status(app, args),
        "ironmem_collab_approve" => handle_collab_approve(app, args),
        "ironmem_collab_register_caps" => handle_collab_register_caps(app, args),
        "ironmem_collab_get_caps" => handle_collab_get_caps(app, args),
        "ironmem_collab_wait_my_turn" => handle_collab_wait_my_turn(app, args),
        "ironmem_collab_end" => handle_collab_end(app, args),
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
    if !matches!(topic, "draft" | "canonical" | "review" | "final") {
        return Err(MemoryError::Validation(format!(
            "unknown collab topic: {topic}"
        )));
    }

    app.db.with_transaction(|tx| {
        crate::collab::queue::ensure_active(tx, session_id)?;
        let mut session = crate::collab::queue::load_session(tx, session_id)?;
        let phase_before = session.phase.to_string();
        let event = match topic {
            "draft" => CollabEvent::SubmitDraft {
                content_hash: sha256_hex(content),
            },
            "canonical" => CollabEvent::PublishCanonical {
                content_hash: sha256_hex(content),
            },
            "review" => CollabEvent::SubmitReview {
                verdict: parse_review_verdict(content)?,
            },
            "final" => {
                let plan = parse_final_payload(content)?;
                CollabEvent::PublishFinal {
                    content_hash: sha256_hex(&plan),
                }
            }
            _ => unreachable!(),
        };

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
            "agent must be 'codex' for ironmem_collab_approve".to_string(),
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

/// Polling cadence for `ironmem_collab_wait_my_turn`. Short enough that
/// turn transitions feel immediate, long enough that idle waits don't
/// hammer SQLite.
const WAIT_MY_TURN_POLL_MS: u64 = 500;
/// Default timeout (seconds) applied when the caller omits `timeout_secs`.
const WAIT_MY_TURN_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard cap on `timeout_secs` — clients that want longer should re-poll.
const WAIT_MY_TURN_MAX_TIMEOUT_SECS: u64 = 60;

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
        let ended = record.ended_at.is_some();
        let phase_is_terminal = matches!(record.session.phase, crate::collab::Phase::PlanLocked);
        let is_my_turn = !ended && !phase_is_terminal && record.session.current_owner == agent;

        if is_my_turn || ended || phase_is_terminal || std::time::Instant::now() >= deadline {
            return Ok(json!({
                "is_my_turn": is_my_turn,
                "phase": record.session.phase.to_string(),
                "current_owner": record.session.current_owner,
                "session_ended": ended,
            }));
        }

        std::thread::sleep(poll_interval);
    }
}

fn handle_collab_end(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let session_id = require_str(args, "session_id")?;
    let agent = require_agent(require_str(args, "agent")?)?;

    app.db.with_transaction(|tx| {
        crate::collab::queue::end_session(tx, session_id)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_end",
            &json!({
                "session_id": session_id,
                "agent": agent,
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
        "ironmem_status"
            | "ironmem_search"
            | "ironmem_add_drawer"
            | "ironmem_delete_drawer"
            | "ironmem_list_wings"
            | "ironmem_list_rooms"
            | "ironmem_get_taxonomy"
            | "ironmem_kg_add"
            | "ironmem_kg_query"
            | "ironmem_kg_invalidate"
            | "ironmem_kg_timeline"
            | "ironmem_kg_stats"
            | "ironmem_traverse"
            | "ironmem_find_tunnels"
            | "ironmem_graph_stats"
            | "ironmem_diary_write"
            | "ironmem_diary_read"
            | "ironmem_collab_start"
            | "ironmem_collab_send"
            | "ironmem_collab_recv"
            | "ironmem_collab_ack"
            | "ironmem_collab_status"
            | "ironmem_collab_approve"
            | "ironmem_collab_register_caps"
            | "ironmem_collab_get_caps"
            | "ironmem_collab_wait_my_turn"
            | "ironmem_collab_end"
    )
}

fn tool_allowed_in_mode(mode: McpAccessMode, name: &str) -> bool {
    if !tool_known(name) {
        return false;
    }
    mode.allows_writes()
        || !matches!(
            name,
            "ironmem_add_drawer"
                | "ironmem_delete_drawer"
                | "ironmem_kg_add"
                | "ironmem_kg_invalidate"
                | "ironmem_diary_write"
                | "ironmem_collab_start"
                | "ironmem_collab_send"
                | "ironmem_collab_ack"
                | "ironmem_collab_approve"
                | "ironmem_collab_register_caps"
                | "ironmem_collab_end"
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
        "ended_at": record.ended_at.as_deref(),
        "created_at": record.created_at.as_str(),
        "updated_at": record.updated_at.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_access_modes_disable_writes_outside_trusted_mode() {
        assert!(tool_allowed_in_mode(
            McpAccessMode::Trusted,
            "ironmem_add_drawer"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::ReadOnly,
            "ironmem_add_drawer"
        ));
        assert!(!tool_allowed_in_mode(
            McpAccessMode::Restricted,
            "ironmem_kg_add"
        ));
        assert!(tool_allowed_in_mode(
            McpAccessMode::Restricted,
            "ironmem_search"
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
