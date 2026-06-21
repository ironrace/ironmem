# ironmem for Codex

Persistent workspace memory for Codex using the local Rust `ironmem` binary.

## What it does

- starts the MCP server
- auto-detects and migrates from `mempalace` on first use when available
- initializes a fresh store if no previous memory exists
- mines the current workspace on first run
- re-mines incrementally on `Stop` and `PreCompact`
- installs the `/collab` command shim used by Codex
- bundles the collab skills used by the Claude/Codex handoff flow

## Memory protocol

The memory protocol is single-sourced from the `MEMORY_PROTOCOL` constant in `crates/ironmem/src/bootstrap.rs`. Stamp it into your rules file with the explicit, opt-in command (no hook runs it for you):

```bash
ironmem write-rules --target AGENTS.md
```

## Bundled skills

`scripts/install-ironmem.sh` installs these Codex skills into `$CODEX_HOME/skills`:

- `writing-plans`
- `subagent-driven-development`
- `finishing-a-development-branch`
- `executing-plans`
- `using-git-worktrees`
- `using-superpowers`
- `requesting-code-review`
- `test-driven-development`
- `pr-review-toolkit`

Existing bundled copies are updated on install; pass `--skip-skills` to leave
local skills, commands, and prompts untouched.

## Notes

- The plugin wrapper builds `ironmem` automatically if the binary does not exist yet.
- The workspace root is inferred from `git rev-parse --show-toplevel` and falls back to the current directory.
