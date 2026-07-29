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
}
