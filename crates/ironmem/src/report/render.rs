//! Plain-text rendering of an assembled [`Report`](crate::report::Report).
//!
//! [`render_text`] is a pure function of the already-assembled report — it
//! reads no database and no wallclock, so its output is deterministic and the
//! §7-derived `cost` figure is labelled distinctly from the stored
//! `provider`(-reported) figure. Sections, in order: a per-task **Headline**
//! list, **Non-completions**, a per-task **by-phase** decomposition, the
//! product-facing **Exploration value** summary (issue #145, sample-gated), the
//! diagnostic **Code-map exploration** line, **Baseline gate** line, and an
//! **Unpriced models** line.

use std::fmt::Write as _;

use crate::report::{HeadlineRow, Report, TaskReport, ValueSummary};

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

/// Render the product-facing **Exploration value** section (issue #145). On a
/// thin sample (`!sufficient_data`) it states so and prints no headline. With a
/// sufficient sample it leads with the hit rate, then a neutral map-hit-vs-
/// map-miss token-proxy comparison — but only when BOTH verdict buckets are
/// populated (the delta is a difference across disjoint turn populations, so a
/// one-sided sample yields no meaningful comparison and the delta is withheld
/// rather than presented as a savings figure). Then any recorded
/// repeated-context indicators.
fn render_value_summary(out: &mut String, vs: &ValueSummary) {
    if !vs.sufficient_data {
        let _ = writeln!(
            out,
            "\nExploration value: not enough exploration data yet ({turns}/{min} turns) — collect more before reading results.",
            turns = vs.total_turns,
            min = vs.min_turns,
        );
        return;
    }

    let headline = format!(
        "Exploration value: {hits}/{total} hit turns ({rate:.1}%)",
        hits = vs.map_hit_turns,
        total = vs.total_turns,
        rate = vs.hit_rate * 100.0,
    );
    if vs.map_hit_turns > 0 && vs.map_miss_turns > 0 {
        let _ = writeln!(
            out,
            "\n{headline} · map-hit ~{hit:.1} vs map-miss ~{miss:.1} tokens/turn (proxy; Δ {delta:.1})",
            hit = vs.mean_tokens_map_hit,
            miss = vs.mean_tokens_map_miss,
            delta = vs.exploration_token_delta,
        );
    } else {
        let _ = writeln!(
            out,
            "\n{headline} · token-proxy delta n/a (need both map-hit and map-miss turns)",
        );
    }

    // Repeated-context indicators, only when their underlying rows were recorded.
    if let Some(mcp) = &vs.mcp_response {
        let _ = writeln!(
            out,
            "  MCP responses (all calls): {count} · mean {mean:.1} tokens (response-size proxy)",
            count = mcp.row_count,
            mean = mcp.mean_output_tokens,
        );
    }
    if let Some(cov) = &vs.transcript_coverage {
        let _ = writeln!(
            out,
            "  Transcript coverage: {turns} turns · {tokens} tokens",
            turns = cov.turn_count,
            tokens = cov.total_tokens,
        );
    }
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

    render_value_summary(&mut out, &report.value_summary);

    let delta = report.exploration.mean_tokens_map_miss - report.exploration.mean_tokens_map_hit;
    let _ = writeln!(
        out,
        "\nCode-map exploration (diagnostic): {hits}/{total} hit turns ({rate:.1}%) · mean hit {hit:.1} tokens · mean miss {miss:.1} tokens · delta {delta:.1}",
        hits = report.exploration.map_hit_turns,
        total = report.exploration.total_turns,
        rate = report.exploration.hit_rate * 100.0,
        hit = report.exploration.mean_tokens_map_hit,
        miss = report.exploration.mean_tokens_map_miss,
    );

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

    /// issue #145: with too few exploration turns, the product-facing value
    /// section must say so rather than print a savings headline.
    #[test]
    fn render_text_value_summary_reports_insufficient_data() {
        use crate::db::metrics::MapStatus;
        let db = Database::open_in_memory().unwrap();
        db.record_exploration_tokens(
            "2026-06-21T00:00:00Z",
            "claude",
            0,
            25,
            Some(MapStatus::Hit),
            Some("turn-hit"),
            Some("core"),
        )
        .unwrap();
        let report = crate::report::run_report(&db, &Default::default()).unwrap();
        let text = crate::report::render_text(&report);
        assert!(
            text.contains("Exploration value"),
            "product section present: {text}"
        );
        assert!(
            text.contains("not enough") || text.contains("Not enough"),
            "insufficient-data notice present: {text}"
        );
        // The savings delta headline must NOT be presented on a thin sample.
        assert!(
            !text.contains("tokens saved"),
            "no savings headline on thin sample: {text}"
        );
    }

    /// issue #145: with a sufficient sample and BOTH verdict buckets populated,
    /// the value section presents the hit rate and a neutral map-hit-vs-map-miss
    /// token-proxy comparison — never the overclaiming word "saved". When no
    /// transcript rows exist, the transcript-coverage line must be omitted.
    #[test]
    fn render_text_value_summary_reports_comparison_when_sufficient() {
        use crate::db::metrics::MapStatus;
        let db = Database::open_in_memory().unwrap();
        for i in 0..8 {
            db.record_exploration_tokens(
                "2026-06-21T00:00:00Z",
                "claude",
                0,
                25,
                Some(MapStatus::Hit),
                Some(&format!("hit-{i}")),
                Some("core"),
            )
            .unwrap();
        }
        for i in 0..2 {
            db.record_exploration_tokens(
                "2026-06-21T00:00:00Z",
                "claude",
                0,
                75,
                Some(MapStatus::Miss),
                Some(&format!("miss-{i}")),
                Some("core"),
            )
            .unwrap();
        }
        let report = crate::report::run_report(&db, &Default::default()).unwrap();
        let text = crate::report::render_text(&report);
        assert!(
            text.contains("Exploration value"),
            "section present: {text}"
        );
        assert!(text.contains("80.0%"), "hit rate rendered: {text}");
        assert!(
            text.contains("map-hit") && text.contains("map-miss"),
            "neutral token-proxy comparison present: {text}"
        );
        // Must NOT overclaim with the word "saved" (issue #145 scope).
        assert!(!text.contains("saved"), "no 'saved' overclaim: {text}");
        assert!(
            !text.contains("not enough"),
            "no insufficient notice: {text}"
        );
        // No transcript rows seeded → indicator line omitted (absent-branch).
        assert!(
            !text.contains("Transcript coverage"),
            "transcript indicator omitted when no rows: {text}"
        );
    }

    /// issue #145: a sufficient sample that is one-sided (all hits, no misses)
    /// must NOT present a numeric token-proxy delta headline — the difference is
    /// across disjoint turn populations, so the renderer says it is n/a.
    #[test]
    fn render_text_value_summary_delta_na_when_one_sided() {
        use crate::db::metrics::MapStatus;
        let db = Database::open_in_memory().unwrap();
        for i in 0..8 {
            db.record_exploration_tokens(
                "2026-06-21T00:00:00Z",
                "claude",
                0,
                25,
                Some(MapStatus::Hit),
                Some(&format!("hit-{i}")),
                Some("core"),
            )
            .unwrap();
        }
        let report = crate::report::run_report(&db, &Default::default()).unwrap();
        let text = crate::report::render_text(&report);
        // Sufficient (8 ≥ 8) so still a headline, but delta is withheld.
        assert!(text.contains("100.0%"), "hit rate rendered: {text}");
        assert!(
            text.contains("delta n/a") || text.contains("delta unavailable"),
            "one-sided sample withholds the delta: {text}"
        );
        assert!(!text.contains("saved"), "no 'saved' overclaim: {text}");
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
