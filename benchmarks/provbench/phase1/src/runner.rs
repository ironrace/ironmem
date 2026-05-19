//! Commit-grouped runner. Reads `eval_rows` grouped by (commit_sha, source_path),
//! opens the commit tree once via gix, parses touched files once with tree-sitter,
//! runs the rule chain per fact, writes results to SQLite and to JSONL artifacts.

use anyhow::{Context, Result};
use provbench_scoring::PredictionRow;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use crate::diffs::CommitDiff;
use crate::facts::FactBody;
use crate::repo::Repo;
use crate::rules::{Decision, RowCtx, RuleChain};

/// Configuration for a single Phase 1 scoring run.
///
/// Aggregates the SQLite handle (already populated by `facts::ingest`,
/// `diffs::ingest`, and `baseline_run::ingest`), a HEAD-only gix repo
/// reader, the T0 anchor commit, the rule-set version label embedded in
/// `request_id`, and the two output JSONL paths.
///
/// All references are short-lived (one `runner::run` call) and the
/// struct is consumed by value; it intentionally borrows everything to
/// avoid copying paths or duplicating the DB connection.
// NOTE: this struct currently leaks `rusqlite::Connection` through the
// public API; trait-ify before ironmem integration.
pub struct RunnerOpts<'a> {
    pub db: &'a Connection,
    pub repo: &'a Repo,
    pub t0: &'a str,
    pub rule_set_version: &'a str,
    pub out_predictions: &'a Path,
    pub out_traces: &'a Path,
}

pub fn run(opts: RunnerOpts<'_>) -> Result<RunStats> {
    let chain = RuleChain::default();

    // Load all facts once.
    let mut facts: HashMap<String, FactBody> = HashMap::new();
    {
        let mut stmt = opts.db.prepare(
            "SELECT fact_id, kind, body, source_path, line_start, line_end, \
             symbol_path, content_hash_at_observation, labeler_git_sha FROM facts",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FactBody {
                fact_id: r.get(0)?,
                kind: r.get(1)?,
                body: r.get(2)?,
                source_path: r.get(3)?,
                line_span: [r.get::<_, i64>(4)? as u64, r.get::<_, i64>(5)? as u64],
                symbol_path: r.get(6)?,
                content_hash_at_observation: r.get(7)?,
                labeler_git_sha: r.get(8)?,
            })
        })?;
        for row in rows {
            let f = row?;
            facts.insert(f.fact_id.clone(), f);
        }
    }

    // T0 blob cache, keyed by source_path.
    let mut t0_blobs: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    // Per-commit tree-listing cache (populated for R7 rename search).
    let mut commit_files_cache: HashMap<String, Vec<String>> = HashMap::new();
    // Per-commit diff-artifact cache (populated for R0 diff_excluded check).
    let mut diff_cache: HashMap<String, Option<CommitDiff>> = HashMap::new();

    // Stream eval_rows ordered for stable output.
    let mut stmt = opts.db.prepare(
        "SELECT row_index, fact_id, commit_sha, batch_id, ground_truth \
         FROM eval_rows ORDER BY row_index ASC",
    )?;
    let mut rows = stmt.query([])?;
    let mut predictions_f = File::create(opts.out_predictions)?;
    let mut traces_f = File::create(opts.out_traces)?;
    let mut ins_pred = opts.db.prepare(
        "INSERT INTO predictions \
         (row_index, fact_id, commit_sha, batch_id, ground_truth, prediction, request_id, wall_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let mut ins_trace = opts.db.prepare(
        "INSERT INTO rule_traces (row_index, rule_id, spec_ref, reason_code, evidence_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;

    let mut stats = RunStats::default();

    while let Some(r) = rows.next()? {
        let row_index: i64 = r.get(0)?;
        let fact_id: String = r.get(1)?;
        let commit_sha: String = r.get(2)?;
        let batch_id: String = r.get(3)?;
        let ground_truth: String = r.get(4)?;

        let fact = facts
            .get(&fact_id)
            .with_context(|| format!("fact_id {} not in facts table", fact_id))?;

        // Cache miss → call gix; propagate errors with context so a corrupt T0 SHA
        // or unreadable repo surfaces immediately rather than silently inflating
        // needs_revalidation counts downstream.
        let t0_blob = if let Some(v) = t0_blobs.get(&fact.source_path) {
            v.clone()
        } else {
            let v = opts
                .repo
                .blob_at(opts.t0, &fact.source_path)
                .with_context(|| {
                    format!(
                        "reading T0 blob at commit {} path {}",
                        opts.t0, fact.source_path
                    )
                })?;
            t0_blobs.insert(fact.source_path.clone(), v.clone());
            v
        };
        let post_blob = opts.repo.blob_at(&commit_sha, &fact.source_path)?;

        let commit_files = if let Some(v) = commit_files_cache.get(&commit_sha) {
            v.clone()
        } else {
            let v = opts
                .repo
                .list_tree(&commit_sha)
                .with_context(|| format!("listing tree at commit {}", commit_sha))?;
            commit_files_cache.insert(commit_sha.clone(), v.clone());
            v
        };

        let diff = if let Some(v) = diff_cache.get(&commit_sha) {
            v.clone()
        } else {
            let v = load_commit_diff(opts.db, &commit_sha)
                .with_context(|| format!("loading diff_artifact for commit {}", commit_sha))?;
            diff_cache.insert(commit_sha.clone(), v.clone());
            v
        };

        let started = Instant::now();
        let ctx = RowCtx {
            fact,
            commit_sha: &commit_sha,
            diff: diff.as_ref(),
            post_blob: post_blob.as_deref(),
            t0_blob: t0_blob.as_deref(),
            post_tree: None,
            commit_files: &commit_files,
        };
        let (decision, rule_id, spec_ref, evidence) = chain.classify_first_match(&ctx);
        let wall_ms = started.elapsed().as_millis() as u64;

        let pred = decision.as_str().to_string();
        let request_id = format!(
            "phase1:{}:{}:{}",
            opts.rule_set_version, commit_sha, row_index
        );

        ins_pred.execute(params![
            row_index,
            &fact_id,
            &commit_sha,
            &batch_id,
            &ground_truth,
            &pred,
            &request_id,
            wall_ms as i64,
        ])?;
        ins_trace.execute(params![row_index, rule_id, spec_ref, "n/a", &evidence])?;

        let evidence_value: Option<serde_json::Value> = match serde_json::from_str(&evidence) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!(
                    "runner: failed to parse evidence JSON for row_index={row_index}: {e}; \
                     storing None in predictions.jsonl"
                );
                None
            }
        };

        let pr_row = PredictionRow {
            fact_id: fact_id.clone(),
            commit_sha: commit_sha.clone(),
            batch_id: batch_id.clone(),
            ground_truth: ground_truth.clone(),
            prediction: pred.clone(),
            request_id: request_id.clone(),
            wall_ms,
            evidence: evidence_value,
        };
        writeln!(predictions_f, "{}", serde_json::to_string(&pr_row)?)?;
        let trace_obj = serde_json::json!({
            "row_index": row_index,
            "rule_id": rule_id,
            "spec_ref": spec_ref,
            "evidence": serde_json::from_str::<serde_json::Value>(&evidence).unwrap_or(serde_json::Value::Null),
        });
        writeln!(traces_f, "{}", trace_obj)?;

        stats.processed += 1;
        match decision {
            Decision::Valid => stats.valid += 1,
            Decision::Stale => stats.stale += 1,
            Decision::NeedsRevalidation => stats.needs_reval += 1,
        }
    }
    Ok(stats)
}

/// Per-run decision tally emitted by `runner::run`.
///
/// `processed` is the total number of `eval_rows` scored; the remaining
/// counters partition that total by final `Decision`.
///
/// `needs_reval` is expected to be non-zero on v1.3+ runs: R4 routes
/// guard-below-floor rows to `NeedsRevalidation`, and earlier rules such as
/// R0/R6/R9 can still emit NR when their structural evidence is insufficient.
#[derive(Default, Debug)]
pub struct RunStats {
    pub processed: u64,
    pub valid: u64,
    pub stale: u64,
    pub needs_reval: u64,
}

/// Load the diff_artifact row for a single commit, if present.
fn load_commit_diff(db: &Connection, commit_sha: &str) -> Result<Option<CommitDiff>> {
    let row = db.query_row(
        "SELECT commit_sha, parent_sha, excluded_reason, unified_diff \
         FROM diff_artifacts WHERE commit_sha = ?1",
        params![commit_sha],
        |r| {
            Ok(CommitDiff {
                commit_sha: r.get(0)?,
                parent_sha: r.get(1)?,
                excluded_reason: r.get(2)?,
                unified_diff: r.get(3)?,
            })
        },
    );
    match row {
        Ok(cd) => Ok(Some(cd)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
