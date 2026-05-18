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
    };
    let s = serde_json::to_string(&row).unwrap();
    assert_eq!(
        s,
        r#"{"fact_id":"DocClaim::auto::CHANGELOG.md::229","commit_sha":"0000157917","batch_id":"0000157917-phase1","ground_truth":"Valid","prediction":"valid","request_id":"phase1:v1.0:0000157917:0","wall_ms":12}"#
    );
    let _back: provbench_scoring::PredictionRow = serde_json::from_str(&s).unwrap();
}
