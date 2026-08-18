#!/usr/bin/env python3
"""Regression check for the fail-closed HumanLayer workspace policy."""

from __future__ import annotations

import json
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SHARED_CONFIG = ROOT / ".humanlayer" / "workspace.json"
LOCAL_CONFIG = ROOT / ".humanlayer" / "workspace.local.json"
LOCAL_CONFIG_PATTERN = ".humanlayer/workspace.local.json"
TRACKED_GITIGNORE = ROOT / ".gitignore"


class HumanLayerWorkspacePolicyTest(unittest.TestCase):
    def test_shared_workspace_setup_is_disabled(self) -> None:
        config = json.loads(SHARED_CONFIG.read_text(encoding="utf-8"))
        self.assertEqual(
            config,
            {"disabled": True},
            "HumanLayer workspace setup must stay fully disabled because default copyGlobs include .env files and cannot be subtracted",
        )

    def test_local_override_is_absent_and_ignored(self) -> None:
        self.assertFalse(
            LOCAL_CONFIG.exists(),
            ".humanlayer/workspace.local.json can re-enable workspace setup and is prohibited in the pilot checkout",
        )
        result = subprocess.run(
            ["git", "check-ignore", "-v", "--", LOCAL_CONFIG_PATTERN],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            result.returncode,
            0,
            "the local HumanLayer override must be ignored by the repository .gitignore; "
            f"git check-ignore output: stdout={result.stdout!r}, stderr={result.stderr!r}",
        )

        tracked_gitignore = subprocess.run(
            ["git", "ls-files", "--error-unmatch", "--", ".gitignore"],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            tracked_gitignore.returncode,
            0,
            "the HumanLayer policy requires a tracked repository .gitignore; "
            f"git ls-files output: stdout={tracked_gitignore.stdout!r}, "
            f"stderr={tracked_gitignore.stderr!r}",
        )
        self.assertEqual(
            tracked_gitignore.stdout.strip(),
            ".gitignore",
            "git ls-files must resolve the policy source to the repository .gitignore",
        )

        matching_lines = [
            line for line in result.stdout.splitlines() if line.strip()
        ]
        self.assertEqual(
            len(matching_lines),
            1,
            "git check-ignore -v must return exactly one match describing the winning rule; "
            f"stdout={result.stdout!r}",
        )
        source_and_pattern, separator, ignored_path = matching_lines[0].partition(
            "\t"
        )
        self.assertTrue(
            separator,
            "git check-ignore -v output must separate rule metadata from the path with a tab; "
            f"line={matching_lines[0]!r}",
        )
        source_fields = source_and_pattern.rsplit(":", 2)
        self.assertEqual(
            len(source_fields),
            3,
            "git check-ignore -v output must include source, line number, and pattern; "
            f"line={matching_lines[0]!r}",
        )
        source, line_number, pattern = source_fields
        self.assertTrue(
            line_number.isdigit(),
            "git check-ignore -v output must include a numeric source line number; "
            f"line={matching_lines[0]!r}",
        )
        source_path = Path(source)
        if not source_path.is_absolute():
            source_path = ROOT / source_path
        self.assertEqual(
            source_path.resolve(),
            TRACKED_GITIGNORE.resolve(),
            "the winning ignore rule must come from the repository's tracked .gitignore; "
            f"source={source!r}, output={matching_lines[0]!r}",
        )
        self.assertEqual(
            pattern,
            LOCAL_CONFIG_PATTERN,
            "the winning repository ignore rule must exactly match the local override path; "
            f"output={matching_lines[0]!r}",
        )
        self.assertEqual(
            ignored_path,
            LOCAL_CONFIG_PATTERN,
            "git check-ignore -v must report the exact local override path; "
            f"output={matching_lines[0]!r}",
        )


if __name__ == "__main__":
    unittest.main()
