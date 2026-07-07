# ironmem for Claude Code

Persistent workspace memory for Claude Code using the local Rust `ironmem` binary.

## Behavior

- auto-migrates from `mempalace` on first use when a palace exists
- initializes a fresh local store otherwise
- mines the current workspace on first run
- incrementally updates memory on `Stop` and `PreCompact`
- bundles the collab skills used by the Claude/Codex handoff flow

## Memory protocol

The memory protocol is single-sourced from the `MEMORY_PROTOCOL` constant in `crates/ironmem/src/bootstrap.rs`. Stamp it into your rules file with the explicit, opt-in command (no hook runs it for you):

```bash
ironmem write-rules --target CLAUDE.md
```

The protocol tells Claude Code to use `add_drawer` with `logical_key` for
mutable current context and to treat collab plan/task/checkpoint drawers as
operational artifacts that can be reviewed with `ironmem memory gc --dry-run`
before any `--apply` pruning.

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

Existing bundled copies are updated on install; pass `--skip-skills` to leave
local skills, prompts, commands, and agents untouched.
