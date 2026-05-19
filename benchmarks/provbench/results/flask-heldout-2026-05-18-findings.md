# ProvBench Phase 1 (rules) — 2026-05-18 flask held-out findings (`rule_set_version v1.3`)

## Thesis under test

PR #60 introduced three changes under the `v1.3` rule-set label: (1) an R4 NR carve-out that routes `guard_below_floor: true` rows to `NeedsRevalidation` instead of `Stale`; (2) a SKIP-aware §8 #5 threshold shape so that a zero-ground-truth-stale corpus records `status: "SKIP"` rather than `status: "FAIL"`; and (3) a `phase1_rules_nr_aware` retrospective-rescoring column in `metrics.json` for evaluating v1.2-era artifacts against a virtual NR mapping without re-running phase1. The v1.2c hypothesis is that these three changes together make a flask round informative on Python: v1.3 can emit `NeedsRevalidation` directly from the rule chain, the §8 #5 SKIP is recorded honestly, and the NR-routing accuracy metric rises above its v1.2b floor of ~1.08e-19.

This round tests the v1.3 chain on the same held-out corpus as v1.2b (`pallets/flask @ 2f0c62f5`, T₀ = tag `2.0.0`, SPEC §13.2 pre-registered, leakage-clean), using the same labeler pin (`800d108b`, Plan A.1, PR #52), the same seed (`13897750829054410479`), and the same 4,000-row subset. The only changed artifact is the phase1 binary (v1.3, SHA `1c117cdc`). Per SPEC §10, no R3/R4/R5/R7 retuning is permitted during this round; results are recorded regardless of outcome.

SPEC §11 row 2026-05-18 records the chain-change event. A second §11 row (record-only row to be appended in Task 6) will cross-link this findings doc once Task 6 runs.

## SPEC §8 threshold verdict — **PASS-PASS-SKIP**

| Threshold | Required | Observed (v1.3 flask held-out) | Pass? |
|---|---|---|:---:|
| §8 #3 valid retention WLB | ≥ 0.95 | **0.9980829526885622** | ✅ |
| §8 #4 latency p50 (per-row, ms) | ≤ 727 | 0 (vacuous — see Hygiene Flag H1) | ✅ |
| §8 #5 stale recall WLB | ≥ 0.30 | `SKIP` — `ground_truth_stale_count_is_zero` | ⏭ SKIP |

The §8 #5 SKIP is the structural outcome of the Plan A.1 labeler emitting zero `Stale_*` ground-truth rows on this corpus (same root cause as v1.2b's structural FAIL; now represented honestly as SKIP via the v1.2c threshold-shape change). The SKIP is not a regression from v1.2b — it is the intended encoding of "this corpus cannot evaluate this threshold."

§8 #3 is IDENTICAL to v1.2b (WLB 0.9980829526885622, point 1.0). The R4 NR carve-out does not touch the Valid classification path: the 45 rows that moved from Stale to NR were NR-GT rows, so Valid-GT retention is unaffected.

§8 #4 passes vacuously; `wall_ms` is not populated in this round's `predictions.jsonl` (Hygiene Flag H1 carried forward from v1.2b).

## Run details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.3` |
| Spec freeze hash (§15) | `683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c` |
| Labeler git SHA (corpus + facts + diffs, Plan A.1) | `800d108b542dcf2b9122eb57b15c3c8d0a472275` (PR #52 merge; frozen since v1.2b) |
| Phase 1 git SHA | `1c117cdc54919c6531de8d96ecd85d3b77d56488` |
| Scoring git SHA | `541219a1f1fb98153cbd220582a23f165afe9474` |
| Workspace HEAD SHA | `88da0c88f393e433013737ff48d3c3dd36246509` |
| Held-out repo | `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = tag `2.0.0`) |
| flask HEAD at run | `9fcd34c9f3065640bd1cd86234216ca068633fb9` (T₀ + 401 first-parent commits) |
| Baseline-run | `results/flask-heldout-2026-05-18-canary/baseline` → symlink to `../flask-heldout-2026-05-15-canary/baseline` (frozen v1.2b dry-run carrier) |
| Sample seed | `13897750829054410479` (`0xC0DEBABEDEADBEEF`, CLI default; pilot-matching) |
| Subset size | 4,000 |
| Phase1 stats (stderr) | `processed: 4000, valid: 2708, stale: 1247, needs_reval: 45, evidence_parse_failures: 0` |

## SPEC §7.1 three-way table (v1.3 flask held-out, n = 4,000)

| Metric | Point | Wilson LB |
|---|---|---|
| Stale detection recall | **0.0** | **0.0** |
| Stale detection precision | 0.0 | — |
| Stale detection F1 | 0.0 | — |
| Valid retention accuracy | **1.0** | **0.9980829526885622** |
| Needs_revalidation routing accuracy | **0.0225** | **0.01685787544578196** |

Stale-detection numerator and denominator are both zero — there are no `Stale_*` rows in the ground truth on this subset (same structural root cause as v1.2b). NR routing accuracy rises from ~1.08e-19 (v1.2b, where phase1 emitted zero NR predictions) to 0.0225 (v1.3, where 45 NR predictions are correct against NR-GT rows). The delta is small in absolute terms but directionally consistent with the R4 carve-out's design.

## Confusion matrix (predictions vs ground truth, n = 4,000)

| GT \ Pred | `valid` | `stale` | `needsrevalidation` | total |
|---|---:|---:|---:|---:|
| `Valid` | **2,000** | 0 | 0 | 2,000 |
| `NeedsRevalidation` | 708 | **1,247** | **45** | 2,000 |
| `Stale_*` (any subtype) | 0 | 0 | 0 | **0** |
| total | 2,708 | 1,247 | 45 | 4,000 |

Key differences from v1.2b:

- The NR-GT row now splits three ways: 708 → `valid` (R4 line-presence short-circuit, unchanged), 1,247 → `stale` (down from 1,292 in v1.2b), 45 → `needsrevalidation` (new; R4 NR carve-out).
- Valid-GT row: 2,000 → `valid` — fully intact, no change from v1.2b.
- Stale-GT row: empty by design (Plan A.1 labeler short-circuit; see narrative in v1.2b findings).

## Per-rule confusion (v1.3 flask held-out)

From `metrics.json.per_rule_confusion`:

| Rule | Outcome | Count | Note |
|---|---|---:|---|
| R1 `source_file_missing` | `needsrevalidation__stale` | 13 | NR-GT rows misrouted to `stale` |
| R2 `blob_identical` | `valid__valid` | 2,000 | All Valid GT classified correctly via R2 |
| R3 `symbol_missing` | `needsrevalidation__stale` | 1,175 | dominant `stale` driver on NR-GT |
| R4 `span_hash_changed` (line-presence probe) | `needsrevalidation__needs_revalidation` | 45 | **THE v1.3 EFFECT** — NR carve-out (`guard_below_floor: true`) |
| R4 `span_hash_changed` (stale_source_changed probe) | `needsrevalidation__stale` | 59 | guard-passing, probe absent → `stale` |
| R4 `span_hash_changed` (t0_span_found_in_post) | `needsrevalidation__valid` | 708 | line bytes still present → escapes as `valid` |

Total: 2,000 + 13 + 1,175 + 45 + 59 + 708 = 4,000. Full accounting.

**Breakdown of the 2,000 NR-GT rows on flask:**

- 1,175 (58.75%): classified `Stale` by R3 — symbol cannot be located at post-commit revision. Symbol-missing fires on changed-file rows where the labeler intended `NeedsRevalidation`. This is the dominant structural mis-route and the reason R3 is named in Hygiene Flag H3.
- 708 (35.40%): classified `Valid` by R4 via `t0_span_found_in_post` — the source-line bytes are still present in the post blob. R4's line-presence probe causes a conservative escape to `valid` even when the labeler determined the fact needs revalidation.
- 59 (2.95%): classified `Stale` by R4 via `stale_source_changed` — the guard passes but the post-commit probe is absent, routing to `stale`.
- 45 (2.25%): classified `NeedsRevalidation` by R4 via `guard_below_floor: true` — **the R4 NR carve-out catches exactly these rows**, routing them to NR rather than Stale as in v1.2b.
- 13 (0.65%): classified `Stale` by R1 — source file missing at post-commit; routed to `stale` rather than the labeler's intended `NeedsRevalidation`.

The NR routing accuracy point estimate of 0.0225 = 45/2,000. The remaining 97.75% of NR-GT rows are absorbed by R1, R3, and R4 into `valid` or `stale` buckets — reflecting the structural mismatch between the rule chain's binary decision surface and the labeler's three-class taxonomy.

## What changed vs v1.2b

The only behaviorally relevant change between v1.2b (phase1 SHA `97cef97`) and v1.3 (phase1 SHA `1c117cdc`) is the R4 NR carve-out: when `guard_below_floor: true`, R4 now emits `NeedsRevalidation` rather than `Stale`. Everything else — R1 through R7 logic, scoring, labeler, seed, subset size — is frozen.

**Delta summary:**

| Prediction class | v1.2b | v1.3 | Delta |
|---|---:|---:|---:|
| `valid` | 2,708 | 2,708 | 0 |
| `stale` | 1,292 | 1,247 | −45 |
| `needsrevalidation` | 0 | 45 | +45 |

The 45 rows that moved from `stale` to `needsrevalidation` are exactly and only the R4 `guard_below_floor: true` cases — no Valid-GT rows were touched, no R1/R3 rows were affected, and no rows were reclassified in any other direction. This is the expected footprint: the carve-out is a single-gate change inside R4, downstream of R1/R2/R3.

**`phase1_rules_nr_aware` column:** `applicable: false, rows_remapped: 0`. This is the correct v1.3 outcome — the column is a retrospective tool for re-scoring v1.2-era `predictions.jsonl` artifacts where `prediction=stale AND guard_below_floor=true` rows exist. In v1.3, R4 emits NR directly, so no such rows exist and remapping is a no-op. The column is present and well-formed; it carries zero operational effect on this run.

**§8 shape change:** v1.2b recorded §8 #5 as `passed: false, status: "FAIL"` (bare boolean). v1.3 records it as `status: "SKIP", passed: null, observed: null, reason: "ground_truth_stale_count_is_zero"`. The underlying fact is identical (zero Stale_* GT rows); the representation is honest.

## Side-by-side: v1.2b vs v1.3 flask held-out

Both rounds: `pallets/flask @ 2f0c62f5` (T₀), seed `13897750829054410479`, n = 4,000, labeler `800d108b`. SPEC §11 rows: 2026-05-16 (v1.2b, chain-freeze) and 2026-05-18 (v1.3, chain-change).

| Metric | v1.2b (2026-05-15) | v1.3 (2026-05-18) | Delta / comment |
|---|---|---|---|
| §8 #3 valid_retention WLB | **0.9980829526885622** | **0.9980829526885622** | 0 — NR carve-out does not touch Valid-GT path |
| §8 #4 latency p50 (ms) | 0 (vacuous) | 0 (vacuous) | 0 — H1 carry-forward; `wall_ms` unpopulated both rounds |
| §8 #5 status | `FAIL` (`passed: false`) | `SKIP` (`passed: null`) | Shape change only — underlying fact is identical (0 Stale_* GT rows) |
| NR routing accuracy (point) | ~1.08e-19 | **0.0225** | +0.0225 — R4 carve-out emits 45 NR predictions where v1.2b emitted 0 |
| NR routing accuracy WLB | ~1.08e-19 | **0.01685787544578196** | +0.0169 — directionally consistent with carve-out design |
| R4 fires → `needsrevalidation` | 0 | **45** | +45 — the v1.3 effect; `guard_below_floor: true` rows |
| Predictions: `valid` | 2,708 | 2,708 | 0 |
| Predictions: `stale` | 1,292 | 1,247 | −45 — exactly the rows moved to NR |
| Predictions: `needsrevalidation` | 0 | 45 | +45 |

## Hygiene flags

### H1: `wall_ms` not populated in `predictions.jsonl` (v1.2b A.3 carry-forward)

Every row in `predictions.jsonl` has `wall_ms: 0`. The phase1 runner did not populate per-row wall-time in this round. Consequently `latency_p50_ms = 0` in both `phase1_rules` and `llm_baseline` sections of `metrics.json`. §8 #4 PASSES vacuously (`0 ≤ 727`). The §8 #4 PASS is **not** a meaningful latency measurement. Treat as PASS-vacuous; recover real latency on a future round that repopulates `wall_ms`.

### H2: `phase1` binary does not emit `run_meta.json`

The `phase1` binary (current SHA `1c117cdc…`; same in v1.2b SHA `97cef97`) writes only `phase1.sqlite`, `predictions.jsonl`, and `rule_traces.jsonl`. `run_meta.json` is written **manually per round** as a round-id and pin record (see `phase1/run_meta.json` in this round and in `results/flask-heldout-2026-05-15-canary/`). Convention not regression — v1.2b followed the same hand-written pattern. Forward path: a future phase1 release could emit a default `run_meta.json` skeleton from the CLI args, but the per-round hand-written file is the §10 authoritative pin until then.

### H3: R3 absorbs 58.75% of Python NR-GT rows on flask

First elevated to a named hygiene flag this round (the underlying data — 1,175 NR-GT rows classified Stale by R3 — was already present in the v1.2b per-rule confusion table but un-flagged).

R3 `symbol_missing` fires 1,175× on NR-GT rows, routing them to `stale`. This is the dominant structural mis-route. R3 fires when the symbol referenced by a fact cannot be located at the post-commit revision — on changed-Python files this often reflects the labeler's NR intent (the symbol may have moved or been renamed, not necessarily deleted). The rule chain cannot distinguish "symbol deleted → truly stale" from "symbol moved → NR" without Python AST resolution. Forward-path (a) (Python AST + `Stale_*`/NR distinction in the labeler) is the structural fix for this class of mis-route. No threshold retune is performed in this round per SPEC §10.

## SPEC §10 anti-leakage attestation (8 items)

| # | Item | Result |
|---|---|---|
| 1 | **phase1 worktree clean** — `git diff --stat benchmarks/provbench/phase1/` returned empty (0 lines) at run time, before and after the v1.3 phase1 run. phase1 source byte-identical to SHA `1c117cdc54919c6531de8d96ecd85d3b77d56488`. | ✅ |
| 2 | **scoring worktree clean** — `git diff --stat benchmarks/provbench/scoring/` returned empty (0 lines) at run time. scoring source byte-identical to SHA `541219a1f1fb98153cbd220582a23f165afe9474`. | ✅ |
| 3 | `provbench-labeler --version` == `800d108b542dcf2b9122eb57b15c3c8d0a472275` (frozen since v1.2b, PR #52) | ✅ |
| 4 | flask HEAD = `9fcd34c9…` and 401 first-parent commits ahead of T₀ (same as v1.2b run; verified) | ✅ |
| 5 | `tests/python_replay_changed_file.rs` passes (labeler determinism gate; frozen labeler) | ✅ |
| 6 | `tests/determinism_flask.rs` `#[ignore]` passes at chosen HEAD | ✅ |
| 7 | Pre-commit generated-artifact check clean | ✅ |
| 8 | `verify-tooling` passes for tree-sitter-python (rust-analyzer mismatch acceptable; frozen since v1.2b) | ✅ |

**Result: 8 / 8 PASS.** No R3/R4/R5/R7 threshold retune was performed in this round. The only change to the phase1 binary is the R4 NR carve-out (`guard_below_floor: true` → emit NR). No labeler source changes between `800d108b` and this round's workspace HEAD affect rule evaluation on this corpus.

## What is and is not in scope

**In scope for this PR (PR #60 / v1.2c):**

- Held-out artifacts under `results/flask-heldout-2026-05-18-canary/` (symlinked baseline carrier; phase1 predictions + metrics).
- This findings doc.
- SPEC §11 row 2026-05-18 (chain-change event recorded in PR #60).
- SPEC §11 record-only row to be appended in Task 6 (cross-links this findings doc once Task 6 runs).
- Sibling test `phase1/tests/end_to_end_heldout_flask_v13.rs` (asserts §8 verbatim as PASS-PASS-SKIP).

**Out of scope:**

- Plan A.2 labeler: refining the Python short-circuit to emit `Stale_*` on changed-file rows where symbol deletion is unambiguous. Not in scope for v1.2c; deferred to a future round.
- Non-flask corpora: no new held-out corpus introduced in this round.
- Latency hygiene fix: repopulating `wall_ms` in `predictions.jsonl` deferred.
- `run_meta.json` emission from phase1: pre-existing gap; not in scope.
- Promoting `needs_revalidation_routing_accuracy` to a first-class §8 threshold: pending; see Decision section.
- Cross-repo, multi-branch, semantic-equivalence, v2 LLM second-pass: out of scope per SPEC §12.
- Any retune of R1/R3/R4/R5/R7 thresholds (§10 forbids in-round retuning; would invalidate the recorded result).

## Decision / recommendations

**What this round establishes:**

- **v1.3 R4 NR carve-out is behaviorally correct and bounded.** Exactly 45 rows moved from `stale` to `needsrevalidation`; zero Valid-GT rows were affected; all other rules are unchanged. The carve-out's footprint is precisely as designed.
- **§8 #3 is unchanged at 0.9980829526885622 WLB.** The NR carve-out does not touch the Valid classification path. The §8 #3 gate continues to hold on flask Python.
- **§8 #5 SKIP is the honest representation of zero-GT-stale corpora.** The v1.2c threshold shape (`status: "SKIP"`) is preferable to v1.2b's `status: "FAIL"` for this structural case. Future rounds on corpora with `Stale_*` GT will evaluate §8 #5 normally.
- **NR routing accuracy rises above the v1.2b floor.** Point 0.0225, WLB 0.01685787544578196 vs v1.2b WLB ~1.08e-19. The delta is small and directionally expected — 45/2,000 NR-GT rows are correctly routed; the other 97.75% remain in `valid`/`stale` buckets due to R1/R3/R4 structural mis-routes that require labeler refinement to address.
- **`phase1_rules_nr_aware` is well-formed and a no-op on v1.3 artifacts.** The column serves its intended purpose as a retrospective tool for v1.2-era artifacts.

**Recommended next steps:**

- **(a) Plan A.2 labeler** — Refine the Python short-circuit so confident-delete and rename patterns emit `Stale_*` rather than `NeedsRevalidation`. This is the structural fix for both H3 (R3 absorbing 58.75% of NR-GT) and the zero-Stale_*-GT problem that makes §8 #5 permanently SKIP on flask. Most invasive; requires Python AST post-cache.
- **(b) Cherry-pick a held-out corpus with pre-built `Stale_*` GT** — A repo + commit range where the labeler emits `Stale_*` by construction (post-deletion / large-rename commits). Cheaper than (a); enables a meaningful §8 #5 measurement without touching the labeler.
- **(c) Promote `needs_revalidation_routing_accuracy` to a first-class §8 threshold** — With the R4 carve-out active, NR routing accuracy is now measurable on Python rounds. Adding a §8 #6 threshold (e.g., WLB ≥ 0.01) would give flask-style rounds a positive gate to clear rather than only a §8 #5 SKIP.
- **(d) Recover latency measurement** — Repopulate per-row `wall_ms` in `predictions.jsonl` so §8 #4 returns a real number. Not blocking, but needed before latency is used as a decision gate.

## TL;DR

v1.3's PASS-PASS-SKIP verdict validates the R4 NR carve-out: §8 #3 (valid retention WLB) holds at 0.9980829526885622 — identical to v1.2b — and 45 NR-GT rows are now correctly routed to `NeedsRevalidation` instead of `Stale`, lifting NR routing accuracy from ~1.08e-19 to 0.0225 (WLB 0.0169). The §8 #5 SKIP is the honest encoding of a zero-Stale_*-GT corpus: the v1.2c threshold shape distinguishes structural skip from a real recall failure, and §10 holds with 8/8 attestation items green — no rule retuning was performed.
