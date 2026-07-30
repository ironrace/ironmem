#!/usr/bin/env python3
"""Drift guard: committed per-harness skills must match the canonical skills/.

Regenerates in memory and diffs against the committed trees. Hard gate, not a
warning: this is the guardrail whose absence let the Codex and Claude copies
of subagent-driven-development diverge by 127 lines without anyone noticing.

Exit codes: 0 clean, 1 drift, 2 unrenderable source.
"""
from __future__ import annotations

import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import sync_skills


def main() -> int:
    try:
        rendered = sync_skills.plan()
    except sync_skills.SkillSyncError as exc:
        detail = f"skills/ does not render: {exc}"
        print(f"::error title=skills sync::{detail}")
        print(f"check_skills_sync: {detail}", file=sys.stderr)
        return 2

    drifted = sync_skills.diff(rendered, sync_skills.TARGETS)
    if not drifted:
        print("check_skills_sync: OK — generated skills match skills/")
        return 0

    detail = (
        f"DRIFT: {len(drifted)} generated skill file(s) do not match skills/ "
        f"({', '.join(drifted[:5])}{', …' if len(drifted) > 5 else ''}). "
        "Run: python3 scripts/sync_skills.py"
    )
    print(f"::error title=skills sync::{detail}")
    for entry in drifted:
        print(f"  {entry}", file=sys.stderr)
    print("Run: python3 scripts/sync_skills.py", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
