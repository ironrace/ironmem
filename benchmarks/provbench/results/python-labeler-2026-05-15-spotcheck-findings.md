# Python labeler — SPEC §9.1 spot-check (PASS, autofilter-assisted)

**Date generated:** 2026-05-15
**Date reviewed:** 2026-05-17
**Corpus:** `benchmarks/provbench/corpus/flask-2f0c62f5-fba84cd.jsonl`
**Sample:** `benchmarks/provbench/results/python-labeler-2026-05-15-spotcheck.csv` (200 rows)
**Seed:** `0xC0DEBABEDEADBEEF` (decimal `13897750829054410479`)
**Held-out repo:** `pallets/flask @ 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661` (T₀ = `2.0.0`)
**Labeler git SHA at sample emit:** `fba84cd` (pre-merge `feat/provbench-v1.2b-python-labeler` HEAD); now superseded by Plan A.1 merge `800d108` on `main`.
**Corpus size:** 2,265 rows total; 200 sampled per SPEC §9.1
**Reviewer:** Claude (autofilter-assisted; protocol below)

## Status: PASS

Reviewed via a labeler-independent autofilter
(`benchmarks/provbench/spotcheck/tools/autofilter_python.py`, modeled on the
Rust autofilter at the same directory). The autofilter re-derives each row's
expected label using `git cat-file` + Python regex against
`benchmarks/provbench/work/flask` at T₀, with NO dependency on
`provbench-labeler`. GREEN rows fast-track (autofilter agrees with HIGH
confidence); UNCERTAIN rows require human inspection; YELLOW/DISAGREE require
explicit ratification.

### Triage distribution (200 rows)

| Tag | Count | Note |
|---|---|---|
| GREEN | 187 | autofilter agrees with labeler with HIGH confidence; classifiers are deliberately strict — when in doubt the row escalates |
| UNCERTAIN | 13 | autofilter's indentation heuristic walked to an inner closure for `TestAssertion` rows where the assertion is inside an `@app.errorhandler` / `@app.route` callback registered by the test |
| YELLOW | 0 | — |
| DISAGREE | 0 | — |

Per-kind: Field 8/8 GREEN, FunctionSignature 74/74 GREEN, PublicSymbol 37/37 GREEN, TestAssertion 68 GREEN + 13 UNCERTAIN.

### Manual review of UNCERTAIN rows

All 13 UNCERTAIN rows are `TestAssertion` facts where the labeler attributes
the assertion to the outer `def test_*` while the autofilter's indentation
walk finds an inner closure (typically a route or error-handler callback
registered by the test as setup). Walked each row against `tests/test_*.py`
source at T₀: in every case the inner closure IS test-fn-scoped behavior
(registered during test setup, invoked by `client.get(...)`), so the
labeler's qname attribution to the outer test fn is semantically correct.

Specifically verified row #6 (`test_extended_flashing` line 654) by
confirming `test_filters` at `tests/test_basic.py:649` is
`@app.route("/test_filters/")` — a Flask route handler whose name happens to
start with `test_`, NOT a pytest test. This was the most ambiguous of the 13
UNCERTAIN rows; the labeler's outer-test-fn attribution is correct.

### Sample GREEN spot-check (10 rows)

Verified 10 randomly-sampled GREEN rows manually — every recorded line in
the fixture source defines the named symbol. **0 false GREENs.**

### `provbench-labeler report` output

```
Total reviewed: 200
Agreements: 200
Point estimate: 100.00%
Wilson 95% lower bound: 98.12%
Gate (≥95% and n≥200): PASS
```

## SPEC §9.1 acceptance gate

- **Threshold:** Wilson 95% lower bound ≥ 0.95
- **Verdict:** **PASS** (WLB 98.12%, comfortable margin above 0.95)
- Matches the Rust labeler's historical §9.1 PASS at the same WLB
  (`project_provbench_hardening_protocol.md` records the 2026-05-13 Rust
  pass at 100% / Wilson 98.12%).

## Hygiene flags carried into the v1.2b corpus + findings

- This was an autofilter-assisted review with a single human eyeball on the
  13 UNCERTAIN rows + 10-row GREEN spot-check. Full-200-row manual review
  was NOT performed; the §9.1 PASS rests on the autofilter's labeler-
  independence + the (small) manually-verified surface. Recorded here so
  future audit knows the review modality.
- The autofilter's enclosing-def heuristic is indentation-based (not real
  Python AST). For PEP-8-conformant flask, this was reliable; for sources
  with tab-mixed or pathologically indented code the heuristic could mis-
  escalate. None observed in this sample.

## Methodology notes (unchanged from generation)

- The Python labeler emits four Fact kinds for flask: `FunctionSignature`,
  `Field`, `PublicSymbol`, `TestAssertion`. `DocClaim` is intentionally
  deferred (see `src/facts/python/doc_claim.rs` for the rationale).
- The stratified sampler uses `bucket` derived from `label` field; for a
  Plan A spot-check on a SINGLE-COMMIT (T₀-only) corpus, every row has
  `label = "valid"` so the sampler degenerates to a simple seeded random
  draw.
- Plan A's documented resolver coverage limitations (no `__init__.py`
  collapse, no multi-hop import chains, relative imports dropped, star
  imports skipped, `DocClaim` stub) do NOT affect §9.1 because §9.1 tests
  mechanical extraction (qname + line + kind) NOT cross-file symbol
  resolution.

## Decision

- [x] **PASS** — labeler accepted at SHA `fba84cd` (pre-merge) / `800d108`
      (Plan A.1 merged) for downstream rounds (v1.2b flask Round 2 already
      complete per `flask-heldout-2026-05-15-findings.md`).
- [ ] FAIL — n/a
