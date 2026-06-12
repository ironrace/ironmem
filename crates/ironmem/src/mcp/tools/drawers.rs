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

    app.ensure_embedder_ready()?;

    let embedding = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        emb.embed_one(content).map_err(MemoryError::Embed)?
    };

    // Compute synthetic sibling, if enrichment is enabled and content qualifies.
    let synth: Option<(String, String, Vec<f32>)> =
        build_synthetic(app, content, &wing, &room, &id)?;

    app.db.with_transaction(|tx| {
        crate::db::schema::Database::insert_drawer_tx(
            tx, &id, content, &embedding, &wing, &room, "", "mcp",
        )?;
        if let Some((sid, scontent, semb)) = synth.as_ref() {
            let parent_ref = format!("{}{id}", crate::db::drawers::PREF_SENTINEL);
            crate::db::schema::Database::insert_drawer_tx(
                tx,
                sid,
                scontent,
                semb,
                &wing,
                &room,
                &parent_ref,
                "mcp",
            )?;
        }
        crate::db::schema::Database::wal_log_tx(
            tx,
            "add_drawer",
            &json!({"id": &id, "wing": &wing, "room": &room, "synth": synth.is_some()}),
            None,
        )?;
        Ok(())
    })?;

    app.insert_into_index(&id, &embedding)?;
    if let Some((sid, _, semb)) = synth.as_ref() {
        if let Err(e) = app.insert_into_index(sid, semb) {
            tracing::warn!(
                error = %e,
                parent = %id,
                synth = %sid,
                "pref_enrich index insert failed; marking dirty for rebuild"
            );
            app.mark_dirty();
        }
    }

    Ok(json!({
        "success": true,
        "id": id,
        "wing": wing,
        "room": room,
        "synth": synth.is_some(),
    }))
}

/// Build a synthetic preference-enrichment drawer, or return Ok(None) if the
/// tunable is off, the content doesn't look conversational, or the extractor
/// produced no phrases. A failure to embed the synthetic body logs at warn
/// and returns Ok(None) — the parent insert continues unaffected.
fn build_synthetic(
    app: &App,
    content: &str,
    wing: &str,
    room: &str,
    parent_id: &str,
) -> Result<Option<(String, String, Vec<f32>)>, MemoryError> {
    use ironrace_pref_extract::{
        looks_conversational, synthesize_doc, PreferenceExtractor, RegexPreferenceExtractor,
    };
    use std::time::Duration;

    if !crate::search::tunables::pref_enrich_enabled() {
        return Ok(None);
    }
    if !looks_conversational(content) {
        return Ok(None);
    }
    // Record a `pref_extract` token_usage row from an LLM response (non-fatal:
    // a failed insert logs at warn and is dropped — pref-enrich is best-effort).
    let record_pref_usage = |resp: &ironrace_rerank::LlmResponse| {
        let ctx = crate::metrics::MetricsContext::resolve(app);
        let row = crate::db::metrics::new_token_usage_from_llm(
            "pref_extract",
            resp,
            chrono::Utc::now().to_rfc3339(),
        )
        .with_context(&ctx);
        if let Err(e) = app.db.insert_token_usage(&row) {
            tracing::warn!(
                error = %e,
                source = %row.source,
                model = ?row.model,
                "pref_extract token_usage insert failed"
            );
        }
    };

    // Test-only seam: a concrete `LlmPreferenceExtractor` override bypasses the
    // OnceLock-cached tunable selection so the usage path is deterministic.
    let override_extractor = app.pref_extractor_override.read().unwrap().clone();

    let phrases: Vec<String> = if let Some(extractor) = override_extractor {
        let (phrases, response) = extractor.extract_with_response(content);
        if let Some(resp) = response {
            record_pref_usage(&resp);
        }
        phrases
    } else {
        match crate::search::tunables::pref_extractor() {
            "llm" => {
                let timeout = Duration::from_millis(crate::search::tunables::pref_llm_timeout_ms());
                let model = crate::search::tunables::pref_llm_model();
                let extractor = match crate::search::tunables::pref_llm_backend() {
                    "api" => crate::search::pref_extract_llm::api_extractor(
                        model,
                        crate::search::tunables::pref_llm_max_tokens(),
                        timeout,
                    ),
                    _ => crate::search::pref_extract_llm::cli_extractor(model, timeout),
                };
                let (phrases, response) = extractor.extract_with_response(content);
                if let Some(resp) = response {
                    record_pref_usage(&resp);
                }
                phrases
            }
            _ => RegexPreferenceExtractor.extract(content),
        }
    };
    let synth_body = match synthesize_doc(&phrases) {
        Some(s) => s,
        None => return Ok(None),
    };
    let synth_id = crate::db::drawers::generate_id(&synth_body, wing, room);
    let synth_emb = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        match emb.embed_one(&synth_body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, parent = parent_id, "pref_enrich embed failed; skipping synth");
                return Ok(None);
            }
        }
    };
    tracing::debug!(
        parent = parent_id,
        synth = %synth_id,
        phrases = phrases.len(),
        "pref_enrich"
    );
    Ok(Some((synth_id, synth_body, synth_emb)))
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

pub(super) fn handle_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let set_tag = args.get("set_task_tag").and_then(Value::as_str);
    let clear_tag = args
        .get("clear_task_tag")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if set_tag.is_some() && clear_tag {
        return Err(MemoryError::Validation(
            "set_task_tag and clear_task_tag are mutually exclusive".into(),
        ));
    }
    if let Some(tag) = set_tag {
        // sanitize_name allows hyphens in the middle (e.g. "issue-85") and
        // enforces a safe character set; round-trips unchanged for normal slugs.
        let tag = sanitize::sanitize_name(tag, "task_tag")?;
        app.set_explicit_task_tag(&tag);
    } else if clear_tag {
        app.clear_explicit_task_tag();
    }

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
        "task_tag": app.explicit_task_tag_snapshot(),
        "active_collab_session_id": app.active_collab_session_snapshot(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EmbedMode, McpAccessMode};
    use serde_json::json;
    use std::sync::Arc;

    fn test_app() -> Arc<App> {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        // Leak the tempdir so the DB file outlives the test.
        std::mem::forget(dir);
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::new(config).unwrap())
    }

    fn test_app_readonly() -> Arc<App> {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        };
        std::mem::forget(dir);
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::new(config).unwrap())
    }

    #[test]
    fn status_sets_clears_and_echoes_task_tag() {
        let app = test_app();

        // set_task_tag stores the tag and echoes it in the response
        let out = handle_status(&app, &json!({"set_task_tag": "issue-85"})).unwrap();
        assert_eq!(out["task_tag"].as_str(), Some("issue-85"));
        assert_eq!(
            app.explicit_task_tag_snapshot().as_deref(),
            Some("issue-85")
        );

        // Existing structural keys must still be present
        assert!(out.get("total_drawers").is_some(), "total_drawers missing");
        assert!(out.get("wings").is_some(), "wings missing");
        assert!(
            out.get("knowledge_graph").is_some(),
            "knowledge_graph missing"
        );
        assert!(
            out.get("memory_protocol").is_some(),
            "memory_protocol missing"
        );
        assert!(out.get("warming_up").is_some(), "warming_up missing");

        // active_collab_session_id is echoed (null when unset)
        assert!(
            out["active_collab_session_id"].is_null(),
            "active_collab_session_id must be null when unset"
        );

        // plain status call (no args) echoes current tag
        let out = handle_status(&app, &json!({})).unwrap();
        assert_eq!(
            out["task_tag"].as_str(),
            Some("issue-85"),
            "plain status echoes current tag"
        );

        // clear_task_tag removes the tag
        let out = handle_status(&app, &json!({"clear_task_tag": true})).unwrap();
        assert!(out["task_tag"].is_null());
        assert!(app.explicit_task_tag_snapshot().is_none());
    }

    #[test]
    fn status_rejects_set_and_clear_together() {
        let app = test_app();
        assert!(
            handle_status(&app, &json!({"set_task_tag": "x", "clear_task_tag": true})).is_err()
        );
    }

    #[test]
    fn status_tag_set_writes_no_db_rows_in_read_only_mode() {
        let app = test_app_readonly();

        // Before: no token_usage rows
        let rows_before = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();

        // set_task_tag must succeed (it's process-local state, not a DB write)
        let out = handle_status(&app, &json!({"set_task_tag": "issue-85"})).unwrap();
        assert_eq!(out["task_tag"].as_str(), Some("issue-85"));
        assert_eq!(
            app.explicit_task_tag_snapshot().as_deref(),
            Some("issue-85")
        );

        // After: still no token_usage rows (tag is process-local only)
        let rows_after = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(
            rows_before.len(),
            rows_after.len(),
            "set_task_tag must not write any DB rows"
        );

        // Also check task_outcomes is untouched
        let task_outcome = app.db.get_task_outcome("issue-85").unwrap();
        assert!(
            task_outcome.is_none(),
            "set_task_tag must not create a task_outcomes row"
        );
    }

    // ── G.6: status rejects invalid task_tag and leaves tag unset ────────────

    #[test]
    fn status_rejects_invalid_task_tag_and_leaves_tag_unset() {
        let app = test_app();

        // "../etc" contains ".." which is rejected by sanitize_name.
        let err = handle_status(&app, &json!({"set_task_tag": "../etc"})).unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "should have returned a validation error"
        );
        assert!(
            app.explicit_task_tag_snapshot().is_none(),
            "task tag must remain unset after a rejected set_task_tag"
        );
    }
}
