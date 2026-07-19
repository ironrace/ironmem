use serde_json::{json, Value};

use crate::db::SearchFilters;
use crate::error::MemoryError;
use crate::sanitize;
use crate::search;

use super::shared::{
    render_sensitive_text, sha256_hex, validate_hex_id, MAX_DRAWER_CONTENT_CHARS, MAX_SEARCH_LIMIT,
    MAX_SEARCH_RESPONSE_CHARS, MAX_SENSITIVE_FIELD_CHARS,
};
use crate::mcp::app::App;

const LOGICAL_KEY_SOURCE_PREFIX: &str = "logical:";
const LOGICAL_KEY_ID_PREFIX: &str = "logical-key:";

/// `add_drawer`'s arguments after validation. Borrows `content` from the
/// request (`sanitize_content` returns a borrowed slice).
pub(super) struct AddDrawerArgs<'a> {
    content: &'a str,
    wing: String,
    room: String,
    logical_key: Option<String>,
}

/// Readiness-independent validation for `add_drawer`.
///
/// Split out from the handler so the daemon can reject a malformed call
/// *before* parking it on the readiness gate — none of this depends on the
/// embedder or index being up. The handler calls it too, so there is exactly
/// one definition of what a valid `add_drawer` is.
pub(super) fn validate_add_drawer_args(args: &Value) -> Result<AddDrawerArgs<'_>, MemoryError> {
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
    let logical_key = args
        .get("logical_key")
        .and_then(|v| v.as_str())
        .map(|v| sanitize::sanitize_name(v, "logical_key"))
        .transpose()?;

    Ok(AddDrawerArgs {
        content: sanitize::sanitize_content(content, MAX_DRAWER_CONTENT_CHARS)?,
        wing: sanitize::sanitize_name(wing, "wing")?,
        room: sanitize::sanitize_name(room, "room")?,
        logical_key,
    })
}

pub(super) fn handle_add_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    // Validate first: argument errors do not depend on readiness, so a
    // malformed call must never serve out the warm-up window before being
    // rejected.
    let AddDrawerArgs {
        content,
        wing,
        room,
        logical_key,
    } = validate_add_drawer_args(args)?;
    app.wait_for_write_ready()?;

    let id_basis = logical_key
        .as_ref()
        .map(|key| format!("{LOGICAL_KEY_ID_PREFIX}{key}"))
        .unwrap_or_else(|| content.to_string());
    let id = crate::db::drawers::generate_id(&id_basis, &wing, &room);
    let source_file = logical_key
        .as_ref()
        .map(|key| format!("{LOGICAL_KEY_SOURCE_PREFIX}{key}"))
        .unwrap_or_default();

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
            tx,
            &id,
            content,
            &embedding,
            &wing,
            &room,
            &source_file,
            "mcp",
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
            &json!({
                "id": &id,
                "wing": &wing,
                "room": &room,
                "synth": synth.is_some(),
                "logical_key": logical_key.as_deref()
            }),
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

    let mut out = json!({
        "success": true,
        "id": id,
        "wing": wing,
        "room": room,
        "synth": synth.is_some(),
        "id_strategy": if logical_key.is_some() { "logical_key" } else { "content" },
    });
    if let Some(key) = logical_key {
        out["logical_key"] = json!(key);
    }
    Ok(out)
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

/// Fetch a single drawer by its exact id. This is the deterministic
/// read-by-primary-key counterpart to `add_drawer`: `search` ranks semantically
/// and cannot reliably return a specific freshly-written staging drawer, so any
/// flow that stages an artifact under a known id (e.g. the collab compose→submit
/// handoff) needs this to read it back. By default it returns the full body
/// (subject only to access-mode redaction); callers that only need identity or
/// freshness checks can pass `include_content:false`, `max_chars`, or
/// `hash_only:true`.
pub(super) fn handle_get_drawer(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("id is required".into()))?;
    validate_hex_id(id, "id")?;
    let hash_only = args
        .get("hash_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_content = !hash_only
        && args
            .get("include_content")
            .and_then(Value::as_bool)
            .unwrap_or(true);
    let max_chars = match args.get("max_chars") {
        Some(value) => {
            let raw = value.as_u64().ok_or_else(|| {
                MemoryError::Validation("max_chars must be a non-negative integer".into())
            })?;
            usize::try_from(raw)
                .unwrap_or(MAX_DRAWER_CONTENT_CHARS)
                .min(MAX_DRAWER_CONTENT_CHARS)
        }
        None => MAX_DRAWER_CONTENT_CHARS,
    };

    let drawer = match app.db.get_drawer(id)? {
        Some(d) => d,
        None => {
            return Ok(json!({ "found": false, "id": id }));
        }
    };

    let redact_content = app.config.mcp_access_mode.redacts_sensitive_content();
    let content_chars = drawer.content.chars().count();
    let (content, truncated, redacted) = if include_content {
        let (content, truncated, redacted, _consumed) =
            render_sensitive_text(&drawer.content, max_chars, redact_content);
        (Some(content), Some(truncated), redacted)
    } else {
        (None, None, redact_content)
    };

    // Parity: when the body is redacted, also withhold source_file (a filesystem
    // path) and added_by — both are potentially sensitive metadata. wing/room/
    // filed_at/date are structural locators and are always returned.
    let source_file = if redact_content {
        Value::Null
    } else {
        json!(drawer.source_file)
    };
    let added_by = if redact_content {
        Value::Null
    } else {
        json!(drawer.added_by)
    };

    let mut out = json!({
        "found": true,
        "id": drawer.id,
        "content_included": include_content && !redacted,
        "content_redacted": redacted,
        "content_chars": content_chars,
        "wing": drawer.wing,
        "room": drawer.room,
        "source_file": source_file,
        "added_by": added_by,
        "filed_at": drawer.filed_at,
        "date": drawer.date,
    });
    if let Some(content) = content {
        out["content"] = content;
    }
    if let Some(truncated) = truncated {
        out["content_truncated"] = json!(truncated);
    }
    if hash_only && !redacted {
        out["content_hash"] = json!(sha256_hex(&drawer.content));
        out["hash_only"] = json!(true);
    } else if hash_only {
        out["hash_only"] = json!(true);
        out["content_hash_redacted"] = json!(true);
    }
    Ok(out)
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
        "metrics": crate::report::one_line_summary(&app.db),
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
    fn status_includes_one_line_metrics_summary() {
        let app = test_app();
        let out = handle_status(&app, &json!({})).unwrap();
        assert_eq!(
            out["metrics"].as_str(),
            Some("no metrics recorded yet"),
            "empty DB status summary"
        );

        app.db
            .upsert_task_outcome(&crate::db::TaskOutcome {
                task_tag: "issue-status".into(),
                collab_session_id: Some("sess-status".into()),
                started_at: Some("2026-06-01T00:00:00Z".into()),
                done_at: Some("2026-06-02T00:00:00Z".into()),
                outcome: Some("merged".into()),
                review_rounds: 0,
                fix_commits: 0,
                handoffs: 0,
                pr_url: None,
            })
            .unwrap();
        app.db
            .insert_token_usage(&crate::db::NewTokenUsage {
                ts: "2026-06-01T01:00:00Z".into(),
                source: "llm_rerank".into(),
                harness: "claude".into(),
                model: Some("claude-opus-4-8".into()),
                tool_name: None,
                session_id: None,
                collab_session_id: Some("sess-status".into()),
                collab_phase: Some("impl".into()),
                task_tag: None,
                input_tokens: 1_000_000,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                estimated: false,
                chars: 0,
                cost_usd: None,
                map_status: None,
                turn_id: None,
                area: None,
            })
            .unwrap();

        let out = handle_status(&app, &json!({})).unwrap();
        assert_eq!(
            out["metrics"].as_str(),
            Some("1 tasks · 1000000 measured tokens · $5.00 (§7) · baseline 1/10")
        );
        // existing keys preserved
        assert!(out.get("total_drawers").is_some());
        assert!(out.get("active_collab_session_id").is_some());
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

    // ── get_drawer: deterministic read-by-id ─────────────────────────────────

    #[test]
    fn get_drawer_returns_full_content_for_existing_id() {
        let app = test_app();
        // A body larger than MAX_SENSITIVE_FIELD_CHARS (4_000): this exercises
        // full-body round-trip and fixes excerpt truncation, one of the two
        // failure modes get_drawer addresses (the other being that semantic
        // search cannot deterministically return a known-id drawer).
        let big = "x".repeat(4_500);
        let added = handle_add_drawer(
            &app,
            &json!({"content": big, "wing": "ironrace-memory", "room": "collab-drafts"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();

        let out = handle_get_drawer(&app, &json!({"id": id})).unwrap();
        assert_eq!(out["found"].as_bool(), Some(true));
        assert_eq!(out["id"].as_str(), Some(id.as_str()));
        assert_eq!(out["wing"].as_str(), Some("ironrace-memory"));
        assert_eq!(out["room"].as_str(), Some("collab-drafts"));
        // Full body round-trips verbatim — not truncated at the excerpt cap.
        assert_eq!(out["content"].as_str(), Some(big.as_str()));
        assert_eq!(out["content_truncated"].as_bool(), Some(false));
        assert_eq!(out["content_redacted"].as_bool(), Some(false));
        // Provenance fields written by add_drawer are present and well-typed.
        assert_eq!(out["added_by"].as_str(), Some("mcp"));
        assert_eq!(out["source_file"].as_str(), Some(""));
        assert!(out.get("filed_at").is_some(), "filed_at must be present");
        assert!(out["filed_at"].is_string(), "filed_at must be a string");
        assert!(out.get("date").is_some(), "date must be present");
        assert!(out["date"].is_string(), "date must be a string");
    }

    #[test]
    fn add_drawer_logical_key_overwrites_current_context() {
        let app = test_app();
        let first = handle_add_drawer(
            &app,
            &json!({
                "content": "current context v1",
                "wing": "project",
                "room": "current",
                "logical_key": "task state"
            }),
        )
        .unwrap();
        let second = handle_add_drawer(
            &app,
            &json!({
                "content": "current context v2",
                "wing": "project",
                "room": "current",
                "logical_key": "task state"
            }),
        )
        .unwrap();

        let id = first["id"].as_str().unwrap();
        assert_eq!(second["id"].as_str(), Some(id));
        assert_eq!(second["id_strategy"].as_str(), Some("logical_key"));
        let out = handle_get_drawer(&app, &json!({"id": id})).unwrap();
        assert_eq!(out["content"].as_str(), Some("current context v2"));
        assert_eq!(out["source_file"].as_str(), Some("logical:task state"));
        assert_eq!(app.db.count_drawers(Some("project")).unwrap(), 1);
    }

    #[test]
    fn get_drawer_can_omit_content_for_metadata_only() {
        let app = test_app();
        let body = "metadata-only body";
        let added = handle_add_drawer(
            &app,
            &json!({"content": body, "wing": "test", "room": "refs"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();

        let out = handle_get_drawer(&app, &json!({"id": id, "include_content": false})).unwrap();
        assert_eq!(out["found"].as_bool(), Some(true));
        assert_eq!(out["content_included"].as_bool(), Some(false));
        assert_eq!(out["content_redacted"].as_bool(), Some(false));
        assert_eq!(out["content_chars"].as_u64(), Some(body.len() as u64));
        assert!(out.get("content").is_none(), "body must be omitted");
        assert!(
            out.get("content_truncated").is_none(),
            "truncation flag only applies when content is returned"
        );
    }

    #[test]
    fn get_drawer_respects_max_chars() {
        let app = test_app();
        let body = "abcdef";
        let added = handle_add_drawer(
            &app,
            &json!({"content": body, "wing": "test", "room": "refs"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();

        let out = handle_get_drawer(&app, &json!({"id": id, "max_chars": 3})).unwrap();
        assert_eq!(out["content"].as_str(), Some("abc"));
        assert_eq!(out["content_truncated"].as_bool(), Some(true));
        assert_eq!(out["content_included"].as_bool(), Some(true));
    }

    #[test]
    fn get_drawer_hash_only_returns_hash_without_body() {
        let app = test_app();
        let body = "hash me";
        let added = handle_add_drawer(
            &app,
            &json!({"content": body, "wing": "test", "room": "refs"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();

        let out = handle_get_drawer(&app, &json!({"id": id, "hash_only": true})).unwrap();
        let expected_hash = super::super::shared::sha256_hex(body);
        assert_eq!(out["hash_only"].as_bool(), Some(true));
        assert_eq!(out["content_hash"].as_str(), Some(expected_hash.as_str()));
        assert_eq!(out["content_included"].as_bool(), Some(false));
        assert!(out.get("content").is_none(), "hash-only must omit body");
    }

    #[test]
    fn get_drawer_hash_only_redacts_hash_in_restricted_mode() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let model_dir = dir.path().join("model");
        let state_dir = dir.path().join("state");

        let trusted = {
            let config = Config {
                db_path: db_path.clone(),
                model_dir: model_dir.clone(),
                model_dir_explicit: true,
                state_dir: state_dir.clone(),
                mcp_access_mode: McpAccessMode::Trusted,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };
        let added = handle_add_drawer(
            &trusted,
            &json!({"content": "secret hash body", "wing": "secrets", "room": "vault"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        drop(trusted);

        let restricted = {
            let config = Config {
                db_path,
                model_dir,
                model_dir_explicit: true,
                state_dir,
                mcp_access_mode: McpAccessMode::Restricted,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };

        let out = handle_get_drawer(&restricted, &json!({"id": id, "hash_only": true})).unwrap();
        assert_eq!(out["hash_only"].as_bool(), Some(true));
        assert_eq!(out["content_hash_redacted"].as_bool(), Some(true));
        assert_eq!(out["content_redacted"].as_bool(), Some(true));
        assert!(out.get("content_hash").is_none());
        assert!(out.get("content").is_none());
    }

    #[test]
    fn get_drawer_reports_not_found_for_unknown_id() {
        let app = test_app();
        // A well-formed (32-char hex) id that was never written.
        let missing = "0".repeat(32);
        let out = handle_get_drawer(&app, &json!({"id": missing})).unwrap();
        assert_eq!(out["found"].as_bool(), Some(false));
        assert_eq!(out["id"].as_str(), Some(missing.as_str()));
        assert!(out.get("content").is_none(), "no content on a miss");
    }

    #[test]
    fn get_drawer_rejects_non_hex_id() {
        let app = test_app();
        assert!(handle_get_drawer(&app, &json!({"id": "not-a-hex-id!!"})).is_err());
        // Missing id is also a validation error.
        assert!(handle_get_drawer(&app, &json!({})).is_err());
    }

    #[test]
    fn get_drawer_returns_content_in_read_only_mode() {
        // ReadOnly does not redact sensitive content — confirm the full body is
        // returned unredacted. Write via a Trusted app (ReadOnly may block writes),
        // then read back via a ReadOnly app against the same database.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let model_dir = dir.path().join("model");
        let state_dir = dir.path().join("state");

        let trusted = {
            let config = Config {
                db_path: db_path.clone(),
                model_dir: model_dir.clone(),
                model_dir_explicit: true,
                state_dir: state_dir.clone(),
                mcp_access_mode: McpAccessMode::Trusted,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };
        let body = "read-only mode content body";
        let added = handle_add_drawer(
            &trusted,
            &json!({"content": body, "wing": "test", "room": "general"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        drop(trusted);

        let readonly = {
            let config = Config {
                db_path,
                model_dir,
                model_dir_explicit: true,
                state_dir,
                mcp_access_mode: McpAccessMode::ReadOnly,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };
        std::mem::forget(dir);

        let out = handle_get_drawer(&readonly, &json!({"id": id})).unwrap();
        assert_eq!(out["found"].as_bool(), Some(true));
        assert_eq!(out["content_redacted"].as_bool(), Some(false));
        assert_eq!(out["content"].as_str(), Some(body));
    }

    #[test]
    fn get_drawer_redacts_content_in_restricted_mode() {
        // Write in a trusted app, then read the same DB via a restricted app to
        // confirm by-id fetch honors access-mode redaction like search does.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mem.sqlite3");
        let model_dir = dir.path().join("model");
        let state_dir = dir.path().join("state");

        let trusted = {
            let config = Config {
                db_path: db_path.clone(),
                model_dir: model_dir.clone(),
                model_dir_explicit: true,
                state_dir: state_dir.clone(),
                mcp_access_mode: McpAccessMode::Trusted,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };
        let added = handle_add_drawer(
            &trusted,
            &json!({"content": "secret body", "wing": "secrets", "room": "vault"}),
        )
        .unwrap();
        let id = added["id"].as_str().unwrap().to_string();
        drop(trusted);

        let restricted = {
            let config = Config {
                db_path,
                model_dir,
                model_dir_explicit: true,
                state_dir,
                mcp_access_mode: McpAccessMode::Restricted,
                embed_mode: EmbedMode::Noop,
            };
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(App::new(config).unwrap())
        };
        let out = handle_get_drawer(&restricted, &json!({"id": id})).unwrap();
        assert_eq!(out["found"].as_bool(), Some(true));
        assert_eq!(out["content_redacted"].as_bool(), Some(true));
        assert!(
            out["content"].is_null(),
            "restricted mode must not leak the body"
        );
        // Parity: sensitive metadata is also withheld when the body is redacted.
        assert!(
            out["source_file"].is_null(),
            "restricted mode must not leak source_file"
        );
        assert!(
            out["added_by"].is_null(),
            "restricted mode must not leak added_by"
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
