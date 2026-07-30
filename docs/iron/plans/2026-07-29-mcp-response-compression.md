# MCP Response Compression Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven-development (recommended) or executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in, lossless JSON array compaction for eligible MCP tool responses and collab failure-log relays, with response-size telemetry comparing enabled vs disabled paths.

**Architecture:** A new `compact` module at `crates/ironmem/src/mcp/compact.rs` provides pure, stateless JSON array compaction (column-major dedup of homogeneous JSON arrays). The MCP response path (`tool_success_response` in `server.rs`) applies compaction when a per-tool opt-in flag is set. A separate function compacts collab failure-report payloads by trimming bounded log bodies while preserving error classification and actionable tail content. Telemetry records pre/post byte sizes through the existing `account_mcp_response` metrics path.

**Tech Stack:** Rust, serde_json, existing ironmem metrics infrastructure

## Global Constraints

- Compression defaults to OFF — no behavioral change without explicit opt-in.
- All compaction is lossless and round-trippable.
- JSON-RPC and MCP tool response shapes are preserved byte-for-byte when compression is disabled or a response is ineligible.
- Do not change collab review-diff ingestion or introduce non-reversible compression.
- Follows existing codebase patterns: pure calc functions + best-effort sinks, feature-gated where appropriate.

---

### Task 1: JSON Array Compactor — Pure Lossless Transform

**Files:**
- Create: `crates/ironmem/src/mcp/compact.rs`
- Modify: `crates/ironmem/src/mcp/mod.rs:1-9` (add `pub mod compact;`)

**Interfaces:**
- Consumes: `serde_json::Value` (tool result JSON)
- Produces:
  - `fn compact_json_value(value: &serde_json::Value) -> serde_json::Value` — recursively compacts top-level JSON arrays into column-major form; non-array values pass through unchanged.
  - `fn expand_compact_value(value: &serde_json::Value) -> serde_json::Value` — inverse: restores the original row-major form. Used in tests to prove round-trip.
  - `struct CompactResult { pub value: serde_json::Value, pub original_bytes: usize, pub compacted_bytes: usize }` — carries the compacted value and size delta for telemetry.
  - `fn try_compact(value: &serde_json::Value) -> CompactResult` — compacts and measures; returns the original unchanged if compaction would increase size or the value has no arrays.

The compaction scheme: given a JSON array of objects with identical key sets, produce `{"__compact_v1": {"keys": [...], "columns": {"key1": [...], ...}}}`. Objects with heterogeneous keys are left as-is. Nested arrays within objects are NOT recursively compacted (scope boundary — top-level arrays only). The `__compact_v1` envelope makes detection and expansion trivial.

- [ ] **Step 1: Write the failing test — round-trip homogeneous array**

```rust
// crates/ironmem/src/mcp/compact.rs (at bottom, in #[cfg(test)] mod tests)
#[test]
fn homogeneous_array_round_trips() {
    let input = serde_json::json!([
        {"id": "a", "score": 1.0, "label": "foo"},
        {"id": "b", "score": 2.0, "label": "bar"},
        {"id": "c", "score": 3.0, "label": "baz"},
    ]);
    let result = try_compact(&input);
    // Must be smaller (column-major deduplicates the keys)
    assert!(result.compacted_bytes < result.original_bytes,
        "compacted ({}) should be smaller than original ({})",
        result.compacted_bytes, result.original_bytes);
    // Must carry the envelope marker
    assert!(result.value.get("__compact_v1").is_some());
    // Round-trip must restore the original
    assert_eq!(expand_compact_value(&result.value), input);
}
```

- [ ] **Step 2: Write the failing test — heterogeneous array passes through unchanged**

```rust
#[test]
fn heterogeneous_array_passes_through() {
    let input = serde_json::json!([
        {"id": "a", "score": 1.0},
        {"id": "b", "name": "different_keys"},
    ]);
    let result = try_compact(&input);
    assert_eq!(result.value, input, "heterogeneous arrays must pass through unchanged");
    assert_eq!(result.compacted_bytes, result.original_bytes);
}
```

- [ ] **Step 3: Write the failing test — non-array value passes through unchanged**

```rust
#[test]
fn non_array_passes_through() {
    let input = serde_json::json!({"status": "ok", "count": 42});
    let result = try_compact(&input);
    assert_eq!(result.value, input);
    assert_eq!(result.compacted_bytes, result.original_bytes);
}
```

- [ ] **Step 4: Write the failing test — empty array passes through unchanged**

```rust
#[test]
fn empty_array_passes_through() {
    let input = serde_json::json!([]);
    let result = try_compact(&input);
    assert_eq!(result.value, input);
}
```

- [ ] **Step 5: Write the failing test — single-element array passes through (compaction would increase size)**

```rust
#[test]
fn single_element_array_passes_through() {
    let input = serde_json::json!([{"id": "a", "score": 1.0}]);
    let result = try_compact(&input);
    // With only one element, column-major adds overhead — should pass through.
    assert_eq!(expand_compact_value(&result.value), input);
}
```

- [ ] **Step 6: Write the failing test — expand on non-compact value is identity**

```rust
#[test]
fn expand_non_compact_is_identity() {
    let input = serde_json::json!({"status": "ok"});
    assert_eq!(expand_compact_value(&input), input);
}
```

- [ ] **Step 7: Run tests to verify they fail**

Run: `cargo test -p ironmem compact:: --no-default-features -- --nocapture 2>&1 | tail -30`
Expected: FAIL — module does not exist yet.

- [ ] **Step 8: Implement `compact.rs`**

```rust
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
    // Build column-major representation.
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
            "keys": keys,
            "columns": columns,
        }
    })
}

pub fn expand_compact_value(value: &Value) -> Value {
    let Some(envelope) = value.get("__compact_v1") else {
        return value.clone();
    };
    let Some(keys) = envelope.get("keys").and_then(Value::as_array) else {
        return value.clone();
    };
    let Some(columns) = envelope.get("columns").and_then(Value::as_object) else {
        return value.clone();
    };
    let key_strs: Vec<&str> = match keys.iter().map(Value::as_str).collect::<Option<Vec<_>>>() {
        Some(ks) => ks,
        None => return value.clone(),
    };
    let row_count = key_strs
        .first()
        .and_then(|k| columns.get(*k))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut rows = Vec::with_capacity(row_count);
    for i in 0..row_count {
        let mut obj = Map::new();
        for key in &key_strs {
            let val = columns
                .get(*key)
                .and_then(Value::as_array)
                .and_then(|col| col.get(i))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert(key.to_string(), val);
        }
        rows.push(Value::Object(obj));
    }
    Value::Array(rows)
}
```

- [ ] **Step 9: Register the module**

Add `pub mod compact;` to `crates/ironmem/src/mcp/mod.rs`.

- [ ] **Step 10: Run tests to verify they pass**

Run: `cargo test -p ironmem compact:: --no-default-features -- --nocapture 2>&1 | tail -30`
Expected: all 6 tests PASS.

- [ ] **Step 11: Commit**

```bash
git add crates/ironmem/src/mcp/compact.rs crates/ironmem/src/mcp/mod.rs
git commit -m "feat(mcp): add lossless JSON array compactor module (#229)"
```

---

### Task 2: Opt-In Tool Response Compaction at the Serialization Boundary

**Files:**
- Modify: `crates/ironmem/src/mcp/server.rs:1034-1047` (`tool_success_response`)
- Modify: `crates/ironmem/src/mcp/tools/mod.rs` (add per-tool opt-in constant)
- Modify: `crates/ironmem/src/mcp/compact.rs` (add `COMPACTABLE_TOOLS` constant)

**Interfaces:**
- Consumes: `compact::try_compact` (from Task 1), `tools::call_tool` result
- Produces:
  - `const COMPACTABLE_TOOLS: &[&str]` in `compact.rs` — tools eligible for response compaction when the env var `IRONMEM_COMPACT_RESPONSES=1` is set.
  - `fn should_compact(tool_name: Option<&str>) -> bool` in `compact.rs` — returns true when the tool is in the opt-in list AND the env var is set.
  - Modified `tool_success_response` that applies compaction when `should_compact` returns true.

Start with `search` as the first compactable tool — it returns homogeneous arrays of drawer results, making it the highest-value target for compaction.

- [ ] **Step 1: Write the failing test — compaction is off by default**

```rust
// In crates/ironmem/src/mcp/compact.rs #[cfg(test)] mod tests
#[test]
fn compaction_disabled_by_default() {
    // Ensure env var is unset for this test
    std::env::remove_var("IRONMEM_COMPACT_RESPONSES");
    assert!(!should_compact(Some("search")));
}
```

- [ ] **Step 2: Write the failing test — compaction enabled for search with env var**

```rust
#[test]
fn compaction_enabled_for_search_with_env_var() {
    let _guard = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
    assert!(should_compact(Some("search")));
}
```

- [ ] **Step 3: Write the failing test — compaction not enabled for non-listed tool**

```rust
#[test]
fn compaction_not_enabled_for_unlisted_tool() {
    let _guard = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
    assert!(!should_compact(Some("status")));
    assert!(!should_compact(None));
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p ironmem compact:: --no-default-features -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `should_compact` and `COMPACTABLE_TOOLS` do not exist.

- [ ] **Step 5: Implement opt-in gating in `compact.rs`**

Add to `compact.rs`:

```rust
/// Tools whose responses may be compacted when `IRONMEM_COMPACT_RESPONSES=1`.
/// Start with `search` — its homogeneous drawer-result arrays are the
/// highest-value target. Add more tools as their response shapes are validated.
pub const COMPACTABLE_TOOLS: &[&str] = &["search"];

/// Whether compaction should be applied to this tool's response.
/// Requires both the env-var opt-in AND the tool being in the allow-list.
pub fn should_compact(tool_name: Option<&str>) -> bool {
    let Some(name) = tool_name else { return false };
    std::env::var("IRONMEM_COMPACT_RESPONSES")
        .ok()
        .is_some_and(|v| v == "1")
        && COMPACTABLE_TOOLS.contains(&name)
}
```

- [ ] **Step 6: Modify `tool_success_response` in `server.rs`**

Change `tool_success_response` signature to accept `tool_name: Option<&str>` and apply compaction:

```rust
fn tool_success_response(
    id: Option<serde_json::Value>,
    content: &serde_json::Value,
    tool_name: Option<&str>,
) -> JsonRpcResponse {
    let effective_content = if super::compact::should_compact(tool_name) {
        let result = super::compact::try_compact(content);
        result.value
    } else {
        content.clone()
    };
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&effective_content).unwrap_or_default()
            }]
        }),
    )
}
```

Update all call sites of `tool_success_response`:
- `dispatch` (line ~1196): pass `Some(name)` from the match arm.
- `dispatch_wait_my_turn` (the wait poll path): pass `Some("collab_wait_my_turn")`.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p ironmem -- --nocapture 2>&1 | tail -30`
Expected: all tests PASS, including existing server tests (compaction defaults to off).

- [ ] **Step 8: Commit**

```bash
git add crates/ironmem/src/mcp/compact.rs crates/ironmem/src/mcp/server.rs crates/ironmem/src/mcp/tools/mod.rs
git commit -m "feat(mcp): opt-in response compaction at serialization boundary (#229)"
```

---

### Task 3: Failure-Report Log Payload Compaction

**Files:**
- Modify: `crates/ironmem/src/mcp/compact.rs` (add `compact_failure_log`)
- Modify: `crates/ironmem/src/mcp/tools/collab_events.rs:133-153` (`parse_failure_report_event`)

**Interfaces:**
- Consumes: `failure_class::classify` (existing), `compact::should_compact` pattern
- Produces:
  - `fn compact_failure_log(coding_failure: &str, max_chars: usize) -> String` in `compact.rs` — trims a failure log body while preserving: (1) the failure classification prefix (first line / prefix up to the first `:`), (2) the actionable tail content (last N lines of the log body). Returns the original string unchanged if it fits within `max_chars`.
  - Modified `parse_failure_report_event` that applies `compact_failure_log` before the length check, gated on `IRONMEM_COMPACT_RESPONSES=1`.

The compaction approach: failure logs are often large stack traces or command output. The classification prefix (e.g., `git_push_failed:`) and the last few lines (the actual error message) are the actionable parts. Middle lines (verbose log output, repetitive stack frames) are the compactable portion. A `[... N lines omitted ...]` marker replaces them.

- [ ] **Step 1: Write the failing test — short failure log passes through unchanged**

```rust
// crates/ironmem/src/mcp/compact.rs #[cfg(test)]
#[test]
fn short_failure_log_passes_through() {
    let log = "git_push_failed: rejected by remote";
    assert_eq!(compact_failure_log(log, 2048), log);
}
```

- [ ] **Step 2: Write the failing test — long failure log preserves prefix and tail**

```rust
#[test]
fn long_failure_log_preserves_prefix_and_tail() {
    let prefix = "git_push_failed:";
    let middle = (0..100).map(|i| format!("  verbose line {i}")).collect::<Vec<_>>().join("\n");
    let tail = "error: failed to push some refs to 'origin'\nhint: Updates were rejected";
    let log = format!("{prefix}\n{middle}\n{tail}");
    let compacted = compact_failure_log(&log, 300);
    assert!(compacted.len() <= 300, "compacted len {} exceeds 300", compacted.len());
    assert!(compacted.starts_with(prefix));
    assert!(compacted.contains("hint: Updates were rejected"));
    assert!(compacted.contains("[..."));
    assert!(compacted.contains("lines omitted"));
}
```

- [ ] **Step 3: Write the failing test — classification is preserved after compaction**

```rust
#[test]
fn compacted_failure_preserves_classification() {
    use crate::collab::failure_class::{classify, FailureClass};
    let prefix = "git_push_failed:";
    let verbose = (0..50).map(|i| format!("  frame {i}")).collect::<Vec<_>>().join("\n");
    let tail = " rejected by hooks";
    let log = format!("{prefix}{verbose}\n{tail}");
    let compacted = compact_failure_log(&log, 200);
    // The classification must still work on the compacted version.
    assert_eq!(classify(&compacted), FailureClass::Tooling);
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p ironmem compact_failure --no-default-features -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `compact_failure_log` does not exist.

- [ ] **Step 5: Implement `compact_failure_log`**

Add to `compact.rs`:

```rust
/// Compact a failure-report log body by preserving the classification prefix
/// and actionable tail content, replacing verbose middle content with an
/// omission marker. Returns the input unchanged if it fits within `max_chars`.
///
/// Preserves at least the first line (classification prefix) and the last
/// 3 lines (typically the actual error message and hints). The middle is
/// replaced with `[... N lines omitted ...]`.
pub fn compact_failure_log(coding_failure: &str, max_chars: usize) -> String {
    if coding_failure.chars().count() <= max_chars {
        return coding_failure.to_string();
    }
    let lines: Vec<&str> = coding_failure.lines().collect();
    if lines.len() <= 4 {
        // Too few lines to meaningfully compact — truncate the whole thing.
        return coding_failure.chars().take(max_chars).collect();
    }
    // Keep first line (prefix) and last 3 lines (actionable tail).
    let tail_count = 3.min(lines.len() - 1);
    let head = &lines[..1];
    let tail = &lines[lines.len() - tail_count..];
    let omitted = lines.len() - 1 - tail_count;
    let marker = format!("[... {omitted} lines omitted ...]");
    let candidate = format!(
        "{}\n{}\n{}",
        head.join("\n"),
        marker,
        tail.join("\n"),
    );
    if candidate.chars().count() <= max_chars {
        candidate
    } else {
        // Even head + marker + tail exceeds budget — truncate.
        coding_failure.chars().take(max_chars).collect()
    }
}
```

- [ ] **Step 6: Wire into `parse_failure_report_event` in `collab_events.rs`**

In `parse_failure_report_event`, after extracting `coding_failure` and before the length check, apply compaction when enabled:

```rust
let coding_failure = if crate::mcp::compact::should_compact(Some("failure_report")) {
    crate::mcp::compact::compact_failure_log(&coding_failure, MAX_CODING_FAILURE_CHARS)
} else {
    coding_failure
};
```

Also add `"failure_report"` to `COMPACTABLE_TOOLS` in `compact.rs`.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p ironmem compact --no-default-features -- --nocapture 2>&1 | tail -20`
Expected: all compact tests PASS.

Run: `cargo test -p ironmem collab_events --no-default-features -- --nocapture 2>&1 | tail -20`
Expected: existing collab_events tests still PASS (compaction defaults to off).

- [ ] **Step 8: Commit**

```bash
git add crates/ironmem/src/mcp/compact.rs crates/ironmem/src/mcp/tools/collab_events.rs
git commit -m "feat(mcp): add failure-report log compaction (#229)"
```

---

### Task 4: Response-Size Telemetry for Compaction Delta

**Files:**
- Modify: `crates/ironmem/src/mcp/server.rs:152-176` (`account_response_metrics`)
- Modify: `crates/ironmem/src/metrics/mod.rs:300-355` (`account_mcp_response`)
- Modify: `crates/ironmem/src/db/metrics.rs` (`NewTokenUsage` struct — add `compact_bytes` field)

**Interfaces:**
- Consumes: `compact::CompactResult` (from Task 1), existing `account_mcp_response`
- Produces:
  - Extended `account_response_metrics` / `account_mcp_response` to accept an optional `compact_delta: Option<(usize, usize)>` (original_bytes, compacted_bytes) and record it in the token_usage row.
  - Modified `write_and_account` that threads the compact delta from `tool_success_response` through to metrics.

The telemetry approach: rather than adding a new metrics table, extend the existing `token_usage` row with two optional columns (`original_response_bytes`, `compacted_response_bytes`). These are `NULL` when compaction is disabled or the tool is ineligible. The delta is comparable: `SELECT tool_name, AVG(original_response_bytes - compacted_response_bytes) AS savings FROM token_usage WHERE compacted_response_bytes IS NOT NULL GROUP BY tool_name`.

- [ ] **Step 1: Write the failing test — metrics row includes compact delta when present**

```rust
// In crates/ironmem/src/metrics/mod.rs #[cfg(test)]
#[test]
fn account_mcp_response_records_compact_delta() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3")).unwrap();
    let ctx = MetricsContext::default();
    account_mcp_response(
        &db, 100, "claude", Some("search"), None, &ctx, None,
        Some((200, 100)),
    );
    // Verify the row was written with the compact delta.
    let rows = db.query_token_usage_latest(1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].original_response_bytes, Some(200));
    assert_eq!(rows[0].compacted_response_bytes, Some(100));
}
```

- [ ] **Step 2: Write the failing test — metrics row has NULL delta when compaction not applied**

```rust
#[test]
fn account_mcp_response_null_delta_when_not_compacted() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("test.sqlite3")).unwrap();
    let ctx = MetricsContext::default();
    account_mcp_response(
        &db, 100, "claude", Some("search"), None, &ctx, None,
        None,
    );
    let rows = db.query_token_usage_latest(1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].original_response_bytes, None);
    assert_eq!(rows[0].compacted_response_bytes, None);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ironmem account_mcp_response --no-default-features -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `account_mcp_response` does not accept the compact delta parameter.

- [ ] **Step 4: Add `original_response_bytes` and `compacted_response_bytes` to `NewTokenUsage`**

In `crates/ironmem/src/db/metrics.rs`, add two optional fields to `NewTokenUsage`:

```rust
pub original_response_bytes: Option<i64>,
pub compacted_response_bytes: Option<i64>,
```

Update the `insert_token_usage` SQL to include these columns. Update `Default`/construction sites.

- [ ] **Step 5: Extend `account_mcp_response` signature**

Add `compact_delta: Option<(usize, usize)>` parameter to `account_mcp_response` in `metrics/mod.rs`. Map it into the new `NewTokenUsage` fields:

```rust
original_response_bytes: compact_delta.map(|(orig, _)| orig as i64),
compacted_response_bytes: compact_delta.map(|(_, comp)| comp as i64),
```

- [ ] **Step 6: Thread compact delta through `account_response_metrics` and `write_and_account`**

In `server.rs`:
- Add `compact_delta: Option<(usize, usize)>` to `account_response_metrics`.
- Pass it through to `account_mcp_response`.
- In `write_and_account`, when compaction was applied, pass the delta; otherwise `None`.

Update all existing call sites (`write_and_account`, `reject_mutation`) to pass `None` as the default.

- [ ] **Step 7: Wire `tool_success_response` to return the delta**

Change `tool_success_response` to return `(JsonRpcResponse, Option<(usize, usize)>)` so the caller can thread the delta to metrics. Update call sites.

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p ironmem -- --nocapture 2>&1 | tail -30`
Expected: all tests PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/ironmem/src/mcp/server.rs crates/ironmem/src/metrics/mod.rs crates/ironmem/src/db/metrics.rs
git commit -m "feat(mcp): response-size telemetry for compaction delta (#229)"
```

---

### Task 5: Integration Tests and Round-Trip Fixtures

**Files:**
- Modify: `crates/ironmem/tests/mcp_protocol.rs` (add compaction integration tests)
- Modify: `crates/ironmem/src/mcp/compact.rs` (add fixture-based round-trip tests)

**Interfaces:**
- Consumes: All prior tasks — the full compaction pipeline end-to-end
- Produces: Integration tests proving:
  1. A `search` response with `IRONMEM_COMPACT_RESPONSES=1` is compacted and the JSON-RPC envelope shape is preserved.
  2. A `search` response without the env var is unchanged.
  3. A compacted response can be expanded back to the original.
  4. A `failure_report` with a long log body is compacted when the env var is set, and the classification is preserved.
  5. Regression test: existing `mcp_protocol.rs` tests still pass (no behavioral change with compaction off).

- [ ] **Step 1: Write integration test — search response compaction end-to-end**

```rust
// crates/ironmem/tests/mcp_protocol.rs
#[tokio::test]
async fn search_response_compacted_when_enabled() {
    let _guard = ironmem::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
    // Set up app, add several drawers with similar structure, call search,
    // verify the response contains __compact_v1 envelope.
    // ... (full test body uses existing test helpers from mcp_protocol.rs)
}

#[tokio::test]
async fn search_response_unchanged_when_disabled() {
    std::env::remove_var("IRONMEM_COMPACT_RESPONSES");
    // Same setup, verify response does NOT contain __compact_v1.
}
```

- [ ] **Step 2: Write fixture-based round-trip test**

```rust
// crates/ironmem/src/mcp/compact.rs #[cfg(test)]
#[test]
fn realistic_search_results_round_trip() {
    let fixture = serde_json::json!([
        {"id": "d-001", "wing": "python-repos", "room": "general", "score": 0.92, "excerpt": "A reusable Python template..."},
        {"id": "d-002", "wing": "python-repos", "room": "general", "score": 0.88, "excerpt": "FastAPI project skeleton..."},
        {"id": "d-003", "wing": "python-repos", "room": "tools", "score": 0.85, "excerpt": "CLI argument parsing..."},
        {"id": "d-004", "wing": "claude-skills", "room": "general", "score": 0.81, "excerpt": "Skill template with..."},
        {"id": "d-005", "wing": "claude-skills", "room": "workflows", "score": 0.79, "excerpt": "Multi-agent workflow..."},
    ]);
    let result = try_compact(&fixture);
    assert!(result.compacted_bytes < result.original_bytes);
    assert_eq!(expand_compact_value(&result.value), fixture);
}
```

- [ ] **Step 3: Write failure-report compaction integration test**

```rust
// crates/ironmem/src/mcp/compact.rs #[cfg(test)]
#[test]
fn failure_report_compaction_preserves_error_and_classification() {
    use crate::collab::failure_class::{classify, FailureClass};
    let prefix = "git_push_failed:";
    let verbose = (0..200).map(|i| format!("remote: Counting objects: {i}")).collect::<Vec<_>>().join("\n");
    let error = "error: failed to push some refs to 'origin'\nhint: Updates were rejected because the remote contains work\nhint: that you do not have locally.";
    let full_log = format!("{prefix}\n{verbose}\n{error}");
    assert!(full_log.chars().count() > 2048);

    let compacted = compact_failure_log(&full_log, 2048);
    assert!(compacted.chars().count() <= 2048);
    assert!(compacted.starts_with(prefix));
    assert!(compacted.contains("hint: that you do not have locally."));
    assert!(compacted.contains("[..."));
    assert_eq!(classify(&compacted), FailureClass::Tooling);
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p ironmem -- --nocapture 2>&1 | tail -40`
Expected: all tests PASS.

- [ ] **Step 5: Run full test suite to verify no regressions**

Run: `cargo test --workspace -- --nocapture 2>&1 | tail -40`
Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/tests/mcp_protocol.rs crates/ironmem/src/mcp/compact.rs
git commit -m "test(mcp): integration tests and fixtures for response compaction (#229)"
```
