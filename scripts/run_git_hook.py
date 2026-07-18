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


def is_docs_path(path: str) -> bool:
    """Explicitly inert documentation surface: any Markdown file, or any path
    under a top-level ``docs/`` directory.

    Matched on the leading ``/``-split segment (``path.split("/", 1)[0]``),
    never on ``"docs" in path`` -- a substring check would wrongly match a
    look-alike directory such as ``docsite/notes.txt``.
    """
    return path.endswith(".md") or path.split("/", 1)[0] == "docs"


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
SURFACE_DOCS = "docs"

# Not a declared surface -- the fail-closed fallback classify_path() returns
# when a path is unsafe-shaped or matches no entry in SURFACES (including
# DOCS). Deliberately absent from SURFACES: unlike DOCS, UNKNOWN is not a
# recognized surface later stages select gates for -- it is the signal that
# forces every gate for the phase to run.
UNKNOWN = "unknown"


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
# is_rust_path/is_collab_protocol_path/is_hook_path classifiers above, plus
# the DOCS surface's is_docs_path. Iteration order is declaration order:
# classify_path() below checks DOCS last, so a more specific surface (e.g.
# collab protocol) wins over the generic inert docs catch-all when a path
# happens to satisfy both (docs/COLLAB.md is both under docs/ and in the
# collab-protocol exact set).
SURFACES: MappingProxyType[str, Callable[[str], bool]] = MappingProxyType(
    {
        SURFACE_RUST_WORKSPACE: is_rust_path,
        SURFACE_COLLAB_PROTOCOL: is_collab_protocol_path,
        SURFACE_HOOK_SELF_TEST: is_hook_path,
        SURFACE_DOCS: is_docs_path,
    }
)

# Control bytes (codepoint < 0x20, plus DEL 0x7F) are rejected as unsafe path
# shapes, except newline: Git's `-z`/NUL-delimited output can legitimately
# carry an embedded newline inside a filename, and that byte must classify
# normally, not be treated as an attack shape.
_ALLOWED_CONTROL_CHARS = frozenset({"\n"})


def _unsafe_path_reason(path: object) -> str | None:
    """Return a reason string if `path` is an unsafe or malformed shape for
    classification, else None.

    Never cleans, strips, or rewrites `path` -- callers must escalate on a
    non-None reason, not sanitize and continue. Sanitizing an
    attacker-influenced path would let classification diverge from what Git
    actually staged.
    """
    if not isinstance(path, str):
        return f"non-str path: {type(path).__name__}"
    if path == "":
        return "empty path"
    if path.startswith("/"):
        return "absolute path"
    if path.startswith("-"):
        return "path starts with '-'"
    for char in path:
        if char in _ALLOWED_CONTROL_CHARS:
            continue
        codepoint = ord(char)
        if codepoint < 0x20 or codepoint == 0x7F:
            return "control byte in path"
    if ".." in path.split("/"):
        return "'..' path segment"
    return None


def classify_path(path: object) -> str:
    """Classify a single Git-reported path into a declared surface id, or
    UNKNOWN.

    Total: never raises regardless of input shape or type. Pure: never
    mutates its argument and performs no `.strip()`/unquoting/case-folding --
    matching is byte-exact on `/`-split segments, not substrings, so a
    look-alike like `docsite/` or `contests/` never matches the surface it
    resembles. Unsafe shapes (absolute paths, `..` segments, NUL/control
    bytes, empty string, leading `-`, non-`str`) are rejected to UNKNOWN
    before any surface is checked, never sanitized. DOCS is a declared
    surface like any other, not a fallback; UNKNOWN is the true fallback,
    returned only when the shape is unsafe or no declared surface (including
    DOCS) matches.
    """
    if _unsafe_path_reason(path) is not None:
        return UNKNOWN
    for surface_id, predicate in SURFACES.items():
        if predicate(path):
            return surface_id
    return UNKNOWN


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


# Declared phase vocabulary for resolve_gates()'s fail-loud guard below --
# not GATES-derived, because an empty/mistyped manifest must not silently
# widen or narrow which phase strings are considered valid.
_KNOWN_PHASES = frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH})


def resolve_gates(phase: str, changes: ChangeSet) -> tuple[Gate, ...]:
    """Select the gates in ``GATES`` that must run for ``phase`` given ``changes``.

    Pure and total: no I/O, no env, no clock, no cwd -- every input is the two
    arguments, every output is a new tuple built fresh from ``GATES`` in
    declaration order. Never mutates ``changes`` or ``GATES``. Output order is
    manifest order, invariant to input path order and duplicates.

    A gate is selected when ``phase in gate.phases`` and at least one of:
    - ``gate.always`` is True, or
    - ``changes.unknown`` is True (the collection layer could not determine
      the real change set and fails closed), or
    - classifying ``changes.paths`` (deduped by first-seen via
      ``dict.fromkeys``, never by sorting) yields ``UNKNOWN`` for any path --
      an unsafe or unrecognized path shape fails closed exactly like
      ``changes.unknown``, forcing every phase-matching gate to run, or
    - the set of surfaces those paths classify to intersects ``gate.surfaces``.

    ``SURFACE_DOCS`` is explicitly inert: classifying to DOCS never by itself
    satisfies a gate's surface intersection, so an all-docs, all-safe-shape
    change selects only ``always`` gates (none exist in today's manifest).

    Raises ``ValueError`` for a phase outside the declared phase vocabulary --
    a typo must not silently disable every gate.
    """
    if phase not in _KNOWN_PHASES:
        raise ValueError(f"unknown phase: {phase!r}")

    deduped_paths = tuple(dict.fromkeys(changes.paths))
    classified_surfaces = frozenset(classify_path(path) for path in deduped_paths)
    escalate = changes.unknown or UNKNOWN in classified_surfaces

    return tuple(
        gate
        for gate in GATES
        if phase in gate.phases
        and (gate.always or escalate or classified_surfaces & gate.surfaces)
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
