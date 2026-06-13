//! `ironmem report` rendering — METRICS_SPEC §10 queries + §7 cost.
//!
//! [`run_report`] assembles the storage-only §10 aggregates (from
//! `crate::db::metrics`) into the [`Report`] output tree, deriving each
//! per-task / per-phase / headline **cost** from the §7 table in
//! [`mod@cost`] rather than the sparse stored `cost_usd` column. The stored
//! provider figure is surfaced separately as `provider_reported_cost_usd`
//! (NULL-preserving). The assembly path injects no wallclock and rounds every
//! emitted cost to 6 dp so the `--json` output is byte-stable for the golden
//! test.
mod cost;

use serde::Serialize;

use crate::db::metrics::{HeadlineTokens, TaskEstimatedSplit, TaskOutcome, TaskPhaseModelTokens};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Canonical phase-bucket order for deterministic `by_phase` rendering, mirroring
/// the bucket names produced by `crate::metrics::phase_bucket` (METRICS_SPEC §3.2).
/// A `None` phase sorts last (after `other`).
const PHASE_ORDER: &[&str] = &["planning", "impl", "review", "rework", "other"];

/// Sort key for a `collab_phase` bucket: its index in [`PHASE_ORDER`], or one
/// past the end for any unrecognized bucket, with `None` sorting last of all.
fn phase_rank(phase: Option<&str>) -> usize {
    match phase {
        Some(p) => PHASE_ORDER
            .iter()
            .position(|&b| b == p)
            .unwrap_or(PHASE_ORDER.len()),
        None => PHASE_ORDER.len() + 1,
    }
}

/// `round6(x) = (x * 1e6).round() / 1e6` — quantize a cost to 6 dp so the
/// serialized JSON is deterministic across platforms / accumulation order.
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// Sum an iterator of optional costs: `None` only when **every** input is
/// `None`; otherwise `Some(Σ of the present values)`, rounded to 6 dp. This is
/// the in-Rust analogue of SQLite `SUM` over a nullable column (all-NULL → NULL).
fn sum_opt<I: IntoIterator<Item = Option<f64>>>(it: I) -> Option<f64> {
    let mut any = false;
    let mut total = 0.0;
    for x in it.into_iter().flatten() {
        any = true;
        total += x;
    }
    if any {
        Some(round6(total))
    } else {
        None
    }
}

/// Options for [`run_report`]. Both filters narrow input only (no aggregation
/// semantics change); see the per-method docs in `crate::db::metrics`.
#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    /// Restrict to one task (`COALESCE(collab_session_id, task_tag)` for token
    /// rows; `task_tag` OR `collab_session_id` for outcomes).
    pub task: Option<String>,
    /// Restrict to rows at/after this RFC3339 instant (inclusive). Applies to
    /// `ts` on token rows and `started_at` on outcomes (METRICS_SPEC §12).
    pub since: Option<String>,
}

/// One (phase) decomposition of a task's measured tokens-to-done.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PhaseReport {
    /// Phase bucket (`planning`/`impl`/`review`/`rework`/`other`) or `null`.
    pub phase: Option<String>,
    /// Σ of all four token kinds for the phase (measured only).
    pub tokens: i64,
    /// §7-derived cost; `None` when no (model,harness) group in the phase priced.
    pub cost_usd: Option<f64>,
    /// `SUM(cost_usd)` over the phase's groups; `None` when all contributing
    /// rows had a NULL stored cost.
    pub provider_reported_cost_usd: Option<f64>,
}

/// Measured-vs-estimated token split for one task (METRICS_SPEC §10.2).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SplitReport {
    pub measured_tokens: i64,
    pub estimated_tokens: i64,
}

/// One task's full report: outcome metadata (§10.3) + measured phase
/// decomposition (§10.1) + estimated/measured split (§10.2).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskReport {
    /// `COALESCE(collab_session_id, task_tag)` — the §10.1 task identity.
    pub task_key: String,
    /// Outcome `task_tag` if an outcome matched this task, else `None`.
    pub task_tag: Option<String>,
    /// Outcome `collab_session_id` if matched, else `None`.
    pub collab_session_id: Option<String>,
    /// Terminal outcome (`merged`/`failed`/`abandoned`) or `None`.
    pub outcome: Option<String>,
    pub started_at: Option<String>,
    pub done_at: Option<String>,
    pub review_rounds: i64,
    pub fix_commits: i64,
    pub handoffs: i64,
    pub pr_url: Option<String>,
    /// Σ measured tokens across all phases.
    pub tokens_to_done: i64,
    /// §7 cost summed over phases (`None` only if no phase priced).
    pub cost_usd: Option<f64>,
    /// Provider-reported cost summed over phases (NULL-preserving).
    pub provider_reported_cost_usd: Option<f64>,
    /// Phase decomposition, ordered by [`PHASE_ORDER`] then `null`.
    pub by_phase: Vec<PhaseReport>,
    pub split: SplitReport,
}

/// One headline / non-completion row (METRICS_SPEC §10.4). `tokens_to_done`
/// and `provider_reported_cost_usd` are the verbatim §10.4 JOIN aggregates;
/// `cost_usd` is the §7-derived figure from the matching task.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HeadlineRow {
    /// `collab_session_id` when present else `task_tag` — matches the §10.1 key.
    pub task_key: String,
    pub task_tag: String,
    pub collab_session_id: Option<String>,
    pub tokens_to_done: i64,
    /// §7-derived cost for this task (`None` when no contributing row priced).
    pub cost_usd: Option<f64>,
    /// Verbatim §10.4 `SUM(cost_usd)` (NULL-preserving).
    pub provider_reported_cost_usd: Option<f64>,
}

/// The full `ironmem report` payload (serialized verbatim by `--json`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    /// Per-task decompositions, ordered by `task_key`.
    pub tasks: Vec<TaskReport>,
    /// Merged-only headline tokens-to-done, ordered by `task_key`.
    pub headline: Vec<HeadlineRow>,
    /// Non-completion (`failed`/`abandoned`) variant, ordered by `task_key`.
    pub non_completions: Vec<HeadlineRow>,
    /// Sorted-unique labels for measured groups with no §7 price (unknown model
    /// id, `"<none>"` for NULL model, `"codex"` for any codex group).
    pub unpriced_models: Vec<String>,
    /// Distinct merged task_keys with ≥1 measured token row.
    pub baseline_task_count: usize,
    /// `baseline_task_count >= 10` (Phase-6 recording gate, METRICS_SPEC §11.5).
    pub baseline_ready: bool,
}

/// Label a §7-unpriceable measured group for `unpriced_models`: `"codex"` for
/// any codex-harness group, `"<none>"` for a NULL model, else the model id.
fn unpriced_label(g: &TaskPhaseModelTokens) -> String {
    if g.harness == "codex" {
        "codex".to_string()
    } else {
        g.model.clone().unwrap_or_else(|| "<none>".to_string())
    }
}

/// §7 per-group cost for a measured aggregate (delegates to [`cost::row_cost_usd`]).
fn group_cost(g: &TaskPhaseModelTokens) -> Option<f64> {
    cost::row_cost_usd(
        &g.harness,
        g.model.as_deref(),
        g.input_tokens,
        g.output_tokens,
        g.cache_creation_input_tokens,
        g.cache_read_input_tokens,
    )
}

/// Match a §10.1 `task_key` to its outcome (METRICS_SPEC §10 uniqueness
/// invariant guarantees ≤1 match): the outcome whose `collab_session_id`
/// equals the key, or — when the outcome has no collab id — whose `task_tag`
/// equals the key.
fn outcome_for_key<'a>(task_key: &str, outcomes: &'a [TaskOutcome]) -> Option<&'a TaskOutcome> {
    outcomes.iter().find(|o| {
        o.collab_session_id.as_deref() == Some(task_key)
            || (o.collab_session_id.is_none() && o.task_tag == task_key)
    })
}

/// Assemble the METRICS_SPEC §10 report with §7-derived cost. No wallclock is
/// read on this path; every emitted cost is rounded to 6 dp at construction.
///
/// Pipeline: pull the five §10 aggregates (filtered by `opts`), group the
/// §10.1 rows by `task_key`, decompose each task by phase (applying §7 rates
/// per (model,harness) group), join the §10.3 outcome, attach the §10.2 split,
/// then render the §10.4 headline / non-completion lists with §7 cost.
pub fn run_report(db: &Database, opts: &ReportOptions) -> Result<Report, MemoryError> {
    let task = opts.task.as_deref();
    let since = opts.since.as_deref();

    let groups = db.report_tokens_by_task_phase(task, since)?;
    let splits = db.report_measured_estimated_split(task, since)?;
    let outcomes = db.report_task_outcomes(task, since)?;
    let headline_rows = db.report_headline(task, since)?;
    let non_completion_rows = db.report_non_completions(task, since)?;

    // Distinct task_keys present in the §10.1 measured roll-up, in deterministic
    // order. `report_tokens_by_task_phase` already orders by task_key, so a
    // dedup-preserving pass keeps them sorted.
    let mut task_keys: Vec<String> = Vec::new();
    for g in &groups {
        if task_keys.last().map(String::as_str) != Some(g.task_key.as_str()) {
            task_keys.push(g.task_key.clone());
        }
    }

    let tasks: Vec<TaskReport> = task_keys
        .iter()
        .map(|key| build_task(key, &groups, &splits, &outcomes))
        .collect();

    // Merged task_keys with ≥1 measured token row → baseline count.
    let baseline_task_count = task_keys
        .iter()
        .filter(|key| {
            outcome_for_key(key, &outcomes).and_then(|o| o.outcome.as_deref()) == Some("merged")
        })
        .count();

    let unpriced_models = collect_unpriced(&groups);

    let headline = build_headline_rows(&headline_rows, &tasks);
    let non_completions = build_headline_rows(&non_completion_rows, &tasks);

    Ok(Report {
        tasks,
        headline,
        non_completions,
        unpriced_models,
        baseline_task_count,
        baseline_ready: baseline_task_count >= 10,
    })
}

/// Build one [`TaskReport`] from the slices of §10 aggregates that belong to
/// `task_key`.
fn build_task(
    task_key: &str,
    groups: &[TaskPhaseModelTokens],
    splits: &[TaskEstimatedSplit],
    outcomes: &[TaskOutcome],
) -> TaskReport {
    let mine: Vec<&TaskPhaseModelTokens> =
        groups.iter().filter(|g| g.task_key == task_key).collect();

    // Distinct phases for this task, ordered canonically.
    let mut phases: Vec<Option<String>> = Vec::new();
    for g in &mine {
        if !phases.contains(&g.collab_phase) {
            phases.push(g.collab_phase.clone());
        }
    }
    phases.sort_by(|a, b| {
        phase_rank(a.as_deref())
            .cmp(&phase_rank(b.as_deref()))
            .then_with(|| a.cmp(b))
    });

    let by_phase: Vec<PhaseReport> = phases
        .iter()
        .map(|phase| {
            let in_phase: Vec<&&TaskPhaseModelTokens> =
                mine.iter().filter(|g| &g.collab_phase == phase).collect();
            let tokens: i64 = in_phase
                .iter()
                .map(|g| {
                    g.input_tokens
                        + g.output_tokens
                        + g.cache_creation_input_tokens
                        + g.cache_read_input_tokens
                })
                .sum();
            PhaseReport {
                phase: phase.clone(),
                tokens,
                cost_usd: sum_opt(in_phase.iter().map(|g| group_cost(g))),
                provider_reported_cost_usd: sum_opt(in_phase.iter().map(|g| g.provider_cost_usd)),
            }
        })
        .collect();

    let tokens_to_done: i64 = by_phase.iter().map(|p| p.tokens).sum();
    let cost_usd = sum_opt(by_phase.iter().map(|p| p.cost_usd));
    let provider_reported_cost_usd = sum_opt(by_phase.iter().map(|p| p.provider_reported_cost_usd));

    let measured_tokens: i64 = splits
        .iter()
        .filter(|s| s.task_key == task_key && !s.estimated)
        .map(|s| s.tokens)
        .sum();
    let estimated_tokens: i64 = splits
        .iter()
        .filter(|s| s.task_key == task_key && s.estimated)
        .map(|s| s.tokens)
        .sum();

    let outcome = outcome_for_key(task_key, outcomes);
    TaskReport {
        task_key: task_key.to_string(),
        task_tag: outcome.map(|o| o.task_tag.clone()),
        collab_session_id: outcome.and_then(|o| o.collab_session_id.clone()),
        outcome: outcome.and_then(|o| o.outcome.clone()),
        started_at: outcome.and_then(|o| o.started_at.clone()),
        done_at: outcome.and_then(|o| o.done_at.clone()),
        review_rounds: outcome.map(|o| o.review_rounds).unwrap_or(0),
        fix_commits: outcome.map(|o| o.fix_commits).unwrap_or(0),
        handoffs: outcome.map(|o| o.handoffs).unwrap_or(0),
        pr_url: outcome.and_then(|o| o.pr_url.clone()),
        tokens_to_done,
        cost_usd,
        provider_reported_cost_usd,
        by_phase,
        split: SplitReport {
            measured_tokens,
            estimated_tokens,
        },
    }
}

/// Sorted-unique unpriced labels across all measured groups with no §7 price.
fn collect_unpriced(groups: &[TaskPhaseModelTokens]) -> Vec<String> {
    let mut labels: Vec<String> = groups
        .iter()
        .filter(|g| group_cost(g).is_none())
        .map(unpriced_label)
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// Map verbatim §10.4 rows to [`HeadlineRow`]s, keying `task_key` as
/// `collab_session_id` else `task_tag` and pulling the §7 `cost_usd` from the
/// matching assembled [`TaskReport`]. Re-sorted by `task_key` so the assembly
/// determinism rule holds even when the §10.4 SQL `ORDER BY task_tag` diverges
/// from `task_key` order (i.e. when a row keys off `collab_session_id`).
fn build_headline_rows(rows: &[HeadlineTokens], tasks: &[TaskReport]) -> Vec<HeadlineRow> {
    let mut out: Vec<HeadlineRow> = rows
        .iter()
        .map(|r| {
            let task_key = r
                .collab_session_id
                .clone()
                .unwrap_or_else(|| r.task_tag.clone());
            let cost_usd = tasks
                .iter()
                .find(|t| t.task_key == task_key)
                .and_then(|t| t.cost_usd);
            HeadlineRow {
                task_key,
                task_tag: r.task_tag.clone(),
                collab_session_id: r.collab_session_id.clone(),
                tokens_to_done: r.tokens_to_done,
                cost_usd,
                provider_reported_cost_usd: r.provider_cost_usd,
            }
        })
        .collect();
    out.sort_by(|a, b| a.task_key.cmp(&b.task_key));
    out
}
