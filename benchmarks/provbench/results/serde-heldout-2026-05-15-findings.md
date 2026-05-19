# ProvBench Phase 1 (rules) — 2026-05-15 serde held-out findings (`rule_set_version v1.1`)

## Thesis under test

A deterministic, structural, single-repo HEAD-only rules pass clears SPEC §8 #3 / #4 / #5 verbatim on a repo the v1.1 rule set was never tuned on. Held-out Round 1 is `serde-rs/serde` @ T₀ `65e1a50749938612cfbdb69b57fc4cf249f87149` (SPEC §13.2 pre-registered, leakage-clean). Pilot tuning was performed on ripgrep only; per SPEC §10 no R3/R4/R5/R7 retuning is permitted on the held-out repo. This document records the result regardless of pass or fail — §10 forbids in-round retuning either way.

## SPEC §8 threshold verdict — **FAIL §8 #3**

> **Note (post-2026-05-18):** `metrics.json.thresholds.section_8_*` in this
> artifact uses the pre-v1.2c bare-boolean shape. From v1.2c onward (SPEC §11
> row 2026-05-18) the field is a structured object
> `{status: "PASS"|"FAIL"|"SKIP", passed: bool|null, metric, observed, target,
> reason?}`. This artifact has not been re-run; readers should not parse it as
> the new shape. See `benchmarks/provbench/scoring/SCHEMA.md` §3 for the
> current shape.

| Threshold | Required | Observed (serde held-out) | Pass? |
|---|---|---|:---:|
| §8 #3 valid retention WLB | ≥ 0.95 | **0.9062** | **❌** |
| §8 #4 latency p50 (per-row, ms) | ≤ 727 | 14 | ✅ |
| §8 #5 stale recall WLB | ≥ 0.30 | 0.9391 | ✅ |

Stretch internal (`stale_detection.recall ≥ 0.80` point estimate): observed `0.9441` — **achieved**, but stretch is informational only when §8 #3 fails.

**v1.1 generalizes on stale-detection and latency but does NOT generalize on valid-retention.** §8 #3 misses by 4.4pp (observed 0.9062 vs required 0.95). The held-out gate did its job — it caught pilot-shaped fit on the valid-retention metric. Per SPEC §10, this v1.1 round does not retune in response; a future v1.2 with retuned R3/R4/R5/R7 thresholds would re-run the leakage clock against a *new* held-out repo (flask is already pre-registered as Round 2).

## Run details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.1` |
| Spec freeze hash (§15) | `683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c` |
| Labeler git SHA (corpus, `Run`) | `c2d3b7b03a51a9047ff2d50077200bb52f149448` |
| Facts labeler git SHA (`emit-facts` / `emit-diffs`) | `ababb376f7cf3f92c36dde6035d90932e083517a` (see Hygiene Flag 1) |
| Phase 1 git SHA | `ccfc901be17124d08c19a6de50294ff79ded6fc3` |
| Held-out repo | serde-rs/serde @ `65e1a50749938612cfbdb69b57fc4cf249f87149` (T₀ = `v1.0.130`) |
| serde HEAD at labeler run | `fa7da4a93567ed347ad0735c28e439fca688ef26` (latest first-parent descendant of T₀; 657 commits forward) |
| Baseline-run subset | `results/serde-heldout-2026-05-15-canary/baseline` (DRY-RUN CARRIER — NOT EVIDENCE) |
| Sample seed | `0xC0DEBABEDEADBEEF` = `13897750829054410479` (CLI default; pilot-matching) |
| Per-stratum targets | `valid:2000, stale_changed:2000, stale_deleted:2000, stale_renamed:usize::MAX, needs_revalidation:2000` (defaults; not tuned) |
| Operational budget | `--budget-usd 250` (SPEC §6.2 cap; needed because serde worst-case $78 > default $25 — no actual cost, dry-run) |
| Corpus row count | 1,903,594 |
| Selected (canary subset) | 12,820 |
| Excluded (commit_t0) | 2,893 |
| Phase1 wall time | 170 s (per-row p50 = 14 ms) |
| Phase1 stats | `processed:12820 valid:2930 stale:9890 needs_reval:0` |

### Phase 1 source byte-stability

`git diff ccfc901be17124d08c19a6de50294ff79ded6fc3..HEAD -- benchmarks/provbench/phase1/src` → empty. The feature-branch phase1 binary used by the acceptance test is byte-identical to phase1@`ccfc901be171`. The §8 result is attributable to v1.1 verbatim.

## SPEC §7.1 three-way table (held-out)

| Metric | Point | Wilson LB |
|---|---|---|
| Stale detection recall | **0.9441** | **0.9391** |
| Stale detection precision | 0.8420 | — |
| Stale detection F1 | 0.8901 | — |
| Valid retention accuracy | **0.9190** | **0.9062** |
| Needs_revalidation routing accuracy | 0.0000 | 0.0000 |

## Side-by-side with the ripgrep v1.1 pilot

| Metric | Pilot (ripgrep) | Held-out (serde) | Δ (held − pilot) |
|---|---|---|---|
| Stale recall WLB | 0.9537 | 0.9391 | −0.0146 |
| Stale recall point | 0.9619 | 0.9441 | −0.0178 |
| Stale precision | 0.7867 | 0.8420 | **+0.0553** |
| Stale F1 | 0.8655 | 0.8901 | +0.0246 |
| Valid retention WLB | 0.9716 | **0.9062** | **−0.0654 (below §8 #3)** |
| Valid retention point | 0.9822 | 0.9190 | −0.0632 |
| Latency p50 (ms, per-row) | 2 | 14 | +12 |
| Latency p95 (ms, per-row) | 21 | 27 | +6 |
| NR routing accuracy | 0.0000 | 0.0000 | 0 |

Stale-detection generalizes well — precision actually IMPROVES on held-out. Valid-retention is where v1.1 over-fit the pilot.

## Per-rule confusion (held-out vs pilot)

| Rule | v1.1 pilot fires | v1.1 held-out fires | Δ |
|---|---|---|---|
| R1 `source_file_missing` | 736 | 1,757 | +1,021 |
| R2 `blob_identical` | 240 | 417 | +177 |
| R3 `symbol_missing` | 824 | 4,282 | +3,458 |
| R4 `span_hash_changed` (line-presence probe) | 2,468 | 6,271 | +3,803 |
| R5 `whitespace_or_comment_only` | 62 | 6 | −56 |
| R6 `doc_claim` | 10 | 0 | −10 |
| R7 `rename_candidate` | 47 | 87 | +40 |

### Where the §8 #3 misses come from

R4 (line-presence probe) is the dominant rule on both repos, but its **false-positive rate on held-out** is the failure driver:

| R4 outcome on held-out | Count | Pilot equivalent | Note |
|---|---|---|---|
| `valid__stale` (false stale → valid retention regression) | **162** | **17** | **10× pilot rate — primary §8 #3 driver** |
| `needsrevalidation__valid` (NR misrouted as Valid) | 599 | 304 | secondary NR routing issue |
| `stalesourcechanged__valid` (real stale missed as Valid) | 491 | 95 | retention-recall trade-off; explains stale_recall WLB drop |

R3 also misroutes 467 `needsrevalidation__stale` (pilot: 252) — but those are stale-direction misses, which help §8 #5 stale recall (no §8 hit).

The pilot R4 line-presence probe was heuristic and tuned to ripgrep span-hash semantics. Hygiene Flag 2 in the pilot findings already called this out: *"R4 line-presence probe is still heuristic. Same caveat as v1.0: 17 false-Stale on GT=Valid and 95 false-Valid on GT=stale_source_changed on this canary. Pilot-only tuning admitted by SPEC §10; flagged for §9.4 follow-up."* — this is that §9.4 follow-up. R4's false-positive rate on valid-stable code does not generalize across repos.

## Latency methodology (unchanged from pilot)

Candidate column reports per-row `wall_ms` (rule-classification cost per fact). The §8 #4 ≤ 727 ms threshold applies to the candidate column alone, satisfied with two orders of magnitude of margin (14 ms p50, 27 ms p95). Held-out is ~7× slower per row than pilot (14 vs 2 ms), but well within budget — likely a function of the larger held-out corpus + bigger serde files.

## Hygiene flags

1. **Dual labeler pin (corpus vs facts/diffs).** The labeler @ `c2d3b7b0` (Phase 0b hardening pass-5) does NOT have `emit-facts` / `emit-diffs` subcommands — those were added later by commits `02c19e2` and `02d1d0b`. The pilot's `facts/ripgrep-af6b6c54-c2d3b7b.facts.jsonl` is stamped `ababb376f7cf3f92c36dde6035d90932e083517a-dirty` (different labeler, NOT c2d3b7b0; the `c2d3b7b` filename suffix is the corpus's labeler, not the emitter's). The held-out reproduces this pattern with a CLEAN ababb37 (the same commit base as the pilot's dirty build; the dirty patches at the pilot were in the `baseline/` crate per the commit message, not in `labeler/`). Source-stability between ababb37 and feature-branch HEAD for `labeler/src/`: only 2 commits touch it (`7392239` adds defense-in-depth SHA validation; `d65599e` changes `emit-facts` to error on unreconstructible fact_ids — for inputs where all fact_ids reconstruct at T₀, output is byte-identical). The held-out facts run completed without error → ababb37 and HEAD emit byte-identical facts for this input.

2. **R4 line-presence probe is still heuristic.** As called out in the pilot findings (hygiene flag #1). The held-out run confirms the pilot's caveat: R4's false-positive rate on GT=Valid rows is 10× the pilot's. Any v1.2 retune must address R4's line-presence proxy specifically.

3. **R7 fires on a narrow class.** Single-token Jaccard between symbol leaf and file stem with same-extension filter. Held-out R7 fires 87 times (vs pilot 47), all on `stalesourcedeleted__stale`. Same narrow regime; no regression, no surprise.

4. **NR routing accuracy = 0.** Same as pilot, same as LLM baseline, same as v1.0. §8 does not gate NR routing.

5. **Coverage: held-out canary subset (12,820 rows), not full corpus (1,903,594 rows).** Same stratified-sampling shape as pilot.

6. **Anti-leakage:**
   - No R3/R4/R5/R7 threshold retune. The §8 #3 miss is **not** acted upon in this round per SPEC §10. A future v1.2 with retuned R4 would re-run the leakage clock against a *new* held-out repo.
   - No labeler / rule-chain source changes.
   - No LLM held-out baseline (Phase 0c on ripgrep collapsed to κ ≈ 0; budget unjustified).
   - pallets/flask (Round 2) NOT run in this round.

7. **Determinism preserved.** Phase 1 two-run gate on held-out: byte-identical predictions across runs (modulo `wall_ms`). The held-out labeler determinism gate is committed as `labeler/tests/determinism_serde.rs` and ignored by default because a full serde replay is expensive; the existing canary `labeler/tests/determinism.rs` remains the default binary-property gate for sort-stable replay + deterministic JSONL writing.

8. **Baseline manifest frozen-hash normalization.** `provbench-baseline sample` computes `spec_freeze_hash` from the live `SPEC.md` bytes. For this committed held-out artifact, `baseline/manifest.json`, `baseline/metrics.json`, and the top-level compare metrics are normalized to the immutable §15 freeze hash `683d023…`; `manifest.content_hash` and `baseline/run_meta.json.manifest_content_hash` were recomputed after that metadata normalization. No baseline source change was made.

9. **serde HEAD positioning.** The labeler `Run` walks `pilot.walk_first_parent()` from the working repo's `HEAD` back to `--t0`. With serde HEAD at T₀ itself (initial Task 1 step), the walk is empty and only 2,893 T₀-snapshot rows are emitted. Moving serde HEAD to `fa7da4a9…` (latest first-parent descendant of T₀; 657 commits forward) produces the full 1.9M-row corpus. This held-out plan now explicitly checks out `fa7da4a9…` after T₀ verification (Task 1 amendment). Pilot ripgrep's `work/ripgrep` HEAD is at `4519153e…` — also a first-parent descendant of pilot T₀.

10. **Operational budget bump.** The default operational budget cap is $25 USD; serde's worst-case at default per-stratum targets is $78 (vs pilot $29.50). Sampled with `--budget-usd 250` (SPEC §6.2 cap) to clear the preflight; no actual cost is incurred under `--dry-run`. Per-stratum thresholds are untouched.

## What is and is not in scope

In scope for this PR:
- Held-out artifacts under `results/serde-heldout-2026-05-15-canary/` (manifest, predictions, run_meta, metrics for both `baseline/` carrier and `phase1/`; top-level compare metrics.json; hand-written phase1 run_meta.json).
- This findings doc.
- One new row in SPEC §11 recording the held-out FAIL.
- Sibling test `labeler/tests/determinism_serde.rs` (ignored; full serde replay determinism gate).
- Sibling test `phase1/tests/end_to_end_heldout_serde.rs` (asserts §8 verbatim — fails at §8 #3 as expected).

Out of scope (per the locked plan and SPEC §12):
- pallets/flask held-out (Round 2; if v1.2 with retuned R4 wants to re-run the leakage clock, that's a separate brief).
- Fresh LLM-as-invalidator column on held-out (Phase 0c κ ≈ 0; budget unjustified).
- v2 LLM second-pass over `needs_revalidation` rows.
- Cross-repo / tunnels / multi-branch / semantic equivalence handling.
- **Any retune of R3/R4/R5/R7 thresholds in this round** (§10 forbids in-round retuning; would invalidate the recorded held-out result).
- Integration into the ironmem runtime hot path.
- Adding hex `value_parser` to `provbench-baseline sample --seed` (baseline source out of scope).
- Adding `build.rs` or auto run_meta.json emission to `provbench-phase1`.
- Running the full held-out labeler determinism gate in default CI; it remains `#[ignore]` because serde replay is expensive.

## What this round establishes

- **v1.1 generalizes on §8 #4 (latency) and §8 #5 (stale recall).** Both pass the held-out gate with significant margin.
- **v1.1 does NOT generalize on §8 #3 (valid retention).** Pilot 0.9716 → held-out 0.9062, a 6.5pp drop, below the 0.95 threshold by 4.4pp.
- **The failure is structural, not stochastic.** Per-rule confusion analysis identifies R4's line-presence probe as the primary failure driver (162 false-Stale on GT=Valid, 10× the pilot rate).
- **The §9.4 held-out gate is doing its job.** A v1.1 that passed the pilot would also have passed a less-rigorous evaluation; the held-out criterion is what surfaced the over-fit.

## What would v1.2 need

(Recorded for future use, NOT part of this round's deliverable.)

- Retune R4 line-presence probe (e.g., raise span-hash similarity threshold, or replace the line-presence proxy with a syntactically richer comparator).
- Re-run pilot tuning under §10 admission with the new R4 logic.
- Re-run the leakage clock by evaluating on `pallets/flask` (Round 2 held-out, pre-registered in SPEC §13.2).
- One new SPEC §11 row recording the v1.1 → v1.2 transition + the v1.2 held-out result.
