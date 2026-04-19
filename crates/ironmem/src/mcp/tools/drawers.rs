use serde_json::{json, Value};

use crate::db::SearchFilters;
use crate::error::MemoryError;
use crate::sanitize;
use crate::search;

use super::shared::{
    render_sensitive_text, validate_hex_id, MAX_SEARCH_LIMIT, MAX_SEARCH_RESPONSE_CHARS,
    MAX_SENSITIVE_FIELD_CHARS,
};
use crate::mcp::app::App;

pub(super) fn handle_add_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_delete_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_list_wings(app: &App) -> Result<Value, MemoryError> {
    let wings = app.db.wing_counts()?;
    Ok(json!({
        "wings": wings.into_iter().collect::<std::collections::HashMap<_, _>>()
    }))
}

pub(super) fn handle_list_rooms(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_get_taxonomy(app: &App) -> Result<Value, MemoryError> {
    let taxonomy = app.db.taxonomy()?;
    Ok(json!({ "taxonomy": taxonomy }))
}

pub(super) fn handle_search(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_status(app: &App) -> Result<Value, MemoryError> {
    let total = app.db.count_drawers(None)?;
    let wings = app.db.wing_counts()?;
    let kg = crate::db::knowledge_graph::KnowledgeGraph::new(&app.db);
    let kg_stats = kg.stats()?;

    Ok(json!({
        "total_drawers": total,
        "wings": wings.into_iter().collect::<std::collections::HashMap<_, _>>(),
        "knowledge_graph": kg_stats,
        "memory_protocol": crate::bootstrap::MEMORY_PROTOCOL,
        "warming_up": app.is_warming_up(),
    }))
}
