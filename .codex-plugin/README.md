# ironmem for Codex

Persistent workspace memory for Codex using the local Rust `ironmem` binary.

## What it does

- starts the MCP server
- auto-detects and migrates from `mempalace` on first use when available
- initializes a fresh store if no previous memory exists
- mines the current workspace on first run
- re-mines incrementally on `Stop` and `PreCompact`
- bundles the collab skills used by the Claude/Codex handoff flow

## Memory protocol

Before answering questions about prior work, decisions, project history, or people, call `search` or the knowledge-graph tools first. After important progress or decisions, write durable summaries back into memory.

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

## Notes

- The plugin wrapper builds `ironmem` automatically if the binary does not exist yet.
- The workspace root is inferred from `git rev-parse --show-toplevel` and falls back to the current directory.
