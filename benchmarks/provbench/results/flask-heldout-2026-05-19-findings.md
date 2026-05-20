# ProvBench Phase 1 (rules) — 2026-05-19 flask held-out findings (`rule_set_version v1.3`, Plan A.2 Python labeler)

## TL;DR — read this first

This is the **first v1.3 held-out round on the Plan A.2 Python labeler** (`bf56f40…`, post-PR #65 merge). The headline result is **not** the SPEC §8 verdict.

1. The SPEC §8 verdict is **PASS-PASS-SKIP** and is **byte-identical** to v1.3 flask Plan A.1 (PR #61, 2026-05-18). It is uninformative about the Plan A.2 labeler change because the frozen v1.2b 4,000-row baseline subset (re-used per §10) contains **zero `Stale_*` ground-truth rows**, so §8 #5 SKIPs and §8 #3 / #4 land on the same numbers as the prior round.
2. The **full-corpus phase1 sidecar** (910,530 rows, NOT a §8 verdict) reveals the load-bearing new finding: **phase1 v1.3 over-fires R3 (`stale_source_deleted`) on Valid-GT Python rows**. Valid retention against the full Plan A.2 population is **0.5193 (WLB 0.5182)** — a margin that would FAIL §8 #3 by ≈ 43 pp if scored as a verdict. R3 alone produces 343,284 false `Stale` predictions on Valid ground truth (~93% of the false-positive Stale volume).
3. The Plan A.2 labeler **does** unlock informative Stale-rule evaluation: full-corpus Stale recall is **0.9269 (WLB 0.9256)** — a margin that would PASS §8 #5 with a wide cushion. Subtype recall: `StaleSourceChanged 0.9532`, `StaleSourceDeleted 0.9305`, `StaleSymbolRenamed 0.6822`. Renames are the weakest of the three subtypes.
4. NR routing accuracy on real `NeedsRevalidation` ground truth in the full corpus is **0.0** (n = 6,652). R3 absorbs 6,052 NR-GT rows as `stale`; R4 absorbs 600 as `valid`. The v1.3 R4 NR carve-out catches zero NR-GT rows on the full Plan A.2 flask corpus (vs. 45/2,000 on the v1.2b frozen subset).
5. R5 (`module_recompiled`) and R7 (`stale_symbol_renamed`) never fire on this corpus — neither on the 4k subset nor on the 910k full population. R7 carries its own subtype that the chain cannot reach via its proxy on Python.
6. The methodology lesson: **re-using a frozen baseline subset across labeler revisions hides labeler-change effects from the §8 verdict layer**. The 4k subset's class demographics (2,000 Valid + 2,000 NR + 0 Stale) come from the Plan A.1 labeler and are no longer representative of the Plan A.2 corpus distribution (824k Valid + 153k Stale + 6.6k NR after canonicalization, ratios ~ 90% / 9% / 0.7%). This round's value is uncovering both the V-retention regression and this blind spot — not a §8 PASS.

## Thesis under test (original) — and how it landed

**Original v1.2c thesis (held over from the v1.2b → v1.3 flask findings doc):** Plan A.2 closes the H3 gap on §8 #5 — flask should now produce informative §8 #5 numbers because the Plan A.2 labeler emits real `Stale_*` ground truth on Python instead of the Plan A.1 `NeedsRevalidation` short-circuit.

**Where the thesis landed:**

- **At the labeler layer:** ✅ supported. The Plan A.2 labeler emits 153,560 `Stale_*` ground-truth rows over the 910,530-row flask corpus (~16.9% population share). Phase1 v1.3 detects them with recall 0.9269 WLB — the expected §8 #5 win exists, on the full corpus.
- **At the §8 verdict layer:** ✗ NOT supported. The frozen v1.2b baseline subset (re-used via symlink) was stratified against the Plan A.1 labeler's class distribution, which produced 2,000 Valid + 2,000 NeedsRevalidation + 0 `Stale_*` rows. The Plan A.2 labeler's `Stale_*` emission does not reach the §8 #5 evaluator because the subset has zero Stale GT to evaluate against. §8 #5 remains `SKIP` (`ground_truth_stale_count_is_zero`), byte-identical to v1.3 Plan A.1.
- **Surprise finding:** ❗ phase1 v1.3 over-fires R3 on Valid Python rows. The 4k frozen subset has only 2,000 Valid-GT rows and R3 fires zero times on Valid in the subset (per `metrics.json.per_rule_confusion`: R2 absorbs all 2,000 Valid as `valid__valid`). On the full 910k corpus R3 fires on 343,284 Valid-GT rows as false `stale`. R3's behavior on Valid was never tested at the §8 verdict layer in any prior flask round because the labeler's `Stale_*` GT was empty (Plan A.1) and the 4k subset's Valid rows are stratification-filtered into R2's domain.

The round's *contribution* is therefore methodological (uncovering the frozen-subset blind spot) and diagnostic (the R3 over-firing regression) — not a clean §8 PASS.

## SPEC §8 threshold verdict — **PASS-PASS-SKIP** (record only; byte-identical to v1.3 Plan A.1)

> ⚠ This §8 verdict is byte-identical to the v1.3 flask Plan A.1 round (PR #61, 2026-05-18 findings doc) at every observed value. The Plan A.2 labeler change does **not** propagate to the §8 verdict layer in this round because the 4,000-row baseline subset was frozen against the Plan A.1 labeler's class demographics. **Use the full-corpus sidecar in §3 below as the load-bearing evidence on Plan A.2's effect.**

| Threshold | Required | Observed (4k subset, v1.3 + Plan A.2) | Pass? |
|---|---|---|:---:|
| §8 #3 valid retention WLB | ≥ 0.95 | **0.9980829526885622** | ✅ |
| §8 #4 latency p50 (per-row, ms) | ≤ 727 | 0 (vacuous — H1 carry-forward) | ✅ |
| §8 #5 stale recall WLB | ≥ 0.30 | `SKIP` — `ground_truth_stale_count_is_zero` | ⏭ SKIP |

The 4k subset's confusion matrix is byte-identical to the v1.3 Plan A.1 round (processed: 4000, valid: 2708, stale: 1247, needs_reval: 45). See §6 below for the matrix and §7 for per-rule confusion.

## Full-corpus sidecar — primary new evidence (910,530 rows; NOT a §8 verdict)

The sidecar runs phase1 v1.3 against the **full** Plan A.2 flask corpus (`benchmarks/provbench/corpus/flask-2f0c62f5-bf56f40.jsonl`, 910,530 rows) and scores predictions against the labeler's per-row canonical labels. Baseline is a synthetic `valid`-everywhere file (`benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/baseline/predictions.jsonl`, 274 MB, gitignored) that exists only to pin phase1's `--baseline-run` loader to the 910k corpus — its `prediction` column is meaningless and **all LLM-baseline columns must be ignored**. The `ground_truth` column in the synthetic predictions equals the canonicalized corpus label, which is what we score against.

Authoritative metrics: `benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/sidecar_metrics.json`.

### Sidecar §7.1 three-way table (full corpus, canonicalized to {valid, stale, needs_revalidation})

| Metric | Point | Wilson LB | n |
|---|---:|---:|---:|
| **Valid retention** | **0.5193** | **0.5182** | 750,318 |
| Stale detection recall | **0.9269** | **0.9256** | 153,560 |
| Stale detection precision | 0.2880 | — | 494,313 (predicted stale) |
| Stale detection F1 | 0.4394 | — | — |
| NR routing accuracy | **0.0000** | **0.0000** | 6,652 |

If these numbers were a §8 verdict (they are not — the sidecar lacks a frozen pre-registered subset), the round would land as **FAIL-PASS-PASS**: §8 #3 fails by ≈ 43 pp; §8 #4 is unmeasured at sidecar level (H1 carry-forward); §8 #5 would clear with a 63-pp cushion above the 0.30 threshold.

### Sidecar per-stale-subtype recall (full corpus)

| Subtype | Point | Wilson LB | n |
|---|---:|---:|---:|
| `StaleSourceChanged` | **0.9532** | **0.9517** | 79,699 |
| `StaleSourceDeleted` | **0.9305** | **0.9285** | 64,353 |
| `StaleSymbolRenamed` | **0.6822** | **0.6727** | 9,508 |

Rename detection is **27–28 pp weaker** than the other two subtypes. The renames slip through R4's line-presence probe: when a symbol is renamed but the source line text remains substantially intact, R4's `t0_span_found_in_post` probe triggers a conservative escape to `valid`. R7 (`stale_symbol_renamed`) does **not** rescue these cases in the full corpus — R7 fires zero times across all 910k rows.

### Sidecar full-corpus confusion matrix (canonicalized GT × phase1 prediction)

| GT \ Pred | `valid` | `stale` | `needs_revalidation` | total |
|---|---:|---:|---:|---:|
| `Valid` | **389,642** | 345,923 | 14,753 | 750,318 |
| `Stale_*` (any subtype) | 8,579 | **142,338** | 2,643 | 153,560 |
| `NeedsRevalidation` | 600 | 6,052 | **0** | 6,652 |
| total | 398,821 | 494,313 | 17,396 | 910,530 |

Row-level reading:

- **Valid-GT (n = 750,318):** 51.93% retained as `valid`; **46.10% false-Staled**; 1.97% false-NR'd. The false-Stale volume (345,923 rows) is the headline regression. These are real Valid facts being invalidated by the rule chain.
- **Stale-GT (n = 153,560):** 92.69% correctly classified as `stale`; 5.59% false-Valid'd; 1.72% false-NR'd. The Stale signal is strong overall, modulated by the rename weakness above.
- **NR-GT (n = 6,652):** 0% correctly routed to NR; 91.0% absorbed into `stale` (R3-dominated); 9.0% absorbed into `valid` (R4-dominated). The R4 NR carve-out introduced in v1.3 catches zero NR-GT rows on this corpus.

### Sidecar per-rule confusion (full corpus)

From `sidecar_metrics.json.per_rule_confusion`:

| Rule | Cell (GT __ Pred) | Count | Comment |
|---|---|---:|---|
| **R3** `stale_source_deleted` | `Valid__stale` | **343,284** | dominant false-positive driver; ~93% of Valid-Stale FPs |
| R3 | `Stale_*__stale` | 113,020 | correct catches; ~74% of all correct Stale calls |
| R3 | `NeedsRevalidation__stale` | 4,055 | mis-routes NR-GT into stale |
| R3 **total** | | **460,359** | fires on ~50.6% of all rows |
| **R4** `span_hash_changed` | `Valid__valid` | 246,882 | conservative line-presence escape; bulk of preserved Valids |
| R4 | `Stale_*__stale` | 22,738 | second-tier Stale catches via `stale_source_changed` |
| R4 | `Stale_*__valid` | **8,579** | false-Valid escape via line-presence — the rename leak |
| R4 | `Valid__needs_revalidation` | 14,753 | guard_below_floor carve-out misfires on Valid-GT |
| R4 | `Valid__stale` | 2,639 | ambiguous guard-passing FPs |
| R4 | `NeedsRevalidation__valid` | 600 | t0_span_found_in_post over-rules NR intent |
| R4 | `Stale_*__needs_revalidation` | 2,643 | guard_below_floor on Stale-GT rows |
| R4 | `NeedsRevalidation__stale` | 1,997 | guard-passing stale_source_changed |
| R4 **total** | | **300,831** | fires on ~33.0% of all rows |
| **R2** `blob_identical` | `Valid__valid` | **142,760** | all-correct; ~15.7% of corpus is blob-identical |
| R1 `source_file_missing` | `Stale_*__stale` | 6,580 | all-correct deletions |
| R5 `module_recompiled` | — | **0** | dead-in-chain on this corpus |
| R7 `stale_symbol_renamed` | — | **0** | dead-in-chain on this corpus |
| **Total** | | **910,530** | |

R3 alone accounts for **343,284 of the 360,876 Valid → non-Valid mis-routes** (95.1%). The remaining 17,592 Valid → non-Valid mis-routes come from R4 (17,392 — 14,753 false-NR + 2,639 false-Stale; plus 600 R4-NR-GT-to-Valid which is a different direction). Taming R3 is the single highest-leverage intervention for restoring §8 #3 generalization on the full Plan A.2 Python corpus.

### Why the §8 verdict is invariant to the labeler change in this round

This round re-uses the frozen v1.2b baseline subset via symlink (`results/flask-heldout-2026-05-19-canary/baseline → ../flask-heldout-2026-05-15-canary/baseline`) per §10 anti-leakage and per the SPEC §11 2026-05-18 row's `baseline reused via symlink to frozen v1.2b dir per §10` clause. The subset was stratified against the **Plan A.1** labeler's class distribution: 2,000 Valid + 2,000 `NeedsRevalidation` + 0 `Stale_*`. Re-using that subset against the Plan A.2 corpus means:

- The 4k Valid-GT slice is drawn from rows the Plan A.1 labeler classified Valid. Plan A.2 only refines the changed-file path; rows the Plan A.1 labeler classified Valid (because R2 `blob_identical` fires) are still Valid under Plan A.2. Those 2,000 rows land in R2's domain at evaluation time. R3 never sees them.
- The 4k NR-GT slice is drawn from rows the Plan A.1 labeler emitted as `NeedsRevalidation`. The Plan A.2 labeler re-classifies many of these into `Stale_*` subtypes — but **the subset still loads them with the Plan A.1 GT label**, because the predictions file under `baseline/` was generated against the Plan A.1 labeler. The scoring loader pairs the Plan A.1 GT against the v1.3 phase1 prediction, producing the byte-identical 4k matrix.
- The 4k subset has **zero Stale_*` GT** because the Plan A.1 labeler's stratification design produces 2,000+2,000+0 here. §8 #5 SKIPs by construction.

The fix for a future round (v1.2d) is to **re-stratify a fresh 4k subset against the Plan A.2 corpus distribution** (or expand to a `Stale_*`-balanced sub-population) and re-run the LLM baseline against that subset, then re-pin the symlink. This consumes the §13.2 flask leakage budget — exactly the §10 cost the labeler-revision drift was supposed to pay.

## Run details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.3` |
| Spec freeze hash (§15 base + §13.1 re-pin layer) | `cd881a32c410a635074d6cec92b31d14382b8a0f1425789d2584f00fe9bacb30` (post-2026-05-19 §13.1 re-pin) |
| Labeler git SHA (corpus + facts + diffs, Plan A.2) | `bf56f40999a5b3f026db517b196fa9d3a5724ded` (post-PR #65 merge + §13.1 RA-hash re-pin; frozen since artifact emission) |
| Phase 1 git SHA | `1c117cdc54919c6531de8d96ecd85d3b77d56488` (unchanged since v1.3) |
| Scoring git SHA | `541219a1f1fb98153cbd220582a23f165afe9474` (unchanged since v1.3) |
| Workspace HEAD SHA at run | `bf56f40999a5b3f026db517b196fa9d3a5724ded` (4k subset run) / `bf56f40999a5b3f026db517b196fa9d3a5724ded` (full-corpus sidecar) |
| Held-out repo | `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = tag `2.0.0`) |
| flask HEAD at run | `9fcd34c9f3065640bd1cd86234216ca068633fb9` (T₀ + 401 first-parent commits) |
| 4k subset baseline-run | `results/flask-heldout-2026-05-19-canary/baseline` → symlink to `../flask-heldout-2026-05-15-canary/baseline` (frozen v1.2b dry-run carrier) |
| Full-corpus baseline-run | `results/flask-heldout-2026-05-19-fullcorpus/baseline` (synthetic; `request_id='synthetic-fullcorpus'`; LLM columns must be ignored) |
| Sample seed (4k subset) | `13897750829054410479` (`0xC0DEBABEDEADBEEF`, pilot-matching, byte-identical to v1.3 Plan A.1 round) |
| Sample seed (full corpus) | `none (full corpus)` |
| 4k subset size | 4,000 |
| Full-corpus row count | 910,530 |
| Phase1 stats (4k subset, stderr) | `processed: 4000, valid: 2708, stale: 1247, needs_reval: 45, evidence_parse_failures: 0` |
| Phase1 stats (full corpus, stderr) | `processed: 910530, valid: 398821, stale: 494313, needs_reval: 17396, wall_seconds: 840` |
| Phase1 wall time (full corpus) | ~840 s (~14 min) on the labeler-host hardware |

## SPEC §7.1 three-way table (4k subset, n = 4,000) — **byte-identical to v1.3 Plan A.1**

From `flask-heldout-2026-05-19-canary/metrics.json.phase1_rules_v1.3_plan_a2.section_7_1`:

| Metric | Point | Wilson LB |
|---|---:|---:|
| Stale detection recall | **0.0** | **0.0** |
| Stale detection precision | 0.0 | — |
| Stale detection F1 | 0.0 | — |
| Valid retention accuracy | **1.0** | **0.9980829526885622** |
| Needs_revalidation routing accuracy | **0.0225** | **0.01685787544578196** |

Same root cause as v1.3 Plan A.1: stale-detection numerator and denominator are both zero (subset has no `Stale_*` GT rows). NR routing accuracy is 45/2,000 — the R4 `guard_below_floor: true` carve-out catches 45 of 2,000 NR-GT rows.

## Confusion matrix — 4k subset

From the 4k `metrics.json.llm_baseline.confusion_matrix_3x3` joined with `phase1_rules_v1.3_plan_a2`:

| GT \ Pred | `valid` | `stale` | `needsrevalidation` | total |
|---|---:|---:|---:|---:|
| `Valid` | **2,000** | 0 | 0 | 2,000 |
| `NeedsRevalidation` | 708 | **1,247** | **45** | 2,000 |
| `Stale_*` (any subtype) | 0 | 0 | 0 | **0** |
| total | 2,708 | 1,247 | 45 | 4,000 |

This matrix is byte-identical to the v1.3 Plan A.1 round (PR #61) including the 45-NR cell. No labeler-change effect reaches this subset.

## Per-rule confusion — 4k subset

From `flask-heldout-2026-05-19-canary/metrics.json.per_rule_confusion`:

| Rule | Outcome | Count | Note |
|---|---|---:|---|
| R1 `source_file_missing` | `needsrevalidation__stale` | 13 | NR-GT rows misrouted to `stale` |
| R2 `blob_identical` | `valid__valid` | 2,000 | All Valid GT classified correctly via R2 |
| R3 `symbol_missing` | `needsrevalidation__stale` | 1,175 | dominant `stale` driver on NR-GT |
| R4 `span_hash_changed` (line-presence probe) | `needsrevalidation__needs_revalidation` | 45 | v1.3 NR carve-out (`guard_below_floor: true`) |
| R4 `span_hash_changed` (stale_source_changed probe) | `needsrevalidation__stale` | 59 | guard-passing, probe absent → `stale` |
| R4 `span_hash_changed` (t0_span_found_in_post) | `needsrevalidation__valid` | 708 | line bytes still present → escapes as `valid` |

Total: 2,000 + 13 + 1,175 + 45 + 59 + 708 = 4,000. Identical to v1.3 Plan A.1.

The contrast between the 4k subset's per-rule table and the 910k sidecar's per-rule table is the methodology lesson of this round: **the 4k subset does not exercise R3 on Valid-GT rows at all** (every Valid-GT row resolves via R2's `blob_identical` short-circuit). The R3 over-firing pathology is invisible at this scope.

## Side-by-side: 4k subset vs full-corpus sidecar

| Metric | 4k subset (v1.3 Plan A.2) | 910k full corpus (v1.3 Plan A.2) | Comment |
|---|---:|---:|---|
| Valid retention (point) | **1.0** | **0.5193** | Subset hides R3's over-firing on Valid-GT |
| Valid retention (WLB) | **0.998083** | **0.518172** | Would FAIL §8 #3 by ≈ 43 pp if scored |
| Stale recall (point) | n/a (0/0) | **0.9269** | Subset has 0 Stale GT; sidecar reveals strong recall |
| Stale recall (WLB) | n/a (0/0) | **0.9256** | Would PASS §8 #5 with 63-pp cushion if scored |
| NR routing (point) | **0.0225** | **0.0000** | Subset's 45 hits are an artifact of Plan A.1's NR-GT slice; full corpus has different NR-GT class |
| Stale precision | n/a | 0.2880 | Drag from R3 over-firing on Valid-GT |
| Stale F1 | n/a | 0.4394 | Recall-vs-precision skew |

## SPEC §10 anti-leakage attestation (8 items)

| # | Item | Result |
|---|---|---|
| 1 | **phase1 worktree clean** — `git diff --stat benchmarks/provbench/phase1/` returned empty (0 lines) at both 4k and full-corpus run times. phase1 source byte-identical to SHA `1c117cdc54919c6531de8d96ecd85d3b77d56488`. | ✅ |
| 2 | **scoring worktree clean** — `git diff --stat benchmarks/provbench/scoring/` returned empty (0 lines) at run time. scoring source byte-identical to SHA `541219a1f1fb98153cbd220582a23f165afe9474`. | ✅ |
| 3 | `provbench-labeler --version` == `bf56f40999a5b3f026db517b196fa9d3a5724ded` (Plan A.2, frozen since corpus + facts + diffs emission for this round). | ✅ |
| 4 | flask HEAD = `9fcd34c9…` and 401 first-parent commits ahead of T₀ `2f0c62f5…` (same as v1.2b and v1.3 Plan A.1 rounds; verified at run time). | ✅ |
| 5 | `tests/python_replay_changed_file.rs` passes (labeler determinism gate; Plan A.2 labeler). | ✅ |
| 6 | `tests/determinism_flask.rs` `#[ignore]` passes at chosen HEAD. | ✅ |
| 7 | Pre-commit generated-artifact check clean. | ✅ |
| 8 | `verify-tooling` passes (rust-analyzer + tree-sitter binary hashes match post-2026-05-19 §13.1 re-pin; tree-sitter-python tarball pin unchanged). | ✅ (PASS_WITH_NOTE: §13.1 RA hash was re-pinned in commit `bf56f40` to recover the gate after a local rustup re-fetch produced byte-different `rust-analyzer` bytes at the same upstream version. Re-pin is record-only — labeler logic, replay, and scoring are byte-identical to pre-re-pin sources. See SPEC §11 row 2026-05-19 §13.1.) |

**Result: 8 / 8 PASS** (item 8 with PASS_WITH_NOTE). No rule retuning was performed in-round. The §13.1 re-pin in Task 0 of this branch (`bf56f40`) restored the tooling-hash gate without touching labeler logic, replay, or scoring — the post-fix `verify-tooling` PASS is what allowed both this round's 4k run and the full-corpus sidecar to execute.

## Hygiene flags

### H1: `wall_ms` not populated in `predictions.jsonl` (v1.2b A.3 / v1.3 Plan A.1 carry-forward)

Every row in both `predictions.jsonl` files (4k and full-corpus) has `wall_ms: 0`. Consequently `latency_p50_ms = 0` in the 4k `metrics.json` and the full-corpus sidecar reports no latency. §8 #4 PASSES vacuously (`0 ≤ 727`). The full-corpus wall-time (~840 s for 910k rows) is captured in the sidecar's `phase1_runner_stats.wall_seconds` field, but per-row latency is still missing. Forward path: repopulate `wall_ms` in a future phase1 release. Not blocking for this round.

### H2: `phase1` binary does not emit `run_meta.json` (v1.2b convention)

Both rounds' `run_meta.json` files (`flask-heldout-2026-05-19-canary/phase1/run_meta.json` and `flask-heldout-2026-05-19-fullcorpus/phase1/run_meta.json`) are hand-written. Convention not regression; same pattern as v1.2b / v1.3 Plan A.1.

### H3: R3 absorbs 60.7% of full-corpus Stale-GT recall but also drives the V-retention collapse (**ELEVATED + RE-CHARACTERIZED**)

Previously framed as "R3 absorbs 58.75% of Python NR-GT rows on flask" (v1.3 Plan A.1 hygiene flag H3 on the 4k subset). The full-corpus sidecar re-characterizes the finding:

- **R3 fires on 460,359 rows (50.6% of the full corpus)** — by far the most-firing rule.
- **Of those, 113,020 are correct Stale catches** (73.6% of all correct Stale predictions in the corpus).
- **343,284 are false-Stale on Valid-GT** (94.3% of the Valid-Stale FP volume).
- 4,055 are false-Stale on NR-GT.

The previously-flagged Plan A.1 NR-vs-Stale taxonomy mismatch is closed (Plan A.2 emits real `Stale_*` GT). The new and more serious finding is that R3's `symbol_missing` heuristic — designed for the case where a symbol has truly been deleted — fires false-positively on a very large fraction of Plan A.2-flagged Valid rows. The likely cause is that the labeler's resolver coverage gaps documented at PR #50 (six documented gaps; carried forward as hygiene flags from v1.2b through v1.3 Plan A.1) produce Valid-GT rows whose symbols cannot be located at post-commit revision by phase1's R3 lookup, even though the labeler correctly classified them Valid via a different code path. R3 then mis-routes these to `stale`. The 2026-05-19 sidecar is the first evidence that this resolver-coverage drift dominates the Valid-GT path on the full Plan A.2 corpus.

### H4: NR routing collapses to 0% on real NR-GT in the full corpus (**NEW**)

On the full Plan A.2 corpus, the 6,652 NR-GT rows split: 6,052 → R3-stale (91.0%), 600 → R4-valid (9.0%), 0 → R4-NR-carve-out. The v1.3 R4 NR carve-out catches zero NR-GT rows on the full corpus, vs. 45/2,000 on the v1.2b frozen subset. The full-corpus NR-GT class has structurally different rule-firing demographics from the subset's NR-GT slice — the carve-out's design works on the Plan A.1 labeler's NR-as-residual-class output, but the Plan A.2 labeler's NR class (rows that genuinely need re-labeling) has a different distribution where R3 short-circuits before R4 sees the row.

### H5: R5 and R7 are dead-in-chain on this corpus (**NEW; verified on 910k rows**)

R5 (`module_recompiled`) and R7 (`stale_symbol_renamed`) fire zero times across the full 910,530-row Plan A.2 flask corpus. R7's dead-in-chain status was previously suggested by per-rule data on the 4k subset (no R7 cells); the sidecar confirms it at full scope. R7 is the obvious candidate to rescue the StaleSymbolRenamed subtype recall (0.6822 — the weakest of the three Stale subtypes), but the chain ordering and proxy design currently let R4's line-presence probe absorb rename rows before R7 can fire. Forward path candidate for v1.2d.

### Resolver-coverage gaps (six documented at PR #50; **carry forward**)

The six documented Plan A.2 labeler resolver coverage gaps from PR #50 (and re-noted in the v1.2b v1.2b-flask-Plan-A.2 findings, PR #53) remain. The sidecar's R3 over-firing on Valid-GT is consistent with these gaps propagating to phase1's symbol-resolution path. No new resolver fix is in scope for this round.

## What this round contributes

1. **First v1.3 held-out round on the Plan A.2 Python labeler.** Establishes that the Plan A.2 labeler runs through the full pipeline end-to-end on a held-out repo, emits 910,530 corpus rows, and produces phase1 predictions and rule traces in ~840 s on the labeler-host hardware.
2. **Documents a methodology blind spot.** The §10 frozen-baseline-subset re-use protocol (designed to control LLM-cost leakage and rule-tuning drift) inadvertently hides labeler-change effects from the §8 verdict layer. A clean §8 verdict on Plan A.2 requires a re-stratified subset.
3. **Uncovers a V-retention regression hidden by the 4k subset.** Phase1 v1.3's Valid retention on the full Plan A.2 flask corpus is 0.5193 (WLB 0.5182) — 43 pp below the §8 #3 threshold. R3 over-firing on Valid-GT is the dominant driver (343,284 of 360,876 Valid-non-Valid mis-routes).
4. **Confirms Plan A.2 unlocks informative Stale-rule evaluation.** Full-corpus Stale recall is 0.9269 (WLB 0.9256), with per-subtype recall {SourceChanged 0.9532, SourceDeleted 0.9305, SymbolRenamed 0.6822}.
5. **Identifies StaleSymbolRenamed as the weakest detected subtype** (recall 0.6822) and ties it to R4's line-presence escape with R7 dead-in-chain — a structural finding, not a threshold-tuning matter.
6. **Identifies the R4 NR carve-out as corpus-distribution-sensitive.** The carve-out catches 45/2,000 on the v1.2b subset slice but 0/6,652 on the full-corpus NR-GT class.
7. **Identifies R5 and R7 as dead-in-chain on flask Python** at the full-corpus scope.

## Forward paths for v1.2d

These are pre-registered ideas (not in-round retunings; SPEC §10 forbids in-round tuning on a held-out result):

- **(α) Re-stratify the flask 4k subset against the Plan A.2 corpus distribution** and re-run the LLM baseline against the new subset. Produces an informative §8 #5 verdict on flask Python. Consumes the §13.2 flask leakage budget — this is the §10-defined cost of the Plan A.2 labeler change.
- **(β) Investigate R3's `symbol_missing` firing on Plan A.2 Valid-GT rows.** The 343,284 false-Stale rows on Valid GT are the highest-leverage intervention for restoring §8 #3 generalization. Likely fix is in the labeler's resolver-coverage path (close PR #50 gaps) and/or in phase1's R3 lookup (relax the symbol-missing decision when the post-commit blob is structurally identical to the t0 blob for the symbol's containing region). Must be tuned on pilot (ripgrep), not flask.
- **(γ) Move R7 ahead of R4 in the chain, or rewrite R7's proxy to fire on Plan A.2 rename-emitted facts.** Targets the 9,508-row StaleSymbolRenamed slice where recall is 0.6822. Pilot tuning only.
- **(δ) Investigate R4's NR carve-out generalization.** The carve-out's 45-row catch on the v1.2b subset versus 0-row catch on the full Plan A.2 NR-GT class suggests the `guard_below_floor: true` condition is structurally biased toward Plan A.1's NR class. Re-evaluate the carve-out's design against Plan A.2's NR class definition.
- **(ε) Recover latency measurement** by populating per-row `wall_ms` in `predictions.jsonl`. Not blocking; closes H1 carry-forward.
- **(ζ) Address PR #50 resolver coverage gaps in the labeler.** Closing these gaps should reduce R3's false-firing on Valid-GT rows by removing the labeler-vs-phase1 resolver disagreement that drives them.

## Reference paths

- 4k subset metrics: `benchmarks/provbench/results/flask-heldout-2026-05-19-canary/metrics.json`
- 4k subset phase1 meta: `benchmarks/provbench/results/flask-heldout-2026-05-19-canary/phase1/run_meta.json`
- Full-corpus sidecar metrics: `benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/sidecar_metrics.json`
- Full-corpus phase1 meta: `benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/phase1/run_meta.json`
- Full-corpus sidecar README + reproducibility: `benchmarks/provbench/results/flask-heldout-2026-05-19-fullcorpus/README.md`
- Prior v1.3 flask Plan A.1 findings (PR #61 — the round this is byte-identical to at the §8 layer): `benchmarks/provbench/results/flask-heldout-2026-05-18-findings.md`
- Prior v1.3 serde findings (PR #62 — first v1.3 PASS-PASS-PASS): `benchmarks/provbench/results/serde-heldout-2026-05-18-findings.md`
- v1.2b flask Plan A.1 findings (predecessor labeler-short-circuit round): `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`
- SPEC §11 row dated 2026-05-19 §9.4 (record only) appended in this PR.
