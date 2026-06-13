//! `ironmem report` rendering — METRICS_SPEC §10 queries + §7 cost.
//!
//! [`run_report`] assembles the storage-only §10 aggregates (from
//! `crate::db::metrics`) into the [`Report`] output tree, deriving each
//! per-task / per-phase / headline **cost** from the §7 table in
//! [`mod@cost`] rather than the sparse stored `cost_usd` column. The stored
//! provider figure is surfaced separately as `provider_reported_cost_usd`
//! (NULL-preserving). The assembly path injects no wallclock and rounds every
//! **§7-derived** cost to 6 dp so the `--json` output is byte-stable; the
//! headline `provider_reported_cost_usd` figures are emitted as the verbatim
//! §10.4 SQL `SUM(cost_usd)` (not re-rounded — clean in practice; the golden
//! pins them).
mod cost;
mod render;

pub use render::render_text;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::Serialize;

use crate::db::metrics::{HeadlineTokens, TaskEstimatedSplit, TaskOutcome, TaskPhaseModelTokens};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Canonical phase-bucket order for deterministic `by_phase` rendering, mirroring
/// the bucket names produced by `crate::metrics::phase_bucket` (METRICS_SPEC §3.2).
/// A `None` phase sorts last (after `other`).
const PHASE_ORDER: &[&str] = &["planning", "impl", "review", "rework", "other"];

/// METRICS_SPEC §11.5 Phase-6 recording gate: `baseline_ready` becomes true at
/// this many distinct merged task_keys with ≥1 measured token row. Single source
/// of truth for the threshold — referenced by [`run_report`], [`one_line_summary`],
/// and the text renderer so the three can never drift apart.
pub(crate) const BASELINE_READY_THRESHOLD: usize = 10;

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

fn task_outcome_key(outcome: &TaskOutcome) -> String {
    outcome
        .collab_session_id
        .clone()
        .unwrap_or_else(|| outcome.task_tag.clone())
}

fn validate_since(since: Option<&str>) -> Result<Option<String>, MemoryError> {
    let Some(since) = since else {
        return Ok(None);
    };

    if let Ok(dt) = DateTime::parse_from_rfc3339(since) {
        return Ok(Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        ));
    }

    if let Ok(date) = NaiveDate::parse_from_str(since, "%Y-%m-%d") {
        let midnight = date
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always a valid time");
        return Ok(Some(
            DateTime::<Utc>::from_naive_utc_and_offset(midnight, Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        ));
    }

    Err(MemoryError::Validation(format!(
        "since must be RFC3339 or YYYY-MM-DD, got: {since}"
    )))
}

/// Options for [`run_report`]. Both filters narrow input only (no aggregation
/// semantics change); see the per-method docs in `crate::db::metrics`.
#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    /// Restrict to one task (`COALESCE(collab_session_id, task_tag)` for token
    /// rows; `task_tag` OR `collab_session_id` for outcomes). Unlike `since`,
    /// this is NOT validated: an unknown/typo'd key matches no rows and yields an
    /// empty report (the text renderer adds a "no metrics matched" note so it is
    /// not mistaken for a genuinely empty task).
    pub task: Option<String>,
    /// Restrict to rows at/after this RFC3339 instant or `YYYY-MM-DD` date
    /// (inclusive). Normalized to UTC; SQL compares instants via `julianday()`.
    /// A stored timestamp that is not a parseable datetime yields a NULL
    /// `julianday` and is excluded once `since` is set — current writers always
    /// emit RFC3339, so this only bites a hand-edited DB.
    pub since: Option<String>,
}

/// Report scope echoed into `--json` output so filtered runs are self-describing.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GeneratedFor {
    pub task: Option<String>,
    /// The **normalized** `--since` (post-`validate_since`): a `YYYY-MM-DD` input
    /// is echoed here as a full UTC RFC3339 instant, so it may differ textually
    /// from the raw value the caller passed.
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

/// Outcome metadata for one task (METRICS_SPEC §10.3), nested under
/// [`TaskReport::outcome`] so the terminal-state fields stay explicitly tied to
/// the outcome row rather than looking like independent token aggregates.
///
/// A deliberate manual projection of [`crate::db::metrics::TaskOutcome`] — the
/// identity fields (`task_tag`/`collab_session_id`) are intentionally dropped
/// (identity lives on [`TaskReport::task_key`]). There is no compile-time link,
/// so a new `task_outcomes` column must be added here in lockstep.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutcomeReport {
    pub outcome: Option<String>,
    pub review_rounds: i64,
    pub fix_commits: i64,
    pub handoffs: i64,
    pub started_at: Option<String>,
    pub done_at: Option<String>,
    pub pr_url: Option<String>,
}

/// One task's full report: outcome metadata (§10.3) + measured phase
/// decomposition (§10.1) + estimated/measured split (§10.2).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskReport {
    /// `COALESCE(collab_session_id, task_tag)` — the §10.1 task identity.
    pub task_key: String,
    /// Phase decomposition, ordered by [`PHASE_ORDER`] then `null`.
    pub by_phase: Vec<PhaseReport>,
    pub split: SplitReport,
    /// Outcome row for this task, if one exists.
    pub outcome: Option<OutcomeReport>,
    /// Σ measured tokens across all phases.
    pub tokens_to_done: i64,
    /// §7 cost summed over phases (`None` only if no phase priced).
    pub cost_usd: Option<f64>,
    /// Provider-reported cost summed over phases (NULL-preserving).
    pub provider_reported_cost_usd: Option<f64>,
}

/// One headline / non-completion row (METRICS_SPEC §10.4). `tokens_to_done`
/// and `provider_reported_cost_usd` are the verbatim §10.4 JOIN aggregates;
/// `cost_usd` is the §7-derived figure from the matching task.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HeadlineRow {
    /// `collab_session_id` when present else `task_tag` — matches the §10.1 key.
    pub task_key: String,
    pub task_tag: String,
    /// `None` ⟹ keyed by `task_tag` (no collab session), so `task_key == task_tag`.
    pub collab_session_id: Option<String>,
    pub tokens_to_done: i64,
    /// §7-derived cost for this task (`None` when no contributing row priced).
    pub cost_usd: Option<f64>,
    /// Verbatim §10.4 JOIN `SUM(cost_usd)` (NULL-preserving). This is the JOIN
    /// aggregate and may differ from a `TaskReport`'s phase-roll-up
    /// `provider_reported_cost_usd`; consumers must not assume the two are equal.
    pub provider_reported_cost_usd: Option<f64>,
}

/// The full `ironmem report` payload (serialized verbatim by `--json`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    /// Filters used to generate this report.
    pub generated_for: GeneratedFor,
    /// Distinct merged task_keys with ≥1 measured token row.
    pub baseline_task_count: usize,
    /// `baseline_task_count >= 10` (Phase-6 recording gate, METRICS_SPEC §11.5).
    pub baseline_ready: bool,
    /// Merged-only headline tokens-to-done, ordered by `task_key`.
    pub headline: Vec<HeadlineRow>,
    /// Non-completion (`failed`/`abandoned`) variant, ordered by `task_key`.
    pub non_completions: Vec<HeadlineRow>,
    /// Per-task decompositions, ordered by `task_key`.
    pub tasks: Vec<TaskReport>,
    /// Sorted-unique labels for measured groups with no §7 price (unknown model
    /// id, `"<none>"` for NULL model, `"codex"` for any codex group).
    pub unpriced_models: Vec<String>,
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

/// METRICS_SPEC §11.5 baseline count: distinct **measured** task_keys (from the
/// §10.1 roll-up) whose matched outcome is `merged`. Estimated-only and
/// outcome-only tasks are excluded — the gate requires real measured tokens
/// (§6.3). Shared by [`run_report`] and [`one_line_summary`] so the gate has a
/// single definition.
fn count_baseline_tasks(groups: &[TaskPhaseModelTokens], outcomes: &[TaskOutcome]) -> usize {
    groups
        .iter()
        .map(|g| g.task_key.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|key| {
            outcome_for_key(key, outcomes).and_then(|o| o.outcome.as_deref()) == Some("merged")
        })
        .count()
}

/// Assemble the METRICS_SPEC §10 report with §7-derived cost. No wallclock is
/// read on this path; every §7-derived cost is rounded to 6 dp at construction
/// (the headline `provider_reported_cost_usd` is the verbatim §10.4 SQL `SUM`).
///
/// Pipeline: pull the five §10 aggregates (filtered by `opts`), group the
/// §10.1 rows by `task_key`, decompose each task by phase (applying §7 rates
/// per (model,harness) group), join the §10.3 outcome, attach the §10.2 split,
/// then render the §10.4 headline / non-completion lists with §7 cost.
pub fn run_report(db: &Database, opts: &ReportOptions) -> Result<Report, MemoryError> {
    let task = opts.task.as_deref();
    let since = validate_since(opts.since.as_deref())?;
    let since_filter = since.as_deref();

    let groups = db.report_tokens_by_task_phase(task, since_filter)?;
    let splits = db.report_measured_estimated_split(task, since_filter)?;
    let outcomes = db.report_task_outcomes(task, since_filter)?;
    let headline_rows = db.report_headline(task, since_filter)?;
    let non_completion_rows = db.report_non_completions(task, since_filter)?;

    // Distinct task_keys present in any canonical task-shaped query (§10.1,
    // §10.2, or §10.3). This keeps estimated-only and outcome-only tasks visible
    // with zero measured tokens instead of silently dropping them from `tasks`.
    let mut task_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for g in &groups {
        task_keys.insert(g.task_key.clone());
    }
    for s in &splits {
        task_keys.insert(s.task_key.clone());
    }
    for o in &outcomes {
        task_keys.insert(task_outcome_key(o));
    }

    let tasks: Vec<TaskReport> = task_keys
        .iter()
        .map(|key| build_task(key, &groups, &splits, &outcomes))
        .collect();

    let baseline_task_count = count_baseline_tasks(&groups, &outcomes);

    let unpriced_models = collect_unpriced(&groups);

    let headline = build_headline_rows(&headline_rows, &tasks);
    let non_completions = build_headline_rows(&non_completion_rows, &tasks);

    Ok(Report {
        generated_for: GeneratedFor {
            task: opts.task.clone(),
            since,
        },
        baseline_task_count,
        baseline_ready: baseline_task_count >= BASELINE_READY_THRESHOLD,
        headline,
        non_completions,
        tasks,
        unpriced_models,
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
    let outcome = outcome.map(|o| OutcomeReport {
        outcome: o.outcome.clone(),
        review_rounds: o.review_rounds,
        fix_commits: o.fix_commits,
        handoffs: o.handoffs,
        started_at: o.started_at.clone(),
        done_at: o.done_at.clone(),
        pr_url: o.pr_url.clone(),
    });

    TaskReport {
        task_key: task_key.to_string(),
        by_phase,
        split: SplitReport {
            measured_tokens,
            estimated_tokens,
        },
        outcome,
        tokens_to_done,
        cost_usd,
        provider_reported_cost_usd,
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
            // Mirror the §10.4 JOIN (`u.task_tag = t.task_tag OR
            // u.collab_session_id = t.collab_session_id`): the §7 cost lives on
            // the `TaskReport` keyed by whichever identity the token rows used,
            // which is not always this row's `task_key` (the collab id). Match on
            // EITHER identity so a `task_tag`-keyed token set still attaches its
            // cost instead of silently rendering `n/a`.
            let cost_usd = tasks
                .iter()
                .find(|t| {
                    t.task_key == r.task_tag
                        || r.collab_session_id.as_deref() == Some(t.task_key.as_str())
                })
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

/// One-line, best-effort metrics summary for the `status` MCP tool.
///
/// This is deliberately infallible: it queries the §10 aggregates and, on ANY
/// error, returns `"metrics unavailable"` after a `tracing::warn!` (it never
/// panics or propagates, so a metrics fault cannot break the `status` tool).
/// An empty database yields exactly `"no metrics recorded yet"`; otherwise a
/// single line (no embedded newline) of task count, measured tokens, §7 cost,
/// and the baseline gate. No wallclock is read, so the output is deterministic.
pub fn one_line_summary(db: &Database) -> String {
    match one_line_summary_inner(db) {
        Ok(line) => line,
        Err(e) => {
            tracing::warn!("one_line_summary: metrics query failed: {e}");
            "metrics unavailable".to_string()
        }
    }
}

/// Fallible core of [`one_line_summary`]; the public wrapper degrades any `Err`
/// to a default string. Queries the §10.1 token roll-up and §10.3 outcomes
/// directly — deliberately cheaper than assembling the full [`run_report`] tree
/// (`status` is a hot endpoint) — for the §7 cost, baseline count, and task
/// count (tasks with an outcome OR ≥1 measured token row).
fn one_line_summary_inner(db: &Database) -> Result<String, MemoryError> {
    let groups = db.report_tokens_by_task_phase(None, None)?;
    let outcomes = db.report_task_outcomes(None, None)?;

    // Distinct task identities across outcomes and the measured roll-up: a task
    // counts if it has an outcome OR ≥1 measured token row. Keep this path much
    // cheaper than assembling the full report; `status` is a hot MCP endpoint.
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for o in &outcomes {
        keys.insert(task_outcome_key(o));
    }
    for g in &groups {
        keys.insert(g.task_key.clone());
    }

    if keys.is_empty() {
        return Ok("no metrics recorded yet".to_string());
    }

    let n = keys.len();
    let measured: i64 = groups
        .iter()
        .map(|g| {
            g.input_tokens
                + g.output_tokens
                + g.cache_creation_input_tokens
                + g.cache_read_input_tokens
        })
        .sum();
    let cost: f64 = round6(groups.iter().filter_map(group_cost).sum());
    let baseline = count_baseline_tasks(&groups, &outcomes);

    Ok(format!(
        "{n} tasks · {measured} measured tokens · ${cost:.2} (§7) · baseline {baseline}/{BASELINE_READY_THRESHOLD}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewTokenUsage;

    /// Minimal local seed: one merged task (`sess-rich`) with two measured
    /// rows, enough to exercise the non-empty `one_line_summary` branch.
    fn seeded_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.upsert_task_outcome(&TaskOutcome {
            task_tag: "issue-rich".into(),
            collab_session_id: Some("sess-rich".into()),
            started_at: Some("2026-06-01T00:00:00Z".into()),
            done_at: None,
            outcome: Some("merged".into()),
            review_rounds: 0,
            fix_commits: 0,
            handoffs: 0,
            pr_url: None,
        })
        .unwrap();
        let row = |phase: &str, model: &str, inp: i64, cost: Option<f64>| NewTokenUsage {
            ts: "2026-06-01T01:00:00Z".into(),
            source: "llm_rerank".into(),
            harness: "claude".into(),
            model: Some(model.into()),
            session_id: None,
            collab_session_id: Some("sess-rich".into()),
            collab_phase: Some(phase.into()),
            task_tag: None,
            input_tokens: inp,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: false,
            chars: 0,
            cost_usd: cost,
        };
        db.insert_token_usage(&row("planning", "claude-opus-4-8", 1_000_000, None))
            .unwrap();
        db.insert_token_usage(&row("impl", "claude-haiku-4-5", 1_000_000, Some(1.0)))
            .unwrap();
        db
    }

    #[test]
    fn one_line_summary_reports_counts_or_empty() {
        let empty = Database::open_in_memory().unwrap();
        assert_eq!(one_line_summary(&empty), "no metrics recorded yet");

        let db = seeded_db();
        let line = one_line_summary(&db);
        assert!(line.contains("task"), "task count surfaced: {line}");
        assert!(!line.contains('\n'), "must be one line: {line}");
    }

    #[test]
    fn null_phase_rows_bucket_last_with_none_phase() {
        // A measured row with `collab_phase = NULL` must still render, and the
        // canonical phase order (PHASE_ORDER then None) sorts it after every
        // named bucket — exercising `phase_rank`'s `None` branch that the
        // golden integration seed does not cover.
        let db = Database::open_in_memory().unwrap();
        let row = |phase: Option<&str>| NewTokenUsage {
            ts: "2026-06-01T01:00:00Z".into(),
            source: "llm_rerank".into(),
            harness: "claude".into(),
            model: Some("claude-opus-4-8".into()),
            session_id: None,
            collab_session_id: Some("sess-np".into()),
            collab_phase: phase.map(Into::into),
            task_tag: None,
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: false,
            chars: 0,
            cost_usd: None,
        };
        db.insert_token_usage(&row(Some("impl"))).unwrap();
        db.insert_token_usage(&row(None)).unwrap();

        let report = run_report(&db, &ReportOptions::default()).unwrap();
        let task = report
            .tasks
            .iter()
            .find(|t| t.task_key == "sess-np")
            .expect("task present");
        assert_eq!(task.by_phase.len(), 2);
        assert_eq!(task.by_phase[0].phase.as_deref(), Some("impl"));
        assert_eq!(task.by_phase[1].phase, None);
    }
}
