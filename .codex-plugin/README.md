# ironmem for Codex

Persistent workspace memory for Codex using the local Rust `ironmem` binary.

## What it does

- starts the MCP server
- auto-detects and migrates from `mempalace` on first use when available
- initializes a fresh store if no previous memory exists
- mines the current workspace on first run
- re-mines incrementally on `Stop` and `PreCompact`
- bundles the Codex `/collab` command, protocol prompts, and skills used by
  the Claude/Codex handoff flow

## Memory protocol

The memory protocol is single-sourced from the `MEMORY_PROTOCOL` constant in `crates/ironmem/src/bootstrap.rs`. Stamp it into your rules file with the explicit, opt-in command (no hook runs it for you):

```bash
ironmem write-rules --target AGENTS.md
```

The protocol tells Codex to use `add_drawer` with `logical_key` for mutable
current context and to treat collab plan/task/checkpoint drawers as operational
artifacts that can be reviewed with `ironmem memory gc --dry-run` before any
`--apply` pruning.

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

Existing identical files are skipped. Packaged baselines are stored under each
target root's hidden `.ironmem-bases/` directory so later installs can
three-way merge packaged updates into locally edited files. If there is no
baseline, a symlink target, or a merge conflict, the local file is left
unchanged and the packaged update is written next to it as `*.ironmem-packaged`
(conflict drafts use `*.ironmem-merge-conflict`). Pass `--skip-skills` to leave
local skills, prompts, and commands untouched.

## Bundled commands and prompts

`scripts/install-ironmem.sh` installs:

- `$CODEX_HOME/commands/collab.md` — the interactive Codex `/collab` slash command.
- `$CODEX_HOME/prompts/collab.md` — the full Codex one-turn collab protocol.
- `$CODEX_HOME/prompts/collab-batch-impl.md` — the slim codex-implementer batch prompt.

The command loads the protocol prompt and substitutes the slash-command
arguments. Claude's background dispatcher still passes the resolved protocol
prompt directly when it drives Codex-owned turns.

## Notes

- The plugin wrapper builds `ironmem` automatically if the binary does not exist yet.
- The workspace root is inferred from `git rev-parse --show-toplevel` and falls back to the current directory.
