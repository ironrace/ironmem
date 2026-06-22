#!/usr/bin/env python3
"""Lint for the public benchmark methodology doc (issue #146).

Asserts that ``docs/BENCHMARKS.md`` exists, covers the required methodology
sections, distinguishes measured rows from estimates, ships reproduction
commands, makes no unsupported headline savings claim, and is linked from both
the README and the marketing site.

Exit 0 iff all checks pass; non-zero with a printed reason otherwise.
Stdlib only (mirrors scripts/check_collab_turn_templates.py).
"""
from __future__ import annotations

import os
import pathlib
import re
import sys

ROOT = pathlib.Path(
    os.environ.get(
        "BENCHMARK_DOC_LINT_ROOT",
        pathlib.Path(__file__).resolve().parents[1],
    )
).resolve()

DOC = ROOT / "docs" / "BENCHMARKS.md"
README = ROOT / "README.md"
SITE = ROOT / "site" / "index.html"

# (label, regex) — each must appear at least once in the methodology doc.
REQUIRED_SECTIONS = [
    ("corpus selection", r"corpus\s+selection"),
    ("harness setup", r"harness\s+setup"),
    ("token accounting", r"token\s+accounting"),
    ("quality gates", r"quality\s+gates"),
    ("sample-size requirements", r"sample[- ]size\s+requirements"),
]

# The measured-vs-estimated distinction must be explicit.
MEASURED_VS_ESTIMATED = r"measured[ -].{0,40}estimat"

# A reproduction surface must be present (a fenced command block reference).
REPRODUCTION = r"reproduc"

# Unsupported headline savings claim guard. A bare percentage tied to a
# savings/fewer/faster word is only allowed when the same line is explicitly
# qualified as not-yet-measured / illustrative / a target. Any unqualified
# "NN% fewer/faster/savings/reduction" headline is a violation.
SAVINGS_CLAIM = re.compile(
    r"\b\d+(?:\.\d+)?\s*%[^.\n]*\b(fewer|faster|savings?|saved|reduction|reduced|less)\b",
    re.IGNORECASE,
)
QUALIFIER = re.compile(
    r"not\s+yet\s+measured|no\s+headline|illustrative|hypothes|target|example only|"
    r"placeholder|not\s+a\s+(?:measured|savings)\s+claim|do\s+not\s+(?:read|treat)",
    re.IGNORECASE,
)


def fail(msg: str) -> None:
    print(f"check_benchmark_methodology: FAIL — {msg}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    if not DOC.is_file():
        fail(f"missing methodology doc: {DOC.relative_to(ROOT)}")

    text = DOC.read_text(encoding="utf-8")
    low = text.lower()

    for label, pattern in REQUIRED_SECTIONS:
        if not re.search(pattern, low):
            fail(f"methodology doc missing required section: {label!r}")

    if not re.search(MEASURED_VS_ESTIMATED, low):
        fail("methodology doc must distinguish measured rows from estimates")

    if not re.search(REPRODUCTION, low):
        fail("methodology doc must include reproduction commands / locations")

    for ln_no, line in enumerate(text.splitlines(), start=1):
        if SAVINGS_CLAIM.search(line) and not QUALIFIER.search(line):
            fail(
                "unsupported headline savings claim at "
                f"docs/BENCHMARKS.md:{ln_no}: {line.strip()!r}"
            )

    if not README.is_file():
        fail("README.md not found")
    if "docs/BENCHMARKS.md" not in README.read_text(encoding="utf-8"):
        fail("README.md does not link to docs/BENCHMARKS.md")

    if not SITE.is_file():
        fail("site/index.html not found")
    if "docs/BENCHMARKS.md" not in SITE.read_text(encoding="utf-8"):
        fail("site/index.html does not link to docs/BENCHMARKS.md")

    print("check_benchmark_methodology: OK")


if __name__ == "__main__":
    main()
