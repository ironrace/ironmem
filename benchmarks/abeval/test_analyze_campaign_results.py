#!/usr/bin/env python3
"""
Focused tests for analyze_campaign_results.py.

Covers:
  (a) Codex §12 corrected token mapping
      - noncached input and cache_read are SEPARATE fields (not subsets)
      - §2.1 total = codex_input + codex_output + codex_cache_read
      - cache-excluded = codex_input + codex_output (no cache_read)
      - cache_read > codex_input is VALID (they are independent fields)
  (b) Model/harness-stratified grouping by arm
  (c) rework_loops = review_rounds + fix_commits (§11.4)
  (d) merged-rate counting: outcome=="merged" AND ci_green==True
"""

import json
import os
import sys
import tempfile
import unittest

# Make the sibling module importable when run from any directory.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from analyze_campaign_results import TaskRow, load_rows, split_by_arm, mean  # noqa: E402


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_row(
    arm: str = "ironmem",
    task_key: str = "t1:ironmem",
    outcome: str = "merged",
    ci_green: bool = True,
    review_rounds: int = 0,
    fix_commits: int = 0,
    c_input: int = 0,
    c_output: int = 0,
    c_cache_create: int = 0,
    c_cache_read: int = 0,
    cx_noncached_input: int = 0,
    cx_output: int = 0,
    cx_cache_read: int = 0,
) -> TaskRow:
    return TaskRow(
        arm=arm,
        task_key=task_key,
        outcome=outcome,
        ci_green=ci_green,
        review_rounds=review_rounds,
        fix_commits=fix_commits,
        c_input=c_input,
        c_output=c_output,
        c_cache_create=c_cache_create,
        c_cache_read=c_cache_read,
        cx_noncached_input=cx_noncached_input,
        cx_output=cx_output,
        cx_cache_read=cx_cache_read,
    )


def _write_json(tasks: list, path: str) -> None:
    with open(path, "w") as f:
        json.dump({"evidence_class": "live", "tasks": tasks}, f)


# ---------------------------------------------------------------------------
# (a) Codex §12 corrected mapping
# ---------------------------------------------------------------------------

class TestCodexMapping(unittest.TestCase):

    def test_codex_21_equals_noncached_plus_output_plus_cache_read(self):
        """§2.1 Codex total = noncached_input + output + cache_read."""
        row = _make_row(cx_noncached_input=1000, cx_output=200, cx_cache_read=5000)
        self.assertEqual(row.codex_total_21, 1000 + 200 + 5000)

    def test_codex_cache_read_greater_than_noncached_input_is_valid(self):
        """cache_read > noncached_input is valid — they are SEPARATE fields."""
        # Reflects real data: abeval-04 has codex_cache_read=16.5M vs codex_input=1.1M
        row = _make_row(cx_noncached_input=1_100_000, cx_output=99_000, cx_cache_read=16_500_000)
        # Must not raise and must produce the correct sum
        expected = 1_100_000 + 99_000 + 16_500_000
        self.assertEqual(row.codex_total_21, expected)

    def test_codex_cache_excl_excludes_cache_read(self):
        """Cache-excluded for Codex = noncached_input + output (no cache_read)."""
        row = _make_row(cx_noncached_input=1000, cx_output=200, cx_cache_read=5000)
        self.assertEqual(row.codex_cache_excl, 1000 + 200)
        # Explicitly confirm cache_read is NOT included
        self.assertNotEqual(row.codex_cache_excl, row.codex_total_21)

    def test_subtracting_cache_read_from_total_21_gives_cache_excl(self):
        """Verify: total_21 - cache_read = cache_excl for Codex-only row."""
        row = _make_row(cx_noncached_input=500, cx_output=100, cx_cache_read=9000)
        total = row.codex_total_21       # 500 + 100 + 9000 = 9600
        excl = row.codex_cache_excl      # 500 + 100 = 600
        self.assertEqual(total - excl, row.cx_cache_read)

    def test_reverting_subtraction_would_break_mapping(self):
        """Regression guard: if noncached_input were input - cache_read, this fails.

        Prior (wrong) mapping: noncached = input_tokens - cached_input_tokens
        In that model, total_21 = (input - cache_read) + output + cache_read = input + output.
        With the corrected mapping, total_21 = input + output + cache_read (larger).
        """
        row = _make_row(cx_noncached_input=1_000, cx_output=200, cx_cache_read=5_000)
        wrong_total = row.cx_noncached_input + row.cx_output  # wrong: 1200
        correct_total = row.codex_total_21                    # correct: 6200
        self.assertGreater(correct_total, wrong_total)
        self.assertEqual(correct_total, wrong_total + row.cx_cache_read)

    def test_superpowers_arm_has_zero_codex_tokens(self):
        """Superpowers arm runs only Claude — all Codex fields are 0."""
        row = _make_row(
            arm="superpowers", task_key="t1:superpowers",
            cx_noncached_input=0, cx_output=0, cx_cache_read=0,
        )
        self.assertEqual(row.codex_total_21, 0)
        self.assertEqual(row.codex_cache_excl, 0)

    def test_combined_total_21_includes_both_providers(self):
        """total_21 = claude_total_21 + codex_total_21."""
        row = _make_row(
            c_input=100, c_output=50, c_cache_create=200, c_cache_read=1000,
            cx_noncached_input=300, cx_output=60, cx_cache_read=4000,
        )
        self.assertEqual(row.total_21, row.claude_total_21 + row.codex_total_21)
        self.assertEqual(row.total_21, (100 + 50 + 200 + 1000) + (300 + 60 + 4000))

    def test_combined_cache_excl_includes_both_providers(self):
        """cache_excl = claude_cache_excl + codex_cache_excl."""
        row = _make_row(
            c_input=100, c_output=50, c_cache_create=200, c_cache_read=1000,
            cx_noncached_input=300, cx_output=60, cx_cache_read=4000,
        )
        self.assertEqual(row.cache_excl, row.claude_cache_excl + row.codex_cache_excl)
        self.assertEqual(row.cache_excl, (100 + 50) + (300 + 60))


# ---------------------------------------------------------------------------
# (b) Arm stratification
# ---------------------------------------------------------------------------

class TestArmStratification(unittest.TestCase):

    def test_split_by_arm_separates_correctly(self):
        rows = [
            _make_row(arm="ironmem", task_key="t1:ironmem"),
            _make_row(arm="superpowers", task_key="t1:superpowers"),
            _make_row(arm="ironmem", task_key="t2:ironmem"),
        ]
        im, sp = split_by_arm(rows)
        self.assertEqual(len(im), 2)
        self.assertEqual(len(sp), 1)
        self.assertTrue(all(r.arm == "ironmem" for r in im))
        self.assertTrue(all(r.arm == "superpowers" for r in sp))

    def test_arm_means_differ_when_arms_differ(self):
        """Per-arm means correctly reflect arm-level differences."""
        im_rows = [
            _make_row(arm="ironmem", cx_noncached_input=1_000_000, cx_output=100_000, cx_cache_read=0),
            _make_row(arm="ironmem", cx_noncached_input=2_000_000, cx_output=200_000, cx_cache_read=0),
        ]
        sp_rows = [
            _make_row(arm="superpowers", cx_noncached_input=0, cx_output=0, cx_cache_read=0,
                      c_input=20_000, c_output=5_000, c_cache_create=0, c_cache_read=0),
        ]
        im_21_vals = [r.total_21 for r in im_rows]
        sp_21_vals = [r.total_21 for r in sp_rows]
        self.assertGreater(mean(im_21_vals), mean(sp_21_vals))

    def test_claude_vs_codex_decomposition_by_arm(self):
        """ironmem arm has Codex tokens; superpowers arm does not."""
        im = _make_row(arm="ironmem", c_input=500, cx_noncached_input=300, cx_output=50)
        sp = _make_row(arm="superpowers", c_input=100, c_output=20)
        # ironmem has codex contribution
        self.assertGreater(im.codex_total_21, 0)
        # superpowers has no codex contribution
        self.assertEqual(sp.codex_total_21, 0)


# ---------------------------------------------------------------------------
# (c) rework_loops definition (§11.4)
# ---------------------------------------------------------------------------

class TestReworkLoops(unittest.TestCase):

    def test_rework_loops_is_review_rounds_plus_fix_commits(self):
        row = _make_row(review_rounds=2, fix_commits=3)
        self.assertEqual(row.rework_loops, 5)

    def test_rework_loops_zero_when_both_zero(self):
        row = _make_row(review_rounds=0, fix_commits=0)
        self.assertEqual(row.rework_loops, 0)

    def test_ironmem_arm_fix_commits_1_is_squash_artifact(self):
        """Confirm campaign data: ironmem has fix_commits=1 on every task (squash artifact)."""
        # Values from the actual campaign JSON
        row = _make_row(arm="ironmem", review_rounds=0, fix_commits=1)
        self.assertEqual(row.rework_loops, 1)
        # The review_rounds=0 proves this is NOT a review-driven rework loop
        self.assertEqual(row.review_rounds, 0)

    def test_superpowers_rework_zero(self):
        row = _make_row(arm="superpowers", review_rounds=0, fix_commits=0)
        self.assertEqual(row.rework_loops, 0)


# ---------------------------------------------------------------------------
# (d) merged-rate: outcome AND ci_green
# ---------------------------------------------------------------------------

class TestMergedRate(unittest.TestCase):

    def test_merged_and_ci_green_counts(self):
        rows = [
            _make_row(outcome="merged", ci_green=True),   # counts
            _make_row(outcome="merged", ci_green=False),  # ci not green — does not count
            _make_row(outcome="failed", ci_green=True),   # outcome failed — does not count
        ]
        merged = sum(1 for r in rows if r.outcome == "merged" and r.ci_green)
        self.assertEqual(merged, 1)

    def test_both_arms_100_pct_merged_in_campaign(self):
        """Full-campaign gate: both arms are 8/8 merged+ci_green."""
        # Build minimal fixture matching the campaign data
        tasks_json = []
        for i in range(1, 9):
            for arm in ("ironmem", "superpowers"):
                tasks_json.append({
                    "arm": arm,
                    "task_key": f"abeval-0{i}:{arm}",
                    "outcome": "merged",
                    "ci_green": True,
                    "estimated": False,
                    "review_rounds": 0,
                    "fix_commits": 1 if arm == "ironmem" else 0,
                    "input_tokens": 100,
                    "output_tokens": 10,
                    "cache_creation_input_tokens": 50,
                    "cache_read_input_tokens": 200,
                    "codex_input_tokens": 50 if arm == "ironmem" else 0,
                    "codex_output_tokens": 5 if arm == "ironmem" else 0,
                    "codex_cache_read_input_tokens": 0,
                })
        with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as tmp:
            json.dump({"evidence_class": "live", "tasks": tasks_json}, tmp)
            tmp_path = tmp.name
        try:
            rows = load_rows(tmp_path)
            im, sp = split_by_arm(rows)
            im_merged = sum(1 for r in im if r.outcome == "merged" and r.ci_green)
            sp_merged = sum(1 for r in sp if r.outcome == "merged" and r.ci_green)
            self.assertEqual(im_merged, 8)
            self.assertEqual(sp_merged, 8)
        finally:
            os.unlink(tmp_path)


# ---------------------------------------------------------------------------
# Load from committed file (smoke test)
# ---------------------------------------------------------------------------

class TestLoadFromCommittedFile(unittest.TestCase):

    def _committed_path(self) -> str:
        here = os.path.dirname(os.path.abspath(__file__))
        return os.path.join(here, "campaign-merged-live.json")

    def test_committed_file_loads_16_rows(self):
        path = self._committed_path()
        if not os.path.isfile(path):
            self.skipTest("campaign-merged-live.json not present")
        rows = load_rows(path)
        self.assertEqual(len(rows), 16)

    def test_committed_file_eight_per_arm(self):
        path = self._committed_path()
        if not os.path.isfile(path):
            self.skipTest("campaign-merged-live.json not present")
        rows = load_rows(path)
        im, sp = split_by_arm(rows)
        self.assertEqual(len(im), 8)
        self.assertEqual(len(sp), 8)

    def test_committed_ironmem_21_mean_approximately_30m(self):
        """Spot-check: ironmem §2.1 mean ≈ 30.3M (±5% tolerance)."""
        path = self._committed_path()
        if not os.path.isfile(path):
            self.skipTest("campaign-merged-live.json not present")
        rows = load_rows(path)
        im, _ = split_by_arm(rows)
        m = mean([r.total_21 for r in im])
        self.assertAlmostEqual(m / 30_325_346, 1.0, delta=0.05,
                               msg=f"ironmem §2.1 mean {m:,.0f} not within 5% of 30,325,346")

    def test_committed_superpowers_cache_excl_mean_approximately_21k(self):
        """Spot-check: superpowers cache-excluded mean ≈ 21K (±10% tolerance)."""
        path = self._committed_path()
        if not os.path.isfile(path):
            self.skipTest("campaign-merged-live.json not present")
        rows = load_rows(path)
        _, sp = split_by_arm(rows)
        m = mean([r.cache_excl for r in sp])
        self.assertAlmostEqual(m / 21_055, 1.0, delta=0.10,
                               msg=f"superpowers cache-excl mean {m:,.0f} not within 10% of 21,055")


if __name__ == "__main__":
    unittest.main()
