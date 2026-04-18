#!/usr/bin/env bash
# E2 parameter sweep — two-stage grid search over RRF/BM25 + shrinkage weights.
#
# Stage 1 (12 runs): sweep RRF_K × BM25_SPARSE_THRESHOLD at baseline weights.
# Stage 2 (12 runs): sweep shrinkage weight combos on the top-3 Stage 1 configs.
#
# Results land in /tmp/e2_sweep/ as individual JSON files.
# A summary table is printed at the end of each stage.
#
# Usage:
#   ./scripts/e2_sweep.sh               # full two-stage sweep
#   ./scripts/e2_sweep.sh --stage1      # Stage 1 only
#   LIMIT=50 ./scripts/e2_sweep.sh      # quick smoke-test with 50 questions

set -euo pipefail

BINARY="${IRONMEM_BINARY:-./target/release/ironmem}"
BENCH="scripts/benchmark_longmemeval.py"
LIMIT="${LIMIT:-165}"        # covers all 30 preference questions
N_RESULTS="${N_RESULTS:-50}" # enough depth to detect rank shifts
OUT_DIR="${HOME}/.cache/ironrace/e2_sweep"
STAGE="${1:-}"

mkdir -p "$OUT_DIR"

# ── helpers ───────────────────────────────────────────────────────────────────

run_config() {
    local tag="$1"; shift
    local out="$OUT_DIR/${tag}.json"
    if [[ -f "$out" ]]; then
        echo "  [skip] $tag (cached)"
        return
    fi
    echo "  [run ] $tag"
    env "$@" python3 "$BENCH" \
        --limit "$LIMIT" \
        --n-results "$N_RESULTS" \
        --output-json "$out" \
        2>/dev/null
}

# Print a summary row: tag, overall R@5, preference R@5
print_row() {
    local tag="$1"
    local f="$OUT_DIR/${tag}.json"
    [[ -f "$f" ]] || return
    python3 - "$f" "$tag" <<'EOF'
import json, sys
data = json.load(open(sys.argv[1]))
tag  = sys.argv[2]
r5   = data["recall"][5] * 100
pref = data["per_type"].get("single-session-preference", {}).get(5, float("nan")) * 100
ms   = data["latency_p50_ms"]
print(f"  {tag:<45}  R@5={r5:5.1f}%  pref={pref:5.1f}%  p50={ms:5.1f}ms")
EOF
}

# ── Stage 1: RRF_K × BM25_SPARSE_THRESHOLD ───────────────────────────────────

echo
echo "=== Stage 1: RRF_K × BM25_SPARSE_THRESHOLD (baseline shrinkage weights) ==="
echo

RRF_KS=(20 40 60 80)
BM25_THRESHOLDS=(3 5 8)

for rrf in "${RRF_KS[@]}"; do
    for bm25 in "${BM25_THRESHOLDS[@]}"; do
        tag="rrf${rrf}_bm25t${bm25}_kw050_qw060_nw020"
        run_config "$tag" \
            IRONMEM_RRF_K="$rrf" \
            IRONMEM_BM25_SPARSE_THRESHOLD="$bm25"
    done
done

echo
echo "Stage 1 results (sorted by preference R@5):"
for rrf in "${RRF_KS[@]}"; do
    for bm25 in "${BM25_THRESHOLDS[@]}"; do
        print_row "rrf${rrf}_bm25t${bm25}_kw050_qw060_nw020"
    done
done | sort -t= -k3 -rn

[[ "$STAGE" == "--stage1" ]] && exit 0

# ── Stage 2: shrinkage weights on top-3 Stage 1 configs ──────────────────────

echo
echo "=== Stage 2: Shrinkage weights on best Stage 1 configs ==="
echo

# Pick the top 3 configs by preference R@5 from Stage 1
TOP3=$(
    for rrf in "${RRF_KS[@]}"; do
        for bm25 in "${BM25_THRESHOLDS[@]}"; do
            tag="rrf${rrf}_bm25t${bm25}_kw050_qw060_nw020"
            f="$OUT_DIR/${tag}.json"
            [[ -f "$f" ]] || continue
            pref=$(python3 -c "import json; d=json.load(open('$f')); print(d['per_type'].get('single-session-preference',{}).get(5,0))")
            echo "$pref $rrf $bm25"
        done
    done | sort -rn | head -3
)

# Weight combos: (kw, quoted, name)
KW_VALS=(0.30 0.50 0.70 0.90)
QW_VALS=(0.40 0.60 0.80)
NW_VALS=(0.10 0.20 0.30)

while IFS=" " read -r _score best_rrf best_bm25; do
    echo "  Base config: RRF_K=$best_rrf BM25_SPARSE_THRESHOLD=$best_bm25"
    for kw in "${KW_VALS[@]}"; do
        for qw in "${QW_VALS[@]}"; do
            for nw in "${NW_VALS[@]}"; do
                kw_str="${kw/./}"
                qw_str="${qw/./}"
                nw_str="${nw/./}"
                tag="rrf${best_rrf}_bm25t${best_bm25}_kw${kw_str}_qw${qw_str}_nw${nw_str}"
                run_config "$tag" \
                    IRONMEM_RRF_K="$best_rrf" \
                    IRONMEM_BM25_SPARSE_THRESHOLD="$best_bm25" \
                    IRONMEM_KW_WEIGHT="$kw" \
                    IRONMEM_QUOTED_WEIGHT="$qw" \
                    IRONMEM_NAME_WEIGHT="$nw"
            done
        done
    done
done <<< "$TOP3"

echo
echo "Stage 2 results (top 15 by preference R@5):"
for f in "$OUT_DIR"/*.json; do
    tag=$(basename "$f" .json)
    print_row "$tag"
done | sort -t= -k3 -rn | head -15

echo
echo "All results in $OUT_DIR/"
echo "Done."
