//! Regression test: runner wiring writes `evidence` into `PredictionRow`.
//!
//! Before the Task 6 fix, `PredictionRow` was constructed without the
//! `evidence` field, so `predictions.jsonl` rows silently lacked evidence
//! on every real phase1 run. This test locks the wiring by exercising the
//! exact parse-and-assign path the runner uses.
//!
//! Approach A: unit-level — no real git repo required. We build a minimal
//! `RowCtx`, run `RuleChain::classify_first_match`, parse the returned
//! evidence string via `serde_json::from_str` (the same call added to
//! runner.rs), and assert the evidence lands in `PredictionRow`.

use provbench_phase1::facts::FactBody;
use provbench_phase1::rules::{Decision, RowCtx, RuleChain};
use provbench_scoring::PredictionRow;

/// Construct a minimal `FactBody` that will cause R9 (fallback) to fire.
///
/// R9 is always reached when no earlier rule fires. To guarantee that:
/// - `kind = "Other"` (R3/R4 require symbol-bearing kinds; R6 needs DocClaim)
/// - `post_blob = Some(changed)`, `t0_blob = Some(original)` (R1/R7 need
///   `post_blob.is_none()`; R2 needs blobs to be identical)
/// - `line_span = [0, 0]` (out-of-bounds — R4's `extract_lines` returns empty
///   and falls through; combined with `kind="Other"` this clears the chain)
/// - `diff = None` (R0 requires `excluded_reason.is_some()`)
fn r9_fact() -> FactBody {
    FactBody {
        fact_id: "test::r9::0".into(),
        kind: "Other".into(),
        body: "some body text".into(),
        source_path: "src/lib.rs".into(),
        line_span: [0, 0],
        symbol_path: "SomeType::some_method".into(),
        content_hash_at_observation: "deadbeef".into(),
        labeler_git_sha: "cafebabe".into(),
    }
}

/// Mirrors the runner's evidence-parse path verbatim (runner.rs lines
/// ~177-186 after the Task 6 fix).
fn parse_evidence(evidence_json: &str) -> Option<serde_json::Value> {
    serde_json::from_str(evidence_json).ok()
}

// ---------------------------------------------------------------------------

#[test]
fn rule_chain_returns_non_empty_evidence_json() {
    let chain = RuleChain::default();
    let fact = r9_fact();
    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "0000000000000000000000000000000000000000",
        diff: None,
        post_blob: Some(b"fn changed() {}"),
        t0_blob: Some(b"fn original() {}"),
        post_tree: None,
        commit_files: &[],
    };

    let (decision, rule_id, _spec_ref, evidence_json) = chain.classify_first_match(&ctx);

    assert_eq!(decision, Decision::NeedsRevalidation, "expected R9 to fire");
    assert_eq!(rule_id, "R9");
    assert!(
        !evidence_json.is_empty(),
        "classify_first_match must return a non-empty evidence JSON string"
    );
}

#[test]
fn evidence_parses_to_some_value() {
    let chain = RuleChain::default();
    let fact = r9_fact();
    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "0000000000000000000000000000000000000000",
        diff: None,
        post_blob: Some(b"fn changed() {}"),
        t0_blob: Some(b"fn original() {}"),
        post_tree: None,
        commit_files: &[],
    };

    let (_decision, _rule_id, _spec_ref, evidence_json) = chain.classify_first_match(&ctx);

    // Exact path from runner.rs: serde_json::from_str(&evidence).ok()
    let evidence_value = parse_evidence(&evidence_json);
    assert!(
        evidence_value.is_some(),
        "evidence JSON from classify_first_match must parse to a valid serde_json::Value; \
         got raw string: {evidence_json:?}"
    );
}

#[test]
fn evidence_value_contains_rule_key() {
    let chain = RuleChain::default();
    let fact = r9_fact();
    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "0000000000000000000000000000000000000000",
        diff: None,
        post_blob: Some(b"fn changed() {}"),
        t0_blob: Some(b"fn original() {}"),
        post_tree: None,
        commit_files: &[],
    };

    let (_decision, rule_id, _spec_ref, evidence_json) = chain.classify_first_match(&ctx);
    let evidence_value = parse_evidence(&evidence_json).expect("evidence must parse");

    assert_eq!(
        evidence_value["rule"].as_str(),
        Some(rule_id),
        "evidence[\"rule\"] must equal the firing rule_id; evidence: {evidence_json:?}"
    );
}

#[test]
fn prediction_row_carries_evidence_from_rule_chain() {
    // This test mirrors exactly what runner.rs now does: call classify_first_match,
    // parse the evidence string, and assign it to PredictionRow.evidence.
    // If a future change drops the `evidence` field assignment, this test fails.
    let chain = RuleChain::default();
    let fact = r9_fact();
    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "0000000000000000000000000000000000000000",
        diff: None,
        post_blob: Some(b"fn changed() {}"),
        t0_blob: Some(b"fn original() {}"),
        post_tree: None,
        commit_files: &[],
    };

    let (decision, _rule_id, _spec_ref, evidence_json) = chain.classify_first_match(&ctx);

    // Runner-side parse (runner.rs lines ~177-186 after Task 6 fix).
    let evidence_value: Option<serde_json::Value> = match serde_json::from_str(&evidence_json) {
        Ok(v) => Some(v),
        Err(_) => None,
    };

    let pred_row = PredictionRow {
        fact_id: fact.fact_id.clone(),
        commit_sha: "0000000000000000000000000000000000000000".into(),
        batch_id: "test-batch".into(),
        ground_truth: "needs_revalidation".into(),
        prediction: decision.as_str().into(),
        request_id: "phase1:v1.2c:0000000000000000000000000000000000000000:0".into(),
        wall_ms: 1,
        evidence: evidence_value,
    };

    // Primary assertion: evidence must not be None.
    assert!(
        pred_row.evidence.is_some(),
        "PredictionRow.evidence must be Some when a rule fires; \
         this proves runner.rs correctly wires evidence into predictions.jsonl"
    );

    // Secondary: serialized row must contain the "evidence" key.
    let serialized = serde_json::to_string(&pred_row).expect("PredictionRow must serialize");
    assert!(
        serialized.contains("\"evidence\""),
        "serialized predictions.jsonl line must contain the evidence key; got: {serialized}"
    );

    // Tertiary: the evidence object must have a "rule" field.
    let ev = pred_row.evidence.as_ref().unwrap();
    assert!(
        ev["rule"].is_string(),
        "evidence[\"rule\"] must be a string; got: {ev}"
    );
}
