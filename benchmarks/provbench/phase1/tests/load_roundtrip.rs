use std::path::PathBuf;
use tempfile::TempDir;

/// Non-empty lines in the facts fixture. The `FunctionSignature` row appears
/// twice with identical fields, so the unique `fact_id` count is one lower.
const FIXTURE_FACT_LINES: usize = 5;
const FIXTURE_UNIQUE_FACTS: usize = 4;
/// `<sha>.json` files in the diffs fixture: the T₀ `excluded` artifact and
/// two commits carrying a `unified_diff`.
const FIXTURE_DIFFS: usize = 3;

/// Loads labeler-format facts + per-commit diff artifacts and the committed
/// canary's baseline-run predictions into a fresh SQLite DB. Asserts the
/// loaded counts match the artifact counts read from disk, that the fixture
/// counts are the ones the fixture was built with, and that raw_json_sha256
/// is deterministic across a re-ingest.
///
/// The facts and diffs come from `tests/fixtures/load_roundtrip/`, hand-built
/// in the shapes `provbench-labeler emit-facts` / `emit-diffs` write
/// (`StampedFactBody` and `DiffArtifact` in the labeler's `output.rs`). The
/// canary's own `facts/ripgrep-af6b6c54-c2d3b7b.*` inputs are gitignored and
/// absent from a fresh checkout; its `predictions.jsonl` is committed and is
/// read as-is.
#[test]
fn load_facts_diffs_evalrows() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixtures = manifest_dir.join("tests/fixtures/load_roundtrip");
    let facts = fixtures.join("sample.facts.jsonl");
    let diffs_dir = fixtures.join("sample.diffs");
    let baseline_run = manifest_dir.join("../results/phase0c/2026-05-13-canary");

    // Unique fact_id count, computed from disk (semantic duplicates with
    // matching fields are ingested once; non-matching duplicates fail closed).
    let (fact_lines, expected_facts) = {
        let text = std::fs::read_to_string(&facts).unwrap();
        let mut lines = 0usize;
        let mut ids = std::collections::HashSet::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            lines += 1;
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            ids.insert(v["fact_id"].as_str().unwrap().to_string());
        }
        (lines, ids.len())
    };
    let expected_diffs = std::fs::read_dir(&diffs_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "json")
        })
        .count();
    let expected_rows = std::fs::read_to_string(baseline_run.join("predictions.jsonl"))
        .unwrap()
        .lines()
        .count();

    // The disk-derived expectations above would agree with an empty or
    // truncated input, so pin the fixture's shape too.
    assert_eq!(fact_lines, FIXTURE_FACT_LINES, "facts fixture line count");
    assert_eq!(
        expected_facts, FIXTURE_UNIQUE_FACTS,
        "facts fixture unique fact_id count"
    );
    assert_eq!(expected_diffs, FIXTURE_DIFFS, "diffs fixture file count");
    assert!(expected_rows > 0, "canary predictions.jsonl is empty");

    let tmp = TempDir::new().unwrap();
    let db = provbench_phase1::storage::open(&tmp.path().join("phase1.sqlite")).unwrap();
    let inserted_facts = provbench_phase1::facts::ingest(&db, &facts).unwrap();
    let inserted_diffs = provbench_phase1::diffs::ingest(&db, &diffs_dir).unwrap();
    let inserted_rows =
        provbench_phase1::baseline_run::ingest(&db, &baseline_run.join("predictions.jsonl"))
            .unwrap();

    assert_eq!(
        inserted_facts, FIXTURE_UNIQUE_FACTS,
        "facts::ingest must insert the duplicated fact_id once"
    );
    assert_eq!(inserted_diffs, FIXTURE_DIFFS, "diffs::ingest return value");
    assert_eq!(
        inserted_rows, expected_rows,
        "baseline_run::ingest return value"
    );

    let got_facts: i64 = db
        .query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0))
        .unwrap();
    let got_diffs: i64 = db
        .query_row("SELECT COUNT(*) FROM diff_artifacts", [], |r| r.get(0))
        .unwrap();
    let got_rows: i64 = db
        .query_row("SELECT COUNT(*) FROM eval_rows", [], |r| r.get(0))
        .unwrap();

    assert_eq!(got_facts as usize, expected_facts, "facts count mismatch");
    assert_eq!(
        got_diffs as usize, expected_diffs,
        "diff_artifacts count mismatch"
    );
    assert_eq!(got_rows as usize, expected_rows, "eval_rows count mismatch");

    // Re-ingest into a second DB; raw_json_sha256 must be identical.
    let tmp2 = TempDir::new().unwrap();
    let db2 = provbench_phase1::storage::open(&tmp2.path().join("phase1.sqlite")).unwrap();
    provbench_phase1::facts::ingest(&db2, &facts).unwrap();
    let h1: String = db
        .query_row(
            "SELECT raw_json_sha256 FROM facts WHERE fact_id = (SELECT MIN(fact_id) FROM facts)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let h2: String = db2
        .query_row(
            "SELECT raw_json_sha256 FROM facts WHERE fact_id = (SELECT MIN(fact_id) FROM facts)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        h1, h2,
        "raw_json_sha256 must be deterministic across re-ingest"
    );
}
