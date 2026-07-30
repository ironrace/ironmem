#!/usr/bin/env python3
"""Self-test for scripts/sync_skills.py.

Stdlib-only unittest, run as `python3 scripts/test_sync_skills.py` (CI) or
under pytest. Fixture-driven: nothing here reads the real skills/ tree, so
the generator's contract is tested independently of skill content.
"""
from __future__ import annotations

import pathlib
import sys
import tempfile
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

    def test_unknown_harness_argument_is_a_hard_error(self) -> None:
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render("no harness-specific content\n", "gemini")
        self.assertIn("gemini", str(ctx.exception))
        self.assertIn("fixture.md", str(ctx.exception))

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


class LoadVocabTests(unittest.TestCase):
    """load_vocab has five distinct hard-error branches; each is exercised
    against a fixture written to a temp directory so this suite never reads
    the real skills/vocab.toml.
    """

    def _write(self, tmp: pathlib.Path, content: str) -> pathlib.Path:
        path = tmp / "vocab.toml"
        path.write_text(content, encoding="utf-8")
        return path

    def test_unreadable_path_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = pathlib.Path(tmp) / "does-not-exist.toml"
            with self.assertRaises(sync_skills.SkillSyncError) as ctx:
                sync_skills.load_vocab(missing)
            self.assertIn(str(missing), str(ctx.exception))

    def test_malformed_toml_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(pathlib.Path(tmp), "not [valid toml\n")
            with self.assertRaises(sync_skills.SkillSyncError) as ctx:
                sync_skills.load_vocab(path)
            self.assertIn("malformed", str(ctx.exception).lower())
            self.assertIn(str(path), str(ctx.exception))

    def test_missing_harness_table_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                pathlib.Path(tmp),
                '[claude]\nDISPATCH = "Task tool"\n',
            )
            with self.assertRaises(sync_skills.SkillSyncError) as ctx:
                sync_skills.load_vocab(path)
            self.assertIn("codex", str(ctx.exception))

    def test_non_table_section_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                pathlib.Path(tmp),
                'claude = "oops"\n\n[codex]\nDISPATCH = "spawn_agent"\n',
            )
            with self.assertRaises(sync_skills.SkillSyncError) as ctx:
                sync_skills.load_vocab(path)
            self.assertIn("claude", str(ctx.exception))
            self.assertIn("not a table", str(ctx.exception))

    def test_asymmetric_harness_tables_is_a_hard_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                pathlib.Path(tmp),
                '[claude]\nDISPATCH = "Task tool"\nTODO = "TodoWrite"\n\n'
                '[codex]\nDISPATCH = "spawn_agent"\n',
            )
            with self.assertRaises(sync_skills.SkillSyncError) as ctx:
                sync_skills.load_vocab(path)
            self.assertIn("TODO", str(ctx.exception))
            self.assertIn("codex", str(ctx.exception))


import shutil


class HeaderTests(unittest.TestCase):
    def test_header_goes_after_yaml_frontmatter(self) -> None:
        text = "---\nname: iron-build\ndescription: x\n---\n\n# Iron Build\n"
        result = sync_skills.inject_header(text)
        lines = result.split("\n")
        self.assertEqual(lines[0], "---")
        self.assertEqual(lines[3], "---")
        self.assertEqual(lines[4], sync_skills.GENERATED_HEADER)
        self.assertIn("# Iron Build", result)

    def test_header_goes_on_top_without_frontmatter(self) -> None:
        result = sync_skills.inject_header("# Tiers\n")
        self.assertTrue(result.startswith(sync_skills.GENERATED_HEADER + "\n"))

    def test_header_is_not_duplicated(self) -> None:
        once = sync_skills.inject_header("# Tiers\n")
        self.assertEqual(sync_skills.inject_header(once), once)


class WalkTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.source = self.tmp / "skills"
        (self.source / "iron-demo").mkdir(parents=True)
        (self.source / "vocab.toml").write_text(
            '[claude]\nDISPATCH = "Task tool"\n\n[codex]\nDISPATCH = "spawn_agent"\n',
            encoding="utf-8",
        )
        (self.source / "iron-demo" / "SKILL.md").write_text(
            "---\nname: iron-demo\ndescription: demo\n---\n\nUse the {{DISPATCH}}.\n",
            encoding="utf-8",
        )
        self.targets = {
            "claude": self.tmp / "claude" / "skills",
            "codex": self.tmp / "codex" / "skills",
        }

    def test_plan_renders_every_harness(self) -> None:
        rendered = sync_skills.plan(self.source)
        self.assertEqual(sorted(rendered), ["claude", "codex"])
        self.assertIn("Use the Task tool.", rendered["claude"]["iron-demo/SKILL.md"])
        self.assertIn("Use the spawn_agent.", rendered["codex"]["iron-demo/SKILL.md"])

    def test_vocab_is_never_emitted(self) -> None:
        rendered = sync_skills.plan(self.source)
        for files in rendered.values():
            self.assertNotIn("vocab.toml", files)

    def test_write_is_idempotent(self) -> None:
        rendered = sync_skills.plan(self.source)
        first = sync_skills.write(rendered, self.targets)
        self.assertTrue(first)
        second = sync_skills.write(rendered, self.targets)
        self.assertEqual(second, [])

    def test_write_prunes_stale_iron_dirs_only(self) -> None:
        claude = self.targets["claude"]
        (claude / "iron-gone").mkdir(parents=True)
        (claude / "iron-gone" / "SKILL.md").write_text("stale\n", encoding="utf-8")
        (claude / "pr-review-toolkit").mkdir(parents=True)
        (claude / "pr-review-toolkit" / "SKILL.md").write_text("keep\n", encoding="utf-8")
        (claude / "ATTRIBUTION.md").write_text("keep\n", encoding="utf-8")

        sync_skills.write(sync_skills.plan(self.source), self.targets)

        self.assertFalse((claude / "iron-gone").exists())
        self.assertTrue((claude / "pr-review-toolkit" / "SKILL.md").is_file())
        self.assertTrue((claude / "ATTRIBUTION.md").is_file())

    def test_generated_output_carries_header(self) -> None:
        rendered = sync_skills.plan(self.source)
        self.assertIn(sync_skills.GENERATED_HEADER, rendered["claude"]["iron-demo/SKILL.md"])

    def test_diff_labels_distinguish_harnesses(self) -> None:
        # Targets not under ROOT, so labels fall back to parent/leaf -- but
        # they must still differ between harnesses for the same relative
        # path, which is the whole point of labeling by root at all.
        drifted = sync_skills.diff(sync_skills.plan(self.source), self.targets)
        claude_entries = [d for d in drifted if d.startswith("claude/")]
        codex_entries = [d for d in drifted if d.startswith("codex/")]
        self.assertTrue(claude_entries)
        self.assertTrue(codex_entries)
        self.assertNotEqual(claude_entries, codex_entries)

    def test_write_labels_distinguish_harnesses(self) -> None:
        changed = sync_skills.write(sync_skills.plan(self.source), self.targets)
        claude_entries = [c for c in changed if c.startswith("claude/")]
        codex_entries = [c for c in changed if c.startswith("codex/")]
        self.assertTrue(claude_entries)
        self.assertTrue(codex_entries)
        self.assertNotEqual(claude_entries, codex_entries)

    def test_write_prunes_nested_stale_iron_dirs(self) -> None:
        # iron-* skills own nested subdirectories (references/, prompts/),
        # not just a flat SKILL.md. Deleting the whole skill from the
        # canonical source must delete the nested directory too, not just
        # the files inside it.
        (self.source / "iron-nested" / "references").mkdir(parents=True)
        (self.source / "iron-nested" / "SKILL.md").write_text(
            "---\nname: iron-nested\ndescription: nested\n---\n\nUse the {{DISPATCH}}.\n",
            encoding="utf-8",
        )
        (self.source / "iron-nested" / "references" / "notes.md").write_text(
            "notes\n", encoding="utf-8"
        )

        sync_skills.write(sync_skills.plan(self.source), self.targets)
        claude = self.targets["claude"]
        self.assertTrue((claude / "iron-nested" / "references" / "notes.md").is_file())

        shutil.rmtree(self.source / "iron-nested")
        sync_skills.write(sync_skills.plan(self.source), self.targets)

        self.assertFalse((claude / "iron-nested" / "references").exists())
        self.assertFalse((claude / "iron-nested").exists())

    def test_diff_detects_crlf_drift(self) -> None:
        rendered = sync_skills.plan(self.source)
        sync_skills.write(rendered, self.targets)
        destination = self.targets["claude"] / "iron-demo" / "SKILL.md"

        crlf_bytes = destination.read_bytes().replace(b"\n", b"\r\n")
        destination.write_bytes(crlf_bytes)

        drifted = sync_skills.diff(rendered, self.targets)
        self.assertIn("claude/skills/iron-demo/SKILL.md (stale)", drifted)

    def test_write_corrects_crlf_drift(self) -> None:
        rendered = sync_skills.plan(self.source)
        sync_skills.write(rendered, self.targets)
        destination = self.targets["claude"] / "iron-demo" / "SKILL.md"

        crlf_bytes = destination.read_bytes().replace(b"\n", b"\r\n")
        destination.write_bytes(crlf_bytes)

        changed = sync_skills.write(rendered, self.targets)
        self.assertIn("claude/skills/iron-demo/SKILL.md", changed)
        self.assertNotIn(b"\r\n", destination.read_bytes())


class LabelRootTests(unittest.TestCase):
    """_label_root is what makes write()/diff() output name *which* plugin
    root a path belongs to -- .claude-plugin/skills and .codex-plugin/skills
    share the leaf name `skills`, so this is the fix for that ambiguity.
    """

    def test_real_targets_get_repo_relative_labels(self) -> None:
        self.assertEqual(
            sync_skills._label_root(sync_skills.TARGETS["claude"]),
            ".claude-plugin/skills",
        )
        self.assertEqual(
            sync_skills._label_root(sync_skills.TARGETS["codex"]),
            ".codex-plugin/skills",
        )

    def test_target_outside_repo_falls_back_without_crashing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            outside = pathlib.Path(tmp) / "claude" / "skills"
            label = sync_skills._label_root(outside)
            self.assertEqual(label, "claude/skills")
            self.assertNotIn(str(sync_skills.ROOT), label)


if __name__ == "__main__":
    unittest.main()
