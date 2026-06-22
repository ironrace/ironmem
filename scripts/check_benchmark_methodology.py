#!/usr/bin/env python3
"""Lint for the public benchmark methodology doc (issue #146).

Asserts that ``docs/BENCHMARKS.md`` exists, covers the required methodology
sections (as real headings, not incidental prose), distinguishes measured rows
from estimates, states a current-baseline status, ships runnable reproduction
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
from typing import NoReturn

ROOT = pathlib.Path(
    os.environ.get(
        "BENCHMARK_DOC_LINT_ROOT",
        pathlib.Path(__file__).resolve().parents[1],
    )
).resolve()

DOC = ROOT / "docs" / "BENCHMARKS.md"
README = ROOT / "README.md"
SITE = ROOT / "site" / "index.html"

# Required sections — each must appear as an actual Markdown heading, so a
# passing mention in prose or a cross-reference line cannot satisfy the gate.
# The measured-vs-estimated distinction and the baseline-status criterion are
# enforced here too, as their own required headings.
REQUIRED_SECTION_HEADINGS = [
    ("corpus selection", r"corpus\s+selection"),
    ("harness setup", r"harness\s+setup"),
    ("token accounting", r"token\s+accounting"),
    ("quality gates", r"quality\s+gates"),
    ("sample-size requirements", r"sample[- ]size\s+requirements"),
    ("measured vs estimated", r"measured\s+vs\.?\s+estimated"),
    ("current baseline status", r"current\s+baseline\s+status"),
]

# Reproduction: a fenced code block carrying a real runner command.
REPRO_COMMAND = re.compile(r"\b(cargo run|abeval|ironmem report)\b", re.IGNORECASE)

# Unsupported headline savings claim guard. A quantified magnitude (NN%,
# NN percent, or NNx) co-occurring on one line with a savings verb/noun is a
# violation — UNLESS the same line carries an explicit disclaimer. The verb set
# is deliberately broad (the doc's own thesis uses "lower"/"fewer"), and both
# orderings (number-then-word and word-then-number) are caught.
_MAGNITUDE = r"\d+(?:\.\d+)?\s*(?:%|percent|x\b)"
_SAVINGS_WORD = (
    r"(?:fewer|faster|savings?|saves?|saved|saving|reduction|reduces?|reduced|"
    r"lowers?|lowered|lowering|cuts?|halves?|halve|cheaper|less)"
)
SAVINGS_CLAIM = re.compile(
    rf"{_MAGNITUDE}[^.\n]{{0,60}}\b{_SAVINGS_WORD}\b"
    rf"|\b{_SAVINGS_WORD}\b[^.\n]{{0,60}}{_MAGNITUDE}",
    re.IGNORECASE,
)
# Explicit disclaimers only — not loose words like "target" that occur in
# unrelated prose ("on target hardware").
QUALIFIER = re.compile(
    r"not\s+yet\s+measured|not\s+measured|no\s+headline|"
    r"not\s+a\s+(?:measured|savings)\s+claim|hypothetical|illustrative|"
    r"for\s+illustration|example\s+only|placeholder|do\s+not\s+(?:read|treat)",
    re.IGNORECASE,
)


def _disp(path: pathlib.Path) -> str:
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def fail(msg: str) -> NoReturn:
    print(f"check_benchmark_methodology: FAIL — {msg}", file=sys.stderr)
    sys.exit(1)


def read(path: pathlib.Path, what: str) -> str:
    if not path.is_file():
        fail(f"{what} not found: {_disp(path)}")
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        fail(f"could not read {what} ({_disp(path)}): {exc}")


def heading_present(text: str, pattern: str) -> bool:
    return re.search(rf"^#{{1,6}}\s+.*{pattern}", text, re.IGNORECASE | re.MULTILINE) is not None


def main() -> None:
    text = read(DOC, "methodology doc")

    for label, pattern in REQUIRED_SECTION_HEADINGS:
        if not heading_present(text, pattern):
            fail(f"methodology doc missing required section heading: {label!r}")

    fenced = re.findall(r"```.*?```", text, re.DOTALL)
    if not any(REPRO_COMMAND.search(block) for block in fenced):
        fail("methodology doc must include a fenced reproduction-command block")

    for ln_no, line in enumerate(text.splitlines(), start=1):
        if SAVINGS_CLAIM.search(line) and not QUALIFIER.search(line):
            fail(
                "unsupported headline savings claim at "
                f"docs/BENCHMARKS.md:{ln_no}: {line.strip()!r}"
            )

    if "docs/BENCHMARKS.md" not in read(README, "README.md"):
        fail("README.md does not link to docs/BENCHMARKS.md")

    if "docs/BENCHMARKS.md" not in read(SITE, "site/index.html"):
        fail("site/index.html does not link to docs/BENCHMARKS.md")

    print("check_benchmark_methodology: OK")


if __name__ == "__main__":
    main()
