#!/usr/bin/env python3
"""Diff-aware local Git hook runner.

The tracked hooks delegate here so local commits and pushes only run gates that
match the changed surface:

- collab protocol/template changes -> collab template lint
- Rust/workspace changes -> Rust gates
- hook runner changes -> hook self-tests and install drift check
"""
from __future__ import annotations

import dataclasses
import pathlib
import subprocess
import sys
from types import MappingProxyType
from typing import Callable, Iterable

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


# --- Frozen data model -------------------------------------------------
#
# `Gate`/`ChangeSet`/`GATES`/`SURFACES` are the pure data layer the rest of
# the collect -> resolve -> execute pipeline (later tasks) will read. Nothing
# below is wired into run_pre_commit()/run_pre_push() yet.

PHASE_PRE_COMMIT = "pre-commit"
PHASE_PRE_PUSH = "pre-push"

SURFACE_RUST_WORKSPACE = "rust_workspace"
SURFACE_COLLAB_PROTOCOL = "collab_protocol"
SURFACE_HOOK_SELF_TEST = "hook_self_test"


@dataclasses.dataclass(frozen=True)
class Gate:
    """One subprocess invocation, gated by phase and changed surface."""

    name: str
    argv: tuple[str, ...]
    phases: frozenset[str]
    surfaces: frozenset[str]
    always: bool

    def __post_init__(self) -> None:
        if not isinstance(self.name, str):
            raise TypeError(f"Gate.name must be a str, got {type(self.name).__name__}")
        if not isinstance(self.argv, tuple):
            raise TypeError(f"Gate.argv must be a tuple, got {type(self.argv).__name__}")
        if not isinstance(self.phases, frozenset):
            raise TypeError(f"Gate.phases must be a frozenset, got {type(self.phases).__name__}")
        if not isinstance(self.surfaces, frozenset):
            raise TypeError(
                f"Gate.surfaces must be a frozenset, got {type(self.surfaces).__name__}"
            )
        if not isinstance(self.always, bool):
            raise TypeError(f"Gate.always must be a bool, got {type(self.always).__name__}")


@dataclasses.dataclass(frozen=True)
class ChangeSet:
    """The changed paths for a phase, plus escalation state.

    `unknown=True` (with `reason` set) marks an unsafe or unrecognized path
    shape that must escalate to running every gate, never be sanitized away.
    """

    paths: tuple[str, ...]
    unknown: bool
    reason: str | None

    def __post_init__(self) -> None:
        if not isinstance(self.paths, tuple):
            raise TypeError(f"ChangeSet.paths must be a tuple, got {type(self.paths).__name__}")
        if not isinstance(self.unknown, bool):
            raise TypeError(
                f"ChangeSet.unknown must be a bool, got {type(self.unknown).__name__}"
            )
        if self.reason is not None and not isinstance(self.reason, str):
            raise TypeError(
                f"ChangeSet.reason must be a str or None, got {type(self.reason).__name__}"
            )


# surface_id -> predicate. Predicates ported unchanged from the existing
# is_rust_path/is_collab_protocol_path/is_hook_path classifiers above.
SURFACES: MappingProxyType[str, Callable[[str], bool]] = MappingProxyType(
    {
        SURFACE_RUST_WORKSPACE: is_rust_path,
        SURFACE_COLLAB_PROTOCOL: is_collab_protocol_path,
        SURFACE_HOOK_SELF_TEST: is_hook_path,
    }
)

# Declaration order IS execution order. Never sorted at runtime. Ported
# unchanged from today's run_pre_commit()/run_pre_push() conditional
# assembly (same argv, same phase membership).
GATES: tuple[Gate, ...] = (
    Gate(
        name="hook_self_test",
        argv=("python3", "scripts/test_run_git_hook.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_HOOK_SELF_TEST}),
        always=False,
    ),
    Gate(
        name="hook_install_check",
        argv=("bash", "scripts/install-git-hooks.sh", "--check"),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_HOOK_SELF_TEST}),
        always=False,
    ),
    Gate(
        name="collab_template_lint",
        argv=("python3", "scripts/check_collab_turn_templates.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_COLLAB_PROTOCOL}),
        always=False,
    ),
    Gate(
        name="rust_fmt_check",
        argv=("cargo", "fmt", "--all", "--", "--check"),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
    Gate(
        name="rust_clippy",
        argv=(
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
    Gate(
        name="rust_test",
        argv=("cargo", "test", "--workspace"),
        phases=frozenset({PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
)


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
