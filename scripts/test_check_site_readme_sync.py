#!/usr/bin/env python3
"""Self-test for scripts/check_site_readme_sync.py (issue #160).

Drives the drift guard through its explicit changed-files interface (positional
paths), so no git history or fixture tree is required. Asserts the warn-only
default never fails the build, that ``--strict`` turns drift into a non-zero
exit, and that docs-only / internal-only / docs-accompanied changes never flag.
Stdlib only (unittest + subprocess); run directly:

    python3 scripts/test_check_site_readme_sync.py
"""
from __future__ import annotations

import pathlib
import subprocess
import sys
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / "check_site_readme_sync.py"

# Representative paths.
CLI = "crates/ironmem/src/main.rs"
TUNABLES = "crates/ironmem/src/search/tunables.rs"
MCP_TOOL = "crates/ironmem/src/mcp/tools/drawers.rs"
INTERNAL = "crates/ironmem/src/db/metrics.rs"
README = "README.md"
SITE = "site/index.html"
SITE_OTHER = "site/styles.css"


def run_guard(*files: str, strict: bool = False) -> subprocess.CompletedProcess:
    args = [sys.executable, str(SCRIPT)]
    if strict:
        args.append("--strict")
    args.extend(files)
    return subprocess.run(args, capture_output=True, text=True)


class GuardSelfTest(unittest.TestCase):
    def assert_drift(self, r: subprocess.CompletedProcess, *, strict: bool) -> None:
        self.assertIn("DRIFT", r.stdout + r.stderr)
        if strict:
            self.assertNotEqual(r.returncode, 0, f"expected strict failure:\n{r.stdout}\n{r.stderr}")
        else:
            self.assertEqual(r.returncode, 0, f"warn-only must exit 0:\n{r.stdout}\n{r.stderr}")

    def assert_clean(self, r: subprocess.CompletedProcess) -> None:
        self.assertEqual(r.returncode, 0, f"expected OK:\n{r.stdout}\n{r.stderr}")
        self.assertNotIn("DRIFT", r.stdout + r.stderr)

    # --- drift: surface changed, docs untouched -------------------------
    def test_cli_change_without_docs_flags(self):
        self.assert_drift(run_guard(CLI), strict=False)

    def test_tunables_change_without_docs_flags(self):
        self.assert_drift(run_guard(TUNABLES), strict=False)

    def test_mcp_tool_change_without_docs_flags(self):
        self.assert_drift(run_guard(MCP_TOOL), strict=False)

    # --- warn-only vs strict exit codes ---------------------------------
    def test_warn_only_default_exits_zero_on_drift(self):
        r = run_guard(CLI)
        self.assertEqual(r.returncode, 0)

    def test_strict_exits_nonzero_on_drift(self):
        self.assert_drift(run_guard(CLI, strict=True), strict=True)

    # --- no false positives ---------------------------------------------
    def test_surface_with_readme_is_clean(self):
        self.assert_clean(run_guard(CLI, README))

    def test_surface_with_site_index_is_clean(self):
        self.assert_clean(run_guard(TUNABLES, SITE))

    def test_docs_only_change_is_clean(self):
        self.assert_clean(run_guard(README, SITE))

    def test_internal_only_change_is_clean(self):
        self.assert_clean(run_guard(INTERNAL))

    def test_empty_changeset_is_clean(self):
        self.assert_clean(run_guard())

    def test_other_site_asset_counts_as_docs(self):
        # Touching any file under site/ satisfies the "front door updated" intent.
        self.assert_clean(run_guard(CLI, SITE_OTHER))

    def test_strict_clean_changeset_exits_zero(self):
        r = run_guard(INTERNAL, strict=True)
        self.assertEqual(r.returncode, 0, f"clean strict run must pass:\n{r.stderr}")


if __name__ == "__main__":
    unittest.main()
