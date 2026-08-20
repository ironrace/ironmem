"""Pytest discovery bridge for the repository HumanLayer workspace policy check."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
POLICY_SCRIPT = ROOT / "scripts" / "test_humanlayer_workspace_policy.py"
LOCAL_CONFIG_PATTERN = ".humanlayer/workspace.local.json"


def _run_policy(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(POLICY_SCRIPT), *arguments],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )


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


def _create_policy_repository(
    root: Path,
    *,
    disabled: bool,
    local_override: bool = False,
    ignore_pattern: str = LOCAL_CONFIG_PATTERN,
) -> Path:
    root.mkdir()
    humanlayer = root / ".humanlayer"
    humanlayer.mkdir()
    (root / ".gitignore").write_text(f"{ignore_pattern}\n", encoding="utf-8")
    (humanlayer / "workspace.json").write_text(
        json.dumps({"disabled": disabled}) + "\n", encoding="utf-8"
    )
    if local_override:
        (humanlayer / "workspace.local.json").write_text(
            '{"disabled": false}\n', encoding="utf-8"
        )
    initialized = subprocess.run(
        ["/usr/bin/git", "init", str(root)],
        check=False,
        capture_output=True,
        text=True,
    )
    assert initialized.returncode == 0, initialized.stderr
    tracked = subprocess.run(
        [
            "/usr/bin/git",
            "-C",
            str(root),
            "add",
            ".gitignore",
            ".humanlayer/workspace.json",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert tracked.returncode == 0, tracked.stderr
    return root


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


def test_humanlayer_workspace_policy_validates_distinct_safe_root(tmp_path: Path) -> None:
    target = _create_policy_repository(tmp_path / "safe", disabled=True)
    _assert_policy_passes(_run_policy("--repo-root", str(target)))


def test_humanlayer_workspace_policy_accepts_equivalent_anchored_ignore_pattern(
    tmp_path: Path,
) -> None:
    target = _create_policy_repository(
        tmp_path / "anchored-pattern",
        disabled=True,
        ignore_pattern=f"/{LOCAL_CONFIG_PATTERN}",
    )
    _assert_policy_passes(_run_policy("--repo-root", str(target)))


def test_humanlayer_workspace_policy_rejects_distinct_unsafe_root(tmp_path: Path) -> None:
    target = _create_policy_repository(tmp_path / "unsafe", disabled=False)
    _assert_policy_passes(_run_policy())
    _assert_policy_rejects(
        _run_policy("--repo-root", str(target)),
        "HumanLayer workspace setup must stay fully disabled",
    )


def test_humanlayer_workspace_policy_rejects_distinct_local_override(tmp_path: Path) -> None:
    target = _create_policy_repository(
        tmp_path / "override", disabled=True, local_override=True
    )
    _assert_policy_passes(_run_policy())
    _assert_policy_rejects(
        _run_policy("--repo-root", str(target)),
        "can re-enable workspace setup",
    )
