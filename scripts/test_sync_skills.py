#!/usr/bin/env python3
"""Self-test for scripts/sync_skills.py.

Stdlib-only unittest, run as `python3 scripts/test_sync_skills.py` (CI) or
under pytest. Fixture-driven: nothing here reads the real skills/ tree, so
the generator's contract is tested independently of skill content.
"""
from __future__ import annotations

import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import sync_skills  # noqa: E402

VOCAB = {
    "claude": {"DISPATCH": "Task tool", "TODO": "TodoWrite"},
    "codex": {"DISPATCH": "spawn_agent", "TODO": "update_plan"},
}


class RenderTests(unittest.TestCase):
    def render(self, text: str, harness: str) -> str:
        return sync_skills.render(text, harness, VOCAB, origin="fixture.md")

    def test_token_substitution_is_per_harness(self) -> None:
        text = "Dispatch with the {{DISPATCH}} and track via {{TODO}}.\n"
        self.assertEqual(
            self.render(text, "claude"),
            "Dispatch with the Task tool and track via TodoWrite.\n",
        )
        self.assertEqual(
            self.render(text, "codex"),
            "Dispatch with the spawn_agent and track via update_plan.\n",
        )

    def test_harness_block_kept_for_matching_harness(self) -> None:
        text = (
            "before\n"
            "<!-- harness:codex -->\n"
            "codex only\n"
            "<!-- /harness -->\n"
            "after\n"
        )
        self.assertEqual(self.render(text, "codex"), "before\ncodex only\nafter\n")

    def test_harness_block_dropped_for_other_harness(self) -> None:
        text = (
            "before\n"
            "<!-- harness:codex -->\n"
            "codex only\n"
            "<!-- /harness -->\n"
            "after\n"
        )
        self.assertEqual(self.render(text, "claude"), "before\nafter\n")

    def test_unknown_token_is_a_hard_error(self) -> None:
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render("use the {{DISPACH}}\n", "claude")
        self.assertIn("DISPACH", str(ctx.exception))
        self.assertIn("fixture.md", str(ctx.exception))

    def test_unclosed_harness_block_is_a_hard_error(self) -> None:
        text = "before\n<!-- harness:codex -->\nno terminator\n"
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render(text, "codex")
        self.assertIn("unclosed", str(ctx.exception).lower())

    def test_unknown_harness_name_in_block_is_a_hard_error(self) -> None:
        text = "<!-- harness:gemini -->\nx\n<!-- /harness -->\n"
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render(text, "claude")
        self.assertIn("gemini", str(ctx.exception))

    def test_stray_close_tag_is_a_hard_error(self) -> None:
        with self.assertRaises(sync_skills.SkillSyncError):
            self.render("before\n<!-- /harness -->\nafter\n", "claude")

    def test_token_inside_kept_block_is_substituted(self) -> None:
        text = "<!-- harness:codex -->\nrun {{DISPATCH}}\n<!-- /harness -->\n"
        self.assertEqual(self.render(text, "codex"), "run spawn_agent\n")

    def test_render_is_idempotent_on_token_free_text(self) -> None:
        text = "plain markdown, no tokens\n"
        once = self.render(text, "claude")
        self.assertEqual(self.render(once, "claude"), once)

    def test_nested_blocks_are_rejected(self) -> None:
        text = (
            "<!-- harness:codex -->\n"
            "<!-- harness:claude -->\n"
            "x\n"
            "<!-- /harness -->\n"
            "<!-- /harness -->\n"
        )
        with self.assertRaises(sync_skills.SkillSyncError):
            self.render(text, "codex")


if __name__ == "__main__":
    unittest.main()
