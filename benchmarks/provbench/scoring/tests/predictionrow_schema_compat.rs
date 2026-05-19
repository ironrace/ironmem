/// Asserts that a JSON row written by either baseline or phase1 deserializes
/// identically through `provbench_scoring::PredictionRow`. Locks the
/// PredictionRow contract so phase1's predictions.jsonl is byte-compatible
/// with what baseline already emits.
#[test]
fn predictionrow_roundtrip_is_byte_stable() {
    let row = provbench_scoring::PredictionRow {
        fact_id: "DocClaim::auto::CHANGELOG.md::229".into(),
        commit_sha: "0000157917".into(),
        batch_id: "0000157917-phase1".into(),
        ground_truth: "Valid".into(),
        prediction: "valid".into(),
        request_id: "phase1:v1.0:0000157917:0".into(),
        wall_ms: 12,
        evidence: None,
        row_index: None,
        wall_us: None,
    };
    let s = serde_json::to_string(&row).unwrap();
    // wall_us: None must not appear in the serialized output (skip_serializing_if).
    // This keeps legacy v1.2c artifacts byte-stable through round-trip.
    assert_eq!(
        s,
        r#"{"fact_id":"DocClaim::auto::CHANGELOG.md::229","commit_sha":"0000157917","batch_id":"0000157917-phase1","ground_truth":"Valid","prediction":"valid","request_id":"phase1:v1.0:0000157917:0","wall_ms":12}"#
    );
    let _back: provbench_scoring::PredictionRow = serde_json::from_str(&s).unwrap();
}

/// Asserts that a row with `wall_us: Some(123)` round-trips faithfully:
/// the field appears in the serialized JSON and deserializes back to `Some(123)`.
#[test]
fn predictionrow_wall_us_some_roundtrips() {
    let row = provbench_scoring::PredictionRow {
        fact_id: "FunctionSignature::auto::src/lib.rs::5".into(),
        commit_sha: "abc123".into(),
        batch_id: "abc123-phase1".into(),
        ground_truth: "Valid".into(),
        prediction: "valid".into(),
        request_id: "phase1:v1.2c:abc123:0".into(),
        wall_ms: 0,
        evidence: None,
        row_index: Some(0),
        wall_us: Some(123),
    };
    let s = serde_json::to_string(&row).unwrap();
    // wall_us must appear in the serialized output when Some.
    assert!(
        s.contains(r#""wall_us":123"#),
        "serialized row must contain wall_us:123; got: {s}"
    );
    let back: provbench_scoring::PredictionRow = serde_json::from_str(&s).unwrap();
    assert_eq!(
        back.wall_us,
        Some(123),
        "wall_us must round-trip to Some(123)"
    );
}

/// Asserts that a legacy JSON row lacking a `wall_us` key deserializes with
/// `wall_us == None` (the `#[serde(default)]` guard).
#[test]
fn predictionrow_wall_us_absent_deserializes_as_none() {
    let legacy_json = r#"{"fact_id":"DocClaim::auto::README.md::1","commit_sha":"deadbeef","batch_id":"deadbeef-0","ground_truth":"Valid","prediction":"valid","request_id":"req_abc","wall_ms":42}"#;
    let row: provbench_scoring::PredictionRow = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(
        row.wall_us, None,
        "legacy JSON without wall_us key must deserialize as None"
    );
}
