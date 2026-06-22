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

import os
import pathlib
import subprocess
import sys
import tempfile
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
        # The annotation severity is the contract with the GitHub Actions UI.
        if strict:
            self.assertIn("::error", r.stdout)
            self.assertNotEqual(r.returncode, 0, f"expected strict failure:\n{r.stdout}\n{r.stderr}")
        else:
            self.assertIn("::warning", r.stdout)
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

    # --- path normalization ---------------------------------------------
    def test_leading_dotslash_surface_still_flags(self):
        self.assert_drift(run_guard("./" + CLI), strict=False)

    def test_leading_dotslash_doc_counts_as_docs(self):
        self.assert_clean(run_guard(CLI, "./" + README))

    def test_backslash_surface_path_flags(self):
        self.assert_drift(run_guard(CLI.replace("/", "\\")), strict=False)

    # --- multi-surface aggregation --------------------------------------
    def test_multiple_surfaces_all_listed_sorted(self):
        r = run_guard(TUNABLES, CLI)  # passed out of order
        self.assertEqual(r.returncode, 0)
        msg = r.stdout + r.stderr
        self.assertIn(CLI, msg)
        self.assertIn(TUNABLES, msg)
        # sorted: main.rs ("crates/ironmem/src/m...") before tunables ("...search/t...")
        self.assertLess(msg.index(CLI), msg.index(TUNABLES))

    # --- argument validation --------------------------------------------
    def test_base_and_files_are_mutually_exclusive(self):
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", "HEAD", CLI],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("not both", r.stderr)

    # --- --base git mode ------------------------------------------------
    def test_base_missing_ref_exits_2(self):
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", "no-such-ref-xyz"],
            capture_output=True,
            text=True,
            cwd=str(pathlib.Path(__file__).resolve().parents[1]),
        )
        self.assertEqual(r.returncode, 2, f"expected exit 2:\n{r.stdout}\n{r.stderr}")
        self.assertIn("does not resolve", r.stderr)

    def test_base_mode_detects_surface_drift_in_temp_repo(self):
        repo = self._git_repo()
        # Base commit: an internal-only file.
        self._write(repo, INTERNAL, "// base\n")
        self._git(repo, "add", "-A")
        self._git(repo, "commit", "-m", "base")
        base = self._git(repo, "rev-parse", "HEAD").strip()
        # HEAD commit: change the CLI surface, no docs.
        self._write(repo, CLI, "// changed\n")
        self._git(repo, "add", "-A")
        self._git(repo, "commit", "-m", "surface change")
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", base],
            capture_output=True,
            text=True,
            cwd=str(repo),
        )
        self.assertEqual(r.returncode, 0, f"warn-only:\n{r.stdout}\n{r.stderr}")
        self.assertIn("DRIFT", r.stdout + r.stderr)
        self.assertIn(CLI, r.stdout + r.stderr)

    # --- temp-git helpers ----------------------------------------------
    def _git_repo(self) -> pathlib.Path:
        d = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(d, ignore_errors=True))
        repo = pathlib.Path(d)
        self._git(repo, "init", "-q")
        self._git(repo, "config", "user.email", "t@example.com")
        self._git(repo, "config", "user.name", "test")
        return repo

    def _git(self, repo: pathlib.Path, *args: str) -> str:
        return subprocess.run(
            ["git", *args],
            cwd=str(repo),
            capture_output=True,
            text=True,
            check=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull},
        ).stdout

    def _write(self, repo: pathlib.Path, rel: str, content: str) -> None:
        p = repo / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")


if __name__ == "__main__":
    unittest.main()
