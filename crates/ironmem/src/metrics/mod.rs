//! Pure metrics helpers shared by the MCP server (response sizing) and the
//! lifecycle hooks (occupancy sampling). No DB or env access lives here — the
//! call sites own writes and tunable reads; these functions are unit-testable
//! in isolation (METRICS_SPEC §5/§6/§8).

/// Token usage extracted from a transcript's last assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
}

/// METRICS_SPEC §6.2 estimate: `ceil(chars / 4)`.
pub fn estimate_tokens(chars: i64) -> i64 {
    if chars <= 0 {
        0
    } else {
        // `i64::div_ceil` is unstable; cast through `u64` (guarded > 0 above)
        // for the stable `div_ceil`, avoiding clippy's `manual_div_ceil`.
        (chars as u64).div_ceil(4) as i64
    }
}

/// METRICS_SPEC §8.1: `(input + cache_read) / context_window`.
/// `None` when the window is non-positive (avoids div-by-zero / inversion).
pub fn occupancy_pct(input_tokens: i64, cache_read_input_tokens: i64, window: i64) -> Option<f64> {
    if window <= 0 {
        return None;
    }
    Some((input_tokens + cache_read_input_tokens) as f64 / window as f64)
}

/// RFC3339 UTC timestamp, matching existing metric-row call sites
/// (`chrono::Utc::now().to_rfc3339()`).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Reverse-scan a transcript JSONL string for the LAST assistant message's
/// `usage` object. Mirrors the reverse-scan shape used by the review extractor
/// in `hook.rs`. Handles both a top-level `usage` and a nested
/// `message.usage` envelope. Missing numeric fields default to 0. Returns
/// `None` when no assistant `usage` is found or the input is empty/malformed.
pub fn extract_last_assistant_usage(raw: &str) -> Option<Usage> {
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(usage) = find_assistant_usage(&value) {
            return Some(usage);
        }
    }
    None
}

fn find_assistant_usage(value: &serde_json::Value) -> Option<Usage> {
    let is_assistant = value.get("type").and_then(|t| t.as_str()) == Some("assistant")
        || value.get("role").and_then(|t| t.as_str()) == Some("assistant")
        || value
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|t| t.as_str())
            == Some("assistant");
    if !is_assistant {
        return None;
    }
    let usage = value
        .get("usage")
        .or_else(|| value.get("message").and_then(|m| m.get("usage")))?;
    let g = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    Some(Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_creation_input_tokens: g("cache_creation_input_tokens"),
        cache_read_input_tokens: g("cache_read_input_tokens"),
    })
}

use crate::db::metrics::{NewTokenUsage, SessionSummary};
use crate::db::schema::Database;

/// Record one MCP response's size (METRICS_SPEC §5.1, Decisions D1/D2/D2b/D6).
/// Always inserts a diagnostic `token_usage` row; accumulates
/// `session_summary.mcp_chars_served` via read-modify-write only when a
/// session id is known. Best-effort: all DB errors are logged, never returned.
pub fn account_mcp_response(db: &Database, chars: i64, harness: &str, session_id: Option<&str>) {
    let row = NewTokenUsage {
        ts: now_rfc3339(),
        source: "mcp_response".to_string(),
        harness: harness.to_string(),
        model: None,
        session_id: session_id.map(|s| s.to_string()),
        collab_session_id: None,
        collab_phase: None,
        task_tag: None,
        input_tokens: 0,
        output_tokens: estimate_tokens(chars),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        estimated: true,
        chars,
        cost_usd: None,
    };
    if let Err(e) = db.insert_token_usage(&row) {
        tracing::warn!("metrics: insert mcp_response token_usage failed: {e}");
    }

    let Some(sid) = session_id else { return };
    let merged = match db.get_session_summary(sid) {
        Ok(Some(mut s)) => {
            s.mcp_chars_served += chars;
            s
        }
        Ok(None) => SessionSummary {
            session_id: sid.to_string(),
            harness: harness.to_string(),
            workspace_root: None,
            started_at: None,
            ended_at: None,
            peak_occupancy_pct: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            mcp_chars_served: chars,
            compactions: 0,
        },
        Err(e) => {
            tracing::warn!("metrics: get_session_summary failed: {e}");
            return;
        }
    };
    if let Err(e) = db.upsert_session_summary(&merged) {
        tracing::warn!("metrics: upsert mcp_chars_served failed: {e}");
    }
}

/// Process-global lock serializing tests that mutate the `IRONMEM_METRICS`
/// env var. Env vars are process-wide, so unrelated test modules that flip the
/// kill switch (here, `search::tunables` and `mcp::server`) must share ONE lock
/// or they clobber each other under the parallel test runner.
#[cfg(test)]
pub(crate) static METRICS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_ceil_div_4() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(8), 2);
    }

    #[test]
    fn occupancy_pct_uses_input_plus_cache_read_over_window() {
        let pct = occupancy_pct(100_000, 50_000, 200_000).unwrap();
        assert!((pct - 0.75).abs() < 1e-9);
    }

    #[test]
    fn occupancy_pct_none_when_window_nonpositive() {
        assert!(occupancy_pct(1, 1, 0).is_none());
        assert!(occupancy_pct(1, 1, -10).is_none());
    }

    #[test]
    fn extract_last_assistant_usage_reverse_scans_to_last_assistant() {
        let raw = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":3}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":111,"output_tokens":22,"cache_creation_input_tokens":5,"cache_read_input_tokens":33}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
        );
        let u = extract_last_assistant_usage(raw).unwrap();
        assert_eq!(u.input_tokens, 111);
        assert_eq!(u.output_tokens, 22);
        assert_eq!(u.cache_creation_input_tokens, 5);
        assert_eq!(u.cache_read_input_tokens, 33);
    }

    #[test]
    fn extract_last_assistant_usage_missing_fields_default_zero() {
        let raw = r#"{"type":"assistant","message":{"usage":{"input_tokens":7}}}"#;
        let u = extract_last_assistant_usage(raw).unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn extract_last_assistant_usage_none_when_absent_or_malformed() {
        assert!(extract_last_assistant_usage("").is_none());
        assert!(extract_last_assistant_usage("not json\n{also not}").is_none());
        assert!(
            extract_last_assistant_usage(r#"{"type":"user","message":{"content":"x"}}"#).is_none()
        );
    }
}
