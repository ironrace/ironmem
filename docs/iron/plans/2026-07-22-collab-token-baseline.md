# Collab Token-Cost Baseline and MCP Regression Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make issue 212 reproducible by capturing a reference `/collab` report, preserving phase/token/MCP/prompt-size measurements in a committed JSON artifact, and failing a regression check when response-size distributions exceed the baseline threshold.

**Architecture:** Extend the existing read-only `ironmem report --json --task <collab-session>` payload with compact per-tool response-size distribution statistics (count, total, mean, p50, p95, max). A Python standard-library tool will invoke `ironmem report`, combine the selected report with measured Codex prompt-file sizes, write a schema-versioned baseline, and compare a later report against that baseline using a configurable relative threshold. Missing baseline tools or malformed data fail closed; absent current tools are reported as regressions.

**Tech Stack:** Rust, SQLite query aggregation, serde JSON, Python 3 standard library, GitHub Actions.

---

### Task 1: Add the report distribution contract with tests first

**Files:**
- Modify: `crates/ironmem/src/db/metrics.rs`
- Modify: `crates/ironmem/src/report/mod.rs`
- Modify: `crates/ironmem/tests/report_golden.rs`

- [ ] Add a failing test proving a selected collab session returns all tool distributions with deterministic p50/p95/max values and excludes another session.
- [ ] Run that focused test and confirm it fails because the distribution API is absent.
- [ ] Implement the distribution structs, nearest-rank percentile helper, filtered SQL query, and report wiring.
- [ ] Run the focused Rust test and the existing report tests.

### Task 2: Add baseline capture and regression checking with tests first

**Files:**
- Create: `scripts/collab_baseline.py`
- Create: `scripts/test_collab_baseline.py`

- [ ] Add failing tests for report extraction, prompt-size measurement, baseline serialization, pass behavior, missing-tool failure, and threshold failure.
- [ ] Run the Python test file and confirm it fails because the tool does not exist.
- [ ] Implement `capture` and `check` subcommands using only the Python standard library; invoke `ironmem report --json`, validate schema/version, and use a strict non-zero exit on regressions.
- [ ] Run the Python tests and self-check the CLI help.

### Task 3: Commit a reference artifact

**Files:**
- Create: `docs/BENCHMARKS/collab-baseline-2026-07-22.json`

- [ ] Capture a complete recorded `CodingComplete` collab session with the report tool and the checked-in Codex prompt templates as the prompt-size inputs.
- [ ] Validate the artifact with the capture/check tool and document its source session and limitations in the artifact metadata.

### Task 4: Wire the repeatable gate and documentation

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `docs/BENCHMARKS.md`
- Modify: `README.md`
- Modify: `docs/METRICS_SPEC.md`
- Modify: `CHANGELOG.md`

- [ ] Add a CI step that checks the committed artifact against a supplied report fixture without requiring a live model or network.
- [ ] Document the live reference-session capture command, before/after protocol, threshold semantics, and the fixture gate.
- [ ] Run formatting, clippy, workspace tests, Python tests, and documentation/methodology checks.

### Task 5: Review and hand off

- [ ] Inspect the complete diff for scope, security, deterministic output, and accidental local-data leakage.
- [ ] Write a concise durable summary to shared project memory.

