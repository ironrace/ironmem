use serde_json::{json, Value};

use crate::diary;
use crate::error::MemoryError;
use crate::sanitize;

use super::shared::{render_sensitive_text, MAX_READ_LIMIT, MAX_SENSITIVE_FIELD_CHARS};
use crate::mcp::app::App;

pub(super) fn handle_diary_write(app: &App, args: &Value) -> Result<Value, MemoryError> {
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

pub(super) fn handle_diary_read(app: &App, args: &Value) -> Result<Value, MemoryError> {
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
