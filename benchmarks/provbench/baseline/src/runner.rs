//! Batch dispatcher with budget guard, atomic checkpointing, and `--resume`.
//!
//! Drives the LLM-as-invalidator pass over a frozen [`SampleManifest`]:
//!   1. Verify the on-disk manifest's `content_hash` matches the in-memory
//!      manifest (on `--resume`; first run skips verification because the
//!      manifest was just minted).
//!   2. Group sampled rows by `commit_sha`, sort each group by `fact_id`
//!      for determinism, chunk into ≤`MAX_FACTS_PER_BATCH` per batch.
//!      Attach diff text and FactBody set per row.
//!   3. On resume: read existing `predictions.jsonl` and skip any batch
//!      whose rows are all already-scored.
//!   4. For each remaining batch: estimate cost, ask the [`CostMeter`]
//!      whether to proceed, build the 5-block prompt, dispatch (live
//!      Anthropic call, fixture replay, or dry-run synthesis), append
//!      results to `predictions.jsonl` atomically (tmp + rename).
//!   5. On budget-abort or fatal error: write a `run_meta.json` recording
//!      the partial state, return `aborted = true`. The caller exits with
//!      code 2 so CI surfaces the budget guardrail.
//!
//! Concurrency: v1 dispatches batches sequentially. `max_concurrency` is
//! accepted for forward-compat but currently ignored — the SPEC's cache
//! semantics require strictly-ordered batches within a commit anyway, and
//! cross-commit parallelism is not worth the resume-checkpointing
//! complexity at this stage. The CLI flag is preserved so a later task
//! can introduce a bounded `JoinSet` without a breaking change.

use crate::budget::{preflight_worst_case_cost, BatchDecision as BudgetDecision, CostMeter};
use crate::client::{AnthropicClient, BatchResponse, Decision, ParseFailureError, Usage};
use crate::constants::*;
use crate::diffs::{load_diffs_dir, DiffArtifact};
use crate::facts::{load_facts, FactBody};
use crate::manifest::SampleManifest;
use crate::prompt::PromptBuilder;
use crate::sample::SampledRow;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use provbench_scoring::PredictionRow;

/// Options for [`run`]. Constructed by the CLI from `RunArgs`.
pub struct RunnerOpts {
    pub run_dir: PathBuf,
    pub manifest: SampleManifest,
    pub budget_usd: f64,
    pub resume: bool,
    pub dry_run: bool,
    pub fixture_mode: Option<PathBuf>,
    pub max_batches: Option<usize>,
    /// Forward-compat knob; ignored in v1 (sequential dispatch).
    pub max_concurrency: usize,
    /// Optional pre-built client (test-only seam: lets the integration
    /// suite point the runner at a wiremock server without touching
    /// global env vars). `None` in the CLI path — `from_env()` is used.
    pub client_override: Option<AnthropicClient>,
}

/// Summary returned by [`run`] and persisted as `run_meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub batches_total: usize,
    pub batches_completed: usize,
    pub batches_skipped_resume: usize,
    /// Batches whose live response failed both parse attempts. The
    /// affected batches contribute zero rows to `predictions.jsonl`;
    /// each failure writes a diagnostic line to `parse_failures.jsonl`.
    #[serde(default)]
    pub batches_parse_failed: usize,
    pub rows_total: usize,
    pub rows_scored: usize,
    pub total_cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub tokens_in_uncached: u64,
    #[serde(default)]
    pub tokens_in_cache_write: u64,
    #[serde(default)]
    pub tokens_in_cache_read: u64,
    #[serde(default)]
    pub tokens_out: u64,
    pub aborted: bool,
    pub abort_reason: Option<String>,
    pub manifest_content_hash: String,
}

/// One unit of work for the dispatcher. `batch_id` is deterministic
/// (`<commit_sha>-<batch_index>`) so resume can identify already-done
/// batches without relying on row order alone.
#[derive(Debug, Clone)]
pub struct Batch {
    pub batch_id: String,
    pub commit_sha: String,
    pub diff: String,
    pub facts: Vec<FactBody>,
    pub rows: Vec<SampledRow>,
    pub batches_in_commit: usize,
}

/// On-disk schema for `--fixture-mode <dir>` replay. One JSON per batch
/// at `<dir>/<batch_id>.json`. Fields beyond `decisions`/`usage` are
/// optional so the schema can be hand-authored for tests.
#[derive(Debug, Deserialize)]
struct FixtureBatchResponse {
    decisions: Vec<FixtureDecision>,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    wall_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct FixtureDecision {
    id: String,
    decision: String,
}

/// Drive the run. See module docs for the full contract.
pub async fn run(opts: RunnerOpts) -> Result<RunResult> {
    let RunnerOpts {
        run_dir,
        manifest,
        budget_usd,
        resume,
        dry_run,
        fixture_mode,
        max_batches,
        max_concurrency: _max_concurrency, // v1: sequential
        client_override,
    } = opts;

    if resume {
        verify_manifest_hash(&run_dir, &manifest)?;
    }

    // Load upstream artifacts once.
    let facts = load_facts(&manifest.facts_path)
        .with_context(|| format!("load facts {}", manifest.facts_path.display()))?;
    let diffs = load_diffs_dir(&manifest.diffs_dir)
        .with_context(|| format!("load diffs {}", manifest.diffs_dir.display()))?;

    let batches = build_batches(&manifest.rows, &facts, &diffs)?;
    let batches_total = batches.len();
    let rows_total: usize = batches.iter().map(|b| b.rows.len()).sum();

    let predictions_path = run_dir.join("predictions.jsonl");
    let done_keys = if resume {
        read_done_keys(&predictions_path)?
    } else {
        HashSet::new()
    };

    let mut meter = CostMeter::new(budget_usd);
    let mut batches_completed = 0usize;
    let mut batches_skipped_resume = 0usize;
    let mut batches_parse_failed = 0usize;
    let mut rows_scored = 0usize;
    let mut aborted = false;
    let mut abort_reason: Option<String> = None;

    // Built only when we actually need to hit the live API. Tests inject
    // a pre-built client via `client_override` to point at a mock server
    // without mutating global env state.
    let client: Option<AnthropicClient> = if let Some(c) = client_override {
        Some(c)
    } else if !dry_run && fixture_mode.is_none() {
        Some(AnthropicClient::from_env()?)
    } else {
        None
    };

    for batch in batches.iter() {
        if let Some(cap) = max_batches {
            if batches_completed >= cap {
                break;
            }
        }

        // Resume: skip if every row in this batch is already scored.
        let all_done = batch
            .rows
            .iter()
            .all(|r| done_keys.contains(&(r.fact_id.clone(), r.commit_sha.clone())));
        if all_done && !batch.rows.is_empty() {
            batches_skipped_resume += 1;
            continue;
        }

        // Budget guard. Cheap per-batch estimate using current diff+fact sizes.
        let estimated = estimate_batch_cost(batch);
        match meter.before_next_batch(estimated) {
            BudgetDecision::Proceed => {}
            BudgetDecision::Abort {
                reason,
                current,
                would_be,
                cap_95,
            } => {
                aborted = true;
                abort_reason = Some(format!(
                    "{reason}: current=${current:.4} would_be=${would_be:.4} cap_95=${cap_95:.4}"
                ));
                tracing::warn!(
                    "budget abort before batch {}: {}",
                    batch.batch_id,
                    abort_reason.as_deref().unwrap_or("?")
                );
                break;
            }
        }

        let multi_batch = batch.batches_in_commit > 1;
        let blocks = PromptBuilder::build(&batch.diff, &batch.facts, multi_batch);

        let response = if dry_run && fixture_mode.is_none() {
            synth_dry_run_response(batch)
        } else if let Some(fix_dir) = fixture_mode.as_ref() {
            load_fixture_response(fix_dir, &batch.batch_id)
                .with_context(|| format!("fixture for batch {}", batch.batch_id))?
        } else {
            // Live path. `client` is Some by construction in this branch.
            let c = client.as_ref().expect("live client present");
            match c.score_batch(blocks).await {
                Ok(r) => r,
                Err(e) => {
                    // Downcast: the addendum-retry path surfaces a typed
                    // ParseFailureError when the model returns non-JSON
                    // twice in a row. Skip the batch, log a sidecar
                    // entry for offline review, continue. All other
                    // errors (network, 4xx, 5xx-after-retries) still
                    // abort the run because they indicate a systemic
                    // problem we don't want to silently sweep past.
                    if let Some(pf) = e.downcast_ref::<ParseFailureError>() {
                        tracing::warn!(
                            "parse failure on batch {}: request_id={} err={}",
                            batch.batch_id,
                            pf.request_id,
                            pf.err_msg,
                        );
                        append_parse_failure(
                            &run_dir.join("parse_failures.jsonl"),
                            &batch.batch_id,
                            pf,
                        )?;
                        batches_parse_failed += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        };

        meter.record(&response.usage)?;

        // Index predictions by id for the parallel-row lookup.
        let pred_by_id: HashMap<&str, &str> = response
            .decisions
            .iter()
            .map(|d| (d.id.as_str(), d.decision.as_str()))
            .collect();
        let mut new_rows: Vec<PredictionRow> = Vec::with_capacity(batch.rows.len());
        for row in &batch.rows {
            if done_keys.contains(&(row.fact_id.clone(), row.commit_sha.clone())) {
                // Partial-resume safety: don't re-emit a row that already
                // landed in a prior run.
                continue;
            }
            let prediction = pred_by_id
                .get(row.fact_id.as_str())
                .copied()
                .unwrap_or("missing")
                .to_string();
            new_rows.push(PredictionRow {
                fact_id: row.fact_id.clone(),
                commit_sha: row.commit_sha.clone(),
                batch_id: batch.batch_id.clone(),
                ground_truth: row.ground_truth.clone(),
                prediction,
                request_id: response.request_id.clone(),
                wall_ms: response.wall_ms,
                evidence: None,
            });
        }

        append_predictions(&predictions_path, &new_rows)?;
        rows_scored += new_rows.len();
        batches_completed += 1;
    }

    let result = RunResult {
        batches_total,
        batches_completed,
        batches_skipped_resume,
        batches_parse_failed,
        rows_total,
        rows_scored,
        total_cost_usd: meter.cost_usd,
        total_tokens: meter.total_tokens(),
        tokens_in_uncached: meter.tokens_in_uncached,
        tokens_in_cache_write: meter.tokens_in_cache_write,
        tokens_in_cache_read: meter.tokens_in_cache_read,
        tokens_out: meter.tokens_out,
        aborted,
        abort_reason,
        manifest_content_hash: manifest.content_hash.clone(),
    };
    write_run_meta(&run_dir, &result)?;
    Ok(result)
}

/// Group sampled rows by commit, sort within each commit by `fact_id`,
/// chunk into batches of ≤`MAX_FACTS_PER_BATCH`. The output Vec is
/// sorted by `(commit_sha, batch_index)` for deterministic dispatch.
///
/// Rows whose commit has no Included diff or whose fact body is absent
/// fail closed. The manifest stage should already have filtered them;
/// if artifacts drift between `sample` and `run`, silently dropping
/// selected rows would make the reported coverage dishonest.
pub fn build_batches(
    rows: &[SampledRow],
    facts: &HashMap<String, FactBody>,
    diffs: &HashMap<String, DiffArtifact>,
) -> Result<Vec<Batch>> {
    let mut by_commit: BTreeMap<String, Vec<&SampledRow>> = BTreeMap::new();
    let mut missing_diffs: Vec<String> = Vec::new();
    let mut missing_facts: Vec<String> = Vec::new();
    for r in rows {
        let included = matches!(
            diffs.get(&r.commit_sha),
            Some(DiffArtifact::Included { .. })
        );
        if !included {
            missing_diffs.push(format!("{}@{}", r.fact_id, r.commit_sha));
            continue;
        }
        if !facts.contains_key(&r.fact_id) {
            missing_facts.push(format!("{}@{}", r.fact_id, r.commit_sha));
            continue;
        }
        by_commit.entry(r.commit_sha.clone()).or_default().push(r);
    }
    anyhow::ensure!(
        missing_diffs.is_empty() && missing_facts.is_empty(),
        "manifest selected rows missing artifacts: missing_diffs={} missing_facts={}",
        summarize_missing(&missing_diffs),
        summarize_missing(&missing_facts)
    );

    let mut out: Vec<Batch> = Vec::new();
    for (commit_sha, mut group) in by_commit {
        group.sort_by(|a, b| a.fact_id.cmp(&b.fact_id));
        let diff_text = match diffs.get(&commit_sha) {
            Some(DiffArtifact::Included { unified_diff, .. }) => unified_diff.clone(),
            _ => unreachable!("filtered above"),
        };
        let n_chunks = group.len().div_ceil(MAX_FACTS_PER_BATCH);
        for (batch_index, chunk) in group.chunks(MAX_FACTS_PER_BATCH).enumerate() {
            let batch_id = format!("{commit_sha}-{batch_index}");
            let rows: Vec<SampledRow> = chunk.iter().map(|r| (*r).clone()).collect();
            let fact_bodies: Vec<FactBody> = chunk
                .iter()
                .map(|r| facts.get(&r.fact_id).expect("checked above").clone())
                .collect();
            out.push(Batch {
                batch_id,
                commit_sha: commit_sha.clone(),
                diff: diff_text.clone(),
                facts: fact_bodies,
                rows,
                batches_in_commit: n_chunks,
            });
        }
    }
    Ok(out)
}

fn summarize_missing(items: &[String]) -> String {
    if items.is_empty() {
        return "0".to_string();
    }
    let preview = items.iter().take(5).cloned().collect::<Vec<_>>().join(",");
    if items.len() > 5 {
        format!("{} [{}...]", items.len(), preview)
    } else {
        format!("{} [{}]", items.len(), preview)
    }
}

/// Cheap per-batch estimate. Mirrors [`preflight_worst_case_cost`] but
/// scoped to one batch with this batch's known diff/fact sizes.
///
/// Conservative: assumes the worst-case 1,800 output tokens and the
/// cache-write price for the cacheable prefix (over-charges on
/// second-and-later batches in a multi-batch commit). The over-charge
/// is intentional — this estimate is the budget gate, not the meter.
fn estimate_batch_cost(batch: &Batch) -> f64 {
    let diff_tokens = (batch.diff.len() as f64) / 4.0;
    let fact_tokens: f64 = batch
        .facts
        .iter()
        .map(|f| (f.body.len() + f.source_path.len() + 80) as f64 / 4.0)
        .sum();
    let static_prefix_tokens = 250.0;
    let worst_case_output_tokens = 1_800.0;

    let cacheable_tokens = static_prefix_tokens + diff_tokens + 10.0;
    let in_usd = (cacheable_tokens / 1_000_000.0) * PRICE_INPUT_CACHE_WRITE_USD_PER_MTOK
        + (fact_tokens / 1_000_000.0) * PRICE_INPUT_UNCACHED_USD_PER_MTOK;
    let out_usd = (worst_case_output_tokens / 1_000_000.0) * PRICE_OUTPUT_USD_PER_MTOK;
    in_usd + out_usd
}

/// Append new prediction rows to `path` atomically.
///
/// Pattern: read-existing → append → tmp-write → rename. On POSIX
/// rename-over-existing is atomic, so the file is never observed in a
/// half-written state. A crash between read and rename loses only the
/// new batch's rows — the prior file is intact.
fn append_predictions(path: &Path, new_rows: &[PredictionRow]) -> Result<()> {
    if new_rows.is_empty() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("predictions path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut existing = if path.exists() {
        std::fs::read(path).with_context(|| format!("read {}", path.display()))?
    } else {
        Vec::new()
    };
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        existing.push(b'\n');
    }
    for row in new_rows {
        let line = serde_json::to_string(row)?;
        existing.extend_from_slice(line.as_bytes());
        existing.push(b'\n');
    }
    let tmp = parent.join(format!(".predictions.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &existing).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Append one diagnostic line to `parse_failures.jsonl`. Uses the same
/// read-existing → append → tmp-write → rename pattern as
/// [`append_predictions`] so a crash can never leave the sidecar in a
/// half-written state.
fn append_parse_failure(path: &Path, batch_id: &str, pf: &ParseFailureError) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("parse_failures path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let entry = serde_json::json!({
        "batch_id": batch_id,
        "request_id": pf.request_id,
        "raw_text": pf.raw_text,
        "err_msg": pf.err_msg,
    });
    let line = serde_json::to_string(&entry)?;
    let mut existing = if path.exists() {
        std::fs::read(path).with_context(|| format!("read {}", path.display()))?
    } else {
        Vec::new()
    };
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        existing.push(b'\n');
    }
    existing.extend_from_slice(line.as_bytes());
    existing.push(b'\n');
    let tmp = parent.join(format!(".parse_failures.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &existing).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Verify the on-disk `manifest.json` content_hash matches the in-memory
/// manifest's recorded hash. Defends against editing the manifest
/// between runs without re-sampling.
fn verify_manifest_hash(run_dir: &Path, manifest: &SampleManifest) -> Result<()> {
    let on_disk_path = run_dir.join("manifest.json");
    let bytes = std::fs::read(&on_disk_path)
        .with_context(|| format!("read manifest for resume: {}", on_disk_path.display()))?;
    let on_disk: SampleManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse manifest {}", on_disk_path.display()))?;

    // Recompute via the exact same helper the writer used. This guarantees
    // identical bytes go into the SHA-256 input on both sides (in particular,
    // serde_json's compact form rather than the pretty-printed on-disk file,
    // and with `created_at` blanked alongside `content_hash`).
    let recorded = on_disk.content_hash.clone();
    let recomputed = on_disk.compute_content_hash();

    anyhow::ensure!(
        recomputed == recorded,
        "on-disk manifest content_hash is inconsistent: recorded={} recomputed={}",
        recorded,
        recomputed
    );
    anyhow::ensure!(
        recorded == manifest.content_hash,
        "resume manifest mismatch: on-disk={} in-memory={}",
        recorded,
        manifest.content_hash
    );
    Ok(())
}

/// Read existing `predictions.jsonl` and collect the (fact_id,
/// commit_sha) pairs already scored. Returns empty set if file absent.
fn read_done_keys(path: &Path) -> Result<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    if !path.exists() {
        return Ok(out);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: PredictionRow = serde_json::from_str(line)
            .with_context(|| format!("parse predictions row {}:{}", path.display(), lineno + 1))?;
        out.insert((row.fact_id, row.commit_sha));
    }
    Ok(out)
}

/// Read `<dir>/<batch_id>.json` and return a [`BatchResponse`].
///
/// Defense-in-depth: after building the candidate path, canonicalize
/// both the fixture directory and the resolved path and verify the
/// latter is still rooted under the former. A malformed `batch_id`
/// containing `..` segments would otherwise let `Path::join` silently
/// escape the fixture directory and read arbitrary files.
fn load_fixture_response(dir: &Path, batch_id: &str) -> Result<BatchResponse> {
    let path = dir.join(format!("{batch_id}.json"));
    if let (Ok(canon_dir), Ok(canon_path)) = (dir.canonicalize(), path.canonicalize()) {
        anyhow::ensure!(
            canon_path.starts_with(&canon_dir),
            "fixture path {} escapes fixture dir {}",
            canon_path.display(),
            canon_dir.display(),
        );
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read fixture {}", path.display()))?;
    let fix: FixtureBatchResponse =
        serde_json::from_str(&text).with_context(|| format!("parse fixture {}", path.display()))?;
    let decisions = fix
        .decisions
        .into_iter()
        .map(|d| Decision {
            id: d.id,
            decision: d.decision,
        })
        .collect();
    Ok(BatchResponse {
        decisions,
        usage: fix.usage,
        request_id: fix.request_id.unwrap_or_else(|| "fixture".into()),
        wall_ms: fix.wall_ms.unwrap_or(0),
    })
}

/// Fabricate a "valid" decision per row for `--dry-run` mode. Zero
/// usage → zero cost, so the meter never trips. Used by the
/// resume-safety test to exercise the full path without network.
fn synth_dry_run_response(batch: &Batch) -> BatchResponse {
    let decisions = batch
        .rows
        .iter()
        .map(|r| Decision {
            id: r.fact_id.clone(),
            decision: "valid".to_string(),
        })
        .collect();
    BatchResponse {
        decisions,
        usage: Usage::default(),
        request_id: "dry-run".into(),
        wall_ms: 0,
    }
}

/// Write `run_meta.json` atomically (tmp + rename).
fn write_run_meta(run_dir: &Path, result: &RunResult) -> Result<()> {
    std::fs::create_dir_all(run_dir)?;
    let path = run_dir.join("run_meta.json");
    let tmp = run_dir.join(format!(".run_meta.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(result)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Compute total cost across all batches in a [`SampleManifest`] —
/// helper for ad-hoc validation, not used by [`run`].
///
/// Kept here (not in `budget.rs`) because it needs the in-memory facts
/// and diffs maps which the runner already loads.
pub fn preflight_run_cost(
    manifest: &SampleManifest,
    facts: &HashMap<String, FactBody>,
    diffs: &HashMap<String, DiffArtifact>,
) -> f64 {
    preflight_worst_case_cost(&manifest.rows, diffs, facts)
}
