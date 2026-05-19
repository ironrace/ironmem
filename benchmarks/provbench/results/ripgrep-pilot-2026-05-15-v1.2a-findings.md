# ProvBench Phase 1 (rules) - 2026-05-15 ripgrep pilot findings (`rule_set_version v1.2`)

## Thesis under test

`v1.2a` tests a narrow R4 guard correction: for `kind = "Field"`, skip the `MIN_PROBE_NONWS_LEN = 8` floor while retaining `probe_has_leaf` as the sanity gate. `TestAssertion` and all other kinds keep the v1.1 guard behavior. This pilot-only round asks whether that structural fix clears the v1.1 ripgrep pilot without regression and without increasing Field false-Valid rows beyond the pre-registered +20 safety bound.

Prerequisite read: `docs/superpowers/specs/2026-05-15-provbench-v1.2a-r4-guard-design.md` and `benchmarks/provbench/results/serde-heldout-2026-05-15-findings.md`.

## SPEC Section 8 Threshold Verdict

| Threshold | Required | Observed (v1.2a) | Pass? |
|---|---:|---:|:---:|
| Section 8 #3 valid retention WLB | >= 0.95 | 0.9729 | yes |
| Section 8 #4 latency p50 (per-row, ms) | <= 727 | 2 | yes |
| Section 8 #5 stale recall WLB | >= 0.30 | 0.9537 | yes |

All three `metrics.json.thresholds.*` flags are `true`.

> **Note (post-2026-05-18):** `metrics.json.thresholds.section_8_*` in this
> artifact uses the pre-v1.2c bare-boolean shape. From v1.2c onward (SPEC §11
> row 2026-05-18) the field is a structured object
> `{status: "PASS"|"FAIL"|"SKIP", passed: bool|null, metric, observed, target,
> reason?}`. This artifact has not been re-run; readers should not parse it as
> the new shape. See `benchmarks/provbench/scoring/SCHEMA.md` §3 for the
> current shape.

## Run Details

| Field | Value |
|---|---|
| Runner | `provbench-phase1` |
| `rule_set_version` | `v1.2` |
| Spec freeze hash | `683d023934c181a8714b9d24c53d011caed31f511becf82ed9e5def92e0ff37c` |
| Labeler pin, corpus | `c2d3b7b03a51a9047ff2d50077200bb52f149448` |
| Labeler pin, facts/diffs | `ababb376f7cf3f92c36dde6035d90932e083517a` |
| Phase 1 git SHA | `97cef97ba347aa7adca0a8367712ab11490f26fe` |
| Repo | `BurntSushi/ripgrep` |
| T0 | `af6b6c543b224d348a8876f0c06245d9ea7929c5` |
| HEAD | `4519153e5e461527f4bca45b042fff45c4ec6fb9` |
| Baseline run | `results/phase0c/2026-05-13-canary` |
| Sample seed | `0xC0DEBABEDEADBEEF` |
| Per-stratum targets | valid 2000; stale_changed 2000; stale_deleted 2000; stale_renamed `usize::MAX`; needs_revalidation 2000 |
| Corpus row count | 2,472,903 |
| Facts row count | 4,101 |
| Evaluated subset | 4,387 rows |
| Coverage | subset (pilot canary; not full-corpus, not held-out) |
| Phase 1 wall time | 23 s |

## v1.1 to v1.2a Delta

| Metric | v1.1 | v1.2a | Delta |
|---|---:|---:|---:|
| `stale_detection.recall.wilson_lower_95` | 0.9537 | 0.9537 | +0.0000 |
| `stale_detection.recall.point` | 0.9619 | 0.9619 | +0.0000 |
| `stale_detection.precision` | 0.7867 | 0.7870 | +0.0003 |
| `stale_detection.f1` | 0.8655 | 0.8657 | +0.0002 |
| `valid_retention.wilson_lower_95` | 0.9716 | 0.9729 | +0.0013 |
| `valid_retention.point` | 0.9822 | 0.9832 | +0.0010 |
| `latency_p50_ms` | 2 | 2 | +0 |
| `latency_p95_ms` | 21 | 22 | +1 |

Gate 2 no-regression clears: stale recall WLB is equal at full precision (`0.9536949768772905`), valid retention WLB improves, and p50 remains 2 ms.

## Per-Rule Confusion Delta

Rule fire counts are unchanged from v1.1: R1 736, R2 240, R3 824, R4 2468, R5 62, R6 10, R7 47. The v1.2a change moves one R4 row from `valid__stale` to `valid__valid`.

| Slice | v1.1 | v1.2a | Delta |
|---|---:|---:|---:|
| R4 `valid__stale` | 17 | 16 | -1 |
| R4 `valid__valid` | 624 | 625 | +1 |
| R4 `stalesourcechanged__valid` | 95 | 95 | 0 |
| Field `stalesourcechanged__valid` (Gate 3) | 0 | 0 | 0 |

The changed row is `Field::CounterWriter::wtr::crates/printer/src/counter.rs::9`, ground truth `Valid`, prediction `stale -> valid`.

Gate 3 clears: Field false-Valid count `0 <= 0 + 20`.

## Section 8 #3 Driver Attribution

The valid-retention gain is attributable to the R4 Field carve-out. The fixed row is a short Field probe whose original `t0_span` is still byte-identical in the post blob, but v1.1 rejected the probe at the length floor. v1.2a lets the probe through because `probe_has_leaf` is true, so R4 returns `Valid`.

The larger serde signal that motivated the change is not re-evaluated here. Serde is burned under the strict Section 10 reading; v1.2a uses it only as prior diagnostic context.

## Hygiene Flags

1. **Dual labeler pins preserved.** Corpus remains `c2d3b7b0`; facts/diffs remain `ababb376`. No labeler, corpus, facts, diffs, baseline, or scoring files changed.
2. **R4 line-presence probe remains heuristic.** v1.2a narrows one failure mode by dropping the length floor only for `Field`. R4 still has 16 `valid__stale` rows and 95 `stalesourcechanged__valid` rows on this pilot.
3. **R7 remains narrow.** Same-extension leaf-symbol-vs-file-stem matching is unchanged from v1.1 and still catches only a limited rename class.
4. **Needs_revalidation routing accuracy = 0.** Same as v1.1; Section 8 does not gate NR routing.
5. **Coverage:** subset pilot canary only, 4,387 evaluated rows. This is not held-out evidence.
6. **Anti-leakage:** serde is not retested under v1.2a. The strict Section 10 leakage clock for held-out validation is deferred to v1.2b.
7. **Determinism preserved:** two v1.2a runs produced 4,387 byte-identical predictions modulo `wall_ms`.
8. **Gate harness note:** the first Gate 2 implementation used full-precision Rust float literals and tripped on an equality-edge f64 parse mismatch. Commit `97cef97` fixes the test to load the v1.1 baseline from `metrics.json` at runtime; the measured stale WLB is equal to v1.1 at full precision.

## v1.2b Reservations

Deferred to v1.2b:

- Python labeler bring-up.
- Flask Round 2 against `pallets/flask` at `2f0c62f5e6e290843f03c1fa70817c7a3c7fd661`.
- Lex normalization in R4 if held-out evidence leaves residual Section 8 #3 misses.
- R3-overrides-R4 chain reorder if held-out evidence shows R4 still preempts better stale-symbol routing.

## Scope

In scope for this round:

- One source behavior change in `phase1/src/rules/r4_span_hash_changed.rs`.
- Unit tests for short Field probes in R4.
- Pilot acceptance gates 1, 2, and 3 in `phase1/tests/end_to_end_canary.rs`.
- v1.2a ripgrep pilot artifacts under `results/ripgrep-pilot-2026-05-15-v1.2a-canary/`.
- This findings document.
- One SPEC Section 11 record row.

Out of scope:

- No Python labeler work.
- No flask Round 2.
- No serde re-run.
- No labeler, corpus, facts, diffs, scoring, or baseline changes.
- No rule changes outside R4.
- No SPEC body edits outside Section 11.
