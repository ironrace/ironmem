"""Collection layer -- Git to `ChangeSet`, fail-closed.

The only layer that shells out to decide anything. It decides nothing itself:
these functions turn Git's own output into a `ChangeSet` and let
`manifest.resolve_gates` decide what runs.
"""
from __future__ import annotations

import os
import subprocess
import sys

from git_hook.manifest import ChangeSet
from git_hook.runtime import ROOT, _GIT_TIMEOUT_SECONDS, _scrub_git_env



def git(args: list[str], *, input_text: str | None = None, check: bool = True) -> str:
    """Un-hardened `subprocess.run(["git", ...])` wrapper (`check=True` by
    default -> raises `SystemExit` on a non-zero exit).

    Retained solely for `_pre_push_manual_upstream_changes()`'s manual `@{u}`
    fallback path (see that function's docstring for why that path's
    abort-on-failure behavior is deliberate and pinned by a test). All other
    Git invocations in this module go through the fail-closed `_run_git` /
    `_git_diff_paths_z` helpers below. New code must use those, not this one.

    Un-hardened refers to its error handling only: like every other Git call
    in this module it runs with `env=_scrub_git_env(os.environ)`, so an
    inherited repo-redirecting `GIT_*` variable cannot point it at another
    repository.
    """
    result = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        input=input_text,
        text=True,
        capture_output=True,
        check=False,
        env=_scrub_git_env(os.environ),
        timeout=_GIT_TIMEOUT_SECONDS,
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
#
# Every diff invocation also passes `--no-renames`. Rename detection is ON by
# default and reports ONLY the destination path, which is a fail-open hole in
# exactly the direction this manifest exists to close: `git mv
# crates/ironmem/src/foo.rs docs/foo.md` would report only `docs/foo.md`, which
# classifies `docs`, selects no gate, and exits 0 -- fmt/clippy/test never
# running on a workspace that may no longer compile. With `--no-renames` Git
# reports both sides, so the source path still classifies `rust_workspace`.
# This flag is load-bearing, not cosmetic: never drop it from a diff here.

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


def _is_zero_sha(value: str) -> bool:
    """True if `value` is Git's all-zero null object id for a supported hash.

    Git uses an all-zero object id to mark a pre-push branch creation
    (remote SHA) or deletion (local SHA). The sentinel has the repository's
    object-id length, so both SHA-1's 40 zeros and SHA-256's 64 zeros must be
    recognized; treating a 64-zero SHA as an ordinary revision would make a
    valid SHA-256 create/delete push fail collection and spuriously escalate.
    """
    return len(value) in _SHA_LENGTHS and value == "0" * len(value)


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
    here becomes a structured signal, never a traceback.

    `reason` is built from the full argv, the exception's class name, and the
    exception's message -- never from the subprocess's captured output or from
    the environment. The class name alone made `PermissionError`, `OSError`,
    and `UnicodeDecodeError` indistinguishable in a failure report; the
    message is what makes them debuggable. The same constraint the argv
    interpolation carries applies to it: every caller in this module passes
    subcommands, flags, and shas -- never a remote URL or a `user:pass@host`
    remote spec -- so neither half can carry credentials.

    Runs with `env=_scrub_git_env(os.environ)`, the same scrub the execution
    layer applies. This is not merely outbound hygiene here: an inherited
    `GIT_DIR`/`GIT_INDEX_FILE`/`GIT_WORK_TREE`/`GIT_CONFIG_COUNT` redirects
    this call at a *different* repository, and Git then exits 0 against it --
    so `unknown` is never set and the hook gates on someone else's change set.
    That is a fail-OPEN outcome, the opposite of this layer's contract.
    """
    try:
        result = subprocess.run(
            ["git", *args],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
            env=_scrub_git_env(os.environ),
            timeout=_GIT_TIMEOUT_SECONDS,
        )
    except Exception as exc:  # fail-closed: never let this propagate
        # `subprocess.TimeoutExpired` lands here like any other failure and
        # becomes an ordinary fail-closed `unknown=True` ChangeSet upstream --
        # a hung Git escalates, it never presents as "no changes".
        #
        # The full argv, not just `args[0]`: `git diff exited` is not enough
        # to tell which of this module's four diff invocations failed. An
        # empty `args` tuple renders as `<no-args>` rather than raising
        # IndexError from inside this handler (which would itself escape the
        # fail-closed boundary this function exists to provide).
        rendered_args = " ".join(args) if args else "<no-args>"
        return (
            False,
            -1,
            "",
            f"git {rendered_args} invocation raised {type(exc).__name__}: {exc}",
        )
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
    if not _is_zero_sha(remote_sha):
        return _git_diff_paths_z(
            ("diff", "--name-only", "--no-renames", "-z", f"{remote_sha}..{local_sha}")
        )

    ok, base, reason = _default_base(local_sha)
    if not ok:
        return False, (), reason
    if base is not None:
        return _git_diff_paths_z(
            ("diff", "--name-only", "--no-renames", "-z", f"{base}..{local_sha}")
        )
    return _git_diff_paths_z(
        (
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-z",
            "-r",
            local_sha,
        )
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

    Runs `git diff --cached --name-only --no-renames -z` -- the only Git
    invocation in this function. Fails closed: a non-zero exit or any
    subprocess-level failure (e.g. unreadable output) returns `unknown=True`
    with a non-empty `reason` and never a traceback. An empty result with
    `unknown=False` means genuinely nothing is staged, never that
    collection broke.
    """
    # No `--diff-filter`: staged deletions are intentionally in scope
    # (deleting a `.rs` file or `Cargo.toml` is exactly the kind of change
    # that should still trigger the Rust gates). The absence of
    # `--diff-filter=ACMRTUXB` here is a deliberate choice, not a lost flag.
    ok, paths, reason = _git_diff_paths_z(
        ("diff", "--cached", "--name-only", "--no-renames", "-z")
    )
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
        if _is_zero_sha(local_sha):
            continue  # deletion ref: nothing to diff

        ok, update_paths, reason = _collect_update_paths(local_sha, remote_sha)
        if not ok:
            return ChangeSet(paths=tuple(seen), unknown=True, reason=reason)
        for path in update_paths:
            seen.setdefault(path, None)

    return ChangeSet(paths=tuple(seen), unknown=False, reason=None)
