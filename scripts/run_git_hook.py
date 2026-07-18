#!/usr/bin/env python3
"""Diff-aware local Git hook runner: collect -> resolve -> execute.

The tracked `.githooks/pre-commit` and `.githooks/pre-push` hooks delegate
here. This module is only the wiring; each layer lives in `git_hook/` and none
of them reaches backwards:

- `git_hook.collect` turns Git's own diff output into a `ChangeSet`.
  Fail-closed: a Git subprocess failure, or malformed pre-push stdin, sets
  `unknown=True` -- never presents as "no changes". Its Git calls run with the
  same scrubbed `GIT_*` env the execution layer uses, so an inherited `GIT_DIR`
  cannot redirect the decision itself at another repository.
- `git_hook.manifest` is pure and total: `resolve_gates(phase, changes)`
  classifies every changed path via `classify_path()` against the declared
  `SURFACES` and selects the matching `GATES` entries, in manifest declaration
  order. `UNKNOWN` -- an unsafe-shaped path, or one matching no declared
  surface -- escalates to running every gate declared for the phase. That
  fail-closed contract is what lets an inert-only change skip heavy local
  gates without ever letting a genuinely unrecognized path skip silently.
- `git_hook.execute` runs the selected gates with a hardened
  `subprocess.run` (scrubbed `GIT_*` env, no shell, `check=False`), printing
  one deterministic line per gate and stopping at the first non-zero exit.
- `git_hook.runtime` holds what the two shelling-out layers share: `ROOT` and
  the `GIT_*` env scrub.

`main(phase)` below is the only place those layers meet. No per-surface
branching lives here, or anywhere outside `manifest`.
"""
from __future__ import annotations

import sys

from git_hook import collect, execute, manifest


# main(phase) -- collect -> resolve -> execute, wired end-to-end.
#
# This is the only place run_pre_commit()/run_pre_push()'s old conditional
# assembly was replaced: no per-surface branching lives here or anywhere
# outside `manifest` -- `manifest.resolve_gates` (via `execute.execute_gates`)
# alone decides which gates run.


def _pre_push_manual_upstream_changes() -> manifest.ChangeSet:
    """Fallback for a manual/direct `pre-push` invocation with genuinely
    empty stdin (nothing piped at all). `main()` calls this only when the raw
    stdin text is exactly empty. Whitespace-only stdin does NOT reach here:
    a blank line is a ref-update line with zero fields, so
    `collect_pre_push_changes` rejects it as malformed and returns
    `unknown=True`, which `main()`'s `not changes.unknown` guard blocks. That
    is fail-closed and deliberate -- whitespace-only stdin escalates to every
    gate rather than silently diffing an unrelated range. The real
    `git push`-invoked hook always pipes at least one ref-update line per
    the pre-push hook contract, so a real push (including a deletion-only
    `git push --delete branch`, whose lines are all-zero-sha and are
    skipped by `collect_pre_push_changes`, not absent) can never reach this
    fallback. This exists solely for a developer running
    `python3 scripts/run_git_hook.py pre-push` directly with no input.

    Gating on stdin emptiness rather than on `changes.paths` being empty
    matters: a deletion-only push has real, non-empty stdin but yields
    `manifest.ChangeSet(paths=(), unknown=False)` from `collect_pre_push_changes`
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
    upstream = collect.git(["rev-parse", "--verify", "@{u}"], check=False).strip()
    if not upstream:
        return manifest.ChangeSet(paths=(), unknown=False, reason=None)
    output = collect.git(["diff", "--name-only", "--no-renames", "-z", f"{upstream}..HEAD"])
    return manifest.ChangeSet(paths=collect._split_nul(output), unknown=False, reason=None)


def main(phase: str) -> int:
    """Run the collect -> resolve -> execute pipeline for `phase`.

    `phase` must be `PHASE_PRE_COMMIT` or `PHASE_PRE_PUSH`; anything else
    raises `ValueError` (the same phase-vocabulary guard `resolve_gates`
    enforces), checked here first so an invalid phase fails before any I/O
    -- CLI-level validation (the usage message, exit code 2) is
    `_cli_main`'s job, not this function's.

    pre-commit collects via `collect.collect_pre_commit_changes()`. pre-push reads
    the ref-update batch from stdin via `collect_pre_push_changes()`; when
    the raw stdin text is exactly empty (nothing piped at all -- never true
    for a real `git push`-invoked hook, which always pipes at least one
    ref-update line per the pre-push hook contract), falls back to
    `_pre_push_manual_upstream_changes()` for a manual invocation outside
    Git's stdin contract (see that function's docstring for why this is kept
    and what it does and does not cover).

    Whitespace-only stdin never reaches that fallback, despite `strip()`
    appearing in the guard below: a blank line is a zero-field ref-update
    line, so `collect_pre_push_changes` has already returned `unknown=True`
    and the `not changes.unknown` half of the guard is False. Whitespace-only
    input therefore escalates to every gate -- fail-closed, and deliberately
    kept that way.

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
    if phase not in manifest._KNOWN_PHASES:
        raise ValueError(f"unknown phase: {phase!r}")
    if phase == manifest.PHASE_PRE_COMMIT:
        changes = collect.collect_pre_commit_changes()
    else:
        stdin_text = sys.stdin.read()
        changes = collect.collect_pre_push_changes(stdin_text)
        if not changes.unknown and not stdin_text.strip():
            changes = _pre_push_manual_upstream_changes()
    return execute.execute_gates(phase, changes)


def _cli_main(argv: list[str]) -> int:
    """Parse `argv` (as `sys.argv`: `argv[0]` the script path, `argv[1]` the
    phase) and dispatch to `main()`. A missing, unrecognized, or extra
    argument prints the usage line to stderr and returns 2 without calling
    `main()` -- the CLI contract preserved unchanged from the pre-Task-6
    `main(argv)`.
    """
    if len(argv) != 2 or argv[1] not in manifest._KNOWN_PHASES:
        print(
            f"usage: scripts/run_git_hook.py <{manifest.PHASE_PRE_COMMIT}|{manifest.PHASE_PRE_PUSH}>",
            file=sys.stderr,
        )
        return 2
    return main(argv[1])


if __name__ == "__main__":
    raise SystemExit(_cli_main(sys.argv))
