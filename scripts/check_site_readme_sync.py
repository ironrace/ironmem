#!/usr/bin/env python3
"""Drift guard: keep ironrace.dev + README in sync with user-facing changes (issue #160).

Flags a change set that touches a *user-facing surface* (the CLI surface, MCP
tool registrations, or env-var tunables) without a matching update to the public
site (``site/``) or ``README.md``. The front door should not drift behind the
features it advertises.

Warn-only by default: drift is reported as a GitHub ``::warning::`` annotation and
the process still exits 0, so it never blocks a merge on a false positive. Pass
``--strict`` to turn drift into a non-zero exit (the one-line change that makes
the guard a hard gate).

Change sources (pick one):
  * positional paths           — explicit changed files (used by the self-test)
  * ``--base <ref>``           — ``git diff --name-only <ref>...HEAD`` (used by CI)
  * (no input)                 — empty change set, always clean

Stdlib only (mirrors scripts/check_benchmark_methodology.py).
"""
from __future__ import annotations

import argparse
import subprocess
import sys

# User-facing surfaces. A change to any of these advertises new behavior that
# the public front door should reflect. Kept deliberately narrow and explicit
# (the three surfaces named in issue #160) so internal refactors do not flag.
#   * exact file paths
SURFACE_FILES = (
    "crates/ironmem/src/main.rs",          # CLI subcommands / flags
    "crates/ironmem/src/search/tunables.rs",  # env-var tunables
)
#   * directory prefixes (any file beneath)
SURFACE_PREFIXES = (
    "crates/ironmem/src/mcp/tools/",       # MCP tool registrations / schemas
)

# "Front door" docs. Touching any file under site/ (html, css, copy) or the
# README counts as keeping the public surface current.
DOC_FILES = ("README.md",)
DOC_PREFIXES = ("site/",)


def _norm(path: str) -> str:
    return path.strip().replace("\\", "/").lstrip("./")


def is_surface(path: str) -> bool:
    p = _norm(path)
    return p in SURFACE_FILES or any(p.startswith(pre) for pre in SURFACE_PREFIXES)


def is_doc(path: str) -> bool:
    p = _norm(path)
    return p in DOC_FILES or any(p.startswith(pre) for pre in DOC_PREFIXES)


def changed_from_git(base: str) -> list[str]:
    """Files changed between ``base`` and HEAD (merge-base diff)."""
    try:
        out = subprocess.run(
            ["git", "diff", "--name-only", f"{base}...HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as exc:
        print(f"check_site_readme_sync: could not diff against {base!r}: {exc}", file=sys.stderr)
        sys.exit(2)
    return [line for line in out.splitlines() if line.strip()]


def evaluate(changed: list[str]) -> tuple[bool, list[str]]:
    """Return ``(drift, surface_hits)``.

    Drift is true iff at least one surface file changed and no doc file did.
    """
    surface_hits = sorted({_norm(p) for p in changed if is_surface(p)})
    docs_touched = any(is_doc(p) for p in changed)
    drift = bool(surface_hits) and not docs_touched
    return drift, surface_hits


def main() -> None:
    parser = argparse.ArgumentParser(description="site/README drift guard (issue #160)")
    parser.add_argument("files", nargs="*", help="explicit changed file paths")
    parser.add_argument("--base", help="git ref to diff HEAD against (e.g. origin/main)")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit non-zero on drift (default: warn-only, exit 0)",
    )
    args = parser.parse_args()

    if args.base and args.files:
        parser.error("pass either positional files or --base, not both")

    changed = changed_from_git(args.base) if args.base else list(args.files)
    drift, surface_hits = evaluate(changed)

    if not drift:
        print("check_site_readme_sync: OK — no user-facing drift")
        sys.exit(0)

    hits = ", ".join(surface_hits)
    detail = (
        f"DRIFT: user-facing surface changed ({hits}) but neither site/ nor "
        "README.md was updated. Update the public front door (ironrace.dev / "
        "README) or confirm this surface is not user-facing."
    )
    # GitHub Actions workflow annotation (surfaces in the PR checks UI).
    severity = "error" if args.strict else "warning"
    print(f"::{severity} title=site/README drift::{detail}")
    print(f"check_site_readme_sync: {detail}", file=sys.stderr)
    sys.exit(1 if args.strict else 0)


if __name__ == "__main__":
    main()
