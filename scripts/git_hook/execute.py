"""Execution layer -- run resolver-selected gates, hardened subprocess contract.

`execute_gates` is the last stage of collect -> resolve -> execute. It
re-derives nothing: `manifest.resolve_gates` alone decides which gates run;
this layer only runs them and reports. `run_git_hook.main()` wires it together
with the collection layer.

`manifest` is imported as a MODULE, and `GATES`/`resolve_gates`/
`_ordered_surfaces` are read through it at call time rather than bound with
`from ... import`. That is load-bearing for the test suite: binding them here
would make `monkeypatch.setattr(manifest, "GATES", ...)` patch a name this
module never reads, and the affected tests would silently exercise the real
manifest instead of their single-gate fixture -- passing or failing for
reasons unrelated to what they assert. Do not "tidy" these into direct
imports.
"""
from __future__ import annotations

import os
import subprocess

from git_hook import manifest
from git_hook.manifest import ChangeSet
from git_hook.runtime import ROOT, _scrub_git_env



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

    Re-derives nothing: `manifest.resolve_gates(phase, changes)` alone decides what
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
    # manifest.resolve_gates() validates `phase` (raises ValueError for an unrecognized
    # one) -- called before any output is printed, so an invalid phase fails
    # loudly with no partial/misleading output ahead of the raise.
    selected = set(manifest.resolve_gates(phase, changes))

    if changes.unknown and changes.reason:
        print(f"[git-hook] escalating: {changes.reason}", flush=True)

    child_env = _scrub_git_env(os.environ)
    ran_count = 0

    for gate in manifest.GATES:
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
            surfaces = ",".join(manifest._ordered_surfaces(gate.surfaces))
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
        ran_count += 1

    # Every phase-matching gate either ran successfully or was skipped -- the
    # run completed. State that plainly rather than leaving a docs/inert-only
    # commit with nothing but `skip (...)` lines and no statement that this
    # was the intended outcome, not a run that broke before printing
    # anything. Restores the pre-Task-6 `[pre-commit] staged files: N` /
    # "no local gates required" summary this refactor had dropped with no
    # replacement. `ran_count` (incremented only after a gate's subprocess
    # actually completed with exit 0), not `len(selected)`, is what's
    # printed -- `selected` is what the resolver picked, which today always
    # equals what ran (the loop returns immediately on the first failure),
    # but counting what ran keeps the two concepts distinct rather than
    # relying on that coincidence.
    if ran_count == 0:
        print(f"[git-hook] {phase}: no local gates required", flush=True)
    else:
        print(f"[git-hook] {phase}: {ran_count} gate(s) run, 0 failed", flush=True)
    return 0
