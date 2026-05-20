#!/usr/bin/env python3
"""Build a synthetic baseline predictions.jsonl from the full Plan A.2 corpus.

This is informational scaffolding so phase1's --baseline-run loader can
pin the eval-row-set to all 910,530 corpus rows. The prediction column is
a placeholder (all "valid") and any LLM-baseline column derived from it
must be ignored — phase1 emits its own predictions independently.

Usage:
    python3 build_synthetic_baseline.py <corpus.jsonl> <out_dir>
"""
import json
import sys
from pathlib import Path


def canonicalize(label):
    """Map corpus label (str or object) to phase1-accepted GT tag."""
    if isinstance(label, str):
        return label  # "Valid" / "NeedsRevalidation"
    # Object form: {"StaleSourceChanged": {...}} etc.
    return next(iter(label.keys()))


def main():
    corpus_path = Path(sys.argv[1])
    out_dir = Path(sys.argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)
    preds = out_dir / "predictions.jsonl"

    counts = {}
    n_total = 0
    with corpus_path.open() as fin, preds.open("w") as fout:
        for line in fin:
            row = json.loads(line)
            gt = canonicalize(row["label"])
            counts[gt] = counts.get(gt, 0) + 1
            n_total += 1
            commit = row["commit_sha"]
            out = {
                "fact_id": row["fact_id"],
                "commit_sha": commit,
                "batch_id": f"{commit}-0",
                "ground_truth": gt,
                "prediction": "valid",
                "request_id": "synthetic-fullcorpus",
                "wall_ms": 0,
            }
            fout.write(json.dumps(out, separators=(",", ":")) + "\n")

    # Minimal accompanying files so the dir mirrors v1.2b layout.
    # Bytes are non-load-bearing for phase1 (only predictions.jsonl is read).
    (out_dir / "manifest.json").write_text(json.dumps({
        "note": "synthetic placeholder; phase1 does not consume this file",
        "rows_total": n_total,
    }, indent=2) + "\n")
    (out_dir / "metrics.json").write_text(json.dumps({
        "note": "synthetic placeholder; do NOT cite as LLM metrics",
    }, indent=2) + "\n")
    (out_dir / "run_meta.json").write_text(json.dumps({
        "round": "v1.2c-flask-heldout-plan-a2-fullcorpus-sidecar",
        "kind": "synthetic-baseline",
        "note": ("Synthetic full-corpus baseline. NOT a real LLM run. "
                 "prediction='valid' is a placeholder; do NOT cite "
                 "any LLM-baseline metric derived from this file. "
                 "Exists solely to pin phase1's --baseline-run loader "
                 "to the full 910,530-row corpus for a sidecar "
                 "evaluation against phase1's own structural rule chain."),
        "corpus_path": str(corpus_path),
        "rows_total": n_total,
        "label_distribution": counts,
        "request_id_marker": "synthetic-fullcorpus",
    }, indent=2) + "\n")

    print(f"Wrote {n_total} rows -> {preds}", file=sys.stderr)
    print(f"Label distribution: {counts}", file=sys.stderr)


if __name__ == "__main__":
    main()
