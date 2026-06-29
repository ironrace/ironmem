#!/usr/bin/env python3
"""
analyze_campaign_results.py — read-only analysis script for the abeval A/B campaign.

Reads benchmarks/abeval/campaign-merged-live.json (committed bytes) and prints:
  1. Per-task breakdown table (four-component token split, cache-excluded, rework)
  2. Per-arm means (merged-rate, rework_loops, §2.1 tokens, cache-excluded tokens,
     Claude-vs-Codex decomposition)

Token accounting rules (METRICS_SPEC §2.1 + §12 corrected Codex mapping):

  Claude fields (all four are SEPARATE buckets):
    input_tokens                  — noncached fresh input
    output_tokens                 — output
    cache_creation_input_tokens   — tokens written to cache
    cache_read_input_tokens       — tokens read from cache
    §2.1 total = input + output + cache_creation + cache_read

  Codex fields (§12 corrected mapping, 2026-06-17):
    codex_input_tokens            — noncached fresh input (already subtracted)
    codex_output_tokens           — output
    codex_cache_read_input_tokens — cached reads (separate bucket, NOT subset of input)
    codex_cache_creation          = 0 (Codex writes no new cache blocks in this run)
    §2.1 total = codex_input + codex_output + codex_cache_read
    Invariant: codex_cache_read > codex_input is valid (separate fields, not subsets)

  Cache-excluded (input+output only, no cache_creation or cache_read):
    claude_cache_excl = input_tokens + output_tokens
    codex_cache_excl  = codex_input_tokens + codex_output_tokens

  rework_loops (METRICS_SPEC §11.4):
    rework_loops = review_rounds + fix_commits

IMPORTANT: This script is read-only. It does not mutate any state, launch agents,
hit the network, or import ironmem runtime crates.
"""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass
from typing import List


@dataclass
class TaskRow:
    arm: str
    task_key: str
    outcome: str
    ci_green: bool
    estimated: bool
    review_rounds: int
    fix_commits: int
    # Claude four-component buckets
    c_input: int
    c_output: int
    c_cache_create: int
    c_cache_read: int
    # Codex fields (§12 corrected mapping)
    cx_noncached_input: int
    cx_output: int
    cx_cache_read: int

    @property
    def rework_loops(self) -> int:
        return self.review_rounds + self.fix_commits

    @property
    def claude_total_21(self) -> int:
        """Claude §2.1 contribution: input + output + cache_creation + cache_read."""
        return self.c_input + self.c_output + self.c_cache_create + self.c_cache_read

    @property
    def codex_total_21(self) -> int:
        """Codex §2.1 contribution: noncached_input + output + cache_read (§12 mapping)."""
        return self.cx_noncached_input + self.cx_output + self.cx_cache_read

    @property
    def total_21(self) -> int:
        """Combined §2.1 tokens for both providers."""
        return self.claude_total_21 + self.codex_total_21

    @property
    def claude_cache_excl(self) -> int:
        """Claude cache-excluded: input + output only."""
        return self.c_input + self.c_output

    @property
    def codex_cache_excl(self) -> int:
        """Codex cache-excluded: noncached_input + output only."""
        return self.cx_noncached_input + self.cx_output

    @property
    def cache_excl(self) -> int:
        """Combined cache-excluded tokens (input + output, both providers)."""
        return self.claude_cache_excl + self.codex_cache_excl


def load_rows(path: str) -> List[TaskRow]:
    """Load and parse campaign-merged-live.json into TaskRow objects."""
    with open(path) as f:
        data = json.load(f)
    assert data.get("evidence_class") == "live", (
        f"Expected evidence_class='live', got {data.get('evidence_class')!r}"
    )
    rows: List[TaskRow] = []
    for t in data["tasks"]:
        row = TaskRow(
            arm=t["arm"],
            task_key=t["task_key"],
            outcome=t["outcome"],
            ci_green=bool(t["ci_green"]),
            estimated=bool(t.get("estimated", False)),
            review_rounds=int(t["review_rounds"]),
            fix_commits=int(t["fix_commits"]),
            c_input=int(t["input_tokens"]),
            c_output=int(t["output_tokens"]),
            c_cache_create=int(t["cache_creation_input_tokens"]),
            c_cache_read=int(t["cache_read_input_tokens"]),
            cx_noncached_input=int(t["codex_input_tokens"]),
            cx_output=int(t["codex_output_tokens"]),
            cx_cache_read=int(t["codex_cache_read_input_tokens"]),
        )
        if row.estimated:
            raise ValueError(
                f"Estimated row is not allowed in headline metrics: {row.task_key}"
            )
        if row.outcome != "merged" or not row.ci_green:
            raise ValueError(
                "Non-completed row is not allowed in headline metrics: "
                f"{row.task_key} outcome={row.outcome!r} ci_green={row.ci_green!r}"
            )
        rows.append(row)
    return rows


def split_by_arm(rows: List[TaskRow]):
    ironmem = [r for r in rows if r.arm == "ironmem"]
    superpowers = [r for r in rows if r.arm == "superpowers"]
    return ironmem, superpowers


def mean(vals: List[int]) -> float:
    return sum(vals) / len(vals) if vals else 0.0


def print_per_task_table(rows: List[TaskRow]) -> None:
    print("Per-task breakdown")
    print("=" * 130)
    hdr = (
        f"{'arm':<12} {'task_key':<48} {'claude_21':>12} {'codex_21':>12}"
        f" {'total_21':>12} {'cache_excl':>12} {'rework':>7}"
    )
    print(hdr)
    print("-" * 130)
    for r in rows:
        short_key = r.task_key.replace(":ironmem", "").replace(":superpowers", "")
        print(
            f"{r.arm:<12} {short_key:<48} {r.claude_total_21:>12,} {r.codex_total_21:>12,}"
            f" {r.total_21:>12,} {r.cache_excl:>12,} {r.rework_loops:>7}"
        )
    print()


def print_arm_stats(rows: List[TaskRow], name: str) -> dict:
    n = len(rows)
    merged = sum(1 for r in rows if r.outcome == "merged" and r.ci_green)
    total_21_vals = [r.total_21 for r in rows]
    cache_excl_vals = [r.cache_excl for r in rows]
    rework_vals = [r.rework_loops for r in rows]
    claude_vals = [r.claude_total_21 for r in rows]
    codex_vals = [r.codex_total_21 for r in rows]

    print(f"Arm: {name}  (n={n})")
    print(f"  merged-rate:          {merged}/{n} ({100 * merged // n}%)")
    print(f"  rework_loops:         mean={mean(rework_vals):.1f}  range=[{min(rework_vals)}, {max(rework_vals)}]")
    print(f"  tokens §2.1:          mean={mean(total_21_vals):>12,.0f}  min={min(total_21_vals):,}  max={max(total_21_vals):,}")
    print(f"  cache-excluded:       mean={mean(cache_excl_vals):>12,.0f}  min={min(cache_excl_vals):,}  max={max(cache_excl_vals):,}")
    print(f"  claude §2.1:          mean={mean(claude_vals):>12,.0f}")
    print(f"  codex  §2.1:          mean={mean(codex_vals):>12,.0f}")
    return {
        "n": n,
        "merged": merged,
        "total_21_mean": mean(total_21_vals),
        "total_21_min": min(total_21_vals),
        "total_21_max": max(total_21_vals),
        "cache_excl_mean": mean(cache_excl_vals),
        "cache_excl_min": min(cache_excl_vals),
        "cache_excl_max": max(cache_excl_vals),
        "rework_mean": mean(rework_vals),
        "claude_21_mean": mean(claude_vals),
        "codex_21_mean": mean(codex_vals),
    }


def main(argv: List[str] | None = None) -> None:
    argv = sys.argv[1:] if argv is None else argv
    script_dir = os.path.dirname(os.path.abspath(__file__))
    default_path = os.path.join(script_dir, "campaign-merged-live.json")
    if len(argv) > 1:
        print(
            "Usage: analyze_campaign_results.py [campaign-merged-live.json]",
            file=sys.stderr,
        )
        sys.exit(2)
    path = argv[0] if argv else default_path

    if not os.path.isfile(path):
        print(f"ERROR: not found: {path}", file=sys.stderr)
        sys.exit(1)

    rows = load_rows(path)
    ironmem_rows, sp_rows = split_by_arm(rows)

    # Print all rows sorted: ironmem first, then superpowers
    all_sorted = ironmem_rows + sp_rows
    print_per_task_table(all_sorted)

    print("Arm means")
    print("=" * 60)
    im = print_arm_stats(ironmem_rows, "ironmem")
    print()
    sp = print_arm_stats(sp_rows, "superpowers")
    print()

    print("Ratios (ironmem / superpowers)")
    print("=" * 60)
    if sp["total_21_mean"] > 0:
        ratio_21 = im["total_21_mean"] / sp["total_21_mean"]
        print(f"  §2.1 tokens ratio:    {ratio_21:.1f}x")
    if sp["cache_excl_mean"] > 0:
        ratio_excl = im["cache_excl_mean"] / sp["cache_excl_mean"]
        print(f"  cache-excl ratio:     {ratio_excl:.1f}x")
    print()
    print("Note: ratios measure harness+workflow differences, not memory-on vs memory-off.")
    print("See docs/METRICS_RESULTS.md for the confound analysis and thesis verdict.")


if __name__ == "__main__":
    main()
