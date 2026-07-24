use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::collab::Agent;
use crate::error::MemoryError;
use crate::search::sanitizer::{is_content_word, normalize_content_word};

/// Maximum allowed value for search `limit`.
pub(super) const MAX_SEARCH_LIMIT: usize = 25;
/// Default-mode per-result ceiling for search excerpts; use `full: true` or
/// `get_drawer` to retrieve complete bodies.
pub(super) const MAX_SEARCH_EXCERPT_CHARS: usize = 300;
/// Maximum allowed value for list/read `limit` parameters.
pub(super) const MAX_READ_LIMIT: usize = 100;
/// Maximum allowed BFS traversal depth.
pub(super) const MAX_DEPTH: usize = 10;
/// Maximum characters returned per sensitive text field.
pub(super) const MAX_SENSITIVE_FIELD_CHARS: usize = 4_000;
/// Shared write/read content ceiling for drawer bodies.
///
/// On the **write side**, `sanitize_content` enforces this as a *byte* length
/// (`value.len()`). On the **read side**, `render_sensitive_text` enforces it
/// as a *char* count (`.chars().take()`). Since chars ≤ bytes (UTF-8 encodes
/// each code-point in 1–4 bytes), the read cap can never truncate a body the
/// write side accepted — the round-trip guarantee holds, though it is a
/// consequence of the encoding contract rather than an explicit equality.
pub(super) const MAX_DRAWER_CONTENT_CHARS: usize = 100_000;
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

pub(super) fn optional_bool(args: &Value, key: &str, default: bool) -> Result<bool, MemoryError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| MemoryError::Validation(format!("{key} must be a boolean"))),
    }
}

pub(super) fn require_agent(value: &str) -> Result<Agent, MemoryError> {
    value
        .parse::<Agent>()
        .map_err(|_| MemoryError::Validation("agent must be 'claude' or 'codex'".to_string()))
}

/// Thin wrapper around `require_agent` for the `implementer` field on
/// `collab_start`. Same accept-set today, but isolates the input-validation
/// site so a future divergence (e.g., adding a `codex-cli` variant only valid
/// as an agent identity, not as a v3 batch implementer) doesn't regress
/// silently.
pub(super) fn require_implementer(value: &str) -> Result<Agent, MemoryError> {
    value
        .parse::<Agent>()
        .map_err(|_| MemoryError::Validation("implementer must be 'claude' or 'codex'".to_string()))
}

/// Return the other collab protocol role for the given sender.
///
/// This is a **two-party collab helper**: it is only meaningful within the
/// bounded Claude↔Codex protocol.  Generic harness code that needs to name a
/// harness should use [`crate::harness::HarnessId`] instead of `Agent`.
pub(super) fn collab_counterpart(agent: Agent) -> Agent {
    match agent {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
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

pub(super) fn render_search_excerpt(
    content: &str,
    clean_query: &str,
    max_chars: usize,
    redact: bool,
) -> (Value, bool, bool, usize) {
    if redact {
        return render_sensitive_text(content, max_chars, redact);
    }

    let tokens: Vec<String> = clean_query
        .split_whitespace()
        .filter(|token| is_content_word(token))
        .map(normalize_content_word)
        .collect();
    if tokens.is_empty() {
        return render_sensitive_text(content, max_chars, false);
    }

    let lowered_content = lowercase_char_map(content);
    // ponytail: naive per-token scan — fine at limit<=25; switch to FTS5 snippet()/offsets or a single Aho-Corasick pass if search widens
    let matched_bytes = tokens
        .iter()
        .filter_map(|token| find_case_insensitive_match(&lowered_content, token))
        .min_by_key(|(start, _)| *start);
    let Some((match_start_byte, match_end_byte)) = matched_bytes else {
        return render_sensitive_text(content, max_chars, false);
    };

    let chars: Vec<char> = content.chars().collect();
    let char_offsets: Vec<usize> = content
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(content.len()))
        .collect();
    debug_assert!(content.is_char_boundary(match_start_byte));
    debug_assert!(content.is_char_boundary(match_end_byte));
    let Some(match_start) = char_offsets.binary_search(&match_start_byte).ok() else {
        return render_sensitive_text(content, max_chars, false);
    };
    let Some(match_end) = char_offsets.binary_search(&match_end_byte).ok() else {
        return render_sensitive_text(content, max_chars, false);
    };

    if max_chars == 0 {
        return (Value::String(String::new()), !content.is_empty(), false, 0);
    }

    let (start, end) = centered_excerpt_bounds(&chars, match_start, match_end, max_chars);
    let leading_marker = start > 0;
    let trailing_marker = end < chars.len();
    let mut excerpt = String::new();
    if leading_marker && max_chars > 0 {
        excerpt.push('…');
    }
    let marker_chars = usize::from(leading_marker) + usize::from(trailing_marker);
    let content_capacity = max_chars.saturating_sub(marker_chars);
    let content_end = start.saturating_add(content_capacity).min(end);
    excerpt.extend(chars[start..content_end].iter());
    if trailing_marker && excerpt.chars().count() < max_chars {
        excerpt.push('…');
    }

    let consumed_chars = excerpt.chars().count();
    debug_assert!(consumed_chars <= max_chars);
    (
        Value::String(excerpt),
        leading_marker || trailing_marker,
        false,
        consumed_chars,
    )
}

const MAX_OUTWARD_WHITESPACE_SNAP_CHARS: usize = 15;

fn lowercase_char_map(content: &str) -> Vec<(char, usize, usize)> {
    let mut lowered = Vec::new();
    for (start, character) in content.char_indices() {
        let end = start + character.len_utf8();
        lowered.extend(character.to_lowercase().map(|lower| (lower, start, end)));
    }
    lowered
}

fn find_case_insensitive_match(
    lowered_content: &[(char, usize, usize)],
    token: &str,
) -> Option<(usize, usize)> {
    let lowered_token: Vec<char> = token.chars().flat_map(|c| c.to_lowercase()).collect();
    if lowered_token.is_empty() {
        return None;
    }

    lowered_content
        .windows(lowered_token.len())
        .find_map(|window| {
            let matches = window
                .iter()
                .map(|(character, _, _)| *character)
                .zip(lowered_token.iter().copied())
                .all(|(left, right)| left == right);
            matches.then(|| (window[0].1, window[window.len() - 1].2))
        })
}

pub(super) fn centered_excerpt_bounds(
    chars: &[char],
    match_start: usize,
    match_end: usize,
    max_chars: usize,
) -> (usize, usize) {
    let total_chars = chars.len();
    if total_chars <= max_chars {
        return (0, total_chars);
    }
    if max_chars == 0 {
        return (0, 0);
    }
    if max_chars == 1 {
        let match_start = match_start.min(total_chars);
        let match_end = match_end.clamp(match_start, total_chars);
        let midpoint = match_start + (match_end - match_start) / 2;
        return (midpoint, midpoint);
    }

    // Reserve both markers first. The loop gives that spare character back
    // when the selected window reaches either content edge.
    let mut capacity = max_chars.saturating_sub(2);
    let mut bounds = (0, 0);
    for _ in 0..8 {
        let centered = centered_window(total_chars, match_start, match_end, capacity);
        bounds = snap_window_outward(chars, centered, capacity, match_start, match_end);
        let marker_count = usize::from(bounds.0 > 0) + usize::from(bounds.1 < total_chars);
        let next_capacity = max_chars.saturating_sub(marker_count);
        if next_capacity == capacity {
            break;
        }
        capacity = next_capacity;
    }

    bounds
}

fn centered_window(
    total_chars: usize,
    match_start: usize,
    match_end: usize,
    capacity: usize,
) -> (usize, usize) {
    let match_start = match_start.min(total_chars);
    let match_end = match_end.clamp(match_start, total_chars);
    let match_len = match_end - match_start;

    if capacity == 0 {
        let midpoint = match_start + match_len / 2;
        return (midpoint, midpoint);
    }
    if capacity >= total_chars {
        return (0, total_chars);
    }

    let match_center = match_start + (match_end - match_start) / 2;
    let mut start = match_center.saturating_sub(capacity / 2);
    if start + capacity > total_chars {
        start = total_chars - capacity;
    }
    let mut end = start + capacity;

    if match_len <= capacity {
        if start > match_start {
            start = match_start;
            end = (start + capacity).min(total_chars);
        }
        if end < match_end {
            end = match_end;
            start = end.saturating_sub(capacity);
        }
    }

    (start, end)
}

#[derive(Clone, Copy)]
struct WindowCandidate {
    start: usize,
    end: usize,
    snapped_boundaries: usize,
}

fn snap_window_outward(
    chars: &[char],
    centered: (usize, usize),
    capacity: usize,
    match_start: usize,
    match_end: usize,
) -> (usize, usize) {
    let (start, end) = centered;
    let left = outward_left_boundary(chars, start);
    let right = outward_right_boundary(chars, end);
    let left_snapped = left < start;
    let right_snapped = right > end;
    let mut candidates = vec![WindowCandidate {
        start,
        end,
        snapped_boundaries: 0,
    }];

    if left_snapped {
        let candidate_end = left.saturating_add(capacity).min(chars.len());
        if candidate_end >= match_end {
            candidates.push(WindowCandidate {
                start: left,
                end: candidate_end,
                snapped_boundaries: 1,
            });
        }
    }
    if right_snapped {
        let candidate_start = right.saturating_sub(capacity);
        if candidate_start <= match_start {
            candidates.push(WindowCandidate {
                start: candidate_start,
                end: right,
                snapped_boundaries: 1,
            });
        }
    }
    if left_snapped && right_snapped && right.saturating_sub(left) <= capacity {
        candidates.push(WindowCandidate {
            start: left,
            end: right,
            snapped_boundaries: 2,
        });
    }

    candidates
        .into_iter()
        .max_by(|left_candidate, right_candidate| {
            left_candidate
                .snapped_boundaries
                .cmp(&right_candidate.snapped_boundaries)
                .then_with(|| {
                    (left_candidate.end - left_candidate.start)
                        .cmp(&(right_candidate.end - right_candidate.start))
                })
                .then_with(|| {
                    let left_distance =
                        (left_candidate.start + left_candidate.end).abs_diff(start + end);
                    let right_distance =
                        (right_candidate.start + right_candidate.end).abs_diff(start + end);
                    right_distance.cmp(&left_distance)
                })
                .then_with(|| right_candidate.start.cmp(&left_candidate.start))
        })
        .map(|candidate| (candidate.start, candidate.end))
        .unwrap_or((start, end))
}

fn outward_left_boundary(chars: &[char], start: usize) -> usize {
    let lower_bound = start.saturating_sub(MAX_OUTWARD_WHITESPACE_SNAP_CHARS);
    for index in (lower_bound..start).rev() {
        if chars[index].is_whitespace() {
            return index;
        }
    }
    start
}

fn outward_right_boundary(chars: &[char], end: usize) -> usize {
    let upper_bound = end
        .saturating_add(MAX_OUTWARD_WHITESPACE_SNAP_CHARS)
        .min(chars.len());
    for (index, character) in chars
        .iter()
        .enumerate()
        .skip(end)
        .take(upper_bound.saturating_sub(end))
    {
        if character.is_whitespace() {
            return index + 1;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::optional_bool;
    use crate::error::MemoryError;
    use serde_json::json;

    #[test]
    fn optional_bool_uses_default_when_absent_or_null() {
        assert!(!optional_bool(&json!({}), "full", false).unwrap());
        assert!(optional_bool(&json!({}), "full", true).unwrap());
        assert!(!optional_bool(&json!({"full": null}), "full", false).unwrap());
        assert!(optional_bool(&json!({"full": null}), "full", true).unwrap());
    }

    #[test]
    fn optional_bool_returns_json_boolean_value() {
        assert!(optional_bool(&json!({"full": true}), "full", false).unwrap());
        assert!(!optional_bool(&json!({"full": false}), "full", true).unwrap());
    }

    #[test]
    fn optional_bool_rejects_non_boolean_values() {
        for value in [json!("true"), json!(1), json!({})] {
            let error = optional_bool(&json!({"full": value}), "full", false).unwrap_err();
            assert!(matches!(
                error,
                MemoryError::Validation(message) if message == "full must be a boolean"
            ));
        }
    }
}
