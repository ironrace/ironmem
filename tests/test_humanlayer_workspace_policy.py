"""Pytest discovery bridge for the authoritative HumanLayer policy check."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
POLICY_SCRIPT = ROOT / "scripts" / "test_humanlayer_workspace_policy.py"


def test_humanlayer_workspace_policy() -> None:
    result = subprocess.run(
        [sys.executable, str(POLICY_SCRIPT)],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        "the authoritative HumanLayer workspace policy check failed "
        f"(exit {result.returncode})\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
