use serde_json::{json, Value};

use crate::db::knowledge_graph::KnowledgeGraph;
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;
use crate::search;

use super::shared::{validate_date_format, validate_hex_id, MAX_DEPTH};

pub(super) fn handle_kg_add(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_kg_query(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_kg_invalidate(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_kg_timeline(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_kg_stats(app: &App) -> Result<Value, MemoryError> {
    let kg = KnowledgeGraph::new(&app.db);
    kg.stats()
}

pub(super) fn handle_traverse(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_find_tunnels(app: &App) -> Result<Value, MemoryError> {
    let tunnels = search::graph::find_tunnels(app)?;
    Ok(json!({ "tunnels": tunnels }))
}

pub(super) fn handle_graph_stats(app: &App) -> Result<Value, MemoryError> {
    let stats = search::graph::graph_stats(app)?;
    Ok(serde_json::to_value(stats)?)
}
