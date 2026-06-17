//! Attribute Codex tokens to a collab task by reading the Codex rollout JSONL
//! logs (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) and matching by the
//! session's working directory (the per-task worktree) and a time window.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::client::Usage;

/// Inclusive time window for matching a rollout's session start.
#[derive(Debug, Clone, Copy)]
pub struct TimeWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl TimeWindow {
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        self.start <= t && t <= self.end
    }
}

/// Tokens + identity extracted from one rollout file.
#[derive(Debug, Clone)]
pub struct CodexSessionTokens {
    pub cwd: String,
    pub started_at: DateTime<Utc>,
    pub usage: Usage,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct TotalTokenUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cached_input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Read one rollout's text. Returns:
/// - `Ok(Some(_))` when both a `session_meta` (cwd + timestamp) AND at least one
///   `token_count` event are present (uses the FINAL, cumulative one).
/// - `Ok(None)` when there is a `session_meta` but no `token_count` yet (the
///   session has not reported usage — not an error).
/// - `Err` when there is no parseable `session_meta` at all (a rollout we cannot
///   attribute is a hard error, surfaced rather than silently dropped).
pub fn parse_rollout(jsonl: &str) -> Result<Option<CodexSessionTokens>> {
    let mut cwd: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut last_usage: Option<TotalTokenUsage> = None;

    for raw in jsonl.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let line: Line = match serde_json::from_str(raw) {
            Ok(l) => l,
            Err(_) => continue, // tolerate non-conforming lines
        };
        match line.kind.as_str() {
            "session_meta" => {
                cwd = line
                    .payload
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                started_at = line
                    .payload
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
            }
            "event_msg" => {
                if line.payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                    if let Some(tu) = line
                        .payload
                        .get("info")
                        .and_then(|v| v.get("total_token_usage"))
                    {
                        if let Ok(parsed) = serde_json::from_value::<TotalTokenUsage>(tu.clone()) {
                            last_usage = Some(parsed);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let cwd = cwd.ok_or_else(|| anyhow!("rollout has no parseable session_meta cwd"))?;
    let started_at =
        started_at.ok_or_else(|| anyhow!("rollout session_meta has no parseable timestamp"))?;

    let Some(tu) = last_usage else {
        return Ok(None);
    };

    // Corrected mapping (see plan Global Constraints): Codex `input_tokens`
    // INCLUDES `cached_input_tokens`, unlike Anthropic's convention. Subtract so
    // `Usage::total()` equals Codex's own `total_tokens` and the cached portion is
    // counted once (as cache_read). `output_tokens` already includes reasoning.
    let usage = Usage {
        input_tokens: tu.input_tokens.saturating_sub(tu.cached_input_tokens),
        cache_read_input_tokens: tu.cached_input_tokens,
        output_tokens: tu.output_tokens,
        cache_creation_input_tokens: 0,
    };

    Ok(Some(CodexSessionTokens {
        cwd,
        started_at,
        usage,
    }))
}
