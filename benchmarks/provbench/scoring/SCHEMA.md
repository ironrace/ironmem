# ProvBench Scoring — Artifact Schema Reference

**Last updated:** 2026-05-18
**Covers:** `provbench-scoring` v1.2c+ (SPEC §11 row 2026-05-18; `wall_us` field added)
**Source files:** `scoring/src/predictions.rs`, `scoring/src/compare.rs`,
  `scoring/src/report.rs`, `scoring/src/metrics.rs`, `phase1/src/runner.rs`

---

## 1. `predictions.jsonl` row schema

One JSON object per line. Field order is fixed by serde derive order; rows
are never rewritten on resume, so existing lines remain byte-identical across
incremental runs.

```json
{
  "fact_id":      "...",
  "commit_sha":   "...",
  "batch_id":     "...",
  "ground_truth": "Valid|StaleSourceChanged|StaleSourceDeleted|StaleSymbolRenamed|NeedsRevalidation",
  "prediction":   "valid|stale|needs_revalidation",
  "request_id":   "req_...|phase1:<version>:<commit_sha>:<row_index>",
  "wall_ms":      0,
  "evidence":     { "rule": "R4", "guard_below_floor": false },
  "row_index":    0
}
```

| Field | Type | Presence | Description |
|---|---|---|---|
| `fact_id` | string | required | Stable identifier for the doc-fact being evaluated |
| `commit_sha` | string | required | The post-T₀ commit against which the fact is scored |
| `batch_id` | string | required | Groups rows that shared a single LLM API call; Phase 1 uses one batch per row |
| `ground_truth` | string | required | Raw labeler tag — one of the six canonical tags or a lowercase alias |
| `prediction` | string | required | Coalesced model/rule output: `valid`, `stale`, or `needs_revalidation` |
| `request_id` | string | required | Opaque audit token. Baseline: Anthropic request id (`req_…`). Phase 1: `phase1:<rule_set_version>:<commit_sha>:<row_index>` |
| `wall_ms` | u64 | required | Wall time in ms. Baseline: per-batch API round-trip. Phase 1: per-row rule classification cost |
| `wall_us` | u64\|null | optional — v1.2c+ added (`#[serde(default, skip_serializing_if = "Option::is_none")]`) | Microsecond-resolution rule-chain latency for a single row. Added to give meaningful latency reporting on sub-millisecond Python rule-chain work: flask predictions had `wall_ms: 0` across the entire subset because the structural rule chain runs in 100–900 μs per row, which rounds to 0 at millisecond granularity. `wall_ms` retains its SPEC §8 #4 contract; `wall_us` is purely additive precision. `None` on legacy artifacts and baseline LLM-runner output |
| `evidence` | object\|null | optional — v1.2c added (`#[serde(default, skip_serializing_if = "Option::is_none")]`) | Rule-specific evidence blob. R4 emits `{"rule":"R4","guard_below_floor":<bool>,...}`. Absent on baseline rows and pre-v1.2c Phase 1 artifacts |
| `row_index` | u64\|null | optional — v1.2c added (`#[serde(default, skip_serializing_if = "Option::is_none")]`) | 0-based SQLite row counter from the runner. Matches `rule_traces.jsonl` `row_index`. Absent on baseline rows and legacy Phase 1 artifacts. The scorer uses it for the `score_candidate_nr_aware` join when present, and falls back to the enumeration counter on legacy artifacts |

### Ground-truth coalescing

The scorer (`metrics::coalesce`) maps raw tags to the 3-class scoring axis:

| Raw tag | Coalesced class |
|---|---|
| `Valid`, `valid` | `valid` |
| `StaleSourceChanged`, `StaleSourceDeleted`, `StaleSymbolRenamed`, `stale_source_changed`, `stale_source_deleted`, `stale_symbol_renamed`, `stale` | `stale` |
| `NeedsRevalidation`, `needs_revalidation` | `needs_revalidation` |
| anything else | `missing` (treated as unclassified) |

---

## 2. `metrics.json` — single-run shape (baseline scorer)

Written atomically by `score_llm_baseline_run` in `scoring/src/report.rs`.
This is the LLM-baseline reference file loaded verbatim into the `llm_baseline`
column of the compare output.

```json
{
  "spec_freeze_hash":   "683d023...",
  "labeler_git_sha":    "c2d3b7b0...",
  "model_id":           "claude-sonnet-4-6",
  "model_snapshot_date": "2026-05-09",
  "sample_seed":        "0xc0debabedeadbeef",
  "coverage":           "subset",
  "per_stratum_sizes":  { "Valid": 2000, "StaleSourceChanged": 1843, ... },
  "population_weights": { "valid": 0.83, "stale": 0.16, "needs_revalidation": 0.01 },
  "section_7_1": { ... },
  "section_7_2_applicable": { ... },
  "llm_validator_agreement": { ... }
}
```

| Key | Type | Description |
|---|---|---|
| `spec_freeze_hash` | string | SHA-256 of the frozen SPEC.md body (§15) |
| `labeler_git_sha` | string | Git SHA of the labeler used to build the corpus |
| `model_id` | string | Model identifier constant from `scoring/src/constants.rs` |
| `model_snapshot_date` | string | Snapshot date of the frozen baseline model (SPEC §6.2) |
| `sample_seed` | string | Hex-formatted random seed used by the sampler |
| `coverage` | string | `"subset"` or `"full"` (§9.2 coverage gate) |
| `per_stratum_sizes` | object | Raw labeler tag → row count in the sample |
| `population_weights` | object | Horvitz-Thompson renormalization weights for §9.2 overall agreement |
| `section_7_1` | object | §7.1 three-way metrics — see below |
| `section_7_2_applicable` | object | §7.2 latency and cost — see below |
| `llm_validator_agreement` | object | §9.2 agreement block — see below |

### `section_7_1` (§7.1 three-way reporting)

```json
"section_7_1": {
  "stale_detection": {
    "precision": 0.0,
    "recall":    0.0039,
    "f1":        0.0078,
    "wilson_lower_95": 0.0024
  },
  "valid_retention_accuracy": {
    "point":          0.999,
    "wilson_lower_95": 0.997
  },
  "needs_revalidation_routing_accuracy": {
    "point":          0.002,
    "wilson_lower_95": 0.0006
  }
}
```

`wilson_lower_95` on `stale_detection` is computed on recall (tp / (tp + fn)).
`precision`, `f1` do not carry Wilson bounds.

### `section_7_2_applicable` (§7.2 latency and cost)

```json
"section_7_2_applicable": {
  "latency_p50_ms": 7267,
  "latency_p95_ms": 20835,
  "cost_per_correct_invalidation": {
    "tokens": 802581,
    "usd":    2.59
  }
}
```

Baseline latency is per-commit median (batched API round-trips); Phase 1
candidate latency is per-row median (rule classification). These are in
different units — see "Latency methodology" note in `compare.rs`.

### `llm_validator_agreement` (§9.2 agreement)

```json
"llm_validator_agreement": {
  "overall": { "point": 0.44, "ht_se": 0.012 },
  "per_class": { "valid": 0.999, "stale": 0.004, "needs_revalidation": 0.002 },
  "confusion_matrix_3x3": [[...], [...], [...]],
  "cohen_kappa": {
    "point_estimate": -0.001,
    "ci_95_lower":    -0.003,
    "ci_95_upper":    0.002,
    "n_bootstrap":    1000
  },
  "per_stale_subtype": { "changed": 0.004, "deleted": 0.003, "renamed": 0.0 }
}
```

`confusion_matrix_3x3` rows are ground truth, columns are prediction, ordered
`[valid, stale, needs_revalidation]`. `cohen_kappa` uses seeded ChaCha20
bootstrap (1000 iterations, seed from `scoring/src/constants.rs::DEFAULT_SEED`).

---

## 3. `metrics.json` — compare output shape

Written by `compare::run` in `scoring/src/compare.rs`. This is the side-by-side
document produced by `provbench-score compare`. It contains both columns plus
deltas, thresholds, and per-rule confusion.

```json
{
  "llm_baseline":           { ... },
  "<candidate_name>":       { ... },
  "phase1_rules_nr_aware":  { ... },
  "deltas":                 { ... },
  "thresholds":             { ... },
  "per_rule_confusion":     { ... }
}
```

| Key | Type | Description |
|---|---|---|
| `llm_baseline` | object | Verbatim contents of the baseline run's `metrics.json` (see §2 above) |
| `<candidate_name>` | object | §7.1 and §7.2 metrics scored from `predictions.jsonl` of the candidate run (see `section_7_1` and `section_7_2_applicable` shapes above) |
| `phase1_rules_nr_aware` | object | Post-hoc NR-aware column — added by SPEC §11 row 2026-05-18 (v1.2c change e) — see below |
| `deltas` | object | Point deltas between candidate and baseline columns — see below |
| `thresholds` | object | SPEC §8 structured threshold-status objects — see below. Shape changed from bare bool to structured object in v1.2c (SPEC §11 row 2026-05-18, change f) |
| `per_rule_confusion` | object | Per-rule confusion matrix joined from `rule_traces.jsonl` — see below |

### `phase1_rules_nr_aware` (v1.2c, SPEC §11 row 2026-05-18)

Post-hoc scoring column that virtually remaps R4 guard-below-floor rows from
`stale` to `needs_revalidation` and re-runs §7.1 math. Reveals whether the
rule chain has a latent NR signal even when the `Decision` API collapses it to
`stale` in earlier rule-set versions. Added by v1.2c change (e).

```json
"phase1_rules_nr_aware": {
  "section_7_1": {
    "stale_detection": {
      "precision": 0.0,
      "recall":    0.0,
      "f1":        0.0,
      "wilson_lower_95": 0.0
    },
    "valid_retention_accuracy": {
      "point":          0.999,
      "wilson_lower_95": 0.997
    },
    "needs_revalidation_routing_accuracy": {
      "point":          0.12,
      "wilson_lower_95": 0.10
    }
  }
}
```

The `applicable` / `rows_remapped` sentinel is the count of rows where the
remap fired (`rule == "R4"`, `prediction == "stale"`, `guard_below_floor ==
true`). Evidence is read from `predictions.jsonl` `evidence` field when
present; falls back to `rule_traces.jsonl` for legacy artifacts where
`evidence` was not yet persisted in the prediction row.

### `deltas`

```json
"deltas": {
  "stale_recall_point_delta":                                    0.957,
  "stale_precision_point_delta":                                 0.787,
  "valid_retention_wilson_lower_95_delta":                       0.025,
  "needs_revalidation_routing_wilson_lower_95_delta":            0.0,
  "latency_p50_ratio_baseline_per_commit_to_candidate_per_row": 3633.5,
  "cost_per_correct_invalidation_usd_delta":                     -2.59,
  "cost_per_correct_invalidation_tokens_delta":                  -802581.0
}
```

`latency_p50_ratio_baseline_per_commit_to_candidate_per_row` is
intentionally verbose: the numerator (baseline) is a per-commit median and the
denominator (candidate) is a per-row median. It is a useful headline but not a
direct apples-to-apples throughput comparison.

### `thresholds` — v1.2c structured shape (SPEC §11 row 2026-05-18)

**Breaking change from pre-v1.2c artifacts:** before v1.2c, each threshold key
held a bare boolean. From v1.2c onward (change f) each key holds a structured
object. Pre-v1.2c artifacts cannot be parsed with the new shape — see the
footnote in each pre-v1.2c findings document.

```json
"thresholds": {
  "section_8_3_valid_retention_ge_0_95": {
    "status":   "PASS",
    "passed":   true,
    "metric":   "valid_retention_wlb",
    "observed": 0.9981,
    "target":   0.95
  },
  "section_8_4_latency_p50_le_727_ms": {
    "status":   "PASS",
    "passed":   true,
    "metric":   "latency_p50_ms",
    "observed": 2.0,
    "target":   727.0
  },
  "section_8_5_stale_recall_wlb_ge_0_30": {
    "status":   "SKIP",
    "passed":   null,
    "metric":   "stale_recall_wlb",
    "observed": null,
    "target":   0.30,
    "reason":   "ground_truth_stale_count_is_zero"
  }
}
```

Threshold object fields:

| Field | Type | Description |
|---|---|---|
| `status` | `"PASS"` \| `"FAIL"` \| `"SKIP"` | Verdict string |
| `passed` | bool \| null | `true`/`false` for PASS/FAIL; `null` for SKIP |
| `metric` | string | Name of the metric being tested |
| `observed` | number \| null | Observed value; `null` when SKIP applies |
| `target` | number | Required threshold value |
| `reason` | string | Present only when `status == "SKIP"` — currently only `"ground_truth_stale_count_is_zero"` (§8 #5 on a corpus with no Stale_* ground-truth rows, per v1.2b flask held-out findings) |

SKIP semantics for §8 #5: when `gt_stale_count == 0`, stale recall is
structurally undefined (0/0 denominator). The threshold records SKIP rather
than FAIL to distinguish this structural condition from a genuine recall failure.
See flask held-out findings (2026-05-15) for the full narrative.

### `per_rule_confusion`

```json
"per_rule_confusion": {
  "R1": { "stalesourcedeleted__stale": 783 },
  "R2": { "valid__valid": 240 },
  "R4": {
    "valid__valid":                  624,
    "stalesourcechanged__stale":     842,
    "valid__stale":                  17,
    "needsrevalidation__valid":      304
  }
}
```

Keys are `rule_id` strings as emitted by the rule chain. Each value is a map of
`"<ground_truth_coalesced>__<prediction_coalesced>"` → count. Ground-truth and
prediction labels are lowercased. Source: `rule_traces.jsonl` joined with
`predictions.jsonl` by `row_index`.

---

## 4. `rule_traces.jsonl` row schema

One JSON object per line, written by the Phase 1 runner (`phase1/src/runner.rs`).
Each row records which rule fired first for the corresponding `predictions.jsonl`
row.

```json
{
  "row_index": 0,
  "rule_id":   "R4",
  "spec_ref":  "§7.1",
  "evidence":  { "rule": "R4", "guard_below_floor": false, "probe_has_leaf": true }
}
```

| Field | Type | Description |
|---|---|---|
| `row_index` | i64 | 0-based row counter matching `predictions.jsonl` `row_index` and the SQLite `rule_traces.row_index` column |
| `rule_id` | string | Short rule identifier (`"R1"` through `"R7"`, or `"?"` if unknown) |
| `spec_ref` | string | SPEC section reference emitted by the rule (e.g., `"§7.1"`) |
| `evidence` | object \| null | Rule-specific evidence blob. Same structure as `predictions.jsonl` `evidence`. For R4: `{"rule":"R4","guard_below_floor":<bool>,...}`. `null` if the runner failed to parse the evidence JSON for that row |

The scorer's `load_per_rule_confusion` and `load_rule_trace_evidence` functions
read `rule_traces.jsonl` joining on `row_index`. The `nr_aware` post-hoc column
(`score_candidate_nr_aware`) uses the trace evidence as a fallback when the
prediction row's own `evidence` field is absent (legacy artifacts).

---

## 5. Version history

| Version | Date | Schema changes |
|---|---|---|
| v1.0 | 2026-05-14 | Initial Phase 1 artifacts. `thresholds.*` are bare booleans. `predictions.jsonl` has no `evidence` or `row_index` |
| v1.1 | 2026-05-15 | No schema changes vs v1.0. `thresholds.*` still bare booleans |
| v1.2a | 2026-05-15 | No schema changes. `thresholds.*` still bare booleans |
| v1.2b | 2026-05-15 | No schema changes. `thresholds.*` still bare booleans |
| v1.2c | 2026-05-18 | `thresholds.*` → structured object `{status, passed, metric, observed, target, reason?}`. `predictions.jsonl` gains optional `evidence` and `row_index` fields. `compare` output gains `phase1_rules_nr_aware` column. §8 #5 gains SKIP semantics when `gt_stale_count == 0` |
| v1.2c+ | 2026-05-18 | `predictions.jsonl` gains optional `wall_us` field (microsecond-resolution per-row rule-chain latency). Closes H1 carry-forward from flask findings: flask `wall_ms: 0` was millisecond rounding of 100–900 μs rule-chain work, not a missing write. `wall_ms` contract unchanged. Legacy artifacts (no `wall_us` key) round-trip cleanly as `None` |
