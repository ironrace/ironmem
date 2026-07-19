"""Diff-aware local Git hook runner, split by pipeline stage.

The tracked `.githooks/pre-commit` and `.githooks/pre-push` hooks delegate to
`scripts/run_git_hook.py`, which wires three layers, each with one job and no
layer reaching backwards:

- `collect` -- Git output to a `ChangeSet`, fail-closed.
- `manifest` -- the pure data layer: surfaces, path classification, the `GATES`
  declaration, and `resolve_gates`. No I/O.
- `execute` -- runs the resolver-selected gates under a hardened subprocess
  contract.

`runtime` holds the two facts both subprocess-running layers share: the repo
root and the `GIT_*` env scrub.

This package is deliberately import-light: it re-exports nothing, so importing
one layer never drags in the others, and there is exactly one binding for every
name (a re-export would make `monkeypatch.setattr` silently patch a copy that
the layer actually reading the value never sees).
"""
