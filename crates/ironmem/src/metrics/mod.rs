//! Metrics helpers shared by the MCP server (response sizing) and the lifecycle
//! hooks (occupancy sampling), per METRICS_SPEC §5/§6/§8.
//!
//! Two layers live here:
//! - **Pure calc** (`estimate_tokens`, `occupancy_pct`, `extract_last_assistant_usage`,
//!   `hook_event_for`, `now_rfc3339`) — no DB/env access, unit-testable in isolation.
//! - **Best-effort sinks** (`account_mcp_response`, `record_occupancy_sample`) — take a
//!   `&Database` and write metric rows. They never propagate DB errors (logged via
//!   `tracing::warn!`) so a metrics failure cannot break MCP transport or a hook.
//!   `IRONMEM_METRICS`/`IRONMEM_CONTEXT_WINDOW` gating is read fresh by the callers
//!   (`search::tunables`), not here.

/// Token usage extracted from a transcript's last assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
}

/// METRICS_SPEC §6.2 estimate: `ceil(chars / 4)`.
pub(crate) fn estimate_tokens(chars: i64) -> i64 {
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
pub(crate) fn occupancy_pct(
    input_tokens: i64,
    cache_read_input_tokens: i64,
    window: i64,
) -> Option<f64> {
    if window <= 0 {
        return None;
    }
    Some((input_tokens + cache_read_input_tokens) as f64 / window as f64)
}

/// RFC3339 UTC timestamp, matching existing metric-row call sites
/// (`chrono::Utc::now().to_rfc3339()`).
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Reverse-scan a transcript JSONL string for the LAST assistant message's
/// `usage` object. Mirrors the reverse-scan shape used by the review extractor
/// in `hook.rs`. Handles both a top-level `usage` and a nested
/// `message.usage` envelope. Missing numeric fields default to 0. Returns
/// `None` when no assistant `usage` is found or the input is empty/malformed.
pub(crate) fn extract_last_assistant_usage(raw: &str) -> Option<Usage> {
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

use crate::db::metrics::{NewOccupancySample, NewTokenUsage, SessionSummary};
use crate::db::schema::Database;

/// Record one MCP response's size (METRICS_SPEC §5.1, Decisions D1/D2/D2b/D6).
/// Always inserts a diagnostic `token_usage` row; atomically accumulates
/// `session_summary.mcp_chars_served` (engine-side, race-free across the
/// MCP-server and hook processes) when a session id is known. Best-effort: all
/// DB errors are logged, never returned.
pub(crate) fn account_mcp_response(
    db: &Database,
    chars: i64,
    harness: &str,
    session_id: Option<&str>,
) {
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
    // Delta carries ONLY this writer's mcp_chars_served increment; every other
    // column is identity (0 / None) so the atomic upsert leaves hook-owned
    // fields untouched.
    let delta = SessionSummary {
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
    };
    if let Err(e) = db.accumulate_session_summary(&delta) {
        tracing::warn!("metrics: accumulate mcp_chars_served failed: {e}");
    }
}

/// Map a hook CLI name to the `occupancy_samples.hook_event` enum value
/// (METRICS_SPEC §5.2 / §8.2). `stop` → `session-stop` (CHECK-constraint safe).
pub(crate) fn hook_event_for(hook_name: &str) -> Option<&'static str> {
    match hook_name {
        "session-start" => Some("session-start"),
        "stop" => Some("session-stop"),
        "precompact" => Some("precompact"),
        _ => None,
    }
}

/// Record one occupancy sample + merge the session summary (Decisions D4/D5/D6).
/// Best-effort. Caller guarantees `session_id` is `Some` (absent-id is skipped
/// by the caller per D4). `usage` is `None` when the transcript had no usable
/// assistant usage → a deterministic zero-token sample is still written.
pub(crate) fn record_occupancy_sample(
    db: &Database,
    harness: &str,
    session_id: &str,
    workspace_root: Option<&str>,
    hook_event: &str,
    usage: Option<Usage>,
    window: i64,
) {
    let u = usage.unwrap_or_default();
    let occ = occupancy_pct(u.input_tokens, u.cache_read_input_tokens, window);
    // One clock read for the whole logical event so the sample row and the
    // summary's started_at/ended_at can never drift apart.
    let ts = now_rfc3339();
    let sample = NewOccupancySample {
        ts: ts.clone(),
        harness: harness.to_string(),
        session_id: Some(session_id.to_string()),
        workspace_root: workspace_root.map(|s| s.to_string()),
        hook_event: Some(hook_event.to_string()),
        input_tokens: u.input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        context_window: window,
        occupancy_pct: occ,
    };
    if let Err(e) = db.insert_occupancy_sample(&sample) {
        tracing::warn!("metrics: insert occupancy_sample failed: {e}");
    }

    // Atomic engine-side merge (preserves mcp_chars_served written by the MCP
    // process; additive fields carry only this event's increment).
    let delta = SessionSummary {
        session_id: session_id.to_string(),
        harness: harness.to_string(),
        workspace_root: workspace_root.map(|s| s.to_string()),
        started_at: Some(ts.clone()),
        ended_at: if hook_event == "session-stop" {
            Some(ts)
        } else {
            None
        },
        peak_occupancy_pct: occ,
        total_input_tokens: u.input_tokens,
        total_output_tokens: u.output_tokens,
        mcp_chars_served: 0,
        compactions: if hook_event == "precompact" { 1 } else { 0 },
    };
    if let Err(e) = db.accumulate_session_summary(&delta) {
        tracing::warn!("metrics: accumulate occupancy summary failed: {e}");
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
