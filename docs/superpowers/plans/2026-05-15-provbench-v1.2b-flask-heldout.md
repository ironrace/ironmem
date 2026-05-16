# ProvBench v1.2b — SPEC §9.4 Held-out Round 2 (pallets/flask) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the SPEC §9.4 held-out evaluation of the Phase 1 rules invalidator (v1.2 — R4 Field-kind guard relaxation from v1.2a, phase1 git SHA `97cef97`, no in-round retuning) on `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = `2.0.0`), record the §8 verdict against pinned thresholds, and append one row to SPEC §11 if the run completes.

**Architecture:** Mirror the serde held-out flow (`2026-05-15-provbench-phase1-heldout-serde.md`) for flask. Build labeler and phase1 binaries in ephemeral git worktrees pinned to their frozen SHAs so the labeler stamps the right `labeler_git_sha` and phase1 source is byte-identical to the frozen SHA. The labeler is the Python-capable build from Plan A (`feat/provbench-v1.2b-python-labeler`, merged); phase1 is v1.2 from v1.2a. Generate held-out corpus/facts/diffs with the pinned labeler, stratified-sample with the existing baseline crate (`--dry-run`, no LLM call), score with phase1 v1.2, and report against the three §8 thresholds. Honor §10 anti-leakage: no retuning, no rule-chain changes, no labeler bump after kickoff.

**Tech Stack:** Rust workspace (cargo, clap), `provbench-labeler` (Plan A's merged SHA) / `provbench-baseline` / `provbench-phase1` / `provbench-score` binaries, jq for JSON inspection, git worktrees for SHA pinning, serde_json for hand-written `run_meta.json`.

**Frozen pins (do not bump):**
- Spec freeze hash: `683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c`
- **Labeler git SHA (Python-capable):** `c623298f88e1704363c60a5873528f457209734e` — pin AT PLAN B KICKOFF from `git rev-parse origin/main` after Plan A's PR merges. Record in this file before any execution begins; once recorded, freeze.
- Phase 1 git SHA: `97cef97` (v1.2; from v1.2a R4 Field-kind guard relaxation, SPEC §11 row dated 2026-05-15)
- Rule set version: `v1.2`
- Sample seed: `13897750829054410479` (= `0xC0DEBABEDEADBEEF`; pass as decimal — clap's default `u64` parser does NOT accept hex)
- Held-out repo: `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (SPEC §13.2 held-out #2)
- Branch: `feat/provbench-v1.2b-flask-heldout` (already cut from `main`)

**Acceptance (SPEC §8 verbatim, read from `<RUNDIR>/metrics.json.phase1_rules.*`):**
- `section_7_1.stale_detection.wilson_lower_95 >= 0.30`
- `section_7_1.valid_retention_accuracy.wilson_lower_95 >= 0.95`
- `section_7_2_applicable.latency_p50_ms <= 727`

**Run directory shape:** `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/{metrics.json, baseline/{manifest.json, predictions.jsonl, run_meta.json, metrics.json}, phase1/{predictions.jsonl, rule_traces.jsonl, run_meta.json}}`. `phase1.sqlite` is NOT committed (matches serde precedent).

**Research question:** Does v1.2's R4 Field-kind guard relaxation generalize on Python? v1.1 on serde Round 1 FAILED §8 #3 (valid retention WLB `0.9062` < `0.95`) with R4 line-presence over-fit being the dominant cause. v1.2a fixed the diagnostic on ripgrep pilot but consumed the §10 admission. This round is the load-bearing held-out test of that fix on a new language family.

**On §8 miss:** No retune. Surface honestly in findings; SPEC §11 row records `Result: FAIL §8 #N`. §10 forbids in-round retuning either way. A FAIL here AND on serde (already on record) means v1.2 has not generalized; the §9.3 kill criterion from `project_provbench_phase0c_subset_result.md` is then live.

**SPEC §11 row gating:** Per the user-directed rule for this plan, the §11 row is appended **only if the held-out run completes end-to-end** (whether PASS or FAIL on §8). If the run aborts mid-way (corpus generation crash, sampler error, baseline manifest divergence that cannot be resolved without breaking §10), do NOT append a row — the round did not produce a recordable outcome. Note: this is a tighter gate than the serde Round 1 plan used; record an aborted run in findings only, leave SPEC §11 untouched.

---

## File Structure

| Path | Responsibility | New / Modified |
|---|---|---|
| `benchmarks/provbench/work/flask/` | flask checkout @ T₀ | New (gitignored, untracked) |
| `benchmarks/provbench/corpus/flask-2f0c62f5-<labeler-sha>.jsonl` | Labeler corpus | New (gitignored) |
| `benchmarks/provbench/facts/flask-2f0c62f5-<labeler-sha>.facts.jsonl` | T₀ fact bodies | New (gitignored) |
| `benchmarks/provbench/facts/flask-2f0c62f5-<labeler-sha>.diffs/` | Per-commit unified diffs | New (gitignored) |
| `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/baseline/` | Dry-run subset carrier (manifest, predictions, run_meta, metrics) | New (committed) |
| `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/phase1/` | Phase1 score output + hand-written run_meta | New (committed; excludes `phase1.sqlite`) |
| `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/metrics.json` | `provbench-score compare` output | New (committed) |
| `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md` | Findings doc | New (committed) |
| `benchmarks/provbench/labeler/tests/determinism_flask.rs` | Already created in Plan A (`#[ignore]`); verify it still passes on this run | Read-only (already committed by Plan A) |
| `benchmarks/provbench/phase1/tests/end_to_end_heldout_flask.rs` | Acceptance test asserting §8 + row-count + rule_set_version | New (committed; `#[ignore]`) |
| `benchmarks/provbench/SPEC.md` | Append one §11 row only if run completes (frozen body untouched) | Modified conditionally |
| `.gitignore` | `benchmarks/provbench/results/flask-heldout-*/phase1/phase1.sqlite` if not already covered | Possibly modified |

---

## Task 1: Pre-flight + pin Plan A merge SHA + worktree pin + flask clone

**Files:**
- Create: `/tmp/ironmem-worktrees/labeler-<plan-A-sha>/` (git worktree)
- Create: `/tmp/ironmem-worktrees/phase1-97cef97/` (git worktree)
- Create: `benchmarks/provbench/work/flask/` (git clone)
- Inspect: `.gitignore`
- Modify: this plan's frontmatter (replace `c623298f88e1704363c60a5873528f457209734e` placeholder)

- [ ] **Step 1: Confirm Plan A has merged to `main`**

```bash
git fetch origin main
git log origin/main --oneline -20 | grep -i "python labeler\|provbench-v1.2b-python"
```

Expected: a merge commit matching Plan A's PR title. If empty, STOP — Plan A must merge first. Plan B's labeler-pin must reference a commit on `origin/main` so reviewers can reproduce.

- [ ] **Step 2: Record Plan A's merged SHA as the labeler pin**

```bash
PLAN_A_SHA=$(git rev-parse origin/main)
echo "$PLAN_A_SHA"
```

Edit this plan file: replace every `c623298f88e1704363c60a5873528f457209734e` placeholder (3 occurrences in the frontmatter "Frozen pins", File Structure, and Task 2 worktree paths) with the recorded SHA. Commit:

```bash
git add docs/superpowers/plans/2026-05-15-provbench-v1.2b-flask-heldout.md
git commit -m "chore(provbench): pin Plan A merged SHA in v1.2b flask plan"
```

(Once committed, the pin is frozen — do NOT update mid-run.)

- [ ] **Step 3: Verify pinned SHAs are reachable**

```bash
git cat-file -e "$PLAN_A_SHA^{commit}"               # exits 0
git cat-file -e 97cef97^{commit}                     # exits 0
git branch --show-current                            # expect: feat/provbench-v1.2b-flask-heldout
```

- [ ] **Step 4: Verify `work/` directory is gitignored**

```bash
git check-ignore -v benchmarks/provbench/work/flask 2>&1
```

Expected output includes `benchmarks/provbench/work` matched by an existing `.gitignore` rule. If non-zero, STOP and add `benchmarks/provbench/work/` to top-level `.gitignore` before continuing.

- [ ] **Step 5: Create ephemeral git worktrees pinned to the frozen SHAs**

```bash
mkdir -p /tmp/ironmem-worktrees
git worktree add /tmp/ironmem-worktrees/labeler-$PLAN_A_SHA "$PLAN_A_SHA"
git worktree add /tmp/ironmem-worktrees/phase1-97cef97   97cef97
```

Expected: two new worktrees, both in detached HEAD at their respective pinned SHAs.

- [ ] **Step 6: Verify worktree HEADs**

```bash
git -C /tmp/ironmem-worktrees/labeler-$PLAN_A_SHA rev-parse HEAD   # expect: $PLAN_A_SHA
git -C /tmp/ironmem-worktrees/phase1-97cef97 rev-parse HEAD        # expect: 97cef97...
```

- [ ] **Step 7: Clone flask and check out T₀**

```bash
git clone https://github.com/pallets/flask benchmarks/provbench/work/flask
git -C benchmarks/provbench/work/flask fetch origin
git -C benchmarks/provbench/work/flask checkout 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661
git -C benchmarks/provbench/work/flask rev-parse HEAD
# expect: 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661
```

- [ ] **Step 8: Verify the held-out checkout is NOT tracked by the feature branch**

```bash
git check-ignore benchmarks/provbench/work/flask   # exits 0; prints the path
git status --short benchmarks/provbench/work/flask # empty (ignored)
```

If the checkout appears in `git status`, fix `.gitignore` BEFORE proceeding (committing the flask tree would explode repo size).

- [ ] **Step 9: Verify flask HEAD is different from T₀**

```bash
git -C benchmarks/provbench/work/flask log --first-parent --oneline 2f0c62f5..origin/main | wc -l
# expect: a positive number — held-out replay requires HEAD ≠ T₀ (see memory project_provbench_labeler_pin_quirks.md)
```

If zero, choose a current HEAD downstream of T₀ (record in findings) — for first-pass execution, use `origin/main` of flask. Record the chosen HEAD SHA for §11.

---

## Task 2: Build labeler@$PLAN_A_SHA in worktree + verify stamp + verify-tooling

**Files:**
- Use: `/tmp/ironmem-worktrees/labeler-$PLAN_A_SHA/benchmarks/provbench/labeler/Cargo.toml`

- [ ] **Step 1: Build the labeler in the pinned worktree (release)**

```bash
cd /tmp/ironmem-worktrees/labeler-$PLAN_A_SHA
cargo build --release -p provbench-labeler
ls target/release/provbench-labeler
```

Expected: build succeeds; binary exists. Note: this is the Python-capable build from Plan A.

- [ ] **Step 2: Verify the labeler stamps the correct git SHA**

```bash
./target/release/provbench-labeler --version 2>&1
```

Expected output includes `$PLAN_A_SHA` (first 7-12 chars) — this is the `labeler_git_sha` that will appear in every emitted JSONL record's `labeler_git_sha` field, and the value SPEC §11 will reference.

- [ ] **Step 3: Verify tooling availability**

```bash
./target/release/provbench-labeler verify-tooling 2>&1
```

Expected: all three of `tree-sitter` CLI (Homebrew binary), `tree-sitter-rust` grammar, `tree-sitter-python` grammar resolve to the SPEC §13.1 binary hashes. If `tree-sitter-python` grammar hash diverges from the SPEC pin (`63b76b3fa8181fd79eaad4abcdb21e2babcb504dbfc7710a89934fa456d26096`), STOP — a tooling drift requires its own SPEC §11 entry and re-runs the §10 leakage clock, which means Plan A must have failed to capture the pin properly.

- [ ] **Step 4: Return to the feature branch root**

```bash
cd <feature-branch-root>           # original repo dir, not the worktree
pwd                                # confirm
git branch --show-current          # expect: feat/provbench-v1.2b-flask-heldout
```

---

## Task 3: Emit flask corpus + facts + diffs

**Files:**
- Create: `benchmarks/provbench/corpus/flask-2f0c62f5-<plan-A-sha-short>.jsonl` (gitignored)
- Create: `benchmarks/provbench/facts/flask-2f0c62f5-<plan-A-sha-short>.facts.jsonl` (gitignored)
- Create: `benchmarks/provbench/facts/flask-2f0c62f5-<plan-A-sha-short>.diffs/` (gitignored)

- [ ] **Step 1: Emit corpus**

```bash
LBL=/tmp/ironmem-worktrees/labeler-$PLAN_A_SHA/target/release/provbench-labeler
SHA7=$(echo "$PLAN_A_SHA" | cut -c1-7)
$LBL emit-corpus \
  --repo benchmarks/provbench/work/flask \
  --t0   2f0c62f5e6e290843f03c1fa70817c7a3c7fd661 \
  --out  benchmarks/provbench/corpus/flask-2f0c62f5-$SHA7.jsonl
wc -l benchmarks/provbench/corpus/flask-2f0c62f5-$SHA7.jsonl
```

Expected: a positive corpus row count. Record the number for findings (this is the §9.4 corpus N).

- [ ] **Step 2: Emit facts**

```bash
$LBL emit-facts \
  --repo benchmarks/provbench/work/flask \
  --t0   2f0c62f5e6e290843f03c1fa70817c7a3c7fd661 \
  --out  benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.facts.jsonl
wc -l benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.facts.jsonl
```

Expected: row count matches the corpus row count (one facts row per corpus row).

- [ ] **Step 3: Emit diffs**

```bash
$LBL emit-diffs \
  --repo benchmarks/provbench/work/flask \
  --t0   2f0c62f5e6e290843f03c1fa70817c7a3c7fd661 \
  --out-dir benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/
ls benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/ | head
ls benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/ | wc -l
```

Expected: one diff file per first-parent commit from T₀ to flask HEAD.

- [ ] **Step 4: Verify two-run determinism (re-run Plan A's `#[ignore]` test)**

```bash
cd /tmp/ironmem-worktrees/labeler-$PLAN_A_SHA
cargo test -p provbench-labeler --release -- --ignored determinism_flask
```

Expected: all three `flask_*_byte_identical_across_runs` tests PASS. If any fail, STOP — nondeterminism in the corpus would invalidate the entire round (a deterministic labeler is the SPEC §9.1 / §9.4 contract). Return to Plan A and fix.

- [ ] **Step 5: Record corpus stats** in scratch notes for findings:

```bash
echo "flask corpus stats (T₀=2f0c62f5, labeler=$PLAN_A_SHA):" > /tmp/v1.2b-stats.txt
echo "  corpus rows: $(wc -l < benchmarks/provbench/corpus/flask-2f0c62f5-$SHA7.jsonl)" >> /tmp/v1.2b-stats.txt
echo "  facts rows:  $(wc -l < benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.facts.jsonl)" >> /tmp/v1.2b-stats.txt
echo "  diff files:  $(ls benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/ | wc -l)" >> /tmp/v1.2b-stats.txt
cat /tmp/v1.2b-stats.txt
```

---

## Task 4: Stratified-sample subset (baseline `--dry-run` schema carrier)

**Files:**
- Create: `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/baseline/manifest.json` (committed)
- Create: `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/baseline/predictions.jsonl` (committed; LLM-free dry-run carrier)
- Create: `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/baseline/run_meta.json` (committed)

- [ ] **Step 1: Run the baseline crate in dry-run mode**

```bash
RUNDIR=benchmarks/provbench/results/flask-heldout-2026-05-15-canary
mkdir -p $RUNDIR/baseline
cargo run --release -p provbench-baseline -- run \
  --corpus benchmarks/provbench/corpus/flask-2f0c62f5-$SHA7.jsonl \
  --facts  benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.facts.jsonl \
  --diffs  benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/ \
  --repo   benchmarks/provbench/work/flask \
  --out-dir $RUNDIR/baseline \
  --seed   13897750829054410479 \
  --dry-run
```

`--dry-run` writes the manifest + predictions skeleton without calling the model — this is the §9.4 stratified-sample carrier per the serde precedent. Sample seed is decimal `13897750829054410479` (= `0xC0DEBABEDEADBEEF`); pass as decimal — clap's default `u64` parser does NOT accept hex.

- [ ] **Step 2: Verify baseline outputs**

```bash
ls $RUNDIR/baseline
wc -l $RUNDIR/baseline/predictions.jsonl
jq '.subset_size, .stratification' $RUNDIR/baseline/manifest.json
```

Expected: `manifest.json`, `predictions.jsonl`, `run_meta.json`, `metrics.json` (dry-run metrics from the schema carrier). Subset size matches the serde Round 1 default (12,820 rows or whatever the stratified default produces from flask's corpus N — record the actual value for findings).

- [ ] **Step 3: Verify the baseline ran against the v1.2 phase1's expected frozen-hash normalization**

The serde plan called out a baseline-manifest frozen-hash normalization gotcha (`project_provbench_labeler_pin_quirks.md`). Check:

```bash
jq '.frozen_hash, .labeler_git_sha' $RUNDIR/baseline/manifest.json
```

Expected: `labeler_git_sha` equals `$PLAN_A_SHA`; `frozen_hash` is non-empty and stable. If a normalization issue appears, document in findings (hygiene flag) — do NOT patch the baseline crate mid-run unless absolutely required to complete the run.

---

## Task 5: Build phase1@97cef97 in worktree + ingest

**Files:**
- Use: `/tmp/ironmem-worktrees/phase1-97cef97/`
- Create: `$RUNDIR/phase1/phase1.sqlite` (gitignored)

- [ ] **Step 1: Build phase1 v1.2 in worktree**

```bash
cd /tmp/ironmem-worktrees/phase1-97cef97
cargo build --release -p provbench-phase1
ls target/release/provbench-phase1
git rev-parse HEAD   # expect: 97cef97...
git diff             # expect: empty (frozen, no in-round retuning per §10)
```

Expected: build succeeds; diff is empty.

- [ ] **Step 2: Ingest corpus + facts + diffs into phase1.sqlite**

```bash
PH1=/tmp/ironmem-worktrees/phase1-97cef97/target/release/provbench-phase1
mkdir -p $RUNDIR/phase1
$PH1 ingest \
  --corpus benchmarks/provbench/corpus/flask-2f0c62f5-$SHA7.jsonl \
  --facts  benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.facts.jsonl \
  --diffs  benchmarks/provbench/facts/flask-2f0c62f5-$SHA7.diffs/ \
  --db     $RUNDIR/phase1/phase1.sqlite
```

Expected: SQLite db populated. Size will likely be 10-100 MB (serde Round 1 was 46 MB).

- [ ] **Step 3: Return to repo root**

```bash
cd <feature-branch-root>
```

---

## Task 6: Run phase1 invalidator (v1.2 rules, no retuning)

**Files:**
- Create: `$RUNDIR/phase1/predictions.jsonl` (committed)
- Create: `$RUNDIR/phase1/rule_traces.jsonl` (committed)

- [ ] **Step 1: Run the rules invalidator on the dry-run subset**

```bash
$PH1 score \
  --db $RUNDIR/phase1/phase1.sqlite \
  --subset $RUNDIR/baseline/predictions.jsonl \
  --out-predictions $RUNDIR/phase1/predictions.jsonl \
  --out-traces      $RUNDIR/phase1/rule_traces.jsonl \
  --rule-set v1.2
```

Expected: predictions + traces written. Confirm `rule_set_version` field in predictions:

```bash
jq -r '.rule_set_version' $RUNDIR/phase1/predictions.jsonl | sort -u
# expect: v1.2
```

- [ ] **Step 2: Confirm phase1 source is byte-identical to 97cef97**

```bash
cd /tmp/ironmem-worktrees/phase1-97cef97
git diff
# expect: empty
git rev-parse HEAD
# expect: 97cef97...
cd <feature-branch-root>
```

If the worktree's diff is non-empty, the rules were touched mid-run — STOP and abort the round (the §10 contract is broken; the §11 row gating says "do NOT append a row").

- [ ] **Step 3: Write run_meta.json for phase1** (hand-edited; mirrors serde precedent)

```bash
cat > $RUNDIR/phase1/run_meta.json <<EOF
{
  "round": "v1.2b-flask-heldout",
  "rule_set_version": "v1.2",
  "phase1_git_sha": "97cef97",
  "labeler_git_sha": "$PLAN_A_SHA",
  "held_out_repo": "pallets/flask",
  "held_out_repo_t0": "2f0c62f5e6e290843f03c1fa70817c7a3c7fd661",
  "held_out_repo_head": "<recorded in Task 1 Step 9>",
  "spec_freeze_hash": "683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c",
  "sample_seed": "13897750829054410479",
  "run_date": "2026-05-15",
  "retuning_in_round": false
}
EOF
```

Fill in `held_out_repo_head` from the value recorded in Task 1 Step 9.

---

## Task 7: Score against §8 thresholds

**Files:**
- Create: `$RUNDIR/metrics.json` (committed)

- [ ] **Step 1: Run scoring**

```bash
cargo run --release -p provbench-score -- compare \
  --baseline  $RUNDIR/baseline/predictions.jsonl \
  --phase1    $RUNDIR/phase1/predictions.jsonl \
  --out       $RUNDIR/metrics.json
```

Expected: `metrics.json` written.

- [ ] **Step 2: Read §8 metric values**

```bash
jq '.phase1_rules.section_7_1.valid_retention_accuracy.wilson_lower_95' $RUNDIR/metrics.json
jq '.phase1_rules.section_7_1.stale_detection.wilson_lower_95'         $RUNDIR/metrics.json
jq '.phase1_rules.section_7_2_applicable.latency_p50_ms'               $RUNDIR/metrics.json
```

Record all three values. Compare against §8 thresholds:
- valid retention WLB ≥ 0.95
- stale detection WLB ≥ 0.30
- latency p50 ≤ 727 ms

- [ ] **Step 3: Read per-rule confusion + Field-kind breakdown**

```bash
jq '.phase1_rules.per_rule_confusion' $RUNDIR/metrics.json
jq '.phase1_rules.per_kind_breakdown // .phase1_rules.per_stale_subtype' $RUNDIR/metrics.json
```

Pivot question for §11 narrative: did R4 Field false-Valid count stay near 0 (the v1.2a Gate 3 target), or did it regress on Python? If v1.2 generalizes, expect Field-kind false-Valid < v1.1 serde's reading (162 on `GT=Valid`).

---

## Task 8: Commit phase1 + baseline artifacts (excluding sqlite)

**Files:**
- Already created: `$RUNDIR/{metrics.json, baseline/*, phase1/{predictions,rule_traces,run_meta}.json}`

- [ ] **Step 1: Verify `.gitignore` excludes phase1.sqlite**

```bash
git check-ignore $RUNDIR/phase1/phase1.sqlite
```

Expected: prints the path (ignored). If not, append to `.gitignore`:

```
benchmarks/provbench/results/flask-heldout-*/phase1/phase1.sqlite
```

(Same pattern as serde Round 1.) Commit the `.gitignore` change separately if added.

- [ ] **Step 2: Stage + commit**

```bash
git add $RUNDIR/metrics.json \
        $RUNDIR/baseline/ \
        $RUNDIR/phase1/predictions.jsonl \
        $RUNDIR/phase1/rule_traces.jsonl \
        $RUNDIR/phase1/run_meta.json
git status --short $RUNDIR  # confirm phase1.sqlite NOT staged
git commit -m "data(provbench): v1.2b flask held-out canary artifacts"
```

---

## Task 9: Add `#[ignore]` acceptance test asserting §8 verbatim

**Files:**
- Create: `benchmarks/provbench/phase1/tests/end_to_end_heldout_flask.rs`

- [ ] **Step 1: Write the failing-on-purpose test**

```rust
// benchmarks/provbench/phase1/tests/end_to_end_heldout_flask.rs
//! v1.2b held-out acceptance for pallets/flask. Mirrors
//! `end_to_end_heldout_serde.rs` byte-for-byte except for the run-dir
//! path and the asserted §8 values. `#[ignore]` keeps it out of default
//! cargo test runs; CI opts in via `--ignored`.

use serde_json::Value;
use std::path::PathBuf;

const RUN_DIR: &str =
    "../results/flask-heldout-2026-05-15-canary";

fn load_metrics() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(RUN_DIR).join("metrics.json");
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[test]
#[ignore]
fn section_8_3_valid_retention_accuracy() {
    let m = load_metrics();
    let wlb = m["phase1_rules"]["section_7_1"]["valid_retention_accuracy"]["wilson_lower_95"]
        .as_f64()
        .unwrap();
    assert!(wlb >= 0.95, "valid retention WLB {} < 0.95", wlb);
}

#[test]
#[ignore]
fn section_8_4_latency_p50() {
    let m = load_metrics();
    let p50 = m["phase1_rules"]["section_7_2_applicable"]["latency_p50_ms"]
        .as_u64()
        .unwrap();
    assert!(p50 <= 727, "latency p50 {}ms > 727ms", p50);
}

#[test]
#[ignore]
fn section_8_5_stale_detection_recall() {
    let m = load_metrics();
    let wlb = m["phase1_rules"]["section_7_1"]["stale_detection"]["wilson_lower_95"]
        .as_f64()
        .unwrap();
    assert!(wlb >= 0.30, "stale recall WLB {} < 0.30", wlb);
}

#[test]
#[ignore]
fn rule_set_version_is_v12() {
    let preds = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(RUN_DIR).join("phase1/predictions.jsonl"),
    )
    .unwrap();
    for line in preds.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["rule_set_version"].as_str().unwrap(), "v1.2");
    }
}

#[test]
#[ignore]
fn row_count_matches_subset_size() {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(RUN_DIR)
                .join("baseline/manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let expected = manifest["subset_size"].as_u64().unwrap();
    let preds = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(RUN_DIR).join("phase1/predictions.jsonl"),
    )
    .unwrap();
    assert_eq!(preds.lines().count() as u64, expected);
}
```

- [ ] **Step 2: Run the test (ignored) to confirm it passes (or fails honestly)**

```bash
cargo test -p provbench-phase1 --release -- --ignored end_to_end_heldout_flask
```

Expected behavior:
- If §8 PASSES on flask: all five tests pass.
- If §8 #3 fails (mirror of serde Round 1): `section_8_3_valid_retention_accuracy` panics with the actual WLB. This is the honest §9.4 result; do NOT relax the threshold.

Either outcome is recorded; the test stays committed with `#[ignore]` so future bisects can re-run it.

- [ ] **Step 3: Commit the acceptance test**

```bash
git add benchmarks/provbench/phase1/tests/end_to_end_heldout_flask.rs
git commit -m "test(provbench-phase1): v1.2b held-out flask acceptance (§8 verbatim, #[ignore])"
```

---

## Task 10: Write findings doc

**Files:**
- Create: `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`

- [ ] **Step 1: Draft findings**

Skeleton — fill in actual numbers from Task 7:

```markdown
# ProvBench v1.2b — Flask held-out findings (2026-05-15)

**Run dir:** `benchmarks/provbench/results/flask-heldout-2026-05-15-canary/`
**Held-out repo:** `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = `2.0.0`)
**HEAD at run:** `<recorded SHA from Task 1 Step 9>`
**Labeler git SHA:** `c623298f88e1704363c60a5873528f457209734e` (Python-capable build from Plan A)
**Phase 1 git SHA:** `97cef97` (`rule_set_version v1.2`, R4 Field-kind guard relaxation from v1.2a)
**Sample seed:** `0xC0DEBABEDEADBEEF` (decimal `13897750829054410479`)
**SPEC freeze hash:** `683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c`

## Corpus stats
- Total corpus rows: <N>
- Facts rows: <N>
- First-parent commits T₀→HEAD: <K>
- Stratified subset size: <S>

## §8 thresholds — verdict

| § | Metric | Pinned threshold | Observed | Verdict |
|---|---|---|---|---|
| §8 #3 | `valid_retention_accuracy.wilson_lower_95` | ≥ 0.95 | <observed> | PASS / FAIL |
| §8 #4 | `latency_p50_ms` | ≤ 727 | <observed> | PASS / FAIL |
| §8 #5 | `stale_detection.wilson_lower_95` | ≥ 0.30 | <observed> | PASS / FAIL |

## Comparison vs v1.1 serde Round 1 (the §9.4 thesis pivot)

| Metric | v1.1 serde | v1.2 flask | Δ |
|---|---|---|---|
| §8 #3 valid retention WLB | 0.9062 | <observed> | <Δ> |
| §8 #4 latency p50 | 14 ms | <observed> | <Δ> |
| §8 #5 stale recall WLB | 0.9391 | <observed> | <Δ> |

[Narrative: did v1.2's R4 Field-kind guard relaxation generalize on Python?
Interpret the per-rule confusion + Field-kind breakdown.]

## Per-rule confusion + Field-kind breakdown

[Insert `jq` output from Task 7 Step 3 here.]

## Hygiene flags
1. [Any baseline manifest normalization issues from Task 4 Step 3]
2. [Determinism gate result from Task 3 Step 4]
3. [Anything else surfaced mid-run]

## §10 anti-leakage attestation
- Phase 1 source byte-identical to `97cef97` across the run (Task 6 Step 2 verified).
- Labeler git SHA frozen at `c623298f88e1704363c60a5873528f457209734e` from kickoff; no labeler bump mid-run.
- No rule retuning in-round.
- v1.0 / v1.1 / v1.2 pilot canary artifacts not rewritten by this round.

## SPEC §11 row gating
[If run completed end-to-end:] §11 row appended in this commit.
[If run aborted:] §11 row NOT appended (per Plan B gating rule). Aborted state recorded above for future re-attempt.

## Decision
[Pass on §8 #3:] v1.2's R4 Field-kind guard relaxation generalizes from pilot to Python held-out. The §9.4 thesis row now reads "v1.2 PASS flask, awaiting next held-out."
[Fail on §8 #3:] v1.2 does NOT generalize. §9.3 kill criterion is now live for the rules-only thesis; next move is documented in the SPEC §11 row's "Spec impact" cell.
```

- [ ] **Step 2: Commit findings**

```bash
git add benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md
git commit -m "docs(provbench): v1.2b flask held-out findings (PASS|FAIL summary)"
```

---

## Task 11: Append SPEC §11 row (CONDITIONAL — only if Task 7 produced metrics)

**Gating rule (from Plan B preamble):** Append §11 row **only if the run completed end-to-end** (Tasks 1-10 all completed, `metrics.json` exists, findings doc written). If any task aborted irrecoverably, SKIP this task and stop.

**Files:**
- Modify: `benchmarks/provbench/SPEC.md` (append one row to §11; frozen body untouched)

- [ ] **Step 1: Verify the run completed**

```bash
test -f $RUNDIR/metrics.json
test -f benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md
git rev-parse HEAD   # has the artifact + findings commits
```

If either file is missing, STOP — do not append a §11 row. Record the aborted state in findings only.

- [ ] **Step 2: Read the current §11 row layout**

```bash
grep -A1 "^| 2026-05-15 | §11 (record only)" benchmarks/provbench/SPEC.md | tail -1
```

Confirm the column structure (date | section | observation | rationale | spec impact).

- [ ] **Step 3: Append the v1.2b row**

Sketch — fill in actual numbers and PASS/FAIL verdicts:

```markdown
| 2026-05-15 | §9.4 (record only) | Second §9.4 held-out result recorded: pallets/flask @ `2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = `2.0.0`, HEAD = `<recorded>`) + labeler @ `c623298f88e1704363c60a5873528f457209734e` (Python-capable build; tree-sitter-python 0.25 per §13.1) + phase1 @ `97cef97`, `rule_set_version v1.2`, no in-round retuning. Result: **<PASS|FAIL> §8 #3** (valid retention WLB `<observed>` <op> `0.95`; v1.1 serde was `0.9062`, v1.2a ripgrep pilot was `0.9729`). §8 #4 (`latency_p50_ms` = `<observed>`) <PASS|FAIL> and §8 #5 (`stale_detection.wilson_lower_95` = `<observed>`) <PASS|FAIL>. Per-rule confusion attributes [§8 #3 verdict] to [dominant rule]. Held-out subset n=`<S>` (stratified, default targets, seed `0xC0DEBABEDEADBEEF`); corpus n=`<N>`; `<K>` first-parent commits T₀→HEAD. Findings: `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`. | Second held-out evaluation per SPEC §9.4 / §10. The v1.2a R4 Field-kind guard relaxation was tuned on ripgrep pilot only; this is its first held-out test on a new language family. [Narrative: did it generalize?] | None for SPEC §§1–10 / §12–§15 (frozen body untouched). The §10 anti-leakage contract holds verbatim: phase1 source byte-identical to `97cef97` (`git diff` empty); v1.0 / v1.1 / v1.2 pilot canary artifacts not rewritten; labeler git SHA frozen at `c623298f88e1704363c60a5873528f457209734e` from kickoff. Acceptance test `phase1/tests/end_to_end_heldout_flask.rs` is `#[ignore]` and asserts §8 verbatim — it [passes/fails honestly on §8 #3, which IS the recorded held-out result]. |
```

- [ ] **Step 4: Commit**

```bash
git add benchmarks/provbench/SPEC.md
git commit -m "docs(provbench): SPEC §11 — record v1.2b flask held-out result"
```

---

## Task 12: Open PR

**Files:**
- External: GitHub PR against `main`

- [ ] **Step 1: Push + open PR**

```bash
git push -u origin feat/provbench-v1.2b-flask-heldout
gh pr create --base main \
  --title "data(provbench): v1.2b Round 2 — flask held-out evaluation" \
  --body "$(cat <<'EOF'
## Summary
- §9.4 held-out evaluation of phase1 v1.2 (R4 Field-kind guard relaxation from v1.2a) on `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661`.
- Labeler: Python-capable build from PR #<plan-A-PR-num> (SHA `c623298f88e1704363c60a5873528f457209734e`).
- No in-round retuning. SPEC §11 row appended.

## Verdict
- §8 #3 valid retention: **<PASS|FAIL>** (`<wlb>`)
- §8 #4 latency p50: **<PASS|FAIL>** (`<ms>`)
- §8 #5 stale recall: **<PASS|FAIL>** (`<wlb>`)

Detailed findings: `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`

## §10 anti-leakage attestation
- Phase 1 source byte-identical to `97cef97` across the run.
- Labeler frozen at `c623298f88e1704363c60a5873528f457209734e` from kickoff.
- No rule retuning, no labeler bump, no pilot canary artifact rewrites.

## Test plan
- [ ] CI green on this branch
- [ ] Reviewer reproduces `metrics.json` numbers from committed `predictions.jsonl` + `baseline/predictions.jsonl`
- [ ] Reviewer verifies §11 row reflects committed metrics.json values
EOF
)"
```

- [ ] **Step 2: After merge, save memory note**

After PR merges, save a project memory entry summarizing the v1.2b verdict, the Δ vs serde Round 1, and (if FAIL) the next-step plan. Use the existing memory naming convention (`project_provbench_v1_2b_result.md`).

---

## Self-Review checklist (run before declaring Plan B complete)

1. **Spec coverage:**
   - SPEC §9.4 held-out #2 (flask) → Tasks 1-7
   - SPEC §8 #3/#4/#5 verbatim assertions → Task 9
   - SPEC §10 anti-leakage (phase1 frozen) → Task 6 Step 2
   - SPEC §11 record → Task 11 (conditional)
   - SPEC §13.1 tree-sitter-python pin → Task 2 Step 3 (verify-tooling)
   - SPEC §13.2 held-out #2 pin → Task 1 Step 7

2. **Placeholder scan:** Three intentional `c623298f88e1704363c60a5873528f457209734e` and one `<recorded>` HEAD placeholder live in this plan — Task 1 Step 2 replaces all `c623298f88e1704363c60a5873528f457209734e` occurrences, and Task 1 Step 9 records HEAD. Findings + §11 templates contain `<observed>` / `<wlb>` / `<ms>` placeholders that are filled in by Task 10 / Task 11 from actual metrics.json values. These are not unbounded placeholders — each has a concrete source.

3. **Type consistency:** `$PLAN_A_SHA`, `$SHA7`, `$RUNDIR`, `$LBL`, `$PH1` bash variables used consistently across tasks 1-7.

4. **Gating consistency:** The SPEC §11 row gating ("only if run completes") is stated in three places: preamble, Task 11 preamble, and Self-Review item 1. All three say the same thing.

---

## Execution handoff

**Plan complete and saved to `docs/superpowers/plans/2026-05-15-provbench-v1.2b-flask-heldout.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — Dispatch a fresh subagent per task; review between tasks. Fast iteration.

**2. Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`; batch with checkpoints.

**Prerequisite:** Plan A (`2026-05-15-provbench-v1.2b-python-labeler.md`) must merge to `main` before Task 1 can pin the Python-capable labeler SHA. Tasks 1-12 then run on the existing `feat/provbench-v1.2b-flask-heldout` branch.
