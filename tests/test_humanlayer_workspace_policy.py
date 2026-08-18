"""Pytest discovery bridge for the repository HumanLayer workspace policy regression check."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
POLICY_SCRIPT = ROOT / "scripts" / "test_humanlayer_workspace_policy.py"


def _run_policy(*arguments: str) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [sys.executable, str(POLICY_SCRIPT), *arguments],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    return result


def _assert_policy_passes(result: subprocess.CompletedProcess[str]) -> None:
    assert result.returncode == 0, (
        "the CI HumanLayer workspace policy check failed "
        f"(exit {result.returncode})\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )


def _assert_policy_rejects(
    result: subprocess.CompletedProcess[str], expected_message: str
) -> None:
    assert result.returncode != 0, (
        "the policy helper accepted an invalid --repo-root value\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
    assert expected_message in result.stderr, (
        f"policy helper error did not mention {expected_message!r}\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )


def test_humanlayer_workspace_policy() -> None:
    _assert_policy_passes(_run_policy())


def test_humanlayer_workspace_policy_explicit_root() -> None:
    _assert_policy_passes(_run_policy("--repo-root", str(ROOT)))


def test_humanlayer_workspace_policy_rejects_relative_root() -> None:
    _assert_policy_rejects(
        _run_policy("--repo-root", "."),
        "must be an absolute path",
    )


def test_humanlayer_workspace_policy_rejects_nonexistent_root() -> None:
    _assert_policy_rejects(
        _run_policy("--repo-root", str(ROOT / ".humanlayer" / "does-not-exist")),
        "does not exist",
    )


def test_humanlayer_workspace_policy_rejects_subdirectory_root() -> None:
    _assert_policy_rejects(
        _run_policy("--repo-root", str(ROOT / "scripts")),
        "must be the Git worktree root",
    )
