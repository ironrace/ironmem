//! Parse `claude -p --output-format stream-json --verbose` output into a
//! [`CliResult`], summing per-assistant-message token usage across the parent
//! orchestrator session AND any Task-subagent sub-sessions.
//!
//! Why this exists (METRICS_SPEC §12 2026-06-19): the single-envelope
//! `--output-format json` `usage` block reports ONLY the orchestrator session's
//! tokens. Task-subagents run in separate sessions whose usage is never rolled
//! up into that top-level block, so the `/ultrareview-local` + `/pr-review-toolkit`
//! review fan-out and the `subagent-driven-development` implement subagents were
//! invisible — undercounting `tokens_to_done` on BOTH arms' Claude side (the
//! superpowers arm's single `claude -p` runs subagent-driven-dev too). The
//! stream-json transcript emits one `assistant` event per message — parent and
//! subagent alike, each carrying its own `message.usage` — so summing those,
//! deduplicated by `message.id`, is the canonical Claude-side accounting.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::client::{CliResult, Usage};

/// Parse a stream-json transcript. Each non-blank line is one JSON event:
///
/// - `type == "assistant"` → `message.id` + `message.usage`. The usage of every
///   distinct message id is summed; this is where subagent tokens enter (each
///   subagent assistant message has its own id, distinct from the parent's).
///   Dedup is last-write-wins per id, so a streamed/repeated id is counted once
///   at its final cumulative usage rather than double-counted.
/// - `type == "result"` → the run's terminal envelope: `is_error` and the printed
///   `result` text (where the collab driver's sentinel lines live). The terminal
///   envelope's OWN top-level `usage` is deliberately NOT added — it is the
///   parent's roll-up and would double-count the per-message assistant usage.
///
/// Fail-loud, never a silent zero row: malformed JSON, an empty transcript, or a
/// transcript with no terminal `result` event are all errors. The summed usage
/// itself may legitimately be zero for a crashed run; the caller's run-level
/// zero-token guard decides whether that is acceptable for the outcome.
pub fn parse_stream_json(stdout: &str) -> Result<CliResult> {
    let mut usage_by_id: BTreeMap<String, Usage> = BTreeMap::new();
    let mut result_text: Option<String> = None;
    let mut is_error = false;
    let mut saw_result = false;
    let mut saw_any = false;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        saw_any = true;
        let event: Value = serde_json::from_str(line).map_err(|e| {
            anyhow::anyhow!("failed to parse stream-json line as JSON: {e} — line: {line}")
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(message) = event.get("message") else {
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_str) else {
                    continue;
                };
                // `Usage` is permissive (every field `#[serde(default)]`, no
                // `deny_unknown_fields` — a real Claude `usage` block carries extra
                // keys like `service_tier`/`cache_creation`, so denying them would
                // reject well-formed data). An ABSENT `usage` is legitimate for some
                // event shapes → default to 0 silently. A PRESENT-but-non-object
                // `usage` is genuine schema drift that would silently undercount
                // `tokens_to_done`, so log it loud rather than swallow (the value is
                // still counted as 0 so one odd message can't fail the whole
                // transcript; aggregate zero is additionally caught run-level).
                let usage = match message.get("usage") {
                    None => Usage::default(),
                    Some(u) => serde_json::from_value::<Usage>(u.clone()).unwrap_or_else(|e| {
                        eprintln!(
                            "abeval: assistant message {id} has a non-deserializable \
                             usage block (schema drift?), counting it as zero: {e} — \
                             usage: {u}"
                        );
                        Usage::default()
                    }),
                };
                usage_by_id.insert(id.to_string(), usage);
            }
            Some("result") => {
                saw_result = true;
                is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                result_text = event
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
    }

    if !saw_any {
        bail!("stream-json transcript was empty — refusing to record a zero-usage row");
    }
    if !saw_result {
        bail!("stream-json transcript had no terminal `result` event (schema drift?)");
    }

    let mut usage = Usage::default();
    for u in usage_by_id.values() {
        usage.add_assign(u);
    }

    Ok(CliResult {
        is_error,
        result: result_text.unwrap_or_default(),
        usage,
    })
}
