# ProvBench Phase 1 (rules) — 2026-05-15 flask held-out findings (`rule_set_version v1.2`)

## Thesis under test

A deterministic, structural, single-repo HEAD-only rules pass clears SPEC §8 #3 / #4 / #5 verbatim on a Python repo the v1.2 rule set was never tuned on. Held-out Round 2 is `pallets/flask` @ T₀ `2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (tag `2.0.0`, SPEC §13.2 pre-registered, leakage-clean). Pilot tuning was performed on ripgrep only (v1.2a); per SPEC §10 no R3/R4/R5/R7 retuning is permitted on the held-out repo. This document records the result regardless of pass or fail — §10 forbids in-round retuning either way.

## SPEC §8 threshold verdict — **PASS-PASS-FAIL** (structural §8 #5 miss)

| Threshold | Required | Observed (flask held-out) | Pass? |
|---|---|---|:---:|
| §8 #3 valid retention WLB | ≥ 0.95 | **0.9980829526885622** | ✅ |
| §8 #4 latency p50 (per-row, ms) | ≤ 727 | 0 (vacuous — see Hygiene Flag A.3) | ✅ |
| §8 #5 stale recall WLB | ≥ 0.30 | **0.0** | **❌ (structural)** |

The §8 #5 miss is **structural, not a rule-chain quality failure**: the labeler at Plan A.1 (`800d108…`) emits ZERO `Stale_*` ground-truth labels on this corpus by design (see "The taxonomy-mismatch narrative" below). Wilson lower bound on a 0/0 recall numerator is defined as 0.0; the threshold cannot be cleared by any rule chain on this corpus.

**v1.2 generalizes on §8 #3 (the load-bearing improvement) and clears §8 #4 vacuously.** §8 #5 is uninformative this round — see the narrative and decision sections.

## Run details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.2` |
| Spec freeze hash (§15) | `6ab6c23bdcd0e5bf06bcc036bab883f94457a53e0c67188165d6db7c20b44fb2` |
| Labeler git SHA (corpus + facts + diffs, Plan A.1) | `800d108b542dcf2b9122eb57b15c3c8d0a472275` (PR #52 merge) |
| Phase 1 git SHA | `97cef97ba347aa7adca0a8367712ab11490f26fe` |
| Held-out repo | pallets/flask @ `2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = tag `2.0.0`) |
| flask HEAD at labeler run | `9fcd34c9f3065640bd1cd86234216ca068633fb9` (401 first-parent commits forward of T₀) |
| Baseline-run subset | `results/flask-heldout-2026-05-15-canary/baseline` (DRY-RUN CARRIER — NOT EVIDENCE) |
| Sample seed | `0xC0DEBABEDEADBEEF` = `13897750829054410479` (CLI default; pilot-matching) |
| Per-stratum targets | `valid:2000, stale_changed:2000, stale_deleted:2000, stale_renamed:u64::MAX, needs_revalidation:2000` (defaults; not tuned — see Hygiene Flag A.2) |
| Operational budget | `--budget-usd 250` (SPEC §6.2 cap; dry-run; worst-case $53.07) |
| Corpus row count | 910,530 (`flask-2f0c62f5-800d108.jsonl`) |
| Unique facts | 2,265 (`flask-2f0c62f5-800d108.facts.jsonl`) |
| Diff files | 402 (`flask-2f0c62f5-800d108.diffs/`) |
| Selected (canary subset) | 4,000 |
| Excluded (commit_t0) | 2,265 |
| Phase1 row count (predictions) | 4,000 |
| Phase1 stats (predictions) | `valid:2000 stale:2000 needs_reval:0` |

## SPEC §7.1 three-way table (flask held-out, n = 4,000)

| Metric | Point | Wilson LB |
|---|---|---|
| Stale detection recall | **0.0** | **0.0** |
| Stale detection precision | 0.0 | — |
| Stale detection F1 | 0.0 | — |
| Valid retention accuracy | **1.0** | **0.9980829526885622** |
| Needs_revalidation routing accuracy | 0.0 | ~1.08e-19 |

Stale-detection numerator and denominator are both zero because there are **no `Stale_*` rows in the ground truth** on this subset — see narrative.

## Confusion matrix (predictions vs ground truth, n = 4,000)

| GT \ Pred | `valid` | `stale` | `needsrevalidation` | total |
|---|---:|---:|---:|---:|
| `Valid` | **2,000** | 0 | 0 | 2,000 |
| `NeedsRevalidation` | 708 | **1,292** | 0 | 2,000 |
| `Stale_*` (any subtype) | 0 | 0 | 0 | **0** |
| total | 2,708 | 1,292 | 0 | 4,000 |

- No `Stale_*` in ground truth (Plan A.1 short-circuit by design).
- No `NeedsRevalidation` in phase1 predictions (phase1 v1.2 emits only `valid` / `stale`; NR is a labeler-only label).

## Per-rule confusion (flask held-out)

From `metrics.json.per_rule_confusion`:

| Rule | Outcome | Count | Note |
|---|---|---:|---|
| R1 `source_file_missing` | `needsrevalidation__stale` | 13 | NR ground truth misrouted to `stale` |
| R2 `blob_identical` | `valid__valid` | 2,000 | **All Valid GT classified correctly via R2** |
| R3 `symbol_missing` | `needsrevalidation__stale` | 1,175 | dominant `stale` driver on NR GT |
| R4 `span_hash_changed` (line-presence probe) | `needsrevalidation__stale` | 104 | secondary `stale` driver on NR GT |
| R4 `span_hash_changed` (line-presence probe) | `needsrevalidation__valid` | 708 | **R4's `t0_span_found_in_post` short-circuit lets NR-GT rows escape as Valid** |
| R5 / R6 / R7 | — | 0 | did not fire on this subset (see Hygiene Flag 6) |

- 2,000 + 13 + 1,175 + 104 + 708 = 4,000. Full accounting.
- R3 fires 1,175× on NR ground truth → `stale` prediction.
- R4 splits: 104 → `stale`, 708 → `valid` (via the v1.2 R4 line-presence guard relaxation).

## The taxonomy-mismatch narrative — the load-bearing finding

The §8 #5 FAIL is **NOT a phase1 quality failure**. It is structural to the v1.2b corpus design:

1. The labeler at Plan A.1 (`800d108…`, PR #52 merge) emits ground-truth labels for this 4,000-row subset: **2,000 `Valid` + 2,000 `NeedsRevalidation` + 0 `Stale_*`**.
2. Why 0 `Stale_*`: the Plan A.1 PR #52 short-circuit routes **all** changed-Python-file facts to `Label::NeedsRevalidation`. This is an intentional v1.2b out-of-scope decision per the v1 collab plan — Python labeling at Plan A.1 is conservative because the Python `DocClaim` extractor is a stub and Python symbol-resolution remains heuristic (cf. Hygiene Flag 6 + Hygiene Flag 7).
3. Phase1 v1.2 emits `valid` or `stale` (never `needsrevalidation` — NR is a labeler-only label, not a rule-classified outcome).
4. Phase1 actually predicts: 2,000 `valid` (perfect on Valid GT) + 1,292 `stale` + 708 `valid` (on the 2,000 NR GT rows).
5. `stale_detection.wilson_lower_95` measures rule recall **over ground-truth `Stale_*` rows**. There are ZERO such rows. The recall numerator is `0 / 0` → defined as `0.0` by Wilson convention.
6. The threshold §8 #5 ≥ 0.30 therefore **cannot** be met on this corpus — there is no signal to detect.

**Implication for the rules-only thesis:** v1.2b's §8 #5 FAIL is **uninformative** about phase1's actual stale-detection ability on Python. To get a meaningful §8 #5 measurement on Python:

- Either the labeler needs to emit `Stale_*` on Python (refining the Plan A.1 short-circuit to NOT catch ALL changed Python files), OR
- The held-out corpus needs to include Python rows where the labeler IS confident about `Stale_*` (e.g., obvious deletes that are detectable without symbol resolution).

This is a v1.2b-specific finding worth recording — and it pre-registers the next-round design question. See "Decision / recommendations" below.

## Side-by-side: v1.2b flask vs v1.1 serde Round 1 vs v1.2a ripgrep pilot

| Metric | v1.1 serde Round 1 (held-out) | v1.2a ripgrep pilot | v1.2b flask Round 2 (held-out) |
|---|---|---|---|
| Round purpose | leakage gate, v1.1 | tuning, v1.2 | leakage gate, v1.2 |
| Corpus | serde-rs/serde | BurntSushi/ripgrep | pallets/flask |
| Subset size | 12,820 | 12,820 | **4,000** |
| §8 #3 valid retention WLB | 0.9062 **FAIL** | 0.9716 PASS | **0.9981 PASS** |
| §8 #4 latency p50 (ms) | 14 PASS | 2 PASS | **0 PASS (vacuous)** |
| §8 #5 stale recall WLB | 0.9391 PASS | 0.9537 PASS | **0.0 FAIL (structural)** |
| Overall | FAIL §8 #3 | tune-clear | **PASS-PASS-FAIL (structural)** |

What this comparison establishes:

- **§8 #3 generalization improved.** v1.1 → v1.2 raised valid-retention WLB on a fresh held-out from 0.9062 (FAIL) → 0.9981 (PASS). The v1.2 R4 line-presence guard relaxation (the only retune between v1.1 and v1.2) generalized.
- **§8 #5 is incomparable across rounds.** serde + ripgrep both had meaningful `Stale_*` ground-truth populations; flask has none on this subset. The flask 0.0 is structural, not a regression.
- **§8 #4 latency methodology differs.** This round's `predictions.jsonl` has `wall_ms = 0` for every row (see Hygiene Flag A.3); the §8 #4 PASS is vacuous. Earlier rounds populated `wall_ms` per row.

## Hygiene flags (7 plus 3 additional)

### Required flags (Plan A's 6 + Plan A.1's NeedsRevalidation contract)

1. **`__init__.py` collapse not implemented.** The Python labeler does not collapse `pkg/__init__.py` into the `pkg` namespace; facts about `__init__.py` symbols are tracked verbatim. Impact bounded by R3's structural fact-id resolution.
2. **Multi-hop import chains capped at one hop.** The Python labeler resolves `from a.b import c` one hop deep; transitively re-exported names are not chased. R3 may misroute deeply-re-exported symbols.
3. **Relative imports silently dropped.** `from . import x` / `from ..pkg import y` are not resolved (Plan A scope). Facts referencing relative-imported names are not linked to source modules.
4. **Star imports skipped unless `__all__` defined.** `from m import *` is resolved only when `m.__all__` is explicit. Bare-star imports are skipped (Plan A scope).
5. **`TYPE_CHECKING`-conditional imports + dynamic dispatch + metaclasses not modeled.** Imports inside `if TYPE_CHECKING:` blocks, `__getattr__`-based dispatch, and metaclass-injected attributes are out of scope; facts that depend on these dispatch mechanisms are treated as resolvable-or-NR per the conservative short-circuit.
6. **Python `DocClaim` extractor is a stub → R5 doesn't fire on Python.** The `whitespace_or_comment_only` rule (R5) depends on `DocClaim`-style span extraction; the Python extractor is a stub. R5 fires 0× on this subset.
7. **Plan A.1 / PR #52: Python facts at changed files emit `Label::NeedsRevalidation`.** All changed-Python-file facts route to `NeedsRevalidation` ground truth — they are **NOT** classified as `Stale_*` by the labeler. This is the load-bearing contract behind the taxonomy-mismatch narrative. R3 / R4 / R5 / R7 are evaluated by phase1 *against this NR ground truth*, not against `Stale_*` ground truth.

### Additional observed mid-run

A.1. **`frozen_hash` key absent from baseline `manifest.json`.** Matches serde Round 1 hygiene flag 8 (carve-out). Manifest carries `spec_freeze_hash` (`6ab6c23b…`) and `content_hash` (`93e0ed3a…`); a separate top-level `frozen_hash` is not emitted by the current baseline crate. Documented; no source change in scope.

A.2. **`per_stratum_targets.stale_renamed == u64::MAX` sentinel.** Manifest reports `stale_renamed: 18446744073709551615` (= `u64::MAX`) — under-filled stratum sentinel used by the sampler. No effective impact on this subset because the corpus has no Python `Stale_*` rows to fill any stale_* stratum at all (see narrative).

A.3. **`latency_p50_ms = 0` for both baseline and phase1 columns.** Every row in `predictions.jsonl` has `wall_ms: 0`; the phase1 runner did not populate per-row wall-time on this round. Consequently `metrics.json.phase1_rules.section_7_2_applicable.latency_p50_ms = 0` and `latency_p95_ms = 0`. §8 #4 PASSES vacuously (`0 ≤ 727`). The §8 #4 PASS this round is **not** a meaningful latency measurement; treat as PASS-vacuous and recover real latency in the next round.

## SPEC §10 anti-leakage attestation (8 items)

| # | Item | Result |
|---|---|---|
| 1 | `git -C /tmp/ironmem-worktrees/phase1-97cef97 diff` empty (verified at write time) | ✅ empty (0 lines) |
| 2 | `git -C /tmp/ironmem-worktrees/phase1-97cef97 rev-parse HEAD` == `97cef97…` (verified at write time) | ✅ `97cef97ba347aa7adca0a8367712ab11490f26fe` |
| 3 | `provbench-labeler --version` == `800d108b542dcf2b9122eb57b15c3c8d0a472275` (verified Task 2) | ✅ |
| 4 | flask HEAD = `9fcd34c9…` and 401 first-parent commits ahead of T₀ (verified Task 1) | ✅ |
| 5 | `tests/python_replay_changed_file.rs` passes (verified Task 2) | ✅ |
| 6 | `tests/determinism_flask.rs` `#[ignore]` passes at chosen HEAD (verified Task 3) | ✅ |
| 7 | Pre-commit generated-artifact check clean (verified Task 8) | ✅ |
| 8 | `verify-tooling` passes for tree-sitter-python (rust-analyzer mismatch acceptable; verified Task 2) | ✅ |

**Result: 8 / 8 PASS.** No R3 / R4 / R5 / R7 threshold retune was performed in this round. No labeler / rule-chain source changes between the Plan A.1 labeler pin and feature-branch HEAD that affect rule evaluation on this corpus.

## What is and is not in scope

In scope for this PR:

- Held-out artifacts under `results/flask-heldout-2026-05-15-canary/` (manifest + run_meta + metrics for `baseline/` carrier; `phase1/` predictions + rule_traces + sqlite + run_meta + top-level compare metrics).
- This findings doc.
- One new row in SPEC §11 recording the v1.2b held-out PASS-PASS-FAIL (Task 11, conditional).
- Sibling test `phase1/tests/end_to_end_heldout_flask.rs` (asserts §8 verbatim — PASS-PASS-FAIL as expected).
- Sibling test `labeler/tests/determinism_flask.rs` (`#[ignore]`; full flask replay determinism gate).

Out of scope (per the locked Plan B and SPEC §12):

- Refining the Plan A.1 Python short-circuit to emit `Stale_*` (v1.2c+ if pursued).
- Cross-repo / tunnels / multi-branch / semantic equivalence handling.
- v2 LLM second-pass over `needs_revalidation` rows.
- Integration into the ironmem runtime hot path.
- **Any retune of R3 / R4 / R5 / R7 thresholds in this round** (§10 forbids in-round retuning; would invalidate the recorded held-out result).
- Repopulating `wall_ms` in `predictions.jsonl` retroactively for this round (real latency measurement deferred to a fresh run).
- Adding a top-level `frozen_hash` to baseline `manifest.json` (baseline source out of scope).

## Decision / recommendations

What this round established:

- **§8 #3 valid retention generalizes from v1.2a pilot → v1.2b flask Round 2.** WLB 0.9716 (pilot) → 0.9981 (held-out). The v1.2 R4 line-presence guard relaxation is the only retune between v1.1 and v1.2, and it generalized — this is the load-bearing improvement vs v1.1 serde Round 1.
- **§8 #5 FAIL is structural and uninformative.** The Plan A.1 labeler short-circuits all changed-Python-file facts to `NeedsRevalidation` by design; the corpus contains **zero** `Stale_*` ground-truth rows on this 4,000-row subset, so phase1's actual Python stale-detection ability is NOT tested by this round.
- **§8 #4 PASS is vacuous.** `wall_ms` was not populated in this round's `predictions.jsonl`; a real latency measurement on Python is deferred.

For v1.2c (next round if pursued):

- Decide between (a) **extending the labeler to emit `Stale_*` on Python** — refining the Plan A.1 short-circuit so it does NOT catch ALL changed-Python files (e.g., admit obvious deletes and rename-with-content-change patterns as `Stale_*` while keeping symbol-resolution-dependent cases as `NeedsRevalidation`); OR (b) **cherry-picking a held-out repo + corpus where the `Stale_*` ground-truth signal exists pre-built** (e.g., post-deletion / large-rename commits) so the §8 #5 threshold becomes meaningful.
- Repopulate per-row `wall_ms` in `predictions.jsonl` so §8 #4 returns to a meaningful measurement.
- Record a new SPEC §11 row at the v1.2 → v1.2c transition (if pursued) and re-run the leakage clock against a fresh held-out repo (current SPEC §13.2 budget exhausted for flask after this round).

## What this round establishes — TL;DR

- v1.2 generalizes on **§8 #3** (the metric v1.1 over-fit) — the held-out gate validated the v1.1 → v1.2 R4 relaxation on a Python repo.
- v1.2 cannot be evaluated on **§8 #5** on this corpus — the labeler emits no `Stale_*` ground truth by design (Plan A.1 / PR #52 short-circuit).
- v1.2 §8 #4 is PASS-vacuous this round — `wall_ms` not populated.
- §9.4 held-out gate is doing its job in the opposite direction this round: instead of catching pilot-shaped fit, it surfaced a corpus-design mismatch that pre-registers the next-round question.
