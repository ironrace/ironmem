# ironmem Benchmark Methodology

This document explains **how ironmem measures whether it helps real coding
workflows**, so any public claim is reproducible and confidence-qualified. It is
a reader-facing summary; the frozen measurement contract is
[`docs/METRICS_SPEC.md`](METRICS_SPEC.md) and is the source of truth for every
counter, unit, and reporting query. Nothing here changes metric semantics — when
this doc and the spec disagree, the spec wins.

> **Status today:** ironmem makes **no headline savings claim**. The A/B
> experiment that would produce one has not yet collected enough data (see
> [Current baseline status](#current-baseline-status)). This page documents the
> method and the bar evidence must clear *before* a number is published.

---

## The thesis we are testing

> ironmem + planning discipline reaches **merged-and-passing** with **fewer
> rework loops** and **lower total tokens per task** than superpowers-alone, and
> stays smart in long sessions because durable server-side state makes `/clear`
> cheap. ([METRICS_SPEC §1.1](METRICS_SPEC.md))

The claim is falsifiable: the A/B protocol below either shows a
confidence-qualified reduction for the ironmem arm, or it does not — and a null
or negative result is reported as written.

---

## Corpus selection

Tasks come from **real, repo-scoped backlog work**, never synthetic puzzles, so
that "done" means the same thing it means in production.

- **8–12 tasks**, each a single PR-sized unit of comparable size
  ([METRICS_SPEC §11.1](METRICS_SPEC.md)).
- The corpus is **frozen before any arm runs**; its content hash is recorded in
  the freezing PR body / merge tag, never embedded in the corpus file.
- Each task pins a `base_commit`, ≥1 acceptance criterion, and ≥1 gate command.
  `abeval validate` enforces the reference *shape*; corpus *authenticity* (each
  task is genuine backlog, not a toy) is verified manually at PR review.
- Frozen corpus + format:
  [`benchmarks/abeval/corpus/tasks.jsonl`](../benchmarks/abeval/README.md#corpus-format).

A second, narrower benchmark — the latency/recall harness in the README
[`## Benchmarking`](../README.md#benchmarking) section
(`scripts/benchmark_vs_mempalace.py`) — measures MCP tool-surface latency, search
hit rate, and storage size against a local `mempalace` checkout. It is about
*engine performance*, not the workflow thesis, and is reported separately.

---

## Harness setup

The A/B experiment runs each task under **two isolated arms**
([METRICS_SPEC §11.2](METRICS_SPEC.md)):

| Arm           | Setup                                                                 |
|---------------|-----------------------------------------------------------------------|
| `ironmem`     | ironmem installed + `/collab` planning discipline + worker dispatch.  |
| `superpowers` | superpowers skills alone — no ironmem server-side state, no `/collab`. |

The `superpowers` arm is isolated by **environment, not by prompt wording**: its
spawned CLI runs with `--strict-mcp-config` and an empty `--mcp-config`, so it
loads zero MCP servers and physically cannot reach ironmem's search, knowledge
graph, or drawers even if it tried (the prompt prohibitions are
belt-and-suspenders). Each task × arm runs in its own git worktree; `main` is
never mutated. Full harness contract:
[`benchmarks/abeval/README.md`](../benchmarks/abeval/README.md).

---

## Token accounting

The primary metric is **tokens-to-done per task** — the total tokens consumed to
take one task from start to done, attributed to that task and decomposed by phase
([METRICS_SPEC §2.1, §3](METRICS_SPEC.md)):

```
tokens_to_done(task) = Σ (input + output
                          + cache_creation_input + cache_read_input)
```

- **Phases.** Every token row is bucketed `planning` / `impl` / `review` /
  `rework`, keyed off the collab session's server-side `Phase` (never a reused
  message topic string). More planning/review tokens that *reduce* rework tokens
  is the thesis succeeding, not waste.
- **Claude side** is summed from a `claude -p --output-format stream-json`
  transcript, deduplicated per `message.id`, so subagent sessions are counted
  (the single-envelope `usage` block undercounts subagent-heavy turns).
- **Codex side** subtracts cached input before summing
  (`input = input_tokens − cached_input_tokens`), because Codex's `input_tokens`
  includes cache reads; a literal field copy would inflate Codex tokens by ~85%.
  Codex tokens count toward `tokens_to_done` but Codex cost is left unpriced —
  it is a different provider ([METRICS_SPEC §7.2, §12 2026-06-17](METRICS_SPEC.md)).

### Measured vs estimated

Token counts come from a real `usage` object whenever one exists. When none is
available, a row may be estimated as `ceil(chars / 4)` and flagged
`estimated = 1` ([METRICS_SPEC §6](METRICS_SPEC.md)). The contract:

- **Estimated tokens never enter a headline number.** Headline deltas are
  computed from measured rows only (`estimated = 0`), or the measured and
  estimated splits are reported separately — an estimate is never silently summed
  into a measured total.
- `ironmem report` always surfaces the measured-vs-estimated split, so a reader
  can see how much of any number is real.

This separation exists because a benchmark that mixes measured and estimated
numbers drifts toward whatever the estimate says.

---

## Quality gates

"Done" is an outcome, not a self-assessment.

- In production, a task is **done iff its PR is merged AND CI is green** on the
  merge commit ([METRICS_SPEC §2.2](METRICS_SPEC.md)). Closed-without-merge and
  abandoned tasks are excluded from headline averages and reported separately
  *with their spend*, so "cheap because it gave up" can never look like "cheap
  because it was efficient".
- For the abeval harness, the **done proxy** is measured, never asserted by the
  agent: the arm's agent process completed without error **and** the task's
  frozen `gates` (`cargo test --workspace` + `cargo clippy … -D warnings`) pass
  in the produced workspace ([METRICS_SPEC §12, 2026-06-16](METRICS_SPEC.md)). A
  failed agent or a red gate is not headline-eligible.
- **Goodhart guards** ([METRICS_SPEC §9](METRICS_SPEC.md)): per-call counters are
  diagnostics, never targets; planning/review tokens are not waste; occupancy is
  a health signal, not a score to minimize; non-completion is never cheap.

---

## Sample-size requirements

A headline delta is published **only** when the evidence clears this bar
([METRICS_SPEC §11.3](METRICS_SPEC.md)):

- **≥ 8** merged + CI-green tasks **per arm** of live evidence. Smoke / dry-run
  output (`evidence_class: "smoke"`) is explicitly non-headline.
- Deltas are reported **confidence-qualified** — a confidence interval, or at
  minimum `n` plus spread — never a bare point estimate.
- The three reported deltas: (1) tokens-to-done per task, (2) rework loops
  (`review_rounds + fix_commits`), (3) merged-rate.

---

## Current baseline status

**No headline savings number is published yet, because the ≥8-per-arm bar is not
met.** The only local live evidence to date is a single-task
(`abeval-01-issue-95`, ironmem arm) smoke-validation run — `n = 1`, which the
§11.3 reporting floor correctly refuses as non-headline. The matched
`superpowers` arm at scale has not been run.

What is still needed before a first baseline can be published:

- A cost-approved live A/B campaign producing **≥ 8 merged + CI-green tasks per
  arm** across the frozen corpus.
- Both arms summed and reported confidence-qualified via `abeval report`.

The in-repo A/B analysis artifact (issue #98) currently records a verdict of
**INDETERMINATE** for exactly this reason — the floor is unmet — and that is
reported honestly rather than rounded up to a claim. This page will link a first
baseline here once the data exists.

---

## Reproduce it yourself

The A/B harness is dry-run by default (no network, no model, no agent spawn). A
real run is paid and reached only with **both** `--execute-live` and an explicit
cost-approval opt-in ([`benchmarks/abeval/README.md`](../benchmarks/abeval/README.md#cost-approval-rule-and-fail-closed-live-path)).

```bash
# Validate the frozen corpus and print its content hash.
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- validate

# Dry-run one task through both arms (smoke, non-headline).
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- run \
  --task abeval-01-issue-95 --arms both --dry-run --out /tmp/abeval-run

# Summarize a run / metrics file and enforce the §11.3 headline gate.
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- report --run /tmp/abeval-run
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- report \
  --metrics benchmarks/abeval/fixtures/live_8_per_arm.json

# Aggregate a batched live campaign (≥8/arm) into one gated report.
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- report --metrics-dir /tmp/abeval-campaign
```

**Raw output locations.** A run writes per-task artifacts under
`<out>/<task_id>/`: `run_meta.json` (arms, dry-run flag, evidence_class, per-arm
usage), `ironmem/usage.json`, `superpowers/usage.json`, and — for approved live
runs — `<out>/<task_id>/live_metrics.json` (`evidence_class: "live"`, consumable
by `report --metrics`). Production session token rows land in the local SQLite
store and are surfaced by `ironmem report` (text and `--json`).

The engine latency/recall benchmark is reproduced separately; see the README
[`## Benchmarking`](../README.md#benchmarking) section.

---

## Cross-references

- [`docs/METRICS_SPEC.md`](METRICS_SPEC.md) — the frozen measurement contract
  (token accounting §2, phases §3, estimation §6, cost §7, occupancy §8,
  Goodhart guards §9, reporting SQL §10, A/B protocol §11).
- [`benchmarks/abeval/README.md`](../benchmarks/abeval/README.md) — the A/B
  harness, corpus format, runner commands, and cost-approval rule.
- [`README.md` `## Benchmarking`](../README.md#benchmarking) — the engine
  latency/recall harness.
