#!/usr/bin/env python3
"""Diff-aware local Git hook runner: collect -> resolve -> execute.

The tracked `.githooks/pre-commit` and `.githooks/pre-push` hooks delegate
here. `main(phase)` wires three layers, each with one job and no layer
reaching backwards:

- Collect (`collect_pre_commit_changes()` / `collect_pre_push_changes(stdin)`)
  turns Git's own diff output into a `ChangeSet`. Fail-closed: a Git
  subprocess failure, or malformed pre-push stdin, sets `unknown=True` --
  never presents as "no changes".
- Resolve (`resolve_gates(phase, changes)`) is pure and total: it classifies
  every changed path via `classify_path()` against the declared `SURFACES`
  (`rust_workspace`, `collab_protocol`, `hook_self_test`, `docs`,
  `inert_config`) and selects the matching `GATES` entries, in manifest
  declaration order. `UNKNOWN` -- an unsafe-shaped path, or one matching no
  declared surface -- escalates to running every gate declared for the
  phase. That fail-closed contract is what lets an inert-only change (docs,
  or config/data formats no gate parses or executes) skip heavy local gates
  without ever letting a genuinely unrecognized path skip silently.
- Execute (`execute_gates(phase, changes)`) runs the selected gates with a
  hardened `subprocess.run` (scrubbed `GIT_*` env, no shell, `check=False`),
  printing one deterministic line per gate (`run` / `skip (...)` /
  `fail (...)`) and stopping at the first non-zero exit.
"""
from __future__ import annotations

import dataclasses
import os
import pathlib
import subprocess
import sys
from types import MappingProxyType
from typing import Callable, Mapping

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


def git(args: list[str], *, input_text: str | None = None, check: bool = True) -> str:
    """Un-hardened `subprocess.run(["git", ...])` wrapper (`check=True` by
    default -> raises `SystemExit` on a non-zero exit).

    Retained solely for `_pre_push_manual_upstream_changes()`'s manual `@{u}`
    fallback path (see that function's docstring for why that path's
    abort-on-failure behavior is deliberate and pinned by a test). All other
    Git invocations in this module go through the fail-closed `_run_git` /
    `_git_diff_paths_z` helpers below. New code must use those, not this one.
    """
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


# --- Collection layer -- Git to ChangeSet, fail-closed ---------------------
#
# The only place in the collection layer that shells out. Decides nothing:
# these functions turn Git's own output into a `ChangeSet` (see the frozen
# data model below) and let `resolve_gates` decide what runs. Every Git
# invocation uses `-z` NUL-delimited output, which removes `core.quotepath`
# escaping at the source and makes newline-bearing filenames unambiguous
# rather than a parsing hazard -- that is what makes byte-exact paths safe
# here. No `.strip()`/unquoting/case-folding is ever applied to a *path*;
# `.strip()` on a sha or ref name below is not a path and is not covered by
# that rule.

_HEX_DIGITS = frozenset("0123456789abcdefABCDEF")


_SHA_LENGTHS = frozenset({40, 64})  # SHA-1 (40 hex) and SHA-256 (64 hex) object ids


def _is_hex_sha(value: str) -> bool:
    """True if `value` is a hex string of a real Git object-id length.

    Git object ids in a pre-push stdin line are always 40-hex (SHA-1) or
    64-hex (SHA-256); a `sha` field is not a path, so validating/rejecting it
    is not covered by the byte-exact-path constraint. The length check
    matters: accepting any positive-length hex run (e.g. `"abc"`) would make
    this guard a formality that a malformed-but-hex-looking stdin line could
    still slip past.
    """
    return len(value) in _SHA_LENGTHS and all(char in _HEX_DIGITS for char in value)


def _split_nul(output: str) -> tuple[str, ...]:
    """Split `-z` NUL-delimited Git output into a byte-exact tuple of paths.

    Drops exactly one trailing empty field: `-z` terminates every path with
    a NUL, so a non-empty listing always splits into N paths plus one
    trailing empty string, and an empty listing is the empty string. That
    trailing field is output *framing*, not path content -- dropping it is
    not the byte-exact-path violation the no-`.strip()` rule forbids, and no
    interior field (including one that happens to be empty, or one carrying
    an embedded newline) is ever touched.
    """
    if output == "":
        return ()
    parts = output.split("\0")
    if parts and parts[-1] == "":
        parts = parts[:-1]
    return tuple(parts)


def _run_git(args: tuple[str, ...]) -> tuple[bool, int, str, str]:
    """Invoke `git <args>`, capturing output without raising.

    Returns `(subprocess_ok, returncode, stdout, reason)`. `subprocess_ok`
    is False only when the subprocess call itself failed (git binary
    missing, output undecodable, etc.) -- never based on git's own exit
    code, which callers interpret themselves; this is the single fail-closed
    boundary for Git subprocess calls in the collection layer, so a failure
    here becomes a structured signal, never a traceback. `reason` is built
    only from the argv and the exception's class name -- never from
    output or environment -- so it cannot carry credentials or secrets.
    """
    try:
        result = subprocess.run(
            ["git", *args],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
    except Exception as exc:  # fail-closed: never let this propagate
        # Guard args[0]: an empty `args` tuple must still fail closed with a
        # structured reason, not raise IndexError from inside this handler
        # (which would itself escape the fail-closed boundary this function
        # exists to provide).
        subcommand = args[0] if args else "<no-args>"
        return False, -1, "", f"git {subcommand} invocation raised {type(exc).__name__}"
    return True, result.returncode, result.stdout, ""


def _git_diff_paths_z(args: tuple[str, ...]) -> tuple[bool, tuple[str, ...], str]:
    """Run a Git command expected to print `-z` NUL-delimited paths.

    Returns `(ok, paths, reason)`. Unlike `_run_git`'s raw semantics (where a
    non-zero exit is left to the caller), here a non-zero exit *is* a
    collection failure: `ok=False` whenever the diff itself failed, not just
    when the subprocess call did.
    """
    ok, returncode, stdout, reason = _run_git(args)
    if not ok:
        return False, (), reason
    if returncode != 0:
        # `reason` interpolates the full argv verbatim. That's safe only
        # because every caller in this module passes subcommands, flags, and
        # shas -- never a remote URL or a `user:pass@host` remote spec. This
        # constraint must hold for every future caller too, or `reason`
        # (which is allowed to reach stderr/logs) could leak credentials.
        return False, (), f"git {' '.join(args)} exited {returncode}"
    return True, _split_nul(stdout), ""


def _default_base(local_sha: str) -> tuple[bool, str | None, str]:
    """Find a merge-base candidate for `local_sha`.

    Tries, in order: `refs/remotes/origin/HEAD` (resolved via
    `symbolic-ref`), then `origin/main`, `origin/master`, `main`, `master`.
    Ported unchanged from the pre-refactor `default_base()`'s candidate
    search.

    Returns `(ok, base_or_none, reason)`. `ok` is False only on a
    subprocess-level Git failure -- a candidate ref simply not existing
    (non-zero exit) is expected and the next candidate is tried, not a
    collection failure. `base_or_none` is `None` when no candidate resolves
    (the caller falls back to a root diff), never when `ok` is False.
    """
    candidates: list[str] = []
    ok, returncode, stdout, reason = _run_git(
        ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD")
    )
    if not ok:
        return False, None, reason
    if returncode == 0:
        origin_head = stdout.strip()  # a ref name, not a path
        if origin_head:
            candidates.append(origin_head)
    candidates.extend(["origin/main", "origin/master", "main", "master"])

    for candidate in candidates:
        ok, returncode, stdout, reason = _run_git(("merge-base", local_sha, candidate))
        if not ok:
            return False, None, reason
        if returncode == 0:
            base = stdout.strip()  # a sha, not a path
            if base:
                return True, base, ""
    return True, None, ""


def _collect_update_paths(local_sha: str, remote_sha: str) -> tuple[bool, tuple[str, ...], str]:
    """Collect the changed paths for one pre-push ref update.

    Diffs `remote_sha..local_sha` directly when `remote_sha` is known.
    Otherwise (branch creation / all-zero remote sha) resolves a base via
    `_default_base` and diffs `base..local_sha`; when no base is found
    (missing upstream), falls back to a root diff of `local_sha`. `-z`
    NUL-delimited output throughout.
    """
    if remote_sha != ZERO_SHA:
        return _git_diff_paths_z(("diff", "--name-only", "-z", f"{remote_sha}..{local_sha}"))

    ok, base, reason = _default_base(local_sha)
    if not ok:
        return False, (), reason
    if base is not None:
        return _git_diff_paths_z(("diff", "--name-only", "-z", f"{base}..{local_sha}"))
    return _git_diff_paths_z(
        ("diff-tree", "--root", "--no-commit-id", "--name-only", "-z", "-r", local_sha)
    )


def _parse_pre_push_line(line: str) -> tuple[str, str, str, str] | None:
    """Split one pre-push stdin line into its four whitespace-separated
    fields (`local_ref local_sha remote_ref remote_sha`), or `None` if the
    line is not exactly four fields.

    `str.split()` (whitespace, no separator argument) is correct here: ref
    names and shas never contain whitespace. Splitting/counting *stdin
    fields* is not a path operation and is not covered by the
    byte-exact-path constraint.
    """
    fields = line.split()
    if len(fields) != 4:
        return None
    return fields[0], fields[1], fields[2], fields[3]


def collect_pre_commit_changes() -> ChangeSet:
    """Collect the `ChangeSet` for the pre-commit phase.

    Runs `git diff --cached --name-only -z` -- the only Git invocation in
    this function. Fails closed: a non-zero exit or any subprocess-level
    failure (e.g. unreadable output) returns `unknown=True` with a
    non-empty `reason` and never a traceback. An empty result with
    `unknown=False` means genuinely nothing is staged, never that
    collection broke.
    """
    # No `--diff-filter`: staged deletions are intentionally in scope
    # (deleting a `.rs` file or `Cargo.toml` is exactly the kind of change
    # that should still trigger the Rust gates). The absence of
    # `--diff-filter=ACMRTUXB` here is a deliberate choice, not a lost flag.
    ok, paths, reason = _git_diff_paths_z(("diff", "--cached", "--name-only", "-z"))
    if not ok:
        return ChangeSet(paths=paths, unknown=True, reason=reason)
    return ChangeSet(paths=paths, unknown=False, reason=None)


def collect_pre_push_changes(stdin_text: str) -> ChangeSet:
    """Collect the `ChangeSet` for the pre-push phase from the ref-update
    lines Git writes to stdin (`<local_ref> <local_sha> <remote_ref>
    <remote_sha>` per line, per the pre-push hook contract).

    Deletion refs (`local_sha` all-zero) are skipped. Each remaining update
    is diffed via `_collect_update_paths`; the resulting paths accumulate
    first-seen and dedupe across every update in the batch.

    Fails closed on the first problem encountered -- a malformed line
    (wrong field count, non-hex sha) or any per-update Git failure -- and
    returns immediately with `unknown=True`, a non-empty `reason`, and
    whatever paths were already collected from earlier updates in this same
    batch. Never returns an empty path list to mean "collection broke";
    that state is only ever `unknown=True`.
    """
    seen: dict[str, None] = {}
    for line_number, line in enumerate(stdin_text.splitlines(), start=1):
        fields = _parse_pre_push_line(line)
        if fields is None:
            return ChangeSet(
                paths=tuple(seen),
                unknown=True,
                reason=f"malformed pre-push stdin line {line_number}: expected 4 fields",
            )
        _local_ref, local_sha, _remote_ref, remote_sha = fields
        if not (_is_hex_sha(local_sha) and _is_hex_sha(remote_sha)):
            return ChangeSet(
                paths=tuple(seen),
                unknown=True,
                reason=f"malformed pre-push stdin line {line_number}: non-hex sha",
            )
        if local_sha == ZERO_SHA:
            continue  # deletion ref: nothing to diff

        ok, update_paths, reason = _collect_update_paths(local_sha, remote_sha)
        if not ok:
            return ChangeSet(paths=tuple(seen), unknown=True, reason=reason)
        for path in update_paths:
            seen.setdefault(path, None)

    return ChangeSet(paths=tuple(seen), unknown=False, reason=None)


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


# Extensions no gate in GATES parses, executes, or otherwise inspects: none
# of the five gates (hook self-test, install-drift check, collab template
# lint, cargo fmt/clippy/test) reads JSON/YAML/shell/CSV/HTML content. A
# `str.endswith()` tuple argument is a byte-exact suffix check, same as
# `is_docs_path`'s `.md` check -- no case-folding.
_INERT_CONFIG_EXTENSIONS = (
    ".json",
    ".jsonc",
    ".jsonl",
    ".yaml",
    ".yml",
    ".sh",
    ".csv",
    ".html",
)


def is_inert_config_path(path: str) -> bool:
    """Second explicitly-inert surface: non-code config/data files that no
    declared gate would catch a defect in.

    Three patterns, each justified by "no existing gate parses, executes, or
    otherwise inspects this file":

    - A file extension in ``_INERT_CONFIG_EXTENSIONS`` (JSON/YAML/shell/CSV/
      HTML) -- none of the five gates reads these formats.
    - Any path under a top-level ``site/`` directory (the static site,
      entirely outside the Rust workspace and the hook/collab scope),
      matched the same way ``is_docs_path`` matches ``docs/``: on the
      leading ``/``-split segment, never a substring, so a look-alike
      directory such as ``sitehost/`` never matches.
    - A ``.py`` file under a top-level ``benchmarks/`` directory. The
      ``benchmarks/*`` Cargo crates are listed in the workspace manifest's
      ``exclude`` list, so `cargo fmt --all`/`cargo clippy --workspace`/
      `cargo test --workspace` never touch them either way -- but their own
      ``.rs`` source files still classify ``rust_workspace`` via
      `is_rust_path` (checked before this surface, see the `SURFACES`
      ordering comment below), so this pattern is scoped to `.py` only, not
      the whole `benchmarks/` tree. `crates/*/migrations/*.sql` is
      deliberately NOT covered by any pattern here: those files are
      `include_str!`'d into the Rust binary and replayed by `cargo test`'s
      migration tests, so a real gate *does* catch a defect there -- they
      must stay `UNKNOWN` (escalate), not become inert.

    Declared after every specific surface in `SURFACES` (see the ordering
    comment there): `scripts/install-git-hooks.sh` matches this predicate's
    `.sh` check too, but `SURFACE_HOOK_SELF_TEST`'s exact-path predicate is
    checked first and wins -- the same ordering protection that lets
    `docs/COLLAB.md` win to `collab_protocol` over the generic `docs`
    catch-all.
    """
    if path.endswith(_INERT_CONFIG_EXTENSIONS):
        return True
    top_segment = path.split("/", 1)[0]
    if top_segment == "site":
        return True
    if top_segment == "benchmarks" and path.endswith(".py"):
        return True
    return False


# --- Frozen data model -------------------------------------------------
#
# `Gate`/`ChangeSet`/`GATES`/`SURFACES` are the pure data layer the
# collect -> resolve -> execute pipeline reads. `main()` (bottom of this
# file) is what wires collection, `resolve_gates`, and `execute_gates`
# together; nothing in this section performs I/O itself.

PHASE_PRE_COMMIT = "pre-commit"
PHASE_PRE_PUSH = "pre-push"

SURFACE_RUST_WORKSPACE = "rust_workspace"
SURFACE_COLLAB_PROTOCOL = "collab_protocol"
SURFACE_HOOK_SELF_TEST = "hook_self_test"
SURFACE_DOCS = "docs"
SURFACE_INERT_CONFIG = "inert_config"

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
# the DOCS surface's is_docs_path and the INERT_CONFIG surface's
# is_inert_config_path. Iteration order is declaration order: classify_path()
# below checks the specific surfaces first and the two generic inert
# catch-alls (DOCS, then INERT_CONFIG) last, so a more specific surface (e.g.
# collab protocol) wins over a generic inert catch-all when a path happens to
# satisfy both (docs/COLLAB.md is both under docs/ and in the collab-protocol
# exact set; scripts/install-git-hooks.sh is both a .sh file and in the
# hook_self_test exact set -- HOOK_SELF_TEST is declared, and therefore
# checked, before INERT_CONFIG, so it wins).
SURFACES: MappingProxyType[str, Callable[[str], bool]] = MappingProxyType(
    {
        SURFACE_RUST_WORKSPACE: is_rust_path,
        SURFACE_COLLAB_PROTOCOL: is_collab_protocol_path,
        SURFACE_HOOK_SELF_TEST: is_hook_path,
        SURFACE_DOCS: is_docs_path,
        SURFACE_INERT_CONFIG: is_inert_config_path,
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
    look-alike like `docsite/`, `sitehost/`, `benchmarksish/`, or `contests/`
    never matches the surface it resembles. Unsafe shapes (absolute paths,
    `..` segments, NUL/control bytes, empty string, leading `-`, non-`str`)
    are rejected to UNKNOWN before any surface is checked, never sanitized.
    DOCS and INERT_CONFIG are declared surfaces like any other, not a
    fallback; UNKNOWN is the true fallback, returned only when the shape is
    unsafe or no declared surface (including DOCS and INERT_CONFIG) matches.
    """
    if _unsafe_path_reason(path) is not None:
        return UNKNOWN
    for surface_id, predicate in SURFACES.items():
        if predicate(path):
            return surface_id
    return UNKNOWN


# Declaration order IS execution order. Never sorted at runtime. Ported
# unchanged from the pre-Task-6 run_pre_commit()/run_pre_push() conditional
# assembly this manifest replaced (same argv, same phase membership).
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

    Byte-preserving: every path in ``changes.paths`` is classified exactly as
    received. Deduping via ``dict.fromkeys`` compares paths byte-for-byte
    (never after ``.strip()``/unquoting/case-folding), and ``classify_path``
    itself never rewrites a path before matching it against ``SURFACES`` --
    an unsafe or attacker-influenced path is escalated to ``UNKNOWN``, never
    sanitized and reclassified. This is deliberate: rewriting a path before
    classifying it would let this function's decision disagree with what Git
    actually staged.

    A gate is selected when ``phase in gate.phases`` and at least one of:
    - ``gate.always`` is True, or
    - ``changes.unknown`` is True (the collection layer could not determine
      the real change set and fails closed), or
    - classifying ``changes.paths`` (deduped by first-seen via
      ``dict.fromkeys``, never by sorting) yields ``UNKNOWN`` for any path --
      an unsafe or unrecognized path shape fails closed exactly like
      ``changes.unknown``, forcing every phase-matching gate to run, or
    - the set of surfaces those paths classify to intersects ``gate.surfaces``.

    ``SURFACE_DOCS`` and ``SURFACE_INERT_CONFIG`` are explicitly inert:
    classifying to either never by itself satisfies a gate's surface
    intersection, so an all-docs/all-inert-config, all-safe-shape change
    selects only ``always`` gates (none exist in today's manifest).

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


# --- Execution layer -- run resolver-selected gates, hardened subprocess
# contract ---------------------------------------------------------------
#
# `execute_gates` is the last stage of collect -> resolve -> execute. It
# re-derives nothing: `resolve_gates` alone decides which gates run, this
# layer only runs them and reports. `main()` (bottom of this file) wires
# this together with the collection layer above.

# Repo-redirecting variables: inheriting any of these points a child Git
# invocation at a *different* repository than the one the hook is running
# in. This is not theoretical -- a pre-push hook exporting GIT_DIR/
# GIT_INDEX_FILE/GIT_WORK_TREE let a `cargo test` tempdir Git fixture
# inherit them and commit into the real repo (PR #186). Always stripped;
# never in the keep-list.
#   GIT_DIR / GIT_WORK_TREE / GIT_INDEX_FILE   -- which repo/worktree/index
#   GIT_OBJECT_DIRECTORY / GIT_ALTERNATE_OBJECT_DIRECTORIES -- which object store
#   GIT_COMMON_DIR                              -- which shared worktree dir
#   GIT_NAMESPACE                                -- which ref namespace
#
# Keep-list: variables that configure *how* Git authenticates or reports,
# not *where* it points. GIT_CONFIG_* is deliberately NOT here (see below).
#   GIT_ASKPASS, GIT_SSH, GIT_SSH_COMMAND -- each execs a caller-chosen helper
#                                              program to authenticate over SSH.
#                                              They cannot redirect a child Git
#                                              invocation at a different repo,
#                                              and dropping them breaks
#                                              legitimate SSH-agent wrappers and
#                                              CI credential setups -- kept
#                                              despite executing arbitrary code,
#                                              not because they are inert.
#   GIT_TERMINAL_PROMPT                    -- whether Git may prompt interactively
#   GIT_TRACE*                             -- GIT_TRACE=<path> appends trace
#                                              output to that path: a file-write
#                                              primitive, not merely diagnostic
#                                              output, but it cannot redirect a
#                                              child Git invocation at another
#                                              repository the way GIT_CONFIG_*
#                                              can (see below), so it stays.
#
# GIT_CONFIG_* is excluded on purpose, not an oversight: GIT_CONFIG_COUNT +
# GIT_CONFIG_KEY_n/GIT_CONFIG_VALUE_n are the documented equivalent of
# `git -c <key>=<value>` for ARBITRARY config, and GIT_CONFIG_GLOBAL/
# GIT_CONFIG_SYSTEM (also matched by this prefix) replace whole config files.
# That includes core.worktree -- the config equivalent of GIT_WORK_TREE, which
# this module strips above as a repo-redirecting variable -- plus core.bare,
# core.hooksPath, credential.helper, core.pager, core.sshCommand, alias.*, and
# *.textconv. A caller could set GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=
# core.worktree GIT_CONFIG_VALUE_0=/real/repo and reproduce exactly the
# redirection this scrub exists to prevent (PR #186) through the front door.
# Keeping it would make the docstring's "no kept variable can redirect a
# child Git invocation" premise false, so it is stripped like any other
# unrecognized GIT_*-prefixed variable.
#
# Default toward scrubbing: anything GIT_*-prefixed that isn't explicitly
# named or prefix-matched here is dropped, including variables not yet
# invented. The keep-list is an allowlist, not a denylist.
_GIT_ENV_KEEP_EXACT: frozenset[str] = frozenset(
    {"GIT_ASKPASS", "GIT_SSH", "GIT_SSH_COMMAND", "GIT_TERMINAL_PROMPT"}
)
_GIT_ENV_KEEP_PREFIXES: tuple[str, ...] = ("GIT_TRACE",)


def _scrub_git_env(source_env: Mapping[str, str]) -> dict[str, str]:
    """Build a child-process env from `source_env` (normally `os.environ`)
    with every `GIT_*` variable removed except the explicit keep-list above.

    Returns a new dict; never mutates `source_env`. Non-`GIT_*` variables
    (PATH, HOME, ...) always pass through untouched.
    """
    return {
        key: value
        for key, value in source_env.items()
        if not key.startswith("GIT_")
        or key in _GIT_ENV_KEEP_EXACT
        or key.startswith(_GIT_ENV_KEEP_PREFIXES)
    }


# Declaration position of each surface id in SURFACES, used only to render a
# gate's `surfaces` frozenset in a deterministic order for the skip line
# below. Frozenset iteration order depends on CPython's per-process string
# hash randomization, so printing straight from the frozenset would make
# the skip line's surfaces field vary run to run for any gate declaring more
# than one surface, even though nothing else changed.
_SURFACE_ORDER: MappingProxyType[str, int] = MappingProxyType(
    {surface_id: index for index, surface_id in enumerate(SURFACES)}
)


def _ordered_surfaces(surface_ids: frozenset[str]) -> tuple[str, ...]:
    # Every `surface_id` here always comes from a `Gate.surfaces` frozenset,
    # and every surface a Gate declares is a key in `SURFACES` (enforced by
    # construction in the GATES manifest above), so `_SURFACE_ORDER[surface_id]`
    # cannot KeyError today. Left unguarded deliberately: a gate declaring a
    # surface absent from SURFACES is a manifest bug that should fail loud
    # and immediately, not be swallowed into a silently-unordered fallback.
    return tuple(sorted(surface_ids, key=lambda surface_id: _SURFACE_ORDER[surface_id]))


def execute_gates(phase: str, changes: ChangeSet) -> int:
    """Run every `GATES` entry matching `phase` that `resolve_gates` selects,
    in manifest declaration order; print one deterministic line per gate
    (`run` / `skip (<surfaces-not-touched>)` / `fail (<exit>)`) and stop at
    the first non-zero exit, returning that exit code immediately without
    invoking any later gate. Returns 0 when every executed gate exits 0,
    including the trivial all-skipped case. A negative (signal-killed)
    returncode is normalized to the positive shell convention (128 + signal)
    before it is printed or returned. A gate binary that can't be exec'd at
    all (`OSError`, e.g. missing `cargo`) prints its own `fail (...)` line
    and then propagates the exception -- fail-loud, not a returned code.

    Re-derives nothing: `resolve_gates(phase, changes)` alone decides what
    runs (and raises `ValueError` for an unknown `phase`, not re-checked
    here). Each subprocess call uses the hardened contract required by this
    layer: `shell=False`, a list argv taken verbatim from `gate.argv`,
    `cwd=ROOT`, `check=False`, and an explicit `env=` -- computed once via
    `_scrub_git_env(os.environ)` and passed to every gate this call runs --
    never the inherited process env as-is.

    When `changes.unknown` is True, `resolve_gates` has already escalated to
    running every phase-matching gate; this function additionally prints
    `changes.reason` first so a surprising full run explains itself rather
    than looking arbitrary.

    After every phase-matching gate has run or been skipped with no failure,
    prints exactly one trailing completion line: `[git-hook] <phase>: no
    local gates required` when nothing was selected (an all-docs or
    all-inert-config change), or `[git-hook] <phase>: N gate(s) run, 0
    failed` otherwise. This line is never printed on the early-return failure
    path -- a run that stopped at a non-zero exit did not complete, so
    nothing claims it did.
    """
    # resolve_gates() validates `phase` (raises ValueError for an unrecognized
    # one) -- called before any output is printed, so an invalid phase fails
    # loudly with no partial/misleading output ahead of the raise.
    selected = set(resolve_gates(phase, changes))

    if changes.unknown and changes.reason:
        print(f"[git-hook] escalating: {changes.reason}", flush=True)

    child_env = _scrub_git_env(os.environ)

    for gate in GATES:
        # This re-checks `phase in gate.phases` even though `selected` (from
        # resolve_gates) already excludes phase-mismatched gates -- not a
        # re-derivation of the resolver's decision, but the candidate set the
        # skip line needs: a gate outside this phase shouldn't print a skip
        # line at all (it isn't a candidate for this run), while a
        # phase-matching gate that `selected` excluded prints skip with its
        # untouched surfaces. `selected` alone can't distinguish those two
        # cases.
        if phase not in gate.phases:
            continue
        if gate not in selected:
            surfaces = ",".join(_ordered_surfaces(gate.surfaces))
            print(f"[git-hook] {gate.name}: skip ({surfaces})", flush=True)
            continue
        print(f"[git-hook] {gate.name}: run", flush=True)
        try:
            result = subprocess.run(
                list(gate.argv), shell=False, cwd=ROOT, env=child_env, check=False
            )
        except OSError as exc:
            # A missing/non-executable gate binary (e.g. no `cargo` on PATH)
            # raises out of subprocess.run before any CompletedProcess
            # exists. Print the fail line the one-line-per-gate contract
            # promises before re-raising -- still fail-loud (this is a
            # broken environment, not a recoverable gate failure), just no
            # longer silent about which gate broke.
            print(f"[git-hook] {gate.name}: fail (exec error: {exc})", flush=True)
            raise
        returncode = result.returncode
        if returncode < 0:
            # A signal-killed gate (e.g. SIGKILL) reports a negative
            # returncode; normalize to the shell convention (128 + signal)
            # so a downstream `sys.exit(code)` doesn't get bitten by
            # Python's exit-code modulo (sys.exit(-9) -> 247, not -9).
            returncode = 128 - returncode
        if returncode != 0:
            print(f"[git-hook] {gate.name}: fail ({returncode})", flush=True)
            return returncode

    # Every phase-matching gate either ran successfully or was skipped -- the
    # run completed. State that plainly rather than leaving a docs/inert-only
    # commit with nothing but `skip (...)` lines and no statement that this
    # was the intended outcome, not a run that broke before printing
    # anything. Restores the pre-Task-6 `[pre-commit] staged files: N` /
    # "no local gates required" summary this refactor had dropped with no
    # replacement.
    if not selected:
        print(f"[git-hook] {phase}: no local gates required", flush=True)
    else:
        print(f"[git-hook] {phase}: {len(selected)} gate(s) run, 0 failed", flush=True)
    return 0


# --- Task 6: main(phase) -- collect -> resolve -> execute, wired end-to-end
#
# This is the only place run_pre_commit()/run_pre_push()'s old conditional
# assembly is replaced: no per-surface branching lives here or anywhere else
# in the file -- `resolve_gates` (via `execute_gates`) alone decides which
# gates run.


def _pre_push_manual_upstream_changes() -> ChangeSet:
    """Fallback for a manual/direct `pre-push` invocation with genuinely
    empty stdin (nothing piped at all). `main()` calls this only when the
    raw stdin text itself is empty or whitespace-only -- the real
    `git push`-invoked hook always pipes at least one ref-update line per
    the pre-push hook contract, so a real push (including a deletion-only
    `git push --delete branch`, whose lines are all-zero-sha and are
    skipped by `collect_pre_push_changes`, not absent) can never reach this
    fallback. This exists solely for a developer running
    `python3 scripts/run_git_hook.py pre-push` directly with no input.

    Gating on stdin emptiness rather than on `changes.paths` being empty
    matters: a deletion-only push has real, non-empty stdin but yields
    `ChangeSet(paths=(), unknown=False)` from `collect_pre_push_changes`
    (nothing to diff for a pure ref deletion) -- indistinguishable from the
    manual-invocation case if the check were `not changes.paths`. Diffing
    `@{u}..HEAD` of the *currently checked-out* branch in that case would be
    a range unrelated to the refs actually being pushed. Gating on the raw
    stdin text keeps this fallback reachable only from genuine manual
    invocation with no piped input.

    DECISION (Task 6): kept, ported unchanged from the retired
    `pushed_paths()`'s `@{u}` fallback -- including reusing the original ad
    hoc `git()` helper (not the fail-closed `_run_git`/`_git_diff_paths_z`
    helpers the real collection path above uses). This is a best-effort
    convenience for manual invocation, not the hardened contract those
    helpers exist to provide: a diff failure here raises `SystemExit` via
    `git()`'s default `check=True`, aborting before any gate runs, the same
    way the pre-Task-6 fallback did. An absent `@{u}` (no upstream
    configured) resolves to a genuinely empty `ChangeSet`, not
    `unknown=True` -- that is an expected outcome for a branch with no
    upstream, not a collection failure.
    """
    upstream = git(["rev-parse", "--verify", "@{u}"], check=False).strip()
    if not upstream:
        return ChangeSet(paths=(), unknown=False, reason=None)
    output = git(["diff", "--name-only", "-z", f"{upstream}..HEAD"])
    return ChangeSet(paths=_split_nul(output), unknown=False, reason=None)


def main(phase: str) -> int:
    """Run the collect -> resolve -> execute pipeline for `phase`.

    `phase` must be `PHASE_PRE_COMMIT` or `PHASE_PRE_PUSH`; anything else
    raises `ValueError` (the same phase-vocabulary guard `resolve_gates`
    enforces), checked here first so an invalid phase fails before any I/O
    -- CLI-level validation (the usage message, exit code 2) is
    `_cli_main`'s job, not this function's.

    pre-commit collects via `collect_pre_commit_changes()`. pre-push reads
    the ref-update batch from stdin via `collect_pre_push_changes()`; when
    the raw stdin text itself is empty or whitespace-only (nothing piped at
    all -- never true for a real `git push`-invoked hook, which always pipes
    at least one ref-update line per the pre-push hook contract), falls back
    to `_pre_push_manual_upstream_changes()` for a manual invocation outside
    Git's stdin contract (see that function's docstring for why this is kept
    and what it does and does not cover).

    Gating on the raw stdin text, not on `changes.paths` being empty, is
    deliberate: a deletion-only push (`git push --delete branch`) pipes
    real, non-empty stdin whose lines are all skipped by
    `collect_pre_push_changes` (all-zero local sha), yielding a genuinely
    empty, non-`unknown` `ChangeSet` from real piped input -- that must
    never be mistaken for the no-stdin-at-all manual-invocation case, or the
    fallback would diff `@{u}..HEAD` of the *currently checked-out* branch,
    a range unrelated to the refs actually being pushed.

    Either way, `execute_gates(phase, changes)` alone decides and runs the
    gates -- this function never re-derives that decision.
    """
    if phase not in _KNOWN_PHASES:
        raise ValueError(f"unknown phase: {phase!r}")
    if phase == PHASE_PRE_COMMIT:
        changes = collect_pre_commit_changes()
    else:
        stdin_text = sys.stdin.read()
        changes = collect_pre_push_changes(stdin_text)
        if not changes.unknown and not stdin_text.strip():
            changes = _pre_push_manual_upstream_changes()
    return execute_gates(phase, changes)


def _cli_main(argv: list[str]) -> int:
    """Parse `argv` (as `sys.argv`: `argv[0]` the script path, `argv[1]` the
    phase) and dispatch to `main()`. A missing, unrecognized, or extra
    argument prints the usage line to stderr and returns 2 without calling
    `main()` -- the CLI contract preserved unchanged from the pre-Task-6
    `main(argv)`.
    """
    if len(argv) != 2 or argv[1] not in _KNOWN_PHASES:
        print(
            f"usage: scripts/run_git_hook.py <{PHASE_PRE_COMMIT}|{PHASE_PRE_PUSH}>",
            file=sys.stderr,
        )
        return 2
    return main(argv[1])


if __name__ == "__main__":
    raise SystemExit(_cli_main(sys.argv))
