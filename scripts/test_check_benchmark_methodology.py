#!/usr/bin/env python3
"""Self-test for scripts/check_benchmark_methodology.py.

Exercises the guard end-to-end through the BENCHMARK_DOC_LINT_ROOT env seam:
builds a minimal valid fixture tree in a temp dir, asserts the guard passes,
then injects one defect per case and asserts it fails loudly with the right
reason. Stdlib only (unittest + subprocess + tempfile); run directly:

    python3 scripts/test_check_benchmark_methodology.py
"""
from __future__ import annotations

import pathlib
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parent / "check_benchmark_methodology.py"

VALID_DOC = textwrap.dedent(
    """\
    # Benchmark Methodology

    Summary linking the spec ([spec](METRICS_SPEC.md)) and a jump to
    [current baseline status](#current-baseline-status).

    ## Corpus selection
    body
    ## Harness setup
    body
    ## Token accounting
    body
    ### Measured vs estimated
    body
    ## Quality gates
    body
    ## Sample-size requirements
    body
    ## Current baseline status
    No headline savings number yet.
    ## Reproduce it yourself
    ```bash
    cargo run --manifest-path benchmarks/abeval/Cargo.toml -- validate
    ```
    """
)

VALID_README = "Docs:\n- [Benchmark Methodology](docs/BENCHMARKS.md)\n"
VALID_SITE = '<a href="https://x.example/docs/BENCHMARKS.md">Benchmarks</a>\n'


def write_fixture(root: pathlib.Path, *, doc=VALID_DOC, readme=VALID_README,
                  site=VALID_SITE, with_doc=True, with_spec=True) -> None:
    (root / "docs").mkdir(parents=True, exist_ok=True)
    (root / "site").mkdir(parents=True, exist_ok=True)
    if with_doc:
        (root / "docs" / "BENCHMARKS.md").write_text(doc, encoding="utf-8")
    if with_spec:
        (root / "docs" / "METRICS_SPEC.md").write_text("# spec\n", encoding="utf-8")
    (root / "README.md").write_text(readme, encoding="utf-8")
    (root / "site" / "index.html").write_text(site, encoding="utf-8")


def run_guard(root: pathlib.Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT)],
        env={"BENCHMARK_DOC_LINT_ROOT": str(root), "PATH": "/usr/bin:/bin"},
        capture_output=True,
        text=True,
    )


class GuardSelfTest(unittest.TestCase):
    def _root(self) -> pathlib.Path:
        d = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(d, ignore_errors=True))
        return pathlib.Path(d)

    def assert_fail(self, root: pathlib.Path, needle: str) -> None:
        r = run_guard(root)
        self.assertNotEqual(r.returncode, 0, f"expected failure, got OK:\n{r.stdout}")
        self.assertIn(needle, r.stderr, f"stderr was:\n{r.stderr}")

    # --- happy path + env seam ------------------------------------------
    def test_valid_fixture_passes_via_env_seam(self):
        root = self._root()
        write_fixture(root)
        r = run_guard(root)
        self.assertEqual(r.returncode, 0, f"expected OK, stderr:\n{r.stderr}")
        self.assertIn("OK", r.stdout)

    def test_missing_doc_fails(self):
        root = self._root()
        write_fixture(root, with_doc=False)
        self.assert_fail(root, "methodology doc not found")

    # --- structural checks ----------------------------------------------
    def test_missing_section_heading_fails(self):
        root = self._root()
        write_fixture(root, doc=VALID_DOC.replace("## Quality gates\n", ""))
        self.assert_fail(root, "quality gates")

    def test_missing_reproduction_block_fails(self):
        root = self._root()
        doc = VALID_DOC.replace(
            "```bash\ncargo run --manifest-path benchmarks/abeval/Cargo.toml -- validate\n```",
            "run it yourself somehow",
        )
        write_fixture(root, doc=doc)
        self.assert_fail(root, "fenced reproduction-command block")

    # --- savings-claim guard --------------------------------------------
    def test_unqualified_savings_claim_fails(self):
        root = self._root()
        write_fixture(root, doc=VALID_DOC + "\nironmem uses 40% fewer tokens.\n")
        self.assert_fail(root, "unsupported headline savings claim")

    def test_qualified_savings_claim_passes(self):
        root = self._root()
        write_fixture(
            root, doc=VALID_DOC + "\nA hypothetical 40% fewer tokens (illustrative).\n"
        )
        r = run_guard(root)
        self.assertEqual(r.returncode, 0, f"expected OK, stderr:\n{r.stderr}")

    # --- link / anchor resolution (NEW behavior) ------------------------
    def test_broken_file_link_fails(self):
        root = self._root()
        write_fixture(root, doc=VALID_DOC + "\nSee [gone](does-not-exist.md).\n")
        self.assert_fail(root, "link")

    def test_broken_anchor_fails(self):
        root = self._root()
        write_fixture(root, doc=VALID_DOC + "\nJump to [nowhere](#no-such-heading).\n")
        self.assert_fail(root, "anchor")

    # --- README / site real-link checks ---------------------------------
    def test_readme_without_link_fails(self):
        root = self._root()
        write_fixture(root, readme="Docs: nothing relevant here.\n")
        self.assert_fail(root, "README.md does not link")

    def test_site_without_link_fails(self):
        root = self._root()
        write_fixture(root, site='<a href="https://x.example/other">x</a>\n')
        self.assert_fail(root, "site/index.html does not link")


if __name__ == "__main__":
    unittest.main()
