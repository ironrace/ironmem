//! Three-way scoring (SPEC §7.1) + LLM-validator agreement (SPEC §9.2).
//!
//! All point estimates over the sampled rows are reported alongside a
//! Wilson 95% lower-bound. The agreement section also emits a HT-weighted
//! overall agreement with standard error, a 3×3 confusion matrix, and a
//! Cohen κ with bootstrap 95% CI (seeded — reproducible).
//!
//! Population weights (one per coalesced ground-truth class) are accepted
//! as input so [`crate::report::score_llm_baseline_run`] can compose them from the labeler
//! corpus's known stratum totals; tests can inject simple uniform
//! weights.
//!
//! All `point` and `wilson_lower_95` figures are computed on the
//! sampled-row counts directly — population weights only adjust the
//! HT-weighted overall agreement in [`AgreementReport`]. This keeps the
//! per-stratum §7.1 numbers comparable to the labeler corpus's spotcheck
//! gate.

use crate::constants::DEFAULT_SEED;
use crate::predictions::PredictionRow;
use rand::seq::SliceRandom;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// z-score for a two-sided 95% Wilson interval.
const Z_95: f64 = 1.959964;
/// Bootstrap iterations for [`KappaReport`]. Fixed for SPEC reproducibility.
pub const KAPPA_BOOTSTRAP_ITERS: usize = 1000;

/// Coalesced class label.
pub const CLASS_VALID: &str = "valid";
pub const CLASS_STALE: &str = "stale";
pub const CLASS_NEEDS_REVAL: &str = "needs_revalidation";

/// Map a raw labeler tag (or already-coalesced model output) to the
/// 3-class scoring axis.
pub fn coalesce(label_tag: &str) -> &'static str {
    match label_tag {
        "Valid" | "valid" => CLASS_VALID,
        "StaleSourceChanged" | "StaleSourceDeleted" | "StaleSymbolRenamed" | "stale" => CLASS_STALE,
        "NeedsRevalidation" | "needs_revalidation" => CLASS_NEEDS_REVAL,
        // Defensive: the runner can emit "missing" if a batch dropped an
        // id. Unknown tags fall into the same bucket so they can't
        // masquerade as any real class.
        _ => "missing",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PointAndLower {
    pub point: f64,
    pub wilson_lower_95: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleDetectionMetric {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub wilson_lower_95: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreeWayReport {
    pub stale_detection: StaleDetectionMetric,
    pub valid_retention_accuracy: PointAndLower,
    pub needs_revalidation_routing_accuracy: PointAndLower,
}

#[derive(Debug, Clone, Serialize)]
pub struct PointAndSe {
    pub point: f64,
    pub ht_se: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct KappaReport {
    pub point_estimate: f64,
    pub ci_95_lower: f64,
    pub ci_95_upper: f64,
    pub n_bootstrap: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgreementReport {
    pub overall: PointAndSe,
    // BTreeMap (not HashMap) for the two `per_*` fields below: serde_json
    // serializes maps in their iteration order, and the byte-stable
    // canary test (`scoring/tests/byte_stable_canary.rs`) asserts the
    // tracked `metrics.json` round-trips byte-for-byte. HashMap iteration
    // order is unspecified — the canary currently passes only because
    // the runtime hash seed happens to land alphabetically; a stdlib
    // bump or rebuild on a different platform could silently flip it.
    // BTreeMap guarantees sorted-key emission and removes that risk.
    pub per_class: BTreeMap<String, f64>,
    pub confusion_matrix_3x3: [[u64; 3]; 3],
    pub cohen_kappa: KappaReport,
    pub per_stale_subtype: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyReport {
    pub p50_ms: u64,
    pub p95_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostReport {
    pub tokens: u64,
    pub usd: f64,
}

/// Wilson lower bound at α=0.05 for `k` successes out of `n` trials.
///
/// Returns 0.0 for `n == 0` to keep downstream JSON serialisable —
/// callers should check the sample size before quoting the bound.
pub fn wilson_lower_95(k: u64, n: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let n_f = n as f64;
    let p = k as f64 / n_f;
    let z2 = Z_95 * Z_95;
    let center = p + z2 / (2.0 * n_f);
    let margin = Z_95 * ((p * (1.0 - p) + z2 / (4.0 * n_f)) / n_f).sqrt();
    let denom = 1.0 + z2 / n_f;
    let lower = (center - margin) / denom;
    lower.max(0.0)
}

/// §7.1 convenience wrapper — runs [`three_way`] with empty population
/// weights (unweighted per-stratum accuracy as the SPEC §7.1 contract
/// calls for). Use this when you don't need HT-weighted agreement.
pub fn three_way_from_rows(rows: &[PredictionRow]) -> ThreeWayReport {
    three_way(rows, &HashMap::new())
}

/// §7.1 — stale-detection P/R/F1, valid-retention accuracy,
/// needs-revalidation routing accuracy.
///
/// `_population_weights` is reserved for future per-class HT-weighting of
/// the §7.1 metrics; v1 reports unweighted per-stratum accuracy as the
/// SPEC §7.1 contract calls for. The parameter is accepted now so the
/// signature is stable when weighted variants land.
pub fn three_way(
    predictions: &[PredictionRow],
    _population_weights: &HashMap<String, f64>,
) -> ThreeWayReport {
    let mut tp_stale: u64 = 0; // GT=stale, pred=stale
    let mut fn_stale: u64 = 0; // GT=stale, pred!=stale
    let mut fp_stale: u64 = 0; // GT!=stale, pred=stale

    let mut valid_total: u64 = 0;
    let mut valid_correct: u64 = 0;

    let mut nr_total: u64 = 0;
    let mut nr_correct: u64 = 0;

    for row in predictions {
        let gt = coalesce(&row.ground_truth);
        let pr = coalesce(&row.prediction);
        match gt {
            CLASS_STALE => {
                if pr == CLASS_STALE {
                    tp_stale += 1;
                } else {
                    fn_stale += 1;
                }
            }
            CLASS_VALID => {
                valid_total += 1;
                if pr == CLASS_VALID {
                    valid_correct += 1;
                }
                if pr == CLASS_STALE {
                    fp_stale += 1;
                }
            }
            CLASS_NEEDS_REVAL => {
                nr_total += 1;
                if pr == CLASS_NEEDS_REVAL {
                    nr_correct += 1;
                }
                if pr == CLASS_STALE {
                    fp_stale += 1;
                }
            }
            _ => {}
        }
    }

    let stale_pos = tp_stale + fn_stale; // GT=stale rows
    let pred_stale = tp_stale + fp_stale; // pred=stale rows

    let recall = if stale_pos > 0 {
        tp_stale as f64 / stale_pos as f64
    } else {
        0.0
    };
    let precision = if pred_stale > 0 {
        tp_stale as f64 / pred_stale as f64
    } else {
        0.0
    };
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    // Stale-detection Wilson lower bound is reported on recall — the
    // SPEC §7.1 acceptance gate is "stale recall ≥ X", so the lower
    // bound that matters is the one on recall.
    let stale_lower = wilson_lower_95(tp_stale, stale_pos);

    ThreeWayReport {
        stale_detection: StaleDetectionMetric {
            precision,
            recall,
            f1,
            wilson_lower_95: stale_lower,
        },
        valid_retention_accuracy: PointAndLower {
            point: if valid_total > 0 {
                valid_correct as f64 / valid_total as f64
            } else {
                0.0
            },
            wilson_lower_95: wilson_lower_95(valid_correct, valid_total),
        },
        needs_revalidation_routing_accuracy: PointAndLower {
            point: if nr_total > 0 {
                nr_correct as f64 / nr_total as f64
            } else {
                0.0
            },
            wilson_lower_95: wilson_lower_95(nr_correct, nr_total),
        },
    }
}

/// §9.2 — LLM-validator agreement.
///
/// Overall agreement is HT-weighted: per-class agreement is multiplied by
/// the input population weight (renormalised over classes present in the
/// sample). Standard error is a simple sample-based pooled estimate from
/// the per-class indicators — sufficient for the SPEC's order-of-magnitude
/// reporting and avoids importing a survey-stats crate.
pub fn llm_validator_agreement(
    predictions: &[PredictionRow],
    population_weights: &HashMap<String, f64>,
) -> AgreementReport {
    // 1. Per-class agreement counts.
    let classes = [CLASS_VALID, CLASS_STALE, CLASS_NEEDS_REVAL];
    let mut class_total: HashMap<&'static str, u64> = HashMap::new();
    let mut class_match: HashMap<&'static str, u64> = HashMap::new();
    for c in classes {
        class_total.insert(c, 0);
        class_match.insert(c, 0);
    }

    // 2. 3x3 confusion matrix (rows: GT, cols: pred).
    let mut confusion = [[0u64; 3]; 3];
    let idx_of = |label: &str| -> Option<usize> { classes.iter().position(|c| *c == label) };

    // 3. Stale subtype agreement (within `Stale*` raw GT tags).
    let mut subtype_total: HashMap<String, u64> = HashMap::new();
    let mut subtype_match: HashMap<String, u64> = HashMap::new();

    for row in predictions {
        let gt = coalesce(&row.ground_truth);
        let pr = coalesce(&row.prediction);
        if let (Some(gi), Some(pj)) = (idx_of(gt), idx_of(pr)) {
            confusion[gi][pj] += 1;
        }
        if let Some(t) = class_total.get_mut(gt) {
            *t += 1;
        }
        if gt == pr {
            if let Some(m) = class_match.get_mut(gt) {
                *m += 1;
            }
        }
        if row.ground_truth.starts_with("Stale") {
            // Strip "Stale" prefix and known suffixes to a short key:
            //   StaleSourceChanged → "changed"
            //   StaleSourceDeleted → "deleted"
            //   StaleSymbolRenamed → "renamed"
            let key = match row.ground_truth.as_str() {
                "StaleSourceChanged" => "changed",
                "StaleSourceDeleted" => "deleted",
                "StaleSymbolRenamed" => "renamed",
                _ => "other",
            }
            .to_string();
            *subtype_total.entry(key.clone()).or_insert(0) += 1;
            if pr == CLASS_STALE {
                *subtype_match.entry(key).or_insert(0) += 1;
            }
        }
    }

    let mut per_class: BTreeMap<String, f64> = BTreeMap::new();
    for c in classes {
        let t = *class_total.get(c).unwrap_or(&0);
        let m = *class_match.get(c).unwrap_or(&0);
        let agree = if t > 0 { m as f64 / t as f64 } else { 0.0 };
        per_class.insert(c.to_string(), agree);
    }

    // HT-weighted overall: renormalise weights over classes present.
    let present_weights: HashMap<&'static str, f64> = classes
        .iter()
        .copied()
        .filter(|c| class_total.get(*c).copied().unwrap_or(0) > 0)
        .map(|c| (c, population_weights.get(c).copied().unwrap_or(0.0)))
        .collect();
    let weight_sum: f64 = present_weights.values().sum();

    let (overall_point, overall_se) = if weight_sum > 0.0 {
        let mut acc = 0.0;
        let mut var_acc = 0.0;
        for (c, w) in &present_weights {
            let n = *class_total.get(*c).unwrap_or(&0) as f64;
            let p = *per_class.get(*c).unwrap_or(&0.0);
            let wn = w / weight_sum;
            acc += wn * p;
            // Sample variance of a Bernoulli p̂ is p(1-p)/n; the weighted
            // overall has Var = Σ wn² · p(1-p)/n  (independent strata).
            if n > 0.0 {
                var_acc += wn * wn * p * (1.0 - p) / n;
            }
        }
        (acc, var_acc.sqrt())
    } else {
        // Fall back to unweighted overall if weights are absent.
        let n_total: u64 = class_total.values().sum();
        let n_match: u64 = class_match.values().sum();
        let p = if n_total > 0 {
            n_match as f64 / n_total as f64
        } else {
            0.0
        };
        let se = if n_total > 0 {
            (p * (1.0 - p) / n_total as f64).sqrt()
        } else {
            0.0
        };
        (p, se)
    };

    // Per-stale subtype: agreement within each Stale* raw class.
    let mut per_stale_subtype: BTreeMap<String, f64> = BTreeMap::new();
    for (k, t) in &subtype_total {
        let m = subtype_match.get(k).copied().unwrap_or(0);
        per_stale_subtype.insert(k.clone(), if *t > 0 { m as f64 / *t as f64 } else { 0.0 });
    }

    let cohen_kappa = cohen_kappa_with_bootstrap(predictions);

    AgreementReport {
        overall: PointAndSe {
            point: overall_point,
            ht_se: overall_se,
        },
        per_class,
        confusion_matrix_3x3: confusion,
        cohen_kappa,
        per_stale_subtype,
    }
}

/// Compute Cohen κ on the coalesced 3-class labels, plus a seeded
/// bootstrap 95% CI.
fn cohen_kappa_with_bootstrap(predictions: &[PredictionRow]) -> KappaReport {
    let n = predictions.len();
    let point = compute_kappa(predictions);
    if n == 0 {
        return KappaReport {
            point_estimate: 0.0,
            ci_95_lower: 0.0,
            ci_95_upper: 0.0,
            n_bootstrap: KAPPA_BOOTSTRAP_ITERS,
        };
    }
    let mut rng = ChaCha20Rng::seed_from_u64(DEFAULT_SEED);
    let mut samples: Vec<f64> = Vec::with_capacity(KAPPA_BOOTSTRAP_ITERS);
    let idx: Vec<usize> = (0..n).collect();
    for _ in 0..KAPPA_BOOTSTRAP_ITERS {
        let resample: Vec<&PredictionRow> = (0..n)
            .map(|_| {
                let i = *idx.choose(&mut rng).expect("non-empty");
                &predictions[i]
            })
            .collect();
        let owned: Vec<PredictionRow> = resample.into_iter().cloned().collect();
        samples.push(compute_kappa(&owned));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = ((KAPPA_BOOTSTRAP_ITERS as f64) * 0.025).floor() as usize;
    let hi_idx = ((KAPPA_BOOTSTRAP_ITERS as f64) * 0.975).ceil() as usize - 1;
    let lo = samples[lo_idx.min(samples.len() - 1)];
    let hi = samples[hi_idx.min(samples.len() - 1)];
    KappaReport {
        point_estimate: point,
        ci_95_lower: lo,
        ci_95_upper: hi,
        n_bootstrap: KAPPA_BOOTSTRAP_ITERS,
    }
}

fn compute_kappa(predictions: &[PredictionRow]) -> f64 {
    let n = predictions.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let classes = [CLASS_VALID, CLASS_STALE, CLASS_NEEDS_REVAL];
    let mut row_totals = [0.0f64; 3];
    let mut col_totals = [0.0f64; 3];
    let mut diag = 0.0f64;
    for row in predictions {
        let gt = coalesce(&row.ground_truth);
        let pr = coalesce(&row.prediction);
        let gi = classes.iter().position(|c| *c == gt);
        let pj = classes.iter().position(|c| *c == pr);
        if let (Some(gi), Some(pj)) = (gi, pj) {
            row_totals[gi] += 1.0;
            col_totals[pj] += 1.0;
            if gi == pj {
                diag += 1.0;
            }
        }
    }
    let p_o = diag / n;
    let p_e: f64 = (0..3)
        .map(|k| (row_totals[k] / n) * (col_totals[k] / n))
        .sum();
    if (1.0 - p_e).abs() < 1e-12 {
        0.0
    } else {
        (p_o - p_e) / (1.0 - p_e)
    }
}

/// p50 / p95 of per-commit wall_ms. Each `(batch_id, commit_sha,
/// wall_ms)` is counted once — multiple rows in one batch share a single
/// wall_ms record.
pub fn latency(predictions: &[PredictionRow]) -> LatencyReport {
    // Dedupe by batch_id; map batch_id -> (commit_sha, wall_ms).
    let mut per_batch: HashMap<String, (String, u64)> = HashMap::new();
    for row in predictions {
        per_batch
            .entry(row.batch_id.clone())
            .or_insert_with(|| (row.commit_sha.clone(), row.wall_ms));
    }
    // Sum batch wall_ms per commit_sha.
    let mut per_commit: HashMap<String, u64> = HashMap::new();
    for (_b, (commit, wall_ms)) in per_batch {
        *per_commit.entry(commit).or_insert(0) += wall_ms;
    }
    let mut values: Vec<u64> = per_commit.into_values().collect();
    values.sort_unstable();
    LatencyReport {
        p50_ms: percentile_u64(&values, 0.50),
        p95_ms: percentile_u64(&values, 0.95),
    }
}

fn percentile_u64(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank method (`q*N`, ceil-indexed, 1-based) — stable for
    // small N and matches our hand-computed test expectations.
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Cost per correct invalidation.
///
/// Per-row token usage is not currently persisted in `PredictionRow`
/// (Phase 0c stored aggregate usage in `run_meta.json`). The caller is
/// expected to supply `total_cost_usd` from `run_meta.json` via
/// [`cost_per_correct_invalidation_from_total`]; this convenience form
/// returns zeros so the function is still safe to call on synthetic
/// fixtures that lack a `run_meta.json`.
pub fn cost_per_correct_invalidation(predictions: &[PredictionRow]) -> CostReport {
    cost_per_correct_invalidation_from_total(predictions, 0.0)
}

/// Same as [`cost_per_correct_invalidation`] but accepts the aggregate
/// `total_cost_usd` from `run_meta.json`. Token total stays 0 until per-row
/// usage is persisted (tracked for a follow-up task).
pub fn cost_per_correct_invalidation_from_total(
    predictions: &[PredictionRow],
    total_cost_usd: f64,
) -> CostReport {
    cost_per_correct_invalidation_from_totals(predictions, 0, total_cost_usd)
}

/// Same as [`cost_per_correct_invalidation_from_total`] but accepts both
/// aggregate token and USD totals from `run_meta.json`.
pub fn cost_per_correct_invalidation_from_totals(
    predictions: &[PredictionRow],
    total_tokens: u64,
    total_cost_usd: f64,
) -> CostReport {
    let tp_stale: u64 = predictions
        .iter()
        .filter(|r| {
            coalesce(&r.ground_truth) == CLASS_STALE && coalesce(&r.prediction) == CLASS_STALE
        })
        .count() as u64;
    if tp_stale == 0 {
        return CostReport {
            tokens: 0,
            usd: 0.0,
        };
    }
    CostReport {
        tokens: total_tokens / tp_stale,
        usd: total_cost_usd / tp_stale as f64,
    }
}
