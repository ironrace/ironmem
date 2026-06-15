//! Plain-text rendering of an assembled [`Report`](crate::report::Report).
//!
//! [`render_text`] is a pure function of the already-assembled report — it
//! reads no database and no wallclock, so its output is deterministic and the
//! §7-derived `cost` figure is labelled distinctly from the stored
//! `provider`(-reported) figure. Sections, in order: a per-task **Headline**
//! list, **Non-completions**, a per-task **by-phase** decomposition, the
//! **Baseline gate** line, and an **Unpriced models** line.

use std::fmt::Write as _;

use crate::report::{HeadlineRow, Report, TaskReport};

/// Format an optional §7/provider cost: `"$12.34"` or `"n/a"` when `None`
/// (the row was unpriceable — never rendered as `$0.00`).
fn fmt_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.2}"),
        None => "n/a".to_string(),
    }
}

/// Render one headline / non-completion row as a single indented line,
/// labelling the §7-derived `cost` and the stored `provider` figure distinctly.
fn render_headline_row(out: &mut String, row: &HeadlineRow) {
    let _ = writeln!(
        out,
        "  {key}: {tokens} tokens · cost {cost} (§7) · provider {provider}",
        key = row.task_key,
        tokens = row.tokens_to_done,
        cost = fmt_cost(row.cost_usd),
        provider = fmt_cost(row.provider_reported_cost_usd),
    );
}

/// Render one task's per-phase decomposition block.
fn render_task_phases(out: &mut String, task: &TaskReport) {
    let outcome = task
        .outcome
        .as_ref()
        .and_then(|o| o.outcome.as_deref())
        .unwrap_or("(no outcome)");
    let _ = writeln!(
        out,
        "  {key} [{outcome}]: {tokens} tokens · cost {cost} (§7) · provider {provider}",
        key = task.task_key,
        tokens = task.tokens_to_done,
        cost = fmt_cost(task.cost_usd),
        provider = fmt_cost(task.provider_reported_cost_usd),
    );
    for phase in &task.by_phase {
        let label = phase.phase.as_deref().unwrap_or("<none>");
        let _ = writeln!(
            out,
            "    {label}: {tokens} tokens · cost {cost} (§7) · provider {provider}",
            tokens = phase.tokens,
            cost = fmt_cost(phase.cost_usd),
            provider = fmt_cost(phase.provider_reported_cost_usd),
        );
    }
    let _ = writeln!(
        out,
        "    split: {measured} measured · {estimated} estimated tokens",
        measured = task.split.measured_tokens,
        estimated = task.split.estimated_tokens,
    );
}

/// Render the assembled [`Report`] as plain text. Pure: no DB access, no
/// wallclock — output depends only on `report`, so it is golden-stable.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();

    // Diagnosability: `--task` is unvalidated, so a typo'd key produces an empty
    // report indistinguishable from a genuinely empty task. Surface a note so the
    // two are not confused (the JSON `generated_for.task` carries the same signal
    // for tooling).
    if let Some(task) = report.generated_for.task.as_deref() {
        if report.headline.is_empty()
            && report.non_completions.is_empty()
            && report.tasks.is_empty()
        {
            let _ = writeln!(out, "(no metrics matched task '{task}')\n");
        }
    }

    let _ = writeln!(out, "Headline (merged tasks):");
    if report.headline.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for row in &report.headline {
            render_headline_row(&mut out, row);
        }
    }

    let _ = writeln!(out, "\nNon-completions (failed/abandoned):");
    if report.non_completions.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for row in &report.non_completions {
            render_headline_row(&mut out, row);
        }
    }

    let _ = writeln!(out, "\nPer-task by phase:");
    if report.tasks.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for task in &report.tasks {
            render_task_phases(&mut out, task);
        }
    }

    let gate = if report.baseline_ready {
        "READY"
    } else {
        "not ready"
    };
    let _ = writeln!(
        out,
        "\nBaseline gate: {count}/{threshold} merged tasks measured — {gate}",
        count = report.baseline_task_count,
        threshold = crate::report::BASELINE_READY_THRESHOLD,
    );

    let unpriced = if report.unpriced_models.is_empty() {
        "(none)".to_string()
    } else {
        report.unpriced_models.join(", ")
    };
    let _ = writeln!(out, "Unpriced models (counted, not §7-priced): {unpriced}");

    out
}

#[cfg(test)]
mod tests {
    use crate::db::schema::Database;
    use crate::db::{NewTokenUsage, TaskOutcome};

    #[allow(clippy::too_many_arguments)]
    fn tok(
        collab: &str,
        phase: &str,
        model: &str,
        harness: &str,
        inp: i64,
        out: i64,
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
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: cr,
            estimated,
            chars: 0,
            cost_usd: cost,
            map_status: None,
            turn_id: None,
            area: None,
        }
    }

    fn outcome(task_tag: &str, collab: &str, outcome: &str, started_at: &str) -> TaskOutcome {
        TaskOutcome {
            task_tag: task_tag.into(),
            collab_session_id: Some(collab.into()),
            started_at: Some(started_at.into()),
            done_at: None,
            outcome: Some(outcome.into()),
            review_rounds: 0,
            fix_commits: 0,
            handoffs: 0,
            pr_url: None,
        }
    }

    /// Minimal local seed exercising the asserted substrings: a merged
    /// `sess-rich` whose §7 cost rounds to $18.80 (planning $5 + impl $13.80,
    /// review/rework unpriced), an unpriced `claude-future-9` model, and a
    /// provider-reported cost ($7.50). Mirrors the golden integration seed but
    /// is defined locally (the integration seed lives in a separate test crate).
    fn seeded_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.upsert_task_outcome(&outcome(
            "issue-rich",
            "sess-rich",
            "merged",
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        // planning / opus-4-8 / claude / 1M in / NULL -> §7 $5.00, provider None
        db.insert_token_usage(&tok(
            "sess-rich",
            "planning",
            "claude-opus-4-8",
            "claude",
            1_000_000,
            0,
            0,
            false,
            None,
            "2026-06-01T01:00:00Z",
        ))
        .unwrap();
        // impl / sonnet-4-6 / claude / 2M in + 500k out / Some(7.50)
        db.insert_token_usage(&tok(
            "sess-rich",
            "impl",
            "claude-sonnet-4-6",
            "claude",
            2_000_000,
            500_000,
            0,
            false,
            Some(7.50),
            "2026-06-01T02:00:00Z",
        ))
        .unwrap();
        // impl / sonnet-4-6 / claude / 1M cache_read / NULL (same §7 group)
        db.insert_token_usage(&tok(
            "sess-rich",
            "impl",
            "claude-sonnet-4-6",
            "claude",
            0,
            0,
            1_000_000,
            false,
            None,
            "2026-06-01T02:30:00Z",
        ))
        .unwrap();
        // review / claude-future-9 / claude / 1M in / NULL -> unpriced model
        db.insert_token_usage(&tok(
            "sess-rich",
            "review",
            "claude-future-9",
            "claude",
            1_000_000,
            0,
            0,
            false,
            None,
            "2026-06-01T03:00:00Z",
        ))
        .unwrap();
        // rework / opus-4-8 / codex / 1M in / NULL -> codex unpriced
        db.insert_token_usage(&tok(
            "sess-rich",
            "rework",
            "claude-opus-4-8",
            "codex",
            1_000_000,
            0,
            0,
            false,
            None,
            "2026-06-01T04:00:00Z",
        ))
        .unwrap();
        db
    }

    #[test]
    fn render_text_includes_headline_and_baseline_and_unpriced() {
        let db = seeded_db();
        let report = crate::report::run_report(&db, &Default::default()).unwrap();
        let text = crate::report::render_text(&report);
        assert!(text.contains("sess-rich"), "headline task key surfaced");
        assert!(text.contains("18.80"), "§7 cost rendered");
        assert!(text.contains("baseline") || text.contains("Baseline"));
        assert!(text.contains("claude-future-9"), "unpriced model surfaced");
        assert!(text.contains("provider"), "provider-reported cost labeled");
        // review/rework phases are unpriced → cost renders `n/a`, never `$0.00`.
        assert!(text.contains("n/a"), "unpriced cost renders n/a");
        assert!(!text.contains("$0.00"), "unpriced cost is never $0.00");
    }

    #[test]
    fn render_text_notes_unmatched_task_filter() {
        let db = Database::open_in_memory().unwrap();
        let report = crate::report::run_report(
            &db,
            &crate::report::ReportOptions {
                task: Some("does-not-exist".into()),
                since: None,
            },
        )
        .unwrap();
        let text = crate::report::render_text(&report);
        assert!(
            text.contains("no metrics matched task 'does-not-exist'"),
            "unmatched --task must be flagged: {text}"
        );
    }
}
