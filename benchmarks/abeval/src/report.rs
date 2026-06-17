//! Parse run artifacts / normalized metrics; compute the §11.3 trio
//! (tokens-to-done, rework_loops, merged-rate) and enforce the headline gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::constants::MIN_TASKS_PER_ARM;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMetric {
    pub arm: String,
    pub task_key: String,
    /// §2.2 done = "merged" AND ci_green.
    pub outcome: String,
    #[serde(default)]
    pub ci_green: bool,
    /// Estimated rows are visible but never eligible for headline deltas.
    #[serde(default)]
    pub estimated: bool,
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub review_rounds: u32,
    #[serde(default)]
    pub fix_commits: u32,
}

impl TaskMetric {
    /// §2.1 four-component sum.
    pub fn tokens_to_done(&self) -> u64 {
        self.input_tokens as u64
            + self.output_tokens as u64
            + self.cache_creation_input_tokens as u64
            + self.cache_read_input_tokens as u64
    }

    /// §11.4 rework_loops = review_rounds + fix_commits.
    pub fn rework_loops(&self) -> u64 {
        self.review_rounds as u64 + self.fix_commits as u64
    }

    /// §2.2 a task counts toward the headline only when merged AND CI-green.
    /// §2.1 headline token totals use measured rows only, never estimates.
    pub fn is_done(&self) -> bool {
        !self.estimated && self.outcome == crate::constants::OUTCOME_MERGED && self.ci_green
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsInput {
    /// "smoke" | "live".
    pub evidence_class: String,
    pub tasks: Vec<TaskMetric>,
}

/// Map one arm's executed [`ArmOutcome`](crate::client::ArmOutcome) plus the
/// gate result into a [`TaskMetric`] (issue #122 done-proxy).
///
/// `outcome:"merged" + ci_green` is the abeval done-proxy for literal
/// merge-to-main: it holds iff the arm's agent process completed without error
/// (`arm_outcome.outcome == "completed"`) AND the task's frozen gates passed in
/// the produced workspace (`ci_green`). Both are *measured* facts (process and
/// gate exit codes), never a self-assertion by the agent under test, and live
/// rows are always measured (`estimated:false`). See METRICS_SPEC §12.
pub fn build_arm_metric(
    task_id: &str,
    arm: &str,
    arm_outcome: &crate::client::ArmOutcome,
    ci_green: bool,
) -> TaskMetric {
    let completed = arm_outcome.outcome == crate::constants::OUTCOME_COMPLETED;
    let green = completed && ci_green;
    let outcome = if green {
        crate::constants::OUTCOME_MERGED.to_string()
    } else {
        // Preserve the measured agent-level outcome ("completed"/"failed");
        // either way it is not headline-eligible without a green gate.
        arm_outcome.outcome.clone()
    };
    let u = &arm_outcome.usage;
    TaskMetric {
        arm: arm.to_string(),
        task_key: format!("{task_id}:{arm}"),
        outcome,
        ci_green: green,
        estimated: false,
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        review_rounds: 0,
        fix_commits: 0,
    }
}

/// Write a normalized `evidence_class:"live"` metrics file consumable by
/// [`load_metrics`] / `abeval report --metrics`. This is the live-evidence
/// ingestion shape — run directories remain smoke-only by contract.
pub fn write_live_metrics(path: impl AsRef<Path>, tasks: &[TaskMetric]) -> Result<()> {
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks: tasks.to_vec(),
    };
    let body = serde_json::to_string_pretty(&input)?;
    crate::runner::atomic_write_str(path.as_ref(), &body)
}

/// Load a normalized metrics file (the live-evidence ingestion path).
///
/// `evidence_class` is validated against the closed set {"smoke","live"} and
/// rejected otherwise, so a typo'd/wrong-cased value (e.g. "Live") is a loud
/// error rather than being silently treated as smoke and withholding a real
/// headline delta. This keeps `load_metrics` symmetric with the run-dir path's
/// strictness in [`metrics_from_run_dir`].
pub fn load_metrics(path: impl AsRef<Path>) -> Result<MetricsInput> {
    let path = path.as_ref();
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading metrics {}", path.display()))?;
    let input: MetricsInput = serde_json::from_str(&body)
        .with_context(|| format!("parsing metrics {}", path.display()))?;
    match input.evidence_class.as_str() {
        "smoke" | "live" => {}
        other => anyhow::bail!(
            "{}: invalid evidence_class {:?} (expected \"smoke\" or \"live\")",
            path.display(),
            other
        ),
    }
    Ok(input)
}

/// Aggregate every per-task `live_metrics.json` written under an `--out` tree
/// into a single live [`MetricsInput`], so batches run separately (e.g. 2 at a
/// time) can be scored together against the §11.3 headline gate.
///
/// Scans immediate subdirectories `<dir>/<task_id>/live_metrics.json` (the shape
/// the live runner writes), in sorted order for determinism, and unions their
/// task rows. Errors if:
/// - the tree holds no such file (nothing to report — never an empty pass), or
/// - any loaded file is not `evidence_class:"live"` (a smoke file must never
///   silently dilute live evidence into a fabricated headline).
///
/// Duplicate `task_key` rows (e.g. a task re-run) are NOT collapsed here; the
/// §11.3 gate already ignores duplicate keys, so aggregation cannot inflate `n`.
pub fn load_metrics_dir(dir: impl AsRef<Path>) -> Result<MetricsInput> {
    let dir = dir.as_ref();
    let mut files: Vec<PathBuf> = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading metrics dir {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading entry in {}", dir.display()))?
            .path();
        if path.is_dir() {
            let candidate = path.join("live_metrics.json");
            if candidate.is_file() {
                files.push(candidate);
            }
        }
    }
    files.sort();

    let mut tasks: Vec<TaskMetric> = Vec::new();
    for file in &files {
        let input = load_metrics(file)?;
        if input.evidence_class != "live" {
            anyhow::bail!(
                "{}: expected evidence_class \"live\" for aggregation, got {:?}",
                file.display(),
                input.evidence_class
            );
        }
        tasks.extend(input.tasks);
    }
    if tasks.is_empty() {
        anyhow::bail!(
            "no */live_metrics.json found under {} (nothing to aggregate)",
            dir.display()
        );
    }
    Ok(MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    })
}

/// The subset of `run_meta.json` the report path reads back. Typed (not
/// `serde_json::Value`) so an unexpected `evidence_class` is rejected rather
/// than silently downgraded to smoke: a mistyped field (e.g. a number) is a
/// serde parse error with file context, and a missing/unknown value deserializes
/// to `None`/other and is caught by the `other =>` bail in `metrics_from_run_dir`.
/// Kept in sync by convention with the writer `runner::RunMeta`/`ArmUsageRecord`
/// (the `tests/report.rs` run-dir tests pin the serialized shape).
#[derive(Deserialize)]
struct RunMetaArmRead {
    arm: String,
    outcome: String,
}

#[derive(Deserialize)]
struct RunMetaRead {
    evidence_class: Option<String>,
    #[serde(default)]
    per_arm: Vec<RunMetaArmRead>,
}

/// Build a MetricsInput from a run directory (reads run_meta.json + per-arm usage.json).
///
/// Run directories are smoke-only by contract: the dry-run path is the only
/// writer of a `run_meta.json` tree, and live evidence is a separate normalized
/// metrics file (`live_metrics.json` → [`load_metrics`]), never a run dir. A run
/// dir is therefore always smoke here: per-arm `outcome` is read back verbatim
/// from `run_meta.json` and
/// `ci_green` is `false` by the smoke contract, so no row is ever headline-
/// eligible. A `"live"` (or unknown) run dir cannot legitimately be produced by
/// this PR — it is a hard error rather than a silent smoke downgrade or a
/// fabricated outcome. Live evidence is consumed via a normalized metrics file
/// (`report --metrics <file>` → [`load_metrics`]).
pub fn metrics_from_run_dir(run: impl AsRef<Path>) -> Result<MetricsInput> {
    let run = run.as_ref();
    let mut tasks = Vec::new();
    // `read_dir` order is filesystem-dependent; sort task dirs so the rendered
    // report and per-task ordering are deterministic across runs and platforms
    // (aggregation is by key, so this affects presentation, not gate math).
    let mut task_dirs: Vec<PathBuf> = std::fs::read_dir(run)
        .with_context(|| format!("reading run dir {}", run.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    task_dirs.sort();
    for task_dir in task_dirs {
        if !task_dir.is_dir() {
            continue;
        }
        let meta_path = task_dir.join("run_meta.json");
        if !meta_path.exists() {
            continue;
        }
        let meta: RunMetaRead = serde_json::from_str(
            &std::fs::read_to_string(&meta_path)
                .with_context(|| format!("reading {}", meta_path.display()))?,
        )
        .with_context(|| format!("parsing {}", meta_path.display()))?;
        match meta.evidence_class.as_deref() {
            Some("smoke") => {}
            Some("live") => anyhow::bail!(
                "{}: live run directories are not supported in this PR (no paid runs); \
                 supply live evidence via `report --metrics <file>`",
                meta_path.display()
            ),
            other => anyhow::bail!(
                "{}: invalid or missing evidence_class {:?} (expected \"smoke\")",
                meta_path.display(),
                other
            ),
        }
        // A non-UTF8 dir name is anomalous (ids are ASCII-constrained at write
        // time); surface it rather than collapsing to an empty task key.
        let task_id = task_dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 task directory name under {}", run.display()))?
            .to_string();
        for arm_rec in &meta.per_arm {
            let usage_path = task_dir.join(&arm_rec.arm).join("usage.json");
            if !usage_path.exists() {
                continue;
            }
            let usage: crate::client::Usage = serde_json::from_str(
                &std::fs::read_to_string(&usage_path)
                    .with_context(|| format!("reading {}", usage_path.display()))?,
            )
            .with_context(|| format!("parsing {}", usage_path.display()))?;
            tasks.push(TaskMetric {
                arm: arm_rec.arm.clone(),
                task_key: format!("{task_id}:{}", arm_rec.arm),
                // Real per-arm outcome from run_meta.json (not hardcoded).
                outcome: arm_rec.outcome.clone(),
                // Smoke run dirs are never CI-green; guaranteed by the
                // evidence_class check above, so this is the contract, not a
                // fabricated value.
                ci_green: false,
                estimated: false,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                review_rounds: 0,
                fix_commits: 0,
            });
        }
    }
    Ok(MetricsInput {
        evidence_class: "smoke".to_string(),
        tasks,
    })
}

struct ArmAgg {
    completed: Vec<u64>, // tokens-to-done for merged+green tasks
    rework: Vec<u64>,
    attempted: usize,
    attempted_tokens: u64,
    merged: usize,
    seen_task_keys: BTreeSet<String>,
    duplicates_ignored: usize,
}

fn aggregate(input: &MetricsInput) -> BTreeMap<String, ArmAgg> {
    let mut by_arm: BTreeMap<String, ArmAgg> = BTreeMap::new();
    for t in &input.tasks {
        let agg = by_arm.entry(t.arm.clone()).or_insert(ArmAgg {
            completed: Vec::new(),
            rework: Vec::new(),
            attempted: 0,
            attempted_tokens: 0,
            merged: 0,
            seen_task_keys: BTreeSet::new(),
            duplicates_ignored: 0,
        });
        if !agg.seen_task_keys.insert(t.task_key.clone()) {
            agg.duplicates_ignored += 1;
            continue;
        }
        agg.attempted += 1;
        agg.attempted_tokens = agg.attempted_tokens.saturating_add(t.tokens_to_done());
        if t.is_done() {
            agg.merged += 1;
            agg.completed.push(t.tokens_to_done());
            agg.rework.push(t.rework_loops());
        }
    }
    by_arm
}

fn mean(xs: &[u64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<u64>() as f64 / xs.len() as f64
}

fn spread(xs: &[u64]) -> u64 {
    match (xs.iter().min(), xs.iter().max()) {
        (Some(lo), Some(hi)) => hi - lo,
        _ => 0,
    }
}

/// Render the report. Headline deltas are emitted ONLY when evidence is live
/// AND every arm has >= MIN_TASKS_PER_ARM completed (merged + CI-green) tasks.
pub fn render_report(input: &MetricsInput) -> String {
    let mut out = String::new();
    let by_arm = aggregate(input);

    // Per-arm visible numbers (always shown, even for smoke/failed).
    for (arm, agg) in &by_arm {
        let duplicate_note = if agg.duplicates_ignored > 0 {
            format!(" duplicates_ignored={}", agg.duplicates_ignored)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "arm {arm}: attempted={} attempted_tokens={} merged={} completed={} \
             mean_tokens={:.1} mean_rework={:.1}{duplicate_note}\n",
            agg.attempted,
            agg.attempted_tokens,
            agg.merged,
            agg.completed.len(),
            mean(&agg.completed),
            mean(&agg.rework),
        ));
    }

    let is_live = input.evidence_class == "live";
    let enough = ["ironmem", "superpowers"].iter().all(|arm| {
        by_arm
            .get(*arm)
            .is_some_and(|a| a.completed.len() >= MIN_TASKS_PER_ARM)
    });

    if !is_live {
        out.push_str("SMOKE — non-headline: no cross-arm delta is claimed.\n");
        return out;
    }
    if !enough {
        out.push_str(&format!(
            "non-headline: each arm needs >= {MIN_TASKS_PER_ARM} merged+CI-green \
             measured tasks for both ironmem and superpowers before any delta is reported.\n"
        ));
        return out;
    }

    // Gate passed: emit confidence-qualified deltas (never bare point estimates).
    let iron = by_arm.get("ironmem");
    let sp = by_arm.get("superpowers");
    if let (Some(i), Some(s)) = (iron, sp) {
        let n = i.completed.len().min(s.completed.len());
        out.push_str(&format!(
            "DELTA (n={n}, confidence-qualified):\n\
             tokens-to-done: ironmem mean={:.1} (spread {}), \
             superpowers mean={:.1} (spread {})\n\
             rework_loops: ironmem mean={:.1}, superpowers mean={:.1}\n\
             merged-rate: ironmem {}/{}, superpowers {}/{}\n",
            mean(&i.completed),
            spread(&i.completed),
            mean(&s.completed),
            spread(&s.completed),
            mean(&i.rework),
            mean(&s.rework),
            i.merged,
            i.attempted,
            s.merged,
            s.attempted,
        ));
    }
    out
}
