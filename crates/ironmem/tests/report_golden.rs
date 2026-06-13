//! Golden `--json` integration test for `ironmem report` (issue #84, Task 3).
//!
//! Exercises ONLY the public API: `Database::open_in_memory` + the `pub` insert
//! methods + `ironmem::report::{run_report, ReportOptions, Report}`. Every
//! number in `EXPECTED_JSON` is hand-verified against the plan's expectation
//! table (Test Integrity: verify, then freeze — never blind-snapshot).

use ironmem::db::schema::Database;
use ironmem::db::{NewTokenUsage, TaskOutcome};
use ironmem::report::{run_report, Report, ReportOptions};

/// Build a measured/estimated `token_usage` row. `source`/`harness` use values
/// the migration-008 CHECK constraints accept (`llm_rerank`, `claude`/`codex`).
#[allow(clippy::too_many_arguments)]
fn tok(
    collab: &str,
    phase: &str,
    model: &str,
    harness: &str,
    inp: i64,
    out: i64,
    cc: i64,
    cr: i64,
    estimated: bool,
    cost: Option<f64>,
    ts: &str,
) -> NewTokenUsage {
    NewTokenUsage {
        ts: ts.into(),
        source: "llm_rerank".into(),
        harness: harness.into(),
        model: Some(model.into()),
        session_id: None,
        collab_session_id: Some(collab.into()),
        collab_phase: Some(phase.into()),
        task_tag: None,
        input_tokens: inp,
        output_tokens: out,
        cache_creation_input_tokens: cc,
        cache_read_input_tokens: cr,
        estimated,
        chars: 0,
        cost_usd: cost,
    }
}

#[allow(clippy::too_many_arguments)]
fn outcome(
    task_tag: &str,
    collab: &str,
    outcome: &str,
    started_at: &str,
    done_at: Option<&str>,
    review_rounds: i64,
    fix_commits: i64,
    pr_url: Option<&str>,
) -> TaskOutcome {
    TaskOutcome {
        task_tag: task_tag.into(),
        collab_session_id: Some(collab.into()),
        started_at: Some(started_at.into()),
        done_at: done_at.map(|s| s.into()),
        outcome: Some(outcome.into()),
        review_rounds,
        fix_commits,
        handoffs: 0,
        pr_url: pr_url.map(|s| s.into()),
    }
}

/// Seed the canonical Task-3 dataset (sess-rich / sess-min / sess-fail).
fn seed() -> Database {
    let db = Database::open_in_memory().unwrap();

    // --- sess-rich (merged) ---
    db.upsert_task_outcome(&outcome(
        "issue-rich",
        "sess-rich",
        "merged",
        "2026-06-01T00:00:00Z",
        Some("2026-06-02T00:00:00Z"),
        2,
        1,
        Some("https://github.com/ironrace/ironmem/pull/100"),
    ))
    .unwrap();
    // planning / opus-4-8 / claude / 1M input / cost NULL  -> §7 $5.00, provider None
    db.insert_token_usage(&tok(
        "sess-rich",
        "planning",
        "claude-opus-4-8",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T01:00:00Z",
    ))
    .unwrap();
    // impl / sonnet-4-6 / claude / 2M in + 500k out / cost Some(7.50)
    db.insert_token_usage(&tok(
        "sess-rich",
        "impl",
        "claude-sonnet-4-6",
        "claude",
        2_000_000,
        500_000,
        0,
        0,
        false,
        Some(7.50),
        "2026-06-01T02:00:00Z",
    ))
    .unwrap();
    // impl / sonnet-4-6 / claude / 1M cache_read / cost NULL (same group as above)
    db.insert_token_usage(&tok(
        "sess-rich",
        "impl",
        "claude-sonnet-4-6",
        "claude",
        0,
        0,
        0,
        1_000_000,
        false,
        None,
        "2026-06-01T02:30:00Z",
    ))
    .unwrap();
    // review / claude-future-9 / claude / 1M in / cost NULL -> §7 None (unpriced)
    db.insert_token_usage(&tok(
        "sess-rich",
        "review",
        "claude-future-9",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T03:00:00Z",
    ))
    .unwrap();
    // rework / opus-4-8 / codex / 1M in / cost NULL -> §7 None (codex)
    db.insert_token_usage(&tok(
        "sess-rich",
        "rework",
        "claude-opus-4-8",
        "codex",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T04:00:00Z",
    ))
    .unwrap();
    // ESTIMATED row: impl / opus-4-8 / claude / 400k in -> split only
    db.insert_token_usage(&tok(
        "sess-rich",
        "impl",
        "claude-opus-4-8",
        "claude",
        400_000,
        0,
        0,
        0,
        true,
        None,
        "2026-06-01T05:00:00Z",
    ))
    .unwrap();

    // --- sess-min (merged) ---
    db.upsert_task_outcome(&outcome(
        "issue-min",
        "sess-min",
        "merged",
        "2026-06-03T00:00:00Z",
        Some("2026-06-04T00:00:00Z"),
        0,
        0,
        Some("https://github.com/ironrace/ironmem/pull/101"),
    ))
    .unwrap();
    // impl / haiku-4-5 / claude / 1M in / cost Some(1.00) -> §7 $1.00
    db.insert_token_usage(&tok(
        "sess-min",
        "impl",
        "claude-haiku-4-5",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        Some(1.00),
        "2026-06-03T01:00:00Z",
    ))
    .unwrap();

    // --- sess-fail (failed) ---
    db.upsert_task_outcome(&outcome(
        "issue-fail",
        "sess-fail",
        "failed",
        "2026-06-05T00:00:00Z",
        None,
        3,
        0,
        None,
    ))
    .unwrap();
    // impl / opus-4-8 / claude / 2M in / cost NULL -> §7 $10.00
    db.insert_token_usage(&tok(
        "sess-fail",
        "impl",
        "claude-opus-4-8",
        "claude",
        2_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-05T01:00:00Z",
    ))
    .unwrap();

    db
}

#[test]
fn report_golden_json_matches_hand_computed() {
    let db = seed();
    let report: Report = run_report(&db, &ReportOptions::default()).unwrap();

    // ---- Hand-verified guard assertions (must pass before the JSON freeze) ----
    assert_eq!(report.generated_for.task, None);
    assert_eq!(report.generated_for.since, None);
    assert_eq!(report.baseline_task_count, 2); // sess-rich, sess-min
    assert!(!report.baseline_ready); // 2 < 10
    assert_eq!(
        report.unpriced_models,
        vec!["claude-future-9".to_string(), "codex".to_string()]
    );

    // headline ordered by task_key: sess-min, sess-rich
    assert_eq!(report.headline.len(), 2);
    assert_eq!(report.headline[0].task_key, "sess-min");
    assert_eq!(report.headline[0].cost_usd, Some(1.0));
    assert_eq!(report.headline[1].task_key, "sess-rich");
    assert_eq!(report.headline[1].tokens_to_done, 6_500_000); // 1M+3.5M+1M+1M
    assert_eq!(report.headline[1].cost_usd, Some(18.8)); // 5.00 + 13.80
    assert_eq!(report.headline[1].provider_reported_cost_usd, Some(7.5));

    assert_eq!(report.non_completions.len(), 1);
    assert_eq!(report.non_completions[0].task_key, "sess-fail");
    assert_eq!(report.non_completions[0].cost_usd, Some(10.0));

    let rich = report
        .tasks
        .iter()
        .find(|t| t.task_key == "sess-rich")
        .unwrap();
    assert_eq!(rich.split.measured_tokens, 6_500_000);
    assert_eq!(rich.split.estimated_tokens, 400_000);

    // ---- Full serialized golden (verified against the table above) ----
    let json = serde_json::to_string_pretty(&report).unwrap();
    assert_eq!(json, EXPECTED_JSON);
}

/// Boundary: `baseline_ready` flips at exactly 10 distinct merged task_keys
/// with ≥1 measured token row (METRICS_SPEC §11.5 gate).
#[test]
fn baseline_ready_flips_at_ten_merged_tasks() {
    let db = Database::open_in_memory().unwrap();
    for i in 0..9 {
        let collab = format!("b{i}");
        db.upsert_task_outcome(&outcome(
            &format!("issue-b{i}"),
            &collab,
            "merged",
            "2026-06-01T00:00:00Z",
            Some("2026-06-02T00:00:00Z"),
            0,
            0,
            None,
        ))
        .unwrap();
        db.insert_token_usage(&tok(
            &collab,
            "impl",
            "claude-opus-4-8",
            "claude",
            1_000_000,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T01:00:00Z",
        ))
        .unwrap();
    }
    let nine = run_report(&db, &ReportOptions::default()).unwrap();
    assert_eq!(nine.baseline_task_count, 9);
    assert!(!nine.baseline_ready);

    // Add the 10th merged task with a measured row.
    db.upsert_task_outcome(&outcome(
        "issue-b9",
        "b9",
        "merged",
        "2026-06-01T00:00:00Z",
        Some("2026-06-02T00:00:00Z"),
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "b9",
        "impl",
        "claude-opus-4-8",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T01:00:00Z",
    ))
    .unwrap();
    let ten = run_report(&db, &ReportOptions::default()).unwrap();
    assert_eq!(ten.baseline_task_count, 10);
    assert!(ten.baseline_ready);
}

#[test]
fn tasks_include_estimated_only_and_outcome_only_identities() {
    let db = Database::open_in_memory().unwrap();

    db.upsert_task_outcome(&outcome(
        "issue-est",
        "sess-est",
        "merged",
        "2026-06-01T00:00:00Z",
        Some("2026-06-02T00:00:00Z"),
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-est",
        "impl",
        "claude-opus-4-8",
        "claude",
        500_000,
        0,
        0,
        0,
        true,
        None,
        "2026-06-01T01:00:00Z",
    ))
    .unwrap();

    db.upsert_task_outcome(&outcome(
        "issue-empty",
        "sess-empty",
        "failed",
        "2026-06-03T00:00:00Z",
        None,
        1,
        0,
        None,
    ))
    .unwrap();

    let report = run_report(&db, &ReportOptions::default()).unwrap();
    let est = report
        .tasks
        .iter()
        .find(|t| t.task_key == "sess-est")
        .expect("estimated-only task should still render");
    assert_eq!(est.tokens_to_done, 0);
    assert_eq!(est.split.measured_tokens, 0);
    assert_eq!(est.split.estimated_tokens, 500_000);
    assert_eq!(
        est.outcome.as_ref().and_then(|o| o.outcome.as_deref()),
        Some("merged")
    );

    let empty = report
        .tasks
        .iter()
        .find(|t| t.task_key == "sess-empty")
        .expect("outcome-only task should still render");
    assert_eq!(empty.tokens_to_done, 0);
    assert_eq!(empty.split.measured_tokens, 0);
    assert_eq!(
        empty.outcome.as_ref().and_then(|o| o.outcome.as_deref()),
        Some("failed")
    );

    assert_eq!(
        report.baseline_task_count, 0,
        "baseline counts merged tasks with measured token rows only"
    );
}

#[test]
fn task_tag_alias_and_rfc3339_since_variants_filter_collab_rows() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_task_outcome(&outcome(
        "issue-84",
        "sess-84",
        "merged",
        "2026-06-01T00:00:00+00:00",
        Some("2026-06-02T00:00:00+00:00"),
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-84",
        "impl",
        "claude-opus-4-8",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T00:00:00+00:00",
    ))
    .unwrap();

    for since in ["2026-06-01T00:00:00Z", "2026-06-01T01:00:00+01:00"] {
        let report = run_report(
            &db,
            &ReportOptions {
                task: Some("issue-84".to_string()),
                since: Some(since.to_string()),
            },
        )
        .unwrap();
        assert_eq!(
            report.generated_for.since.as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        assert_eq!(
            report.tasks.len(),
            1,
            "task-tag alias must find collab tokens"
        );
        assert_eq!(report.tasks[0].task_key, "sess-84");
        assert_eq!(report.tasks[0].tokens_to_done, 1_000_000);
        assert_eq!(report.tasks[0].cost_usd, Some(5.0));
        assert_eq!(report.baseline_task_count, 1);
        assert_eq!(report.headline.len(), 1);
        assert_eq!(report.headline[0].task_key, "sess-84");
        assert_eq!(report.headline[0].tokens_to_done, 1_000_000);
    }
}

#[test]
fn invalid_since_is_rejected() {
    let db = Database::open_in_memory().unwrap();
    let err = run_report(
        &db,
        &ReportOptions {
            task: None,
            since: Some("not-a-date".to_string()),
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("since must be"),
        "unexpected error: {err}"
    );
}

#[test]
fn headline_cost_attaches_when_tokens_keyed_by_task_tag_not_collab_id() {
    // Regression for the §10.4 either-key cost match: an outcome carrying a
    // collab_session_id whose token rows are keyed by task_tag ONLY. The §10.4
    // JOIN matches via task_tag, so the §7 cost must still attach to the headline
    // (it previously fell to `n/a` because the lookup keyed only on collab id).
    let db = Database::open_in_memory().unwrap();
    db.upsert_task_outcome(&outcome(
        "issue-x",
        "sess-x",
        "merged",
        "2026-06-01T00:00:00Z",
        Some("2026-06-02T00:00:00Z"),
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&NewTokenUsage {
        ts: "2026-06-01T01:00:00Z".into(),
        source: "llm_rerank".into(),
        harness: "claude".into(),
        model: Some("claude-opus-4-8".into()),
        session_id: None,
        collab_session_id: None, // keyed by task_tag only
        collab_phase: Some("impl".into()),
        task_tag: Some("issue-x".into()),
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        estimated: false,
        chars: 0,
        cost_usd: None,
    })
    .unwrap();

    let report = run_report(&db, &ReportOptions::default()).unwrap();
    assert_eq!(report.headline.len(), 1);
    assert_eq!(report.headline[0].task_tag, "issue-x");
    assert_eq!(report.headline[0].tokens_to_done, 1_000_000);
    assert_eq!(
        report.headline[0].cost_usd,
        Some(5.0),
        "§7 cost must attach via the task_tag identity, not render n/a"
    );
}

#[test]
fn non_completion_since_filters_token_side() {
    // A failed task with two measured rows straddling a `--since` cutoff: the
    // §10.4 token total must drop the pre-cutoff row (filter is on u.ts).
    let db = Database::open_in_memory().unwrap();
    db.upsert_task_outcome(&outcome(
        "issue-f",
        "sess-f",
        "failed",
        "2026-06-01T00:00:00Z",
        None,
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-f",
        "impl",
        "claude-opus-4-8",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T00:00:00Z",
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-f",
        "impl",
        "claude-opus-4-8",
        "claude",
        3_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-10T00:00:00Z",
    ))
    .unwrap();

    let all = run_report(&db, &ReportOptions::default()).unwrap();
    assert_eq!(all.non_completions[0].tokens_to_done, 4_000_000);

    let windowed = run_report(
        &db,
        &ReportOptions {
            task: None,
            since: Some("2026-06-05".into()),
        },
    )
    .unwrap();
    assert_eq!(windowed.non_completions.len(), 1);
    assert_eq!(
        windowed.non_completions[0].tokens_to_done, 3_000_000,
        "--since drops the pre-cutoff token row from the non-completion total"
    );
}

#[test]
fn since_accepts_bare_date_and_normalizes_echo() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_task_outcome(&outcome(
        "issue-d",
        "sess-d",
        "merged",
        "2026-06-01T00:00:00Z",
        Some("2026-06-02T00:00:00Z"),
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-d",
        "impl",
        "claude-opus-4-8",
        "claude",
        1_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-03T00:00:00Z",
    ))
    .unwrap();

    let report = run_report(
        &db,
        &ReportOptions {
            task: None,
            since: Some("2026-06-01".into()),
        },
    )
    .unwrap();
    assert_eq!(
        report.generated_for.since.as_deref(),
        Some("2026-06-01T00:00:00Z"),
        "YYYY-MM-DD normalizes to a UTC RFC3339 instant in the echo"
    );
    assert_eq!(report.tasks.len(), 1);
    assert_eq!(report.tasks[0].tokens_to_done, 1_000_000);
}

#[test]
fn abandoned_outcome_lands_in_non_completions() {
    let db = Database::open_in_memory().unwrap();
    db.upsert_task_outcome(&outcome(
        "issue-a",
        "sess-a",
        "abandoned",
        "2026-06-01T00:00:00Z",
        None,
        0,
        0,
        None,
    ))
    .unwrap();
    db.insert_token_usage(&tok(
        "sess-a",
        "impl",
        "claude-opus-4-8",
        "claude",
        2_000_000,
        0,
        0,
        0,
        false,
        None,
        "2026-06-01T01:00:00Z",
    ))
    .unwrap();

    let report = run_report(&db, &ReportOptions::default()).unwrap();
    assert!(report.headline.is_empty(), "abandoned is not merged");
    assert_eq!(report.non_completions.len(), 1);
    assert_eq!(report.non_completions[0].task_key, "sess-a");
    assert_eq!(report.non_completions[0].cost_usd, Some(10.0));
}

/// Frozen pretty-JSON golden. Filled after the guard assertions pass and every
/// value is reconciled with the plan's expectation table (Task 3): sess-fail
/// §7 $10.00 / no provider cost; sess-min §7 $1.00 == provider; sess-rich
/// planning $5.00, impl $13.80 (provider $7.50), review/rework unpriced, total
/// §7 $18.80 / provider $7.50 / 6.5M measured + 400k estimated; baseline 2/10.
const EXPECTED_JSON: &str = r#"{
  "generated_for": {
    "task": null,
    "since": null
  },
  "baseline_task_count": 2,
  "baseline_ready": false,
  "headline": [
    {
      "task_key": "sess-min",
      "task_tag": "issue-min",
      "collab_session_id": "sess-min",
      "tokens_to_done": 1000000,
      "cost_usd": 1.0,
      "provider_reported_cost_usd": 1.0
    },
    {
      "task_key": "sess-rich",
      "task_tag": "issue-rich",
      "collab_session_id": "sess-rich",
      "tokens_to_done": 6500000,
      "cost_usd": 18.8,
      "provider_reported_cost_usd": 7.5
    }
  ],
  "non_completions": [
    {
      "task_key": "sess-fail",
      "task_tag": "issue-fail",
      "collab_session_id": "sess-fail",
      "tokens_to_done": 2000000,
      "cost_usd": 10.0,
      "provider_reported_cost_usd": null
    }
  ],
  "tasks": [
    {
      "task_key": "sess-fail",
      "by_phase": [
        {
          "phase": "impl",
          "tokens": 2000000,
          "cost_usd": 10.0,
          "provider_reported_cost_usd": null
        }
      ],
      "split": {
        "measured_tokens": 2000000,
        "estimated_tokens": 0
      },
      "outcome": {
        "outcome": "failed",
        "review_rounds": 3,
        "fix_commits": 0,
        "handoffs": 0,
        "started_at": "2026-06-05T00:00:00Z",
        "done_at": null,
        "pr_url": null
      },
      "tokens_to_done": 2000000,
      "cost_usd": 10.0,
      "provider_reported_cost_usd": null
    },
    {
      "task_key": "sess-min",
      "by_phase": [
        {
          "phase": "impl",
          "tokens": 1000000,
          "cost_usd": 1.0,
          "provider_reported_cost_usd": 1.0
        }
      ],
      "split": {
        "measured_tokens": 1000000,
        "estimated_tokens": 0
      },
      "outcome": {
        "outcome": "merged",
        "review_rounds": 0,
        "fix_commits": 0,
        "handoffs": 0,
        "started_at": "2026-06-03T00:00:00Z",
        "done_at": "2026-06-04T00:00:00Z",
        "pr_url": "https://github.com/ironrace/ironmem/pull/101"
      },
      "tokens_to_done": 1000000,
      "cost_usd": 1.0,
      "provider_reported_cost_usd": 1.0
    },
    {
      "task_key": "sess-rich",
      "by_phase": [
        {
          "phase": "planning",
          "tokens": 1000000,
          "cost_usd": 5.0,
          "provider_reported_cost_usd": null
        },
        {
          "phase": "impl",
          "tokens": 3500000,
          "cost_usd": 13.8,
          "provider_reported_cost_usd": 7.5
        },
        {
          "phase": "review",
          "tokens": 1000000,
          "cost_usd": null,
          "provider_reported_cost_usd": null
        },
        {
          "phase": "rework",
          "tokens": 1000000,
          "cost_usd": null,
          "provider_reported_cost_usd": null
        }
      ],
      "split": {
        "measured_tokens": 6500000,
        "estimated_tokens": 400000
      },
      "outcome": {
        "outcome": "merged",
        "review_rounds": 2,
        "fix_commits": 1,
        "handoffs": 0,
        "started_at": "2026-06-01T00:00:00Z",
        "done_at": "2026-06-02T00:00:00Z",
        "pr_url": "https://github.com/ironrace/ironmem/pull/100"
      },
      "tokens_to_done": 6500000,
      "cost_usd": 18.8,
      "provider_reported_cost_usd": 7.5
    }
  ],
  "unpriced_models": [
    "claude-future-9",
    "codex"
  ]
}"#;
