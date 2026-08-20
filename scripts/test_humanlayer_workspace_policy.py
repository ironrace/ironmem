#!/usr/bin/env python3
"""Regression check for the fail-closed HumanLayer workspace policy."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GIT_EXECUTABLE = Path("/usr/bin/git")
LOCAL_CONFIG_PATTERN = ".humanlayer/workspace.local.json"


def _validate_git_executable() -> None:
    if not GIT_EXECUTABLE.is_file() or not os.access(GIT_EXECUTABLE, os.X_OK):
        raise ValueError(
            f"approved Git executable {GIT_EXECUTABLE} is unavailable or not executable"
        )


def _resolve_repo_root(requested_root: str | None) -> Path:
    requested_path = ROOT if requested_root is None else Path(requested_root)
    if not requested_path.is_absolute():
        raise ValueError("--repo-root must be an absolute path")

    try:
        repo_root = requested_path.resolve(strict=True)
    except FileNotFoundError as error:
        raise ValueError(f"--repo-root does not exist: {requested_path}") from error
    except OSError as error:
        raise ValueError(
            f"--repo-root could not be resolved: {requested_path}: {error}"
        ) from error
    if not repo_root.is_dir():
        raise ValueError(f"--repo-root is not a directory: {repo_root}")

    _validate_git_executable()
    result = subprocess.run(
        [
            str(GIT_EXECUTABLE),
            "-C",
            str(repo_root),
            "rev-parse",
            "--show-toplevel",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ValueError(
            "--repo-root is not a Git worktree root; "
            f"git rev-parse output: stdout={result.stdout!r}, "
            f"stderr={result.stderr!r}"
        )

    top_level_lines = [line for line in result.stdout.splitlines() if line.strip()]
    if len(top_level_lines) != 1:
        raise ValueError(
            "--repo-root Git top-level resolution returned unexpected output: "
            f"stdout={result.stdout!r}"
        )
    try:
        resolved_top_level = Path(top_level_lines[0]).resolve(strict=True)
    except OSError as error:
        raise ValueError(
            "--repo-root Git top-level path could not be resolved: "
            f"{top_level_lines[0]!r}: {error}"
        ) from error
    if resolved_top_level != repo_root:
        raise ValueError(
            "--repo-root must be the Git worktree root; "
            f"requested {repo_root}, Git reported {resolved_top_level}"
        )
    return repo_root


class HumanLayerWorkspacePolicyTest(unittest.TestCase):
    repo_root = ROOT

    def test_shared_workspace_setup_is_disabled(self) -> None:
        config = json.loads(
            (self.repo_root / ".humanlayer" / "workspace.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(
            config,
            {"disabled": True},
            "HumanLayer workspace setup must stay fully disabled because default copyGlobs include .env files and cannot be subtracted",
        )

    def test_local_override_is_absent_and_ignored(self) -> None:
        local_config = self.repo_root / ".humanlayer" / "workspace.local.json"
        self.assertFalse(
            local_config.exists(),
            ".humanlayer/workspace.local.json can re-enable workspace setup and is prohibited in the pilot checkout",
        )
        result = subprocess.run(
            [str(GIT_EXECUTABLE), "check-ignore", "--", LOCAL_CONFIG_PATTERN],
            cwd=self.repo_root,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            result.returncode,
            0,
            "the local HumanLayer override must be ignored so the fail-closed policy "
            "cannot be silently re-enabled; "
            f"git check-ignore output: stdout={result.stdout!r}, stderr={result.stderr!r}",
        )

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Check the fail-closed HumanLayer workspace policy."
    )
    parser.add_argument(
        "--repo-root",
        metavar="ABSOLUTE_PATH",
        help="validate this absolute Git worktree root instead of the helper's repository",
    )
    cli_args, unittest_args = parser.parse_known_args()
    try:
        selected_root = _resolve_repo_root(cli_args.repo_root)
    except ValueError as error:
        print(f"HumanLayer workspace policy error: {error}", file=sys.stderr)
        raise SystemExit(2)

    HumanLayerWorkspacePolicyTest.repo_root = selected_root
    program = unittest.main(
        module=__name__,
        argv=[sys.argv[0], *unittest_args],
        exit=False,
    )
    raise SystemExit(0 if program.result.wasSuccessful() else 1)
