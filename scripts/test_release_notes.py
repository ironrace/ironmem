#!/usr/bin/env python3
"""Self-test for scripts/release_notes.py.

Drives the extractor through fixture changelogs (never the real CHANGELOG.md,
so the test does not depend on what has been released). Covers the section
bounds, exact version matching, the missing/empty failure that gates a
release, and truncation at GitHub's body limit. Stdlib only (unittest +
subprocess); run directly:

    python3 scripts/test_release_notes.py
"""
from __future__ import annotations

import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / "release_notes.py"

_spec = importlib.util.spec_from_file_location("release_notes", SCRIPT)
release_notes = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release_notes)

CHANGELOG = """# Changelog

## [Unreleased]

### Added

- Not released yet.

## [0.6.0] - 2026-10-17

### Added

- **Handoff nudge.** Stop asks for a handoff.

### Fixed

- `get_drawer` loads in Claude Code.

## [0.6.0-rc1] - 2026-10-10

- Release candidate.

## [0.5.1] - 2026-07-29

## [0.5.0] - 2026-07-07

### Added

- Oldest entry.
"""


def run_cli(changelog: str, *args: str) -> subprocess.CompletedProcess:
    with tempfile.TemporaryDirectory() as tmp:
        path = pathlib.Path(tmp) / "CHANGELOG.md"
        path.write_text(changelog, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args, "--changelog", str(path)],
            capture_output=True,
            text=True,
        )


class ExtractSectionTest(unittest.TestCase):
    def test_section_stops_at_the_next_version_heading(self):
        self.assertEqual(
            release_notes.extract_section(CHANGELOG, "0.6.0"),
            "### Added\n\n- **Handoff nudge.** Stop asks for a handoff.\n\n"
            "### Fixed\n\n- `get_drawer` loads in Claude Code.\n",
        )

    def test_last_section_runs_to_end_of_file(self):
        self.assertEqual(
            release_notes.extract_section(CHANGELOG, "0.5.0"),
            "### Added\n\n- Oldest entry.\n",
        )

    def test_version_must_match_exactly(self):
        self.assertIsNone(release_notes.extract_section(CHANGELOG, "0.6"))
        self.assertEqual(
            release_notes.extract_section(CHANGELOG, "0.6.0-rc1"), "- Release candidate.\n"
        )

    def test_missing_and_empty_sections_are_none(self):
        self.assertIsNone(release_notes.extract_section(CHANGELOG, "9.9.9"))
        self.assertIsNone(release_notes.extract_section(CHANGELOG, "0.5.1"))


class FitReleaseBodyTest(unittest.TestCase):
    URL = "https://github.com/ironrace/ironmem/blob/v1.0.0/CHANGELOG.md"

    def test_short_body_is_unchanged(self):
        self.assertEqual(release_notes.fit_release_body("- a\n", self.URL, limit=100), "- a\n")

    def test_long_body_is_cut_at_a_line_and_links_the_changelog(self):
        body = "".join(f"- entry {n:03}\n" for n in range(200))
        fitted = release_notes.fit_release_body(body, self.URL, limit=500)
        self.assertLessEqual(len(fitted), 500)
        self.assertIn(self.URL, fitted)
        kept = fitted.split("\n\n_Truncated")[0]
        self.assertTrue(body.startswith(kept + "\n"), "cut mid-line")


class CliTest(unittest.TestCase):
    def test_tag_with_v_prefix_prints_the_section(self):
        result = run_cli(CHANGELOG, "v0.6.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.startswith("### Added\n"))
        self.assertNotIn("0.6.0-rc1", result.stdout)

    def test_out_writes_the_file_and_nothing_to_stdout(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp) / "notes.md"
            result = run_cli(CHANGELOG, "0.5.0", "--out", str(out))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertEqual(out.read_text(encoding="utf-8"), "### Added\n\n- Oldest entry.\n")

    def test_missing_section_fails_and_names_the_version(self):
        for version in ("v9.9.9", "0.5.1"):
            with self.subTest(version=version):
                result = run_cli(CHANGELOG, version)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertIn(f"## [{version.removeprefix('v')}]", result.stderr)


if __name__ == "__main__":
    unittest.main()
