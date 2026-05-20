# flask-heldout-2026-05-19-fullcorpus

Sidecar evaluation of the Plan A.2 flask **full corpus** (910,530 rows) against
phase1's v1.3 structural rule chain. Held-out repo: `pallets/flask` first-parent
T₀..HEAD = `2f0c62f5..9fcd34c9` (401 commits).

**This is NOT a SPEC §8 verdict.** The §8 verdict for v1.2c/v1.3 flask is
recorded in the sibling `flask-heldout-2026-05-19-canary/` directory against the
frozen 4,000-row v1.2b subset (subset has zero Stale_* GT, so §8 #5 is SKIP
there). This sidecar exists to provide the missing evidence on phase1's
behavior against the real ~153k Stale_* rows present in the full corpus.

## Layout

```
.
├── README.md                  (this file)
├── sidecar_metrics.json       3×3 confusion, per-class WLBs, per-rule cells
├── baseline/
│   ├── manifest.json          (gitignored) synthetic placeholder
│   ├── metrics.json           (gitignored) synthetic placeholder
│   ├── predictions.jsonl      (gitignored, 274MB) synthetic, prediction='valid'
│   └── run_meta.json          synthetic-baseline marker — DO NOT cite as LLM
├── phase1/
│   ├── phase1.sqlite          (gitignored, 672MB) phase1 working store
│   ├── predictions.jsonl      (gitignored, 447MB) 910,530 rows
│   ├── rule_traces.jsonl      (gitignored, 163MB) 910,530 rows
│   └── run_meta.json          phase1 round metadata
└── scripts/
    ├── build_synthetic_baseline.py   builds baseline/predictions.jsonl
    └── compute_sidecar_metrics.py    builds sidecar_metrics.json
```

## Synthetic baseline caveat

`baseline/predictions.jsonl` is **synthetic** — every row has
`prediction="valid"` and `request_id="synthetic-fullcorpus"`. It exists only
to pin phase1's `--baseline-run` loader to the 910,530-row corpus. Any metric
in `manifest.json` / `metrics.json` derived from this file is meaningless and
must be ignored. `ground_truth` in the synthetic predictions equals the
canonicalized corpus label, which is what we score phase1 against.

## Reproducibility

All artifacts regenerate from the labeler at `bf56f40` over the ripgrep'd
flask clone:

```
# 1. Build labeler/phase1 binaries
cargo build --release --workspace

# 2. Regenerate corpus (see SPEC §4 + Plan A.2)
benchmarks/provbench/target/release/labeler emit-corpus \
    --repo <path-to-flask> --t0 2f0c62f5 --head 9fcd34c9 \
    --out benchmarks/provbench/corpus/flask-2f0c62f5-bf56f40.jsonl

# 3. Build synthetic baseline
python3 scripts/build_synthetic_baseline.py \
    benchmarks/provbench/corpus/flask-2f0c62f5-bf56f40.jsonl \
    benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/baseline

# 4. Run phase1 over the full corpus (~14 minutes)
benchmarks/provbench/target/release/phase1 \
    --corpus benchmarks/provbench/corpus/flask-2f0c62f5-bf56f40.jsonl \
    --baseline-run benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/baseline \
    --out benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/phase1

# 5. Compute sidecar metrics
python3 scripts/compute_sidecar_metrics.py \
    benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus
```

Wall time for step 4 on this run: ~14 minutes (≈ 840 s). Step 5 is a
single-pass stream over `phase1/predictions.jsonl` (~2 min).
