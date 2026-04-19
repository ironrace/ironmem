use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::MemoryError;

/// Maximum allowed value for search `limit`.
pub(super) const MAX_SEARCH_LIMIT: usize = 25;
/// Maximum allowed value for list/read `limit` parameters.
pub(super) const MAX_READ_LIMIT: usize = 100;
/// Maximum allowed BFS traversal depth.
pub(super) const MAX_DEPTH: usize = 10;
/// Maximum characters returned per sensitive text field.
pub(super) const MAX_SENSITIVE_FIELD_CHARS: usize = 4_000;
/// Maximum aggregate characters returned across search results.
pub(super) const MAX_SEARCH_RESPONSE_CHARS: usize = 32_000;
/// Maximum content length accepted by collab queue messages.
pub(super) const MAX_COLLAB_CONTENT_CHARS: usize = 32_000;
/// Maximum capability field length.
pub(super) const MAX_COLLAB_CAP_FIELD_CHARS: usize = 512;

pub(super) fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, MemoryError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| MemoryError::Validation(format!("{key} is required")))
}

pub(super) fn require_agent(value: &str) -> Result<&str, MemoryError> {
    if matches!(value, "claude" | "codex") {
        Ok(value)
    } else {
        Err(MemoryError::Validation(
            "agent must be 'claude' or 'codex'".to_string(),
        ))
    }
}

pub(super) fn other_agent(agent: &str) -> &'static str {
    if agent == "claude" {
        "codex"
    } else {
        "claude"
    }
}

/// Validate that an ID is a 16 or 32-character hex string (SHA-256 truncated).
/// Accepts both lengths for backwards compatibility with existing data.
pub(super) fn validate_hex_id(value: &str, field_name: &str) -> Result<(), MemoryError> {
    if !(value.len() == 16 || value.len() == 32) || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be a 16 or 32-character hex string"
        )));
    }
    Ok(())
}

/// Validate that a date string matches YYYY-MM-DD format.
pub(super) fn validate_date_format(value: &str, field_name: &str) -> Result<(), MemoryError> {
    if chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be in YYYY-MM-DD format, got: {value}"
        )));
    }
    Ok(())
}

pub(super) fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

pub(super) fn render_sensitive_text(
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
