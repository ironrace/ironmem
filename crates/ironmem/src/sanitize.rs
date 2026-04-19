//! Input sanitization helpers for user-facing tool arguments and hook metadata.

use regex::Regex;
use std::sync::LazyLock;

use crate::error::MemoryError;

const MAX_NAME_LENGTH: usize = 128;

static SAFE_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_ .'\-]{0,126}[a-zA-Z0-9]$").unwrap());

/// Validate and sanitize a wing/room/entity name.
pub fn sanitize_name(value: &str, field_name: &str) -> Result<String, MemoryError> {
    let value = value.trim();

    if value.is_empty() {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be a non-empty string"
        )));
    }

    if value.len() < 2 {
        return Err(MemoryError::Validation(format!(
            "{field_name} must be at least 2 characters long"
        )));
    }

    if value.len() > MAX_NAME_LENGTH {
        return Err(MemoryError::Validation(format!(
            "{field_name} exceeds maximum length of {MAX_NAME_LENGTH}"
        )));
    }

    if value.contains("..") || value.contains('/') || value.contains('\\') {
        return Err(MemoryError::Validation(format!(
            "{field_name} contains invalid path characters"
        )));
    }

    if value.contains('\0') {
        return Err(MemoryError::Validation(format!(
            "{field_name} contains null bytes"
        )));
    }

    if !SAFE_NAME_RE.is_match(value) {
        return Err(MemoryError::Validation(format!(
            "{field_name} contains invalid characters"
        )));
    }

    Ok(value.to_string())
}

/// Validate content length and null bytes.
pub fn sanitize_content(value: &str, max_length: usize) -> Result<&str, MemoryError> {
    let value = value.trim();

    if value.is_empty() {
        return Err(MemoryError::Validation(
            "content must be a non-empty string".into(),
        ));
    }

    if value.len() > max_length {
        return Err(MemoryError::Validation(format!(
            "content exceeds maximum length of {max_length}"
        )));
    }

    if value.contains('\0') {
        return Err(MemoryError::Validation(
            "content contains null bytes".into(),
        ));
    }

    Ok(value)
}

/// Sanitize a harness or hook name for safe inclusion in diary entries.
///
/// Keeps only `[a-zA-Z0-9_-]`, truncates to 64 characters, and returns
/// `"unknown"` if the result would be empty — preventing shell metacharacter
/// injection into durable diary content.
pub fn sanitize_harness(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

/// Sanitize a session ID to prevent path traversal.
pub fn sanitize_session_id(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .collect();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_name_valid() {
        assert_eq!(sanitize_name("projects", "wing").unwrap(), "projects");
        assert_eq!(sanitize_name("my notes", "room").unwrap(), "my notes");
        assert_eq!(sanitize_name("v2.0", "tag").unwrap(), "v2.0");
    }

    #[test]
    fn test_sanitize_name_trims() {
        assert_eq!(sanitize_name("  hello  ", "field").unwrap(), "hello");
    }

    #[test]
    fn test_sanitize_name_rejects_empty() {
        assert!(sanitize_name("", "field").is_err());
        assert!(sanitize_name("   ", "field").is_err());
    }

    #[test]
    fn test_sanitize_name_rejects_path_traversal() {
        assert!(sanitize_name("../etc/passwd", "field").is_err());
        assert!(sanitize_name("foo/bar", "field").is_err());
        assert!(sanitize_name("foo\\bar", "field").is_err());
    }

    #[test]
    fn test_sanitize_name_rejects_null_bytes() {
        assert!(sanitize_name("hello\0world", "field").is_err());
    }

    #[test]
    fn test_sanitize_name_rejects_too_long() {
        let long = "a".repeat(200);
        assert!(sanitize_name(&long, "field").is_err());
    }

    #[test]
    fn test_sanitize_name_rejects_single_char() {
        let err = sanitize_name("a", "field").unwrap_err();
        assert!(
            err.to_string().contains("at least 2 characters"),
            "Expected length error, got: {err}"
        );
    }

    #[test]
    fn test_sanitize_name_rejects_special_chars() {
        assert!(sanitize_name("<script>", "field").is_err());
        assert!(sanitize_name("DROP TABLE;", "field").is_err());
    }

    #[test]
    fn test_sanitize_content_valid() {
        assert_eq!(
            sanitize_content("hello world", 1000).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn test_sanitize_content_rejects_empty() {
        assert!(sanitize_content("", 1000).is_err());
    }

    #[test]
    fn test_sanitize_content_rejects_too_long() {
        let long = "x".repeat(1001);
        assert!(sanitize_content(&long, 1000).is_err());
    }

    #[test]
    fn test_sanitize_content_rejects_null_bytes() {
        assert!(sanitize_content("hello\0world", 1000).is_err());
    }

    #[test]
    fn test_sanitize_session_id_strips_unsafe() {
        assert_eq!(sanitize_session_id("abc-123_def"), "abc-123_def");
        assert_eq!(sanitize_session_id("../../../etc"), "etc");
        assert_eq!(sanitize_session_id(""), "unknown");
    }

    #[test]
    fn sanitize_harness_passes_valid_values_unchanged() {
        assert_eq!(sanitize_harness("claude-code"), "claude-code");
        assert_eq!(sanitize_harness("codex"), "codex");
        assert_eq!(sanitize_harness("claude_code_2"), "claude_code_2");
    }

    #[test]
    fn sanitize_harness_strips_metacharacters() {
        // `-` is allowed (needed for "claude-code"); spaces, semicolons, slashes are stripped
        assert_eq!(sanitize_harness("codex; rm -rf /"), "codexrm-rf");
        assert_eq!(sanitize_harness("$(evil)"), "evil");
        assert_eq!(sanitize_harness("a&b|c`d"), "abcd");
    }

    #[test]
    fn sanitize_harness_truncates_to_64_chars() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_harness(&long).len(), 64);
    }

    #[test]
    fn sanitize_harness_returns_unknown_for_empty_result() {
        assert_eq!(sanitize_harness(""), "unknown");
        assert_eq!(sanitize_harness(";;;"), "unknown");
    }
}
