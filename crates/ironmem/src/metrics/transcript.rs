//! Full-transcript token-usage parsers for the `stop`/`precompact` hooks.
//!
//! Distinct from the occupancy tail reader (`extract_last_assistant_usage`):
//! - The tail reads only the last 2 MB and extracts a single last-assistant
//!   message — sufficient for occupancy sampling but undercounts subagent-heavy
//!   streams (METRICS_SPEC §12).
//! - These parsers consume the ENTIRE transcript and emit one [`TranscriptRow`]
//!   per distinct Claude assistant message (keyed by `message.id`) or one
//!   cumulative row for a Codex rollout.
//!
//! Neither parser touches `extract_last_assistant_usage` — that function stays
//! unchanged for occupancy. This module is pure (no DB / env access); the hook
//! wiring in `hook.rs` calls the parsers and then delegates to the DB layer.
//!
//! Production crate MUST NOT import the `abeval` benchmark crate. The parsing
//! rules are ported from `benchmarks/abeval/src/stream_usage.rs` and
//! `benchmarks/abeval/src/codex_tokens.rs` per METRICS_SPEC §12 2026-06-19.

use std::collections::BTreeMap;

/// Four-component token usage for one assistant message or one Codex session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TranscriptUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
}

/// One parsed token-usage row from a transcript.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TranscriptRow {
    /// Idempotency key: `transcript:<session_or_hash>:<message_id_or_codex_final>`.
    pub turn_id: String,
    /// Model name, if known (e.g. from `message.model`).
    pub model: Option<String>,
    pub usage: TranscriptUsage,
}

// ---------------------------------------------------------------------------
// Claude stream-json parser
// ---------------------------------------------------------------------------

/// Parse a `claude -p --output-format stream-json --verbose` transcript.
///
/// Rules (METRICS_SPEC §12 / ported from abeval `stream_usage.rs`):
/// - Every non-blank line must be valid JSON → `Err` on any non-JSON line.
/// - `type == "assistant"` events contribute `message.usage`; dedup by
///   `message.id` is last-write-wins.
/// - `type == "result"` event marks the end; its own `usage` is NOT summed
///   (it is the parent roll-up and would double-count).
/// - A missing terminal `result` event → `Err` (loud error, never silent zero).
/// - An empty transcript → `Err`.
///
/// Returns one `TranscriptRow` per distinct `message.id`, ordered by id (BTreeMap
/// iteration order). The `turn_id` for each row is
/// `transcript:<session_id_or_hash>:<message_id>` where `session_id_or_hash` is
/// the caller-supplied `harness_session_id` (or a content hash when None).
pub(crate) fn parse_claude_stream_json(
    raw: &str,
    harness_session_id: Option<&str>,
) -> Result<Vec<TranscriptRow>, String> {
    let mut usage_by_id: BTreeMap<String, (Option<String>, TranscriptUsage)> = BTreeMap::new();
    let mut saw_result = false;
    let mut saw_any = false;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        saw_any = true;
        let event: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("non-JSON line in transcript: {e} — line: {line}"))?;

        match event.get("type").and_then(|v| v.as_str()) {
            Some("assistant") => {
                let Some(message) = event.get("message") else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let model = message
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let usage = parse_usage_object(message.get("usage"));
                // Last-write-wins per message.id for dedup.
                usage_by_id.insert(id.to_string(), (model, usage));
            }
            Some("result") => {
                saw_result = true;
                // Deliberately do NOT add result.usage — it is the parent
                // roll-up and would double-count per-message assistant usage.
            }
            _ => {}
        }
    }

    if !saw_any {
        return Err("stream-json transcript was empty".to_string());
    }
    if !saw_result {
        return Err(
            "stream-json transcript had no terminal `result` event — refusing to persist possibly-incomplete usage".to_string(),
        );
    }

    let session_key = harness_session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| content_hash(raw));

    let rows = usage_by_id
        .into_iter()
        .map(|(msg_id, (model, usage))| TranscriptRow {
            turn_id: format!("transcript:{session_key}:{msg_id}"),
            model,
            usage,
        })
        .collect();

    Ok(rows)
}

/// Parse a Codex rollout JSONL transcript.
///
/// Rules (METRICS_SPEC §12 / ported from abeval `codex_tokens.rs`):
/// - `session_meta` event carries `cwd` identity (used by caller) — not parsed here
///   since the hook doesn't need cross-session attribution; the caller supplies the
///   `harness_session_id`.
/// - `event_msg` with `type == "token_count"` → uses the FINAL cumulative
///   `total_token_usage`.
/// - `input = input_tokens − cached_input_tokens` (Codex `input_tokens` INCLUDES
///   cached; Anthropic's convention does not).
/// - `cache_read = cached_input_tokens`.
/// - `cache_creation = 0` (not tracked by Codex rollout).
/// - `cached > input` → `Err` (loud error, never silent miscount).
/// - No `token_count` event → `Ok(None)` (session still running or no usage yet).
/// - No parseable `session_meta` cwd is acceptable for the hook path (we use the
///   caller-supplied `harness_session_id` directly).
///
/// Returns `Some(TranscriptRow)` when usage is available, `None` when it is not
/// yet present (callers should skip gracefully).
pub(crate) fn parse_codex_rollout(
    raw: &str,
    harness_session_id: Option<&str>,
) -> Result<Option<TranscriptRow>, String> {
    let mut last_usage: Option<(i64, i64, i64)> = None; // (input, cached, output)

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // Codex rollout tolerates non-conforming lines
        };
        let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "event_msg" {
            let payload = &event["payload"];
            if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                if let Some(tu) = payload.get("info").and_then(|v| v.get("total_token_usage")) {
                    let g = |k: &str| tu.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                    let input = g("input_tokens");
                    let cached = g("cached_input_tokens");
                    let output = g("output_tokens");
                    last_usage = Some((input, cached, output));
                }
            }
        }
    }

    let Some((input_raw, cached, output)) = last_usage else {
        return Ok(None);
    };

    if cached > input_raw {
        return Err(format!(
            "corrupt Codex rollout: cached_input_tokens ({cached}) exceeds input_tokens ({input_raw}) — refusing to miscount"
        ));
    }

    let session_key = harness_session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| content_hash(raw));

    Ok(Some(TranscriptRow {
        turn_id: format!("transcript:{session_key}:codex-final"),
        model: None,
        usage: TranscriptUsage {
            input_tokens: input_raw - cached,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cached,
        },
    }))
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn parse_usage_object(v: Option<&serde_json::Value>) -> TranscriptUsage {
    let Some(u) = v else {
        return TranscriptUsage::default();
    };
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    TranscriptUsage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_creation_input_tokens: g("cache_creation_input_tokens"),
        cache_read_input_tokens: g("cache_read_input_tokens"),
    }
}

/// Stable 16-hex-char hash of raw content for use as a session key fallback.
/// SHA-256 truncated to 8 bytes (16 hex chars) — sufficient for dedup within
/// one transcript; NOT a cryptographic guarantee.
fn content_hash(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Task 1: Claude parser — happy path ───────────────────────────────────

    fn make_assistant_line(id: &str, model: &str, inp: i64, out: i64, cc: i64, cr: i64) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": id,
                "model": model,
                "usage": {
                    "input_tokens": inp,
                    "output_tokens": out,
                    "cache_creation_input_tokens": cc,
                    "cache_read_input_tokens": cr
                }
            }
        })
        .to_string()
    }

    fn result_line() -> String {
        serde_json::json!({"type": "result", "is_error": false, "result": "done"}).to_string()
    }

    #[test]
    fn claude_sums_two_assistant_messages() {
        let raw = format!(
            "{}\n{}\n{}\n",
            make_assistant_line("msg-1", "claude-sonnet-4-6", 100, 20, 5, 10),
            make_assistant_line("msg-2", "claude-sonnet-4-6", 200, 30, 0, 50),
            result_line()
        );
        let rows = parse_claude_stream_json(&raw, Some("sess-abc")).unwrap();
        assert_eq!(rows.len(), 2, "one row per distinct message.id");

        let r1 = rows.iter().find(|r| r.turn_id.ends_with(":msg-1")).unwrap();
        assert_eq!(r1.usage.input_tokens, 100);
        assert_eq!(r1.usage.output_tokens, 20);
        assert_eq!(r1.usage.cache_creation_input_tokens, 5);
        assert_eq!(r1.usage.cache_read_input_tokens, 10);
        assert_eq!(r1.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(r1.turn_id, "transcript:sess-abc:msg-1");

        let r2 = rows.iter().find(|r| r.turn_id.ends_with(":msg-2")).unwrap();
        assert_eq!(r2.usage.input_tokens, 200);
        assert_eq!(r2.usage.output_tokens, 30);
        assert_eq!(r2.usage.cache_read_input_tokens, 50);
        assert_eq!(r2.turn_id, "transcript:sess-abc:msg-2");
    }

    #[test]
    fn claude_dedup_duplicate_message_id_last_write_wins() {
        let raw = format!(
            "{}\n{}\n{}\n",
            make_assistant_line("msg-1", "claude-sonnet-4-6", 100, 20, 0, 0),
            // Same id, different usage — second write wins.
            make_assistant_line("msg-1", "claude-sonnet-4-6", 999, 888, 7, 6),
            result_line()
        );
        let rows = parse_claude_stream_json(&raw, Some("sess-abc")).unwrap();
        assert_eq!(rows.len(), 1, "dedup: only one row for msg-1");
        assert_eq!(rows[0].usage.input_tokens, 999, "last write wins");
        assert_eq!(rows[0].usage.output_tokens, 888);
    }

    #[test]
    fn claude_does_not_sum_result_usage() {
        // If we accidentally summed the terminal `result.usage`, input_tokens
        // would be 100 + 1000 = 1100. Assert it is exactly 100.
        let result_with_usage = serde_json::json!({
            "type": "result",
            "is_error": false,
            "result": "done",
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 500
            }
        })
        .to_string();
        let raw = format!(
            "{}\n{}\n",
            make_assistant_line("msg-1", "claude-sonnet-4-6", 100, 20, 0, 0),
            result_with_usage
        );
        let rows = parse_claude_stream_json(&raw, Some("sess-abc")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].usage.input_tokens, 100,
            "result.usage must not be summed"
        );
    }

    // ── Task 2: Claude parser error-semantics + Codex parser ─────────────────

    #[test]
    fn claude_rejects_non_json_line() {
        let raw = format!("this is not json\n{}\n", result_line());
        let err = parse_claude_stream_json(&raw, Some("sess")).unwrap_err();
        assert!(
            err.contains("non-JSON"),
            "expected non-JSON error, got: {err}"
        );
    }

    #[test]
    fn claude_rejects_empty_transcript() {
        let err = parse_claude_stream_json("", Some("sess")).unwrap_err();
        assert!(
            err.contains("empty"),
            "expected empty-transcript error, got: {err}"
        );
        let err2 = parse_claude_stream_json("   \n  \n", Some("sess")).unwrap_err();
        assert!(err2.contains("empty"));
    }

    #[test]
    fn claude_rejects_missing_terminal_result() {
        let raw = format!(
            "{}\n",
            make_assistant_line("msg-1", "claude-sonnet-4-6", 100, 20, 0, 0)
        );
        let err = parse_claude_stream_json(&raw, Some("sess")).unwrap_err();
        assert!(
            err.contains("result"),
            "expected missing-result error, got: {err}"
        );
    }

    #[test]
    fn codex_uses_final_cumulative_token_count_with_cache_subtraction() {
        let first_count = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 1000,
                        "cached_input_tokens": 400,
                        "output_tokens": 200
                    }
                }
            }
        })
        .to_string();
        // Final cumulative overrides the first.
        let second_count = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 1500,
                        "cached_input_tokens": 600,
                        "output_tokens": 300
                    }
                }
            }
        })
        .to_string();
        let raw = format!("{}\n{}\n", first_count, second_count);
        let row = parse_codex_rollout(&raw, Some("codex-sess-1"))
            .unwrap()
            .unwrap();
        assert_eq!(row.turn_id, "transcript:codex-sess-1:codex-final");
        // input = 1500 − 600 = 900
        assert_eq!(row.usage.input_tokens, 900);
        assert_eq!(row.usage.cache_read_input_tokens, 600);
        assert_eq!(row.usage.output_tokens, 300);
        assert_eq!(row.usage.cache_creation_input_tokens, 0);
    }

    #[test]
    fn codex_returns_none_when_no_token_count_event() {
        let raw = serde_json::json!({
            "type": "session_meta",
            "payload": { "cwd": "/tmp/repo", "timestamp": "2026-06-19T10:00:00Z" }
        })
        .to_string();
        let result = parse_codex_rollout(&raw, Some("codex-sess")).unwrap();
        assert!(result.is_none(), "no token_count → Ok(None)");
    }

    #[test]
    fn codex_rejects_cached_greater_than_input() {
        let raw = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 200,
                        "output_tokens": 50
                    }
                }
            }
        })
        .to_string();
        let err = parse_codex_rollout(&raw, Some("codex-sess")).unwrap_err();
        assert!(
            err.contains("cached") && err.contains("exceeds"),
            "expected cached>input error, got: {err}"
        );
    }

    #[test]
    fn content_hash_is_stable_and_16_hex_chars() {
        let h1 = content_hash("hello");
        let h2 = content_hash("hello");
        assert_eq!(h1, h2, "hash must be deterministic");
        assert_eq!(h1.len(), 16, "hash must be 16 hex chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
