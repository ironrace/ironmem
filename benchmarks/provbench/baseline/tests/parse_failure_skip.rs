//! When the model's response text fails to parse on both attempts, the
//! runner must not abort the run. It must write a diagnostic sidecar
//! (`parse_failures.jsonl`) and continue to the next batch. The failed
//! batch contributes zero rows to `predictions.jsonl`, and `run_meta`
//! records the count under `batches_parse_failed`.

use provbench_baseline::client::AnthropicClient;
use provbench_baseline::manifest::SampleManifest;
use provbench_baseline::runner::{run, RunnerOpts};
use provbench_baseline::sample::PerStratumTargets;
use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from("fixtures").join(name)
}

#[tokio::test]
async fn runner_skips_batch_and_logs_sidecar_on_parse_failure() {
    let mock = MockServer::start().await;
    // Every request returns non-JSON content text. The runner's two-attempt
    // parse-retry path will exhaust and surface ParseFailureError.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("request-id", "req_bad")
                .set_body_string(
                    r#"{"id":"msg_bad","type":"message","role":"assistant","content":[{"type":"text","text":"I cannot do this."}],"usage":{"input_tokens":3,"output_tokens":2}}"#,
                ),
        )
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let run_dir = tmp.path().to_path_buf();

    // Mint a manifest from the in-tree fixtures.
    let manifest = SampleManifest::from_corpus(
        &fixture("sample_corpus.jsonl"),
        &fixture("sample_facts.jsonl"),
        &fixture("sample_diffs"),
        0xC0DEBABEDEADBEEF,
        PerStratumTargets::default(),
        25.0,
    )
    .unwrap();
    manifest
        .save_atomic(&run_dir.join("manifest.json"))
        .unwrap();
    let batches_total_expected_at_least_one = !manifest.rows.is_empty();
    assert!(
        batches_total_expected_at_least_one,
        "fixture manifest must select at least one row"
    );

    // Inject a client wired to the mock server.
    let client = AnthropicClient::with_base_url(mock.uri(), "fake-key".into());

    let opts = RunnerOpts {
        run_dir: run_dir.clone(),
        manifest,
        budget_usd: 10.0,
        resume: false,
        dry_run: false,
        fixture_mode: None,
        max_batches: None,
        max_concurrency: 1,
        max_input_tokens_per_minute: 0,
        client_override: Some(client),
    };

    let result = run(opts)
        .await
        .expect("runner must not abort on parse failure");

    // Sidecar must exist with at least one entry.
    let sidecar_path = run_dir.join("parse_failures.jsonl");
    assert!(
        sidecar_path.exists(),
        "parse_failures.jsonl must be written when a batch fails to parse"
    );
    let sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
    let entries: Vec<serde_json::Value> = sidecar
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !entries.is_empty(),
        "parse_failures.jsonl must have at least one entry"
    );
    let first = &entries[0];
    assert!(first["batch_id"].is_string(), "entry has batch_id");
    assert_eq!(first["request_id"], "req_bad", "entry preserves request_id");
    assert_eq!(
        first["raw_text"], "I cannot do this.",
        "entry preserves raw model text"
    );
    assert!(
        first["err_msg"].is_string() && !first["err_msg"].as_str().unwrap().is_empty(),
        "entry has non-empty err_msg"
    );

    // Run result tallies the failure.
    assert!(
        result.batches_parse_failed >= 1,
        "result.batches_parse_failed must be >= 1, got {}",
        result.batches_parse_failed
    );
    assert!(!result.aborted, "run must complete cleanly (not aborted)");

    // No prediction rows landed for the failed batch(es): since every
    // mocked response fails to parse, predictions.jsonl is either absent
    // or empty.
    let preds_path = run_dir.join("predictions.jsonl");
    if preds_path.exists() {
        let preds = std::fs::read_to_string(&preds_path).unwrap();
        let row_count = preds.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            row_count, 0,
            "no predictions should land when every batch fails to parse"
        );
    }
}
