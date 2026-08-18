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
            ["git", "check-ignore", "-q", ".humanlayer/workspace.local.json"],
            cwd=ROOT,
            check=False,
        )
        self.assertEqual(
            result.returncode,
            0,
            ".humanlayer/workspace.local.json must be ignored so machine-specific secrets and unsafe overrides are not committed",
        )


if __name__ == "__main__":
    unittest.main()
