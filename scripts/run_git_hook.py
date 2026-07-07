#!/usr/bin/env python3
"""Diff-aware local Git hook runner.

The tracked hooks delegate here so local commits and pushes only run gates that
match the changed surface:

- collab protocol/template changes -> collab template lint
- Rust/workspace changes -> Rust gates
- hook runner changes -> hook self-tests and install drift check
"""
from __future__ import annotations

import pathlib
import subprocess
import sys
from typing import Iterable

ROOT = pathlib.Path(__file__).resolve().parents[1]
ZERO_SHA = "0" * 40

COLLAB_EXACT_PATHS = {
    ".claude-plugin/commands/collab.md",
    ".codex-plugin/commands/collab.md",
    ".codex-plugin/prompts/collab.md",
    ".codex-plugin/prompts/collab-batch-impl.md",
    "docs/COLLAB.md",
    "scripts/check_collab_turn_templates.py",
}

HOOK_EXACT_PATHS = {
    ".githooks/pre-commit",
    ".githooks/pre-push",
    "scripts/install-git-hooks.sh",
    "scripts/run_git_hook.py",
    "scripts/test_run_git_hook.py",
}


def run(cmd: list[str]) -> int:
    print(f"[git-hook] {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, cwd=ROOT).returncode


def git(args: list[str], *, input_text: str | None = None, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        input=input_text,
        text=True,
        capture_output=True,
        check=False,
    )
    if check and result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    return result.stdout


def unique(paths: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    ordered: list[str] = []
    for path in paths:
        clean = path.strip()
        if clean and clean not in seen:
            seen.add(clean)
            ordered.append(clean)
    return ordered


def staged_paths() -> list[str]:
    output = git(["diff", "--cached", "--name-only", "--diff-filter=ACMRTUXB"])
    return unique(output.splitlines())


def default_base(local_sha: str) -> str | None:
    candidates: list[str] = []
    origin_head = git(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"], check=False).strip()
    if origin_head:
        candidates.append(origin_head)
    candidates.extend(["origin/main", "origin/master", "main", "master"])

    for candidate in candidates:
        base = git(["merge-base", local_sha, candidate], check=False).strip()
        if base:
            return base
    return None


def pushed_paths(stdin_text: str) -> list[str]:
    paths: list[str] = []
    for line in stdin_text.splitlines():
        parts = line.split()
        if len(parts) != 4:
            continue
        _local_ref, local_sha, _remote_ref, remote_sha = parts
        if local_sha == ZERO_SHA:
            continue
        if remote_sha != ZERO_SHA:
            output = git(["diff", "--name-only", f"{remote_sha}..{local_sha}"])
        else:
            base = default_base(local_sha)
            if base:
                output = git(["diff", "--name-only", f"{base}..{local_sha}"])
            else:
                output = git(["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", local_sha])
        paths.extend(output.splitlines())

    if paths:
        return unique(paths)

    # Defensive fallback for direct/manual invocation outside Git's pre-push
    # stdin contract.
    upstream = git(["rev-parse", "--verify", "@{u}"], check=False).strip()
    if upstream:
        output = git(["diff", "--name-only", f"{upstream}..HEAD"])
        return unique(output.splitlines())
    return []


def is_rust_path(path: str) -> bool:
    name = pathlib.PurePosixPath(path).name
    return (
        path.endswith(".rs")
        or name in {"Cargo.toml", "Cargo.lock", "build.rs"}
        or path.startswith(".cargo/")
    )


def is_collab_protocol_path(path: str) -> bool:
    return (
        path in COLLAB_EXACT_PATHS
        or path.startswith(".claude-plugin/prompts/collab-turn-")
        or path.startswith("tests/collab_turn_templates/")
    )


def is_hook_path(path: str) -> bool:
    return path in HOOK_EXACT_PATHS


def gate_summary(paths: list[str]) -> tuple[bool, bool, bool]:
    return (
        any(is_collab_protocol_path(path) for path in paths),
        any(is_rust_path(path) for path in paths),
        any(is_hook_path(path) for path in paths),
    )


def run_pre_commit() -> int:
    paths = staged_paths()
    if not paths:
        print("[pre-commit] no staged files; skipping gates")
        return 0

    collab_changed, rust_changed, hooks_changed = gate_summary(paths)
    print(f"[pre-commit] staged files: {len(paths)}")

    commands: list[list[str]] = []
    if hooks_changed:
        commands.extend(
            [
                ["python3", "scripts/test_run_git_hook.py"],
                ["bash", "scripts/install-git-hooks.sh", "--check"],
            ]
        )
    if collab_changed:
        commands.append(["python3", "scripts/check_collab_turn_templates.py"])
    if rust_changed:
        commands.extend(
            [
                ["cargo", "fmt", "--all", "--", "--check"],
                [
                    "cargo",
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-D",
                    "warnings",
                ],
            ]
        )

    if not commands:
        print("[pre-commit] docs/config-only change; no local gates required")
        return 0

    for cmd in commands:
        rc = run(cmd)
        if rc != 0:
            return rc
    return 0


def run_pre_push() -> int:
    paths = pushed_paths(sys.stdin.read())
    if not paths:
        print("[pre-push] no pushed file changes detected; skipping gates")
        return 0

    collab_changed, rust_changed, hooks_changed = gate_summary(paths)
    print(f"[pre-push] pushed files: {len(paths)}")

    commands: list[list[str]] = []
    if hooks_changed:
        commands.append(["python3", "scripts/test_run_git_hook.py"])
    if collab_changed:
        commands.append(["python3", "scripts/check_collab_turn_templates.py"])
    if rust_changed:
        commands.append(["cargo", "test", "--workspace"])

    if not commands:
        print("[pre-push] docs/config-only change; no local gates required")
        return 0

    for cmd in commands:
        rc = run(cmd)
        if rc != 0:
            return rc
    return 0


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in {"pre-commit", "pre-push"}:
        print("usage: scripts/run_git_hook.py <pre-commit|pre-push>", file=sys.stderr)
        return 2
    if argv[1] == "pre-commit":
        return run_pre_commit()
    return run_pre_push()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
