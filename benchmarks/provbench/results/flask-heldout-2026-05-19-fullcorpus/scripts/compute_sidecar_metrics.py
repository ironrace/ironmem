#!/usr/bin/env python3
"""Compute the sidecar 3x3 + per-class + per-rule metrics for the full
Plan A.2 flask corpus against phase1 v1.3 predictions.

NOT a SPEC §8 verdict. Informational only.

Usage:
    python3 compute_sidecar_metrics.py <fullcorpus_dir>
"""
import json
import math
import subprocess
import sys
from collections import defaultdict
from pathlib import Path


def coalesce(label):
    """Mirror benchmarks/provbench/scoring/src/metrics.rs `coalesce`."""
    if label in ("Valid", "valid"):
        return "valid"
    if label in ("StaleSourceChanged", "StaleSourceDeleted",
                 "StaleSymbolRenamed", "stale_source_changed",
                 "stale_source_deleted", "stale_symbol_renamed", "stale",
                 "Stale"):
        return "stale"
    if label in ("NeedsRevalidation", "needs_revalidation"):
        return "needs_revalidation"
    return "missing"


def wilson_lower_95(successes, n):
    """Mirror scoring metrics.rs `wilson_lower_95`. Returns None for n=0."""
    if n == 0:
        return None
    z = 1.959963984540054  # z_{0.975}
    p = successes / n
    denom = 1.0 + z * z / n
    center = (p + z * z / (2.0 * n)) / denom
    radius = (z / denom) * math.sqrt(p * (1.0 - p) / n + z * z / (4.0 * n * n))
    return max(0.0, center - radius)


def git_sha():
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True
        ).strip()
    except Exception:
        return "unknown"


GT_KEYS = ["Valid", "Stale_*", "NeedsRevalidation"]
PRED_KEYS = ["valid", "stale", "needs_revalidation"]


def main():
    root = Path(sys.argv[1])
    corpus_path = Path("benchmarks/provbench/corpus/flask-2f0c62f5-bf56f40.jsonl")
    preds_path = root / "phase1" / "predictions.jsonl"

    # Single-pass over predictions.jsonl (910,530 rows).
    # 3x3 confusion (Valid / Stale_* / NeedsRevalidation) x (valid / stale / needs_revalidation)
    cm_3x3 = {gt: {pr: 0 for pr in PRED_KEYS} for gt in GT_KEYS}

    valid_total = stale_total = nr_total = 0
    valid_correct = stale_correct = nr_correct = 0

    stale_subtype_total = defaultdict(int)
    stale_subtype_correct = defaultdict(int)

    pred_stale_total = 0
    pred_stale_tp = 0

    # Per-rule confusion: rule_id -> {"<GT>__<pred>": count}
    per_rule_cells = defaultdict(lambda: defaultdict(int))
    per_rule_totals = defaultdict(int)

    runner_processed = 0
    runner_valid = 0
    runner_stale = 0
    runner_needs_reval = 0

    with preds_path.open() as f:
        for line in f:
            r = json.loads(line)
            raw_gt = r["ground_truth"]
            pr = coalesce(r["prediction"])

            # Coarse GT bucket for 3x3
            if raw_gt == "Valid":
                gt_bucket = "Valid"
                valid_total += 1
                if pr == "valid":
                    valid_correct += 1
            elif raw_gt == "NeedsRevalidation":
                gt_bucket = "NeedsRevalidation"
                nr_total += 1
                if pr == "needs_revalidation":
                    nr_correct += 1
            elif raw_gt.startswith("Stale"):
                gt_bucket = "Stale_*"
                stale_total += 1
                stale_subtype_total[raw_gt] += 1
                if pr == "stale":
                    stale_correct += 1
                    stale_subtype_correct[raw_gt] += 1
            else:
                gt_bucket = None

            if gt_bucket and pr in cm_3x3[gt_bucket]:
                cm_3x3[gt_bucket][pr] += 1

            if pr == "stale":
                pred_stale_total += 1
                if gt_bucket == "Stale_*":
                    pred_stale_tp += 1

            runner_processed += 1
            if pr == "valid":
                runner_valid += 1
            elif pr == "stale":
                runner_stale += 1
            elif pr == "needs_revalidation":
                runner_needs_reval += 1

            # Per-rule
            rule = r.get("evidence", {}).get("rule")
            if rule:
                gt_bk = gt_bucket if gt_bucket else "Other"
                per_rule_cells[rule][f"{gt_bk}__{pr}"] += 1
                per_rule_totals[rule] += 1

    n_total = runner_processed

    stale_recall = stale_correct / stale_total if stale_total else 0.0
    stale_precision = (pred_stale_tp / pred_stale_total
                       if pred_stale_total else 0.0)
    stale_f1 = (2 * stale_precision * stale_recall
                / (stale_precision + stale_recall)
                if (stale_precision + stale_recall) > 0 else 0.0)

    subtype_key_map = {
        "StaleSourceChanged": "StaleSourceChanged",
        "StaleSourceDeleted": "StaleSourceDeleted",
        "StaleSymbolRenamed": "StaleSymbolRenamed",
    }
    per_subtype = {}
    for raw_sub, out_key in subtype_key_map.items():
        tot = stale_subtype_total.get(raw_sub, 0)
        cor = stale_subtype_correct.get(raw_sub, 0)
        per_subtype[out_key] = {
            "point": (cor / tot) if tot else None,
            "wilson_lower_95": wilson_lower_95(cor, tot),
            "n": tot,
        }

    per_rule_out = {}
    for rid in sorted(per_rule_cells.keys(),
                      key=lambda k: -per_rule_totals[k]):
        per_rule_out[rid] = dict(per_rule_cells[rid])
        per_rule_out[rid]["_total"] = per_rule_totals[rid]

    out = {
        "corpus_path": str(corpus_path),
        "labeler_git_sha": "bf56f40999a5b3f026db517b196fa9d3a5724ded",
        "phase1_git_sha": "bf56f40999a5b3f026db517b196fa9d3a5724ded",
        "scoring_git_sha": "bf56f40999a5b3f026db517b196fa9d3a5724ded",
        "rule_set_version": "v1.3",
        "run_date": "2026-05-19",
        "n_total": n_total,
        "phase1_runner_stats": {
            "processed": runner_processed,
            "valid": runner_valid,
            "stale": runner_stale,
            "needs_reval": runner_needs_reval,
            "wall_seconds": 840,
        },
        "confusion_matrix_3x3": cm_3x3,
        "per_class_accuracy": {
            "valid_retention": {
                "point": (valid_correct / valid_total) if valid_total else None,
                "wilson_lower_95": wilson_lower_95(valid_correct, valid_total),
                "n": valid_total,
            },
            "stale_detection_recall": {
                "point": stale_recall,
                "wilson_lower_95": wilson_lower_95(stale_correct, stale_total),
                "n": stale_total,
            },
            "stale_detection_precision": {
                "point": stale_precision,
                "wilson_lower_95": None,
                "n_predicted_stale": pred_stale_total,
            },
            "stale_detection_f1": stale_f1,
            "needs_revalidation_routing_accuracy": {
                "point": (nr_correct / nr_total) if nr_total else None,
                "wilson_lower_95": wilson_lower_95(nr_correct, nr_total),
                "n": nr_total,
            },
        },
        "per_stale_subtype_recall": per_subtype,
        "per_rule_confusion": per_rule_out,
        "notes": [
            "Sidecar evaluation against the full Plan A.2 flask corpus "
            "(910,530 rows) — NOT a SPEC §8 verdict.",
            "Baseline at ../baseline/ is synthetic "
            "(request_id='synthetic-fullcorpus'); "
            "LLM-baseline columns must be ignored.",
            "Frozen baseline subset (4k rows; v1.2b reuse) used for §8 "
            "verdict in flask-heldout-2026-05-19-canary/ has zero Stale_* "
            "GT, so §8 #5 is SKIP there. This sidecar provides the "
            "missing evidence on phase1's behavior against real Stale_* rows.",
            "GT canonicalization: raw labels (Valid, StaleSourceChanged, "
            "StaleSourceDeleted, StaleSymbolRenamed, NeedsRevalidation) "
            "used verbatim — phase1's scoring/coalesce already maps each "
            "Stale_* subtype into the 'stale' axis.",
        ],
    }

    out_path = root / "sidecar_metrics.json"
    out_path.write_text(json.dumps(out, indent=2) + "\n")
    print(f"Wrote {out_path}", file=sys.stderr)
    print(json.dumps({
        "n_total": n_total,
        "confusion_matrix_3x3": cm_3x3,
        "per_class_accuracy": out["per_class_accuracy"],
        "per_stale_subtype_recall": per_subtype,
        "per_rule_totals": {k: per_rule_totals[k] for k in
                            sorted(per_rule_totals, key=lambda x: -per_rule_totals[x])},
    }, indent=2))


if __name__ == "__main__":
    main()
