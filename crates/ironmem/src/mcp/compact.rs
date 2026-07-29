//! Lossless JSON array compaction for MCP tool responses.

use serde_json::{json, Map, Value};

pub struct CompactResult {
    pub value: Value,
    pub original_bytes: usize,
    pub compacted_bytes: usize,
}

pub fn try_compact(value: &Value) -> CompactResult {
    let original_bytes = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    let compacted = compact_json_value(value);
    let compacted_bytes = serde_json::to_vec(&compacted).map(|v| v.len()).unwrap_or(0);
    if compacted_bytes >= original_bytes {
        return CompactResult {
            value: value.clone(),
            original_bytes,
            compacted_bytes: original_bytes,
        };
    }
    CompactResult {
        value: compacted,
        original_bytes,
        compacted_bytes,
    }
}

pub fn compact_json_value(value: &Value) -> Value {
    let Value::Array(items) = value else {
        return value.clone();
    };
    if items.len() < 2 {
        return value.clone();
    }
    // All items must be objects with identical key sets.
    let Some(first) = items.first().and_then(Value::as_object) else {
        return value.clone();
    };
    let keys: Vec<String> = first.keys().cloned().collect();
    if keys.is_empty() {
        return value.clone();
    }
    for item in items {
        let Some(obj) = item.as_object() else {
            return value.clone();
        };
        if obj.len() != keys.len() || !keys.iter().all(|k| obj.contains_key(k)) {
            return value.clone();
        }
    }
    // Build column-major representation. Note: the key list is intentionally
    // NOT duplicated as a separate "keys" array — `columns` (a JSON object)
    // already carries every key as its own map key, and JSON object equality
    // is order-independent, so a parallel "keys" array would be pure
    // redundancy. For narrow arrays (few rows, few columns) that redundant
    // array's overhead can exceed the savings from column-major layout,
    // defeating the whole point of compaction. Deriving keys solely from
    // `columns` keeps the envelope minimal while remaining fully
    // self-describing for `expand_compact_value`.
    let mut columns = Map::new();
    for key in &keys {
        let col: Vec<Value> = items
            .iter()
            .map(|item| item.get(key).cloned().unwrap_or(Value::Null))
            .collect();
        columns.insert(key.clone(), Value::Array(col));
    }
    json!({
        "__compact_v1": {
            "columns": columns,
        }
    })
}

/// Tools whose responses may be compacted when `IRONMEM_COMPACT_RESPONSES=1`.
/// Start with `search` — its homogeneous drawer-result arrays are the
/// highest-value target. Add more tools as their response shapes are validated.
pub const COMPACTABLE_TOOLS: &[&str] = &["search"];

/// Whether compaction should be applied to this tool's response.
/// Requires both the env-var opt-in AND the tool being in the allow-list, so
/// compaction never changes response shape without an explicit operator opt-in.
pub fn should_compact(tool_name: Option<&str>) -> bool {
    let Some(name) = tool_name else { return false };
    compaction_enabled() && COMPACTABLE_TOOLS.contains(&name)
}

/// Whether long `failure_report` topic payloads may be compacted. A failure
/// report is a `collab_send` topic rather than an advertised MCP tool, so it
/// intentionally does not belong in [`COMPACTABLE_TOOLS`].
pub fn should_compact_failure_reports() -> bool {
    compaction_enabled()
}

fn compaction_enabled() -> bool {
    std::env::var("IRONMEM_COMPACT_RESPONSES")
        .ok()
        .is_some_and(|value| value == "1")
}

/// Compact a failure-report log while retaining its classification prefix and
/// the final actionable lines. The input is returned unchanged when it fits.
pub fn compact_failure_log(coding_failure: &str, max_chars: usize) -> String {
    if coding_failure.chars().count() <= max_chars {
        return coding_failure.to_string();
    }

    let lines: Vec<&str> = coding_failure.lines().collect();
    if lines.len() <= 4 {
        return coding_failure.chars().take(max_chars).collect();
    }

    let tail_count = 3.min(lines.len() - 1);
    let omitted = lines.len() - 1 - tail_count;
    let candidate = format!(
        "{}\n[... {omitted} lines omitted ...]\n{}",
        lines[0],
        lines[lines.len() - tail_count..].join("\n")
    );

    if candidate.chars().count() <= max_chars {
        candidate
    } else {
        coding_failure.chars().take(max_chars).collect()
    }
}

pub fn expand_compact_value(value: &Value) -> Value {
    let Some(envelope) = value.get("__compact_v1") else {
        return value.clone();
    };
    let Some(columns) = envelope.get("columns").and_then(Value::as_object) else {
        return value.clone();
    };
    let row_count = columns
        .values()
        .next()
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut rows = Vec::with_capacity(row_count);
    for i in 0..row_count {
        let mut obj = Map::new();
        for (key, col) in columns {
            let val = col
                .as_array()
                .and_then(|c| c.get(i))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert(key.clone(), val);
        }
        rows.push(Value::Object(obj));
    }
    Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homogeneous_array_round_trips() {
        let input = serde_json::json!([
            {"id": "a", "score": 1.0, "label": "foo"},
            {"id": "b", "score": 2.0, "label": "bar"},
            {"id": "c", "score": 3.0, "label": "baz"},
        ]);
        let result = try_compact(&input);
        // Must be smaller (column-major deduplicates the keys)
        assert!(
            result.compacted_bytes < result.original_bytes,
            "compacted ({}) should be smaller than original ({})",
            result.compacted_bytes,
            result.original_bytes
        );
        // Must carry the envelope marker
        assert!(result.value.get("__compact_v1").is_some());
        // Round-trip must restore the original
        assert_eq!(expand_compact_value(&result.value), input);
    }

    #[test]
    fn heterogeneous_array_passes_through() {
        let input = serde_json::json!([
            {"id": "a", "score": 1.0},
            {"id": "b", "name": "different_keys"},
        ]);
        let result = try_compact(&input);
        assert_eq!(
            result.value, input,
            "heterogeneous arrays must pass through unchanged"
        );
        assert_eq!(result.compacted_bytes, result.original_bytes);
    }

    #[test]
    fn non_array_passes_through() {
        let input = serde_json::json!({"status": "ok", "count": 42});
        let result = try_compact(&input);
        assert_eq!(result.value, input);
        assert_eq!(result.compacted_bytes, result.original_bytes);
    }

    #[test]
    fn empty_array_passes_through() {
        let input = serde_json::json!([]);
        let result = try_compact(&input);
        assert_eq!(result.value, input);
    }

    #[test]
    fn single_element_array_passes_through() {
        let input = serde_json::json!([{"id": "a", "score": 1.0}]);
        let result = try_compact(&input);
        // With only one element, column-major adds overhead — should pass through.
        assert_eq!(expand_compact_value(&result.value), input);
    }

    #[test]
    fn expand_non_compact_is_identity() {
        let input = serde_json::json!({"status": "ok"});
        assert_eq!(expand_compact_value(&input), input);
    }

    #[test]
    fn compaction_disabled_by_default() {
        // `EnvGuard::pin` takes `ENV_LOCK` for this var so an unguarded
        // `remove_var` here cannot race `compaction_enabled_for_search_with_env_var`
        // running concurrently in another thread of the same test binary — see
        // `ENV_LOCK`'s doc comment in `config.rs` for the failure mode this
        // avoids. Drop restores whatever the var held before this test ran.
        let _guard = crate::config::EnvGuard::pin("IRONMEM_COMPACT_RESPONSES");
        std::env::remove_var("IRONMEM_COMPACT_RESPONSES");
        assert!(!should_compact(Some("search")));
    }

    #[test]
    fn compaction_enabled_for_search_with_env_var() {
        let _guard = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
        assert!(should_compact(Some("search")));
    }

    #[test]
    fn compaction_not_enabled_for_unlisted_tool() {
        let _guard = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
        assert!(!should_compact(Some("status")));
        assert!(!should_compact(None));
    }

    #[test]
    fn failure_report_compaction_requires_env_opt_in() {
        let _guard = crate::config::EnvGuard::pin("IRONMEM_COMPACT_RESPONSES");
        std::env::remove_var("IRONMEM_COMPACT_RESPONSES");
        assert!(!should_compact_failure_reports());
        std::env::set_var("IRONMEM_COMPACT_RESPONSES", "1");
        assert!(should_compact_failure_reports());
    }

    #[test]
    fn short_failure_log_passes_through() {
        let log = "git_push_failed: rejected by remote";
        assert_eq!(compact_failure_log(log, 2048), log);
    }

    #[test]
    fn long_failure_log_preserves_prefix_and_tail() {
        let prefix = "git_push_failed:";
        let middle = (0..100)
            .map(|index| format!("  verbose line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = "error: failed to push some refs to 'origin'\nhint: Updates were rejected";
        let log = format!("{prefix}\n{middle}\n{tail}");
        let compacted = compact_failure_log(&log, 300);

        assert!(
            compacted.chars().count() <= 300,
            "compacted length {} exceeds 300",
            compacted.chars().count()
        );
        assert!(compacted.starts_with(prefix));
        assert!(compacted.contains("hint: Updates were rejected"));
        assert!(compacted.contains("[..."));
        assert!(compacted.contains("lines omitted"));
    }

    #[test]
    fn compacted_failure_preserves_classification() {
        use crate::collab::{classify, FailureClass};

        let prefix = "git_push_failed:";
        let verbose = (0..50)
            .map(|index| format!("  frame {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = " rejected by hooks";
        let log = format!("{prefix}{verbose}\n{tail}");
        let compacted = compact_failure_log(&log, 200);

        assert_eq!(classify(&compacted), FailureClass::Tooling);
    }
}
