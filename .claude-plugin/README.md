# ironmem for Claude Code

Persistent workspace memory for Claude Code using the local Rust `ironmem` binary.

## Behavior

- auto-migrates from `mempalace` on first use when a palace exists
- initializes a fresh local store otherwise
- mines the current workspace on first run
- incrementally updates memory on `Stop` and `PreCompact`
- bundles the collab skills used by the Claude/Codex handoff flow

## Memory protocol

Before answering questions about prior work, decisions, project history, or people, check `search` or the KG tools first. After important progress or decisions, write durable summaries back into memory.

## Bundled skills

`scripts/install-ironmem.sh` installs these Claude Code skills into `$CLAUDE_HOME/skills`:

- `writing-plans`
- `subagent-driven-development`
- `finishing-a-development-branch`
- `executing-plans`
- `using-git-worktrees`
- `using-superpowers`
- `requesting-code-review`
- `test-driven-development`

It also installs the `code-reviewer` agent into `$CLAUDE_HOME/agents`.
