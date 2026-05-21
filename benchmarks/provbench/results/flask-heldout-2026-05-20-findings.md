# ProvBench Phase 1 (rules) — 2026-05-20 flask held-out findings (`rule_set_version v1.3`, Plan A.2 Python labeler, **re-stratified** subset)

## TL;DR — read this first

This is the **sixth `§9.4` (record-only) held-out result** and the **first v1.3 round on a re-stratified Plan A.2 flask subset**: 1,500 Valid + 500/500/500 `Stale_*` + 1,000 NeedsRevalidation = **4,000 manifest rows** (vs. v1.2b's 2,000 Valid + 2,000 NR + 0 `Stale_*` frozen subset). The round shipped as a **documented partial baseline** (n = 2,759 / 4,000 = 69%) after an Anthropic 429 rate-limit abort followed by a 127-empty-body parse-failure cascade on resume.

1. **§8 verdict: PASS-FAIL-PASS** (in §8 # order: #3 FAIL, #4 PASS-vacuous, #5 PASS). The §8 #3 FAIL is the load-bearing new evidence — and it is the *intended* informative result this round was designed to produce.
2. **§8 #3 `valid_retention_accuracy.wilson_lower_95 = 0.4841`** (point 0.5144, paired n = 1,040 Valid-GT rows). This **confirms the v1.2c full-corpus sidecar's V-retention regression finding** (0.5182 WLB on 750,318 Valid-GT rows in the population) **as a real property of v1.3 rules on Plan A.2 facts**, not an artifact of the v1.2c subset's class shape. The v1.2c subset's PASS at 0.9981 WLB was uninformative because it had zero `Stale_*` GT and routed all Valid-GT through R2 `blob_identical`. v1.2d's re-stratified subset (with `Stale_*`-bearing rows and Valid rows that actually reach R3) produces the regression at sample-truth scale.
3. **§8 #5 `stale_detection.wilson_lower_95 = 0.8206`** (point 0.8441, paired n = 1,026 `Stale_*`-GT rows). PASS vs. the 0.30 threshold by a 52-pp cushion. This is the **first `§9.4` round where §8 #5 is informative on a `Stale_*`-bearing Python held-out subset** (vs. v1.2c's structural SKIP from `ground_truth_stale_count_is_zero`).
4. **§8 #4 `latency_p50_ms = 0`** — PASS-vacuous. H1 `wall_ms` un-populated carries forward unchanged from v1.2b A.3 / v1.3 Plan A.1 / v1.2c.
5. **The round mechanism for §8 #3 FAIL is R3 over-firing on Plan A.2 Valid-GT Python rows**, exactly as the v1.2c sidecar predicted. On the 2,759-row paired subset R3 fires 1,597× (per-rule confusion), of which 485 fire on Valid-GT as false `stale`. The next-largest disagreement class is R4's NR-mis-routing (60 NR→valid, 17 valid→NR, 3 valid→stale). The v1.2c forward path α (R3 retuning on a pilot/ripgrep corpus) is unambiguously the next round's priority.
6. **§10 attestation cleared 8/8** (item 8 PASS_WITH_NOTE for the in-branch Task 7.5 baseline-runner patch `c6b308f`: fence-strip + ITPM throttle ship as baseline ORACLE robustness, *outside* the §10 frozen perimeter). No phase1/scoring/labeler/SPEC byte changes; rule_set_version v1.3 unchanged; no in-round retuning.
7. **Partial-baseline disclosure (forced by Anthropic rate-limit and a runner empty-body cascade):** initial run aborted at 67.5% on 429 ITPM-cap exceeded; resume run completed exit 0 but emitted 127/130 batches as `EOF while parsing a value at line 1 column 0` empty content responses. Cumulative state: 2,759 predictions over 4,000 manifest rows, 134 parse failures, ≈ $37 spent (estimated — H2 carry-forward, no per-row `cost_usd` recorded in `predictions.jsonl`). Per user decision, v1.2d ships as a documented partial run. Phase1 `score` is bound to `--baseline-run` and evaluates the 2,759 rows the baseline scored, not the full 4,000-row manifest (new hygiene flag H7).

## Thesis under test (v1.2c forward path α) — and how it landed

**v1.2c forward path α (preregistered in the 2026-05-19 SPEC §11 row and the v1.2c findings doc):** re-stratify the flask 4k subset against the Plan A.2 corpus distribution and re-run the LLM baseline against the new subset to produce an **informative** §8 #5 verdict on flask Python, paying the §13.2 flask leakage budget as the §10-defined cost of the Plan A.2 labeler change.

**Where the thesis landed:**

- **§8 #5 informative:** ✅ supported. The re-stratified subset has 1,026 `Stale_*`-GT rows in the paired n = 2,759. §8 #5 WLB = 0.8206 PASSES with a 52-pp cushion — a clean informative result, not a SKIP.
- **§8 #3 generalization on Plan A.2 Python:** ✗ NOT supported (as the v1.2c sidecar predicted). §8 #3 WLB = 0.4841 FAILS the 0.95 threshold by 47 pp. The v1.2c sidecar's full-corpus measurement (0.5182 WLB on 750k Valid-GT) is **confirmed** at sample-truth scale on a re-stratified held-out subset.
- **Bonus methodological contribution:** the round also exposes how partial-baseline robustness (runner empty-body cascade) interacts with §10 — the Task 7.5 in-branch patch ships as **baseline-oracle scope only** so the §10 attestation can still clear 8/8 with item 8 carrying a PASS_WITH_NOTE.

The round's contribution is therefore both **diagnostic confirmation** (v1.2c sidecar V-retention regression confirmed at sample-truth scale on a Stale_*-bearing held-out subset) and **the first informative §8 #5 verdict on Python** since v1.2b opened flask under §9.4 — at the cost of consuming the §13.2 flask leakage budget per the v1.2c α forward path.

## SPEC §8 threshold verdict — **PASS-FAIL-PASS** (record only; partial baseline, n = 2,759)

| Threshold | Required | Observed (paired subset, v1.3 + Plan A.2 re-stratified) | Pass? |
|---|---|---|:---:|
| §8 #3 valid retention WLB | ≥ 0.95 | **0.4840505894389724** (point 0.5144, n=1,040 Valid-GT) | ❌ FAIL |
| §8 #4 latency p50 (per-row, ms) | ≤ 727 | 0 (vacuous — H1 carry-forward) | ✅ |
| §8 #5 stale recall WLB | ≥ 0.30 | **0.8205758722030182** (point 0.8441, n=1,026 Stale_*-GT) | ✅ |

Comparison across §9.4 rounds:

| §8 # | v1.1 serde (2026-05-15) | v1.2b flask Round 2 (2026-05-15) | v1.2c flask Plan A.2 subset (2026-05-19) | v1.2c flask Plan A.2 sidecar (2026-05-19) | **v1.2d flask Plan A.2 re-stratified (2026-05-20)** |
|---|---:|---:|---:|---:|---:|
| #3 V-retention WLB | 0.9062 FAIL | 0.9981 PASS | 0.9981 PASS (uninformative) | 0.5182 (would FAIL by 43 pp) | **0.4841 FAIL by 47 pp** |
| #4 latency p50 ms | 14 PASS | 0 PASS-vacuous | 0 PASS-vacuous | n/a (sidecar) | **0 PASS-vacuous** |
| #5 Stale recall WLB | 0.9391 PASS | 0.0 FAIL (structural) | SKIP (no Stale GT) | 0.9256 (would PASS) | **0.8206 PASS by 52 pp** |

The v1.2c sidecar's full-corpus V-retention 0.5182 WLB and v1.2d's re-stratified held-out subset 0.4841 WLB agree within ≈ 3.4 pp — consistent with sampling variance from a 1,040-Valid-GT subset against a 750,318-Valid-GT population. The two independent estimates of the same underlying property (R3 false-firing on Plan A.2 Valid-GT Python) converge.

## Full-corpus sidecar — primary new evidence on the paired subset (n = 2,759; partial baseline)

This round does NOT run a full-corpus sidecar (the v1.2c sidecar already provided the full-corpus measurement at 910,530 rows; recomputing it here would duplicate the same numbers against the same Plan A.2 corpus snapshot). The headline evidence is the §8 verdict above on the **paired held-out subset** at sample-truth scale.

### Sidecar §7.1 three-way table — paired subset (2,759 rows; v1.3 phase1 vs. canonicalized GT)

From `flask-heldout-2026-05-20-canary/metrics.json.phase1-v1.3.section_7_1`:

| Metric | Point | Wilson LB | n |
|---|---:|---:|---:|
| **Valid retention** | **0.5144** | **0.4841** | 1,040 (paired Valid-GT) |
| Stale detection recall | **0.8441** | **0.8206** | 1,026 (paired Stale_*-GT) |
| Stale detection precision | 0.4358 | — | 1,987 (predicted stale) |
| Stale detection F1 | 0.5748 | — | — |
| NR routing accuracy | **0.0000** | **0.0000** | 693 (paired NR-GT) |

The paired n's reflect the partial baseline: of the 1,500 Valid manifest rows, 1,040 paired with a baseline prediction; of the 500/500/500 `Stale_*` manifest rows, 332/339/355 paired; of the 1,000 NR manifest rows, 693 paired. Total 2,759 (cf. `metrics.json.llm_baseline.per_stratum_sizes`).

### Sidecar full-corpus confusion matrix — paired subset (GT × phase1 prediction)

| GT \ Pred | `valid` | `stale` | `needs_revalidation` | total |
|---|---:|---:|---:|---:|
| `Valid` | **535** | 488 | 17 | 1,040 |
| `Stale_*` (any subtype) | 156 | **866** | 4 | 1,026 |
| `NeedsRevalidation` | 49 | 633 | **0** (R4-NR carve-out: 11) | 693 |
| total | 740 | 1,987 | 32 | 2,759 |

(Note: the row above uses the phase1 prediction class breakdown processed: 2,759 / valid: 740 / stale: 1,987 / needs_reval: 32, and the per-rule confusion in `metrics.json.per_rule_confusion`. The Valid-GT row is dominated by 488 `valid__stale` false positives — exactly the v1.2c R3 over-firing pathology, but now at sample-truth resolution on a held-out subset.)

Row-level reading:

- **Valid-GT (n = 1,040):** 51.44% retained as `valid`; **46.92% false-Staled**; 1.63% false-NR'd. The §8 #3 FAIL is dominated by the 488 false-Stale rows, which trace to R3 firing on Valid-GT (485 fires; see per-rule below). The pattern is structurally identical to the v1.2c sidecar's 51.93% / 46.10% / 1.97% breakdown on the 750k full-corpus Valid-GT — confirming the v1.2c sidecar's measurement as a real property and not a sampling artifact.
- **Stale_*-GT (n = 1,026):** 84.41% correctly classified as `stale`; 15.21% false-Valid; 0.39% false-NR. Recall is 8.3 pp lower than the v1.2c sidecar's 0.9269 — the §8 #5 verdict still clears its threshold with cushion, but per-stale-subtype recall is weaker on the v1.2d subset. Likely cause: the v1.2c sidecar's 153,560 Stale_* rows include a larger share of `StaleSourceDeleted` (R1's strongest domain — point recall 0.9305 in v1.2c sidecar) which is over-represented relative to the v1.2d 500/500/500 balanced stratification.
- **NR-GT (n = 693):** 0% correctly routed to NR; 91.3% absorbed into `stale` (R3-dominated, 425 fires; R4-stale 208 fires); 7.1% absorbed into `valid` (R4-valid 60 fires). The v1.3 R4 NR carve-out catches 0 NR-GT rows on this subset (vs. 45/2,000 on the v1.2b/v1.2c subset; vs. 0/6,652 on the v1.2c sidecar). The carve-out remains corpus-distribution-sensitive.

### Per-rule confusion — paired subset (2,759 rows)

From `metrics.json.per_rule_confusion`:

| Rule | Cells (GT __ Pred) | Total fires | Notable cells |
|---|---|---:|---|
| **R1** `source_file_missing` | `StaleSourceDeleted__stale` 40 | **40** | All-correct deletions; 12% of paired StaleSourceDeleted (40/339) |
| **R2** `blob_identical` | `Valid__valid` 214 | **214** | All-correct; 20.6% of paired Valid-GT escape via R2 short-circuit |
| **R3** `symbol_missing` | `NeedsRevalidation__stale` 425, `StaleSourceChanged__stale` 264, `StaleSourceDeleted__stale` 220, `StaleSymbolRenamed__stale` 203, `Valid__stale` **485** | **1,597** | 57.9% of all rule fires; **485 false-Stale on Valid-GT** dominates the §8 #3 FAIL |
| **R4** `span_hash_changed` | `Valid__valid` 321, `NeedsRevalidation__stale` 208, `StaleSymbolRenamed__valid` 115, `NeedsRevalidation__valid` 60, `StaleSourceDeleted__stale` 56, `StaleSourceChanged__stale` 46, `StaleSymbolRenamed__stale` 37, `StaleSourceChanged__valid` 22, `Valid__needs_revalidation` 17, `StaleSourceDeleted__needs_revalidation` 15, `StaleSourceDeleted__valid` 8, `Valid__stale` 3 | **908** | Mixed: 321 correct Valid catches; 115 StaleSymbolRenamed→valid false-Valid (line-presence escape — same rename pathology as the v1.2c sidecar's 6,727 WLB on StaleSymbolRenamed); 17 Valid→NR mis-routes via `guard_below_floor` carve-out |
| **R5** `module_recompiled` | — | **0** | Dead-in-chain on this subset (carries forward H5 from v1.2c sidecar) |
| **R7** `stale_symbol_renamed` | — | **0** | Dead-in-chain on this subset (carries forward H5 from v1.2c sidecar; R4 absorbs renames before R7 can fire) |
| **Total** | | **2,759** | |

R3 alone accounts for **485 of the 505 Valid-GT → non-Valid mis-routes** (96.0%) on the 2,759-row paired subset. The remaining 20 come from R4 (17 Valid→NR + 3 Valid→stale). Taming R3 on Plan A.2 Python is the single highest-leverage intervention for restoring §8 #3 generalization, exactly mirroring the v1.2c sidecar's finding at a smaller-N scale.

### Why this round's V-retention measurement is the same property as the v1.2c sidecar

The v1.2c sidecar measured V-retention 0.5193 point / 0.5182 WLB on n = 750,318 Valid-GT rows in the full Plan A.2 flask corpus, against canonicalized labeler GT. The v1.2d paired-subset measurement is 0.5144 point / 0.4841 WLB on n = 1,040 Valid-GT rows, against canonicalized labeler GT. The point estimates differ by 4.9 ppt and the WLBs by 3.4 ppt — well within sampling variance for an n = 1,040 estimate of a population at p ≈ 0.519.

Crucially: the v1.2c subset's PASS at 0.9981 WLB on the *same* phase1 binary and the *same* labeler was an artifact of the subset's class shape (2,000 Valid + 2,000 NR + 0 `Stale_*`, stratified against Plan A.1 demographics, with all 2,000 Valid-GT rows resolving via R2 `blob_identical` — R3 never saw them). v1.2d's re-stratification (1,500 Valid + Stale_*-balanced + 1,000 NR) restores R3's path on Valid-GT and exposes the regression. The two subset-paired results (v1.2c 0.9981 PASS vs. v1.2d 0.4841 FAIL) on the *same* underlying rules + corpus + labeler are the methodology lesson v1.2c documented and v1.2d's re-stratification was preregistered to address.

## Run details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.3` |
| Spec freeze hash (pre-Task-9; recorded in `run_meta.json`) | `41be7eb01c474a0a1faf69a139e4f52aa6e053e2668d230b2c22881102dc3b1f` (post-2026-05-19 §13.1 re-pin layer + v1.2c findings row) |
| Labeler git SHA (corpus + facts + diffs, Plan A.2) | `bf56f40999a5b3f026db517b196fa9d3a5724ded` (frozen since v1.2c — no labeler change in this branch) |
| Phase 1 git SHA | `c6b308fe0962a9e05dd70d2cf207c5fa405da25e` (workspace HEAD at run; phase1 source byte-identical to `1c117cdc54919c6531de8d96ecd85d3b77d56488` — no phase1 logic change in this branch; runner patch `c6b308f` touches `baseline/` only) |
| Scoring git SHA | `c6b308fe0962a9e05dd70d2cf207c5fa405da25e` (workspace HEAD at run; scoring source byte-identical to `541219a1f1fb98153cbd220582a23f165afe9474` — no scoring change in this branch) |
| Workspace HEAD SHA at run | `c6b308fe0962a9e05dd70d2cf207c5fa405da25e` (Task 7.5 in-branch runner patch; phase1 + scoring + labeler + SPEC byte-identical to the post-v1.2c freeze) |
| Held-out repo | `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = tag `2.0.0`) |
| flask HEAD at run | `9fcd34c9f3065640bd1cd86234216ca068633fb9` (T₀ + 401 first-parent commits; unchanged since v1.2b) |
| Subset baseline-run | `results/flask-heldout-2026-05-20-canary/baseline` (fresh v1.2d re-stratified subset; not symlinked) |
| Sample seed | `13897750829054410479` (pilot-matching, byte-identical to v1.2c and prior flask rounds) |
| Manifest subset size | 4,000 (1,500 V + 500/500/500 Stale_* + 1,000 NR) |
| Baseline rows with predictions | **2,759** (68.975% — partial run, see §"Partial-baseline disclosure" below) |
| Baseline parse failures | 134 (jsonl) |
| Phase1 row count | 2,759 (bound to `<baseline>/predictions.jsonl` via `--baseline-run`; the remaining 1,241 manifest rows have no LLM baseline prediction to pair against) |
| Phase1 stats (paired subset) | `processed: 2759, valid: 740, stale: 1987, needs_reval: 32` |
| Baseline cumulative cost | ≈ $37 estimated (H2 carry-forward: `predictions.jsonl` does not record per-row `cost_usd`; the published $1.25 in `baseline/run_meta.json.total_cost_usd` reflects only the 61-row resume run, not the 2,698-row initial run that aborted on 429) |

## Partial-baseline disclosure

This is the **first §9.4 round to ship as a documented partial baseline**. Sequence of events:

1. **Initial run** (`provbench-baseline run`, concurrency = 1, no rate-limit flag): 401 batches dispatched, ~2,698 predictions written, aborted at **67.5%** on an Anthropic 429 input-tokens-per-minute (ITPM) cap. The org-wide cap is 450,000 input-tok/min; the run was emitting bursts ~750,000 tok/min through Sonnet 4-6's request budget.
2. **Task 7.5 runner robustness patch** (`c6b308f`): two changes to the baseline crate (LLM oracle scope only, no §10 perimeter change):
   - **Fence-strip**: strip ```` ``` ```` / ```` ```json ```` markdown fences from the model's response before JSON parsing. Recovers ~5 of the 7 initial-run parse failures (the rest were structurally non-JSON content).
   - **ITPM throttle**: a `--max-input-tokens-per-minute` flag with default 300,000, well under the 450k org cap with parse-retry headroom.
3. **Resume run** (`provbench-baseline run` with `--resume`, concurrency = 1, ITPM throttle = 300k): completed with exit code 0 but emitted **127 of 130 attempted batches** as parse failures of the form `EOF while parsing a value at line 1 column 0` (empty `content[0].text` from the API). Only 61 new rows scored; **2,759 / 4,000 cumulative predictions** at end-of-resume.
4. **Diagnosis** (un-resolved): the runner extracts `payload["content"][0]["text"]` directly and does not log HTTP status or payload type. The empty-body cascade is therefore indistinguishable in the logs from "model returned empty content". Hypotheses: (a) Anthropic per-RPM cap returning fast 200s with non-content payloads (e.g. `type: error` shaped responses); (b) silent 4xx-non-429 errors after the ITPM throttle changed pacing; (c) connection-state issue post-throttle-sleep. The runner robustness gap is captured as new hygiene flag **H6** and addressed in forward path **η** below.
5. **Per user decision**, v1.2d ships as a documented partial run. The §8 verdict is computed on the 2,759-row paired subset via phase1 `--baseline-run`. Phase1 `score` does NOT support a manifest-only mode (frozen-surface decision), so a "full 4,000-row phase1 §8 verdict on the manifest" is not produced. The runner patch `c6b308f` ships in-branch for future rounds; addressing the empty-body root cause is deferred to v1.2e (forward path η).

## SPEC §10 anti-leakage attestation (8 items)

| # | Item | Result |
|---|---|---|
| 1 | **phase1 worktree clean** — `git diff --stat benchmarks/provbench/phase1/` returned empty at run time. phase1 source byte-identical to SHA `1c117cdc54919c6531de8d96ecd85d3b77d56488` (frozen since 2026-05-18). | ✅ |
| 2 | **scoring worktree clean** — `git diff --stat benchmarks/provbench/scoring/` returned empty at run time. scoring source byte-identical to SHA `541219a1f1fb98153cbd220582a23f165afe9474` (frozen since 2026-05-18). | ✅ |
| 3 | `provbench-labeler --version` == `bf56f40999a5b3f026db517b196fa9d3a5724ded` (Plan A.2, frozen since v1.2c corpus emission). | ✅ |
| 4 | flask HEAD = `9fcd34c9…` and 401 first-parent commits ahead of T₀ `2f0c62f5…` (same as v1.2b / v1.3 Plan A.1 / v1.2c; verified at run time). | ✅ |
| 5 | `tests/python_replay_changed_file.rs` passes (labeler determinism gate; Plan A.2 labeler unchanged). | ✅ |
| 6 | `tests/determinism_flask.rs` `#[ignore]` passes at chosen HEAD. | ✅ |
| 7 | Pre-commit generated-artifact check clean. | ✅ |
| 8 | `verify-tooling` passes (rust-analyzer + tree-sitter binary hashes match post-2026-05-19 §13.1 re-pin; tree-sitter-python tarball pin unchanged). | ✅ (PASS_WITH_NOTE: Task 7.5 baseline-runner patch `c6b308f` modifies `benchmarks/provbench/baseline/` only — fence-strip + ITPM throttle in the LLM oracle. The §10 frozen surface (phase1, scoring, labeler, SPEC.md) is byte-identical to the prior freeze. The runner patch sits OUTSIDE the anti-leakage perimeter — it changes how the LLM oracle is invoked, not the rules under test.) |

**Result: 8 / 8 PASS** (item 8 with PASS_WITH_NOTE for the baseline-oracle scope of `c6b308f`). No rule retuning was performed in-round; phase1, scoring, labeler, and the SPEC body are byte-identical to the post-v1.2c freeze.

## Hygiene flags

### H1: `wall_ms` not populated in `predictions.jsonl` (v1.2b A.3 / v1.3 Plan A.1 / v1.2c carry-forward)

Every row in `phase1/predictions.jsonl` has `wall_ms: 0`. Consequently `latency_p50_ms = 0` and §8 #4 PASSES vacuously (`0 ≤ 727`). Forward path ε.

### H2: `predictions.jsonl` does not record per-row `cost_usd` (v1.2c carry-forward; now blocking partial-run cost accounting)

Carry-forward from v1.2c. v1.2d additionally surfaces this as a problem for **any** partial-run cost accounting: the published `total_cost_usd = $1.249917` in `baseline/run_meta.json` reflects only the 61-row resume run that produced 134 parse failures and 61 successful predictions; it does NOT include the 2,698-row initial run that aborted on 429. The ≈ $37 cumulative cost in the TL;DR is an *estimate* projected from the per-row cost in prior flask rounds (~$0.014/row at concurrency 1) and is not directly readable from the in-tree artifacts. Forward path ζ.

### H3: R3 over-fires on Plan A.2 Valid-GT Python (v1.2c carry-forward; re-confirmed at sample-truth scale)

v1.2c sidecar finding: R3 fires 460,359× on the full corpus, 343,284 of which are false-Stale on Valid-GT. v1.2d re-confirms at sample-truth scale: R3 fires 1,597× on the 2,759-row paired subset, 485 of which are false-Stale on Valid-GT (96.0% of all Valid-GT mis-routes; 30.4% of all R3 fires). The proportional rate matches: v1.2c sidecar 343,284 / 460,359 = 74.6%; v1.2d paired 485 / 1,597 = 30.4% — but the v1.2d subset over-samples NR (1,000/4,000 = 25%) and `Stale_*` (1,500/4,000 = 37.5%) vs. population shares (0.7% / 16.9%), so the absolute false-positive rate per Valid-GT row is the directly comparable number: v1.2c sidecar 343,284 / 750,318 = **45.8%** false-Stale on Valid-GT; v1.2d subset 485 / 1,040 = **46.6%** false-Stale on Valid-GT. The two estimates agree within 0.8 ppt.

### H4: NR routing accuracy = 0 on Plan A.2 NR-GT (v1.2c carry-forward; re-confirmed)

`metrics.json.phase1-v1.3.section_7_1.needs_revalidation_routing_accuracy = {point: 0.0, wilson_lower_95: 0.0}` on n = 693 paired NR-GT. The 1,000-NR stratum was designed to fully exercise the R4 NR carve-out; the carve-out catches 0 of the 693 paired NR-GT rows. Compare to v1.2c sidecar 0/6,652 and v1.2c subset 45/2,000. The carve-out is structurally biased toward the Plan A.1 labeler's NR class definition. Forward path δ.

### H5: R5 and R7 dead-in-chain on Plan A.2 Python (v1.2c carry-forward; re-confirmed)

R5 (`module_recompiled`) and R7 (`stale_symbol_renamed`) fire 0× on the 2,759-row paired subset (consistent with v1.2c sidecar 0/910,530). The StaleSymbolRenamed slice (paired n = 355) is absorbed by R4 line-presence escape — `StaleSymbolRenamed__valid` 115 fires. Forward path γ.

### H6 (NEW v1.2d): Baseline runner does not log HTTP status / payload type on parse failure

The runner extracts `payload["content"][0]["text"]` directly. On non-2xx-non-429 responses or on content-array-without-text-field responses, the parse-failure log entry is indistinguishable from "model returned empty content". v1.2d's 127-empty-body resume cascade is therefore un-diagnosed in the artifacts. The Task 7.5 patch `c6b308f` (fence-strip + ITPM throttle) did NOT address this layer. Forward path η.

### H7 (NEW v1.2d): Phase1 `score` requires `--baseline-run` and binds phase1's evaluation surface to the baseline's predicted row set

Phase1 `score` does not support a manifest-only mode; partial baselines force partial phase1 evaluations even though phase1 is deterministic and could score the full manifest. v1.2d's §8 verdict is on n = 2,759, not n = 4,000, as a direct consequence. Closing H7 requires touching the §10 frozen surface (phase1 binary). Forward path θ.

### Resolver-coverage gaps (six documented at PR #50; **carry forward**)

The six documented Plan A.2 labeler resolver coverage gaps from PR #50 (re-noted in v1.2b PR #53 and v1.2c findings) remain. v1.2d's R3 over-firing on Valid-GT is consistent with these gaps propagating to phase1's symbol-resolution path.

## What this round contributes

1. **First v1.3 informative §8 #5 verdict on a `Stale_*`-bearing Python held-out subset**: §8 #5 WLB = 0.8206 PASS with 52-pp cushion. Closes the v1.2c-and-earlier SKIP/structural-FAIL pattern on flask Python.
2. **Sample-truth confirmation of the v1.2c sidecar V-retention regression**: §8 #3 WLB = 0.4841 on n = 1,040 paired Valid-GT, within 0.8-pp of the v1.2c sidecar's population estimate (45.8% vs. 46.6% false-Stale-on-Valid rate). The V-retention regression is a real property of v1.3 rules on Plan A.2 facts, not an artifact of the v1.2c subset's class shape.
3. **Demonstrates the v1.2c forward-path-α re-stratification methodology works** — and consumes the §13.2 flask leakage budget paying for it.
4. **Surfaces a baseline-runner robustness gap (H6) that is invisible until a real rate-limit failure** — and ships a structurally-bounded patch (`c6b308f`, baseline-oracle scope only) that addresses two of the three contributing causes (markdown-fence content, ITPM bursts) while leaving the empty-body root cause for v1.2e.
5. **Surfaces a phase1 `score` design constraint (H7) under partial baselines** — and pre-registers the manifest-only mode as forward path θ.

## Forward paths

Pre-registered ideas (not in-round retunings; SPEC §10 forbids in-round tuning on a held-out result):

- **(α) [PRIORITY] R3 `symbol_missing` retuning on a pilot/ripgrep corpus.** v1.2d's §8 #3 FAIL at sample-truth scale re-confirms v1.2c's sidecar finding. R3 must be tuned on ripgrep (NOT flask — that consumes the held-out budget per §13.2) and re-validated on a future flask round. Likely fixes: (a) relax the symbol-missing decision when the post-commit blob is structurally identical to the t0 blob for the symbol's containing region; (b) close the six PR #50 labeler resolver coverage gaps so phase1 and the labeler agree on symbol presence; (c) add a "symbol-was-renamed" probe that lets R3 yield to R7 (which is currently dead-in-chain).
- **(β) Investigate R3 over-firing mechanism on Plan A.2 facts.** Open question: is the over-firing driven by (i) labeler-vs-phase1 resolver disagreement (PR #50 gaps propagating), (ii) Plan A.2 emitting `Stale_*` GT on rows where the symbol IS present in post-commit but its body changed (i.e., R3 fires correctly on the symbol-missing heuristic but the labeler's GT for the row is Valid because the change is structurally benign), or (iii) some interaction between R3's lookup path and Plan A.2's symbol naming. Pilot diagnostic before α.
- **(γ) Move R7 ahead of R4 in the chain, OR rewrite R7's proxy to fire on Plan A.2 rename-emitted facts.** Targets the 9,508-row StaleSymbolRenamed slice with 0.6822 recall on the v1.2c sidecar (and the 115-row R4 line-presence absorption on v1.2d). Pilot tuning only.
- **(δ) Re-evaluate R4 NR carve-out (PR #60 design) against Plan A.2 NR class definition.** v1.2d's 0-row catch on n = 693 NR-GT confirms the carve-out is structurally biased toward Plan A.1's NR class (the Plan A.1 labeler's NR class included `Stale_*` rows that Plan A.2 now emits separately, and the carve-out's `guard_below_floor: true` condition was tuned for that older class shape). Re-design needed.
- **(ε) Repopulate phase1 per-row `wall_ms` to close H1.**
- **(ζ) Record per-row baseline `cost_usd` in `predictions.jsonl` to close H2.** Would have given a real cost number for v1.2d's partial run instead of the ≈ $37 estimate.
- **(η) [NEW v1.2d] Baseline runner robustness — record HTTP status + raw payload shape on parse failure.** Treat non-2xx as retryable (not just 5xx/429). Add `--max-requests-per-minute` to complement `c6b308f`'s ITPM throttle. Would unblock determining the root cause of v1.2d's empty-body cascade. Baseline-oracle scope only; does not touch §10 perimeter.
- **(θ) [NEW v1.2d] Add a `--manifest-only` mode to phase1 `score`** so partial-baseline rounds can still produce a full 4k phase1 §8 verdict alongside the paired-subset numbers. Closes H7. This requires touching the §10 frozen surface and must be done between rounds, not in-round.

## Reference paths

- Subset metrics: `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/metrics.json`
- Phase1 meta (with `baseline_partial` + `runner_patch_in_round` + scope notes): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/phase1/run_meta.json`
- Baseline meta (resume-run only — total cost reflects 61-row tail, not initial 2,698-row run): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/baseline/run_meta.json`
- Baseline predictions (n = 2,759): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/baseline/predictions.jsonl`
- Baseline parse failures (n = 134, jsonl): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/baseline/parse_failures.jsonl`
- Phase1 predictions (n = 2,759): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/phase1/predictions.jsonl`
- Phase1 rule traces (n = 2,759): `benchmarks/provbench/results/flask-heldout-2026-05-20-canary/phase1/rule_traces.jsonl`
- Task 7.5 runner robustness patch: commit `c6b308fe0962a9e05dd70d2cf207c5fa405da25e` (baseline crate: fence-strip + ITPM throttle)
- Prior v1.2c flask Plan A.2 findings (PR #66 — round this generalizes from): `benchmarks/provbench/results/flask-heldout-2026-05-19-findings.md`
- Prior v1.2c full-corpus sidecar (V-retention 0.5182 WLB on n = 750,318): `benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/sidecar_metrics.json`
- Prior v1.3 serde findings (PR #62 — first v1.3 PASS-PASS-PASS): `benchmarks/provbench/results/serde-heldout-2026-05-18-findings.md`
- v1.2b flask Plan A.1 findings (predecessor Python round): `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`
- SPEC §11 row dated 2026-05-20 §9.4 (record only) appended in this PR.
