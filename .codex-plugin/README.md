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

The memory protocol is single-sourced from the `MEMORY_PROTOCOL` constant in
`crates/ironmem/src/bootstrap.rs` and is written as the canonical managed block in
`AGENTS.md`:

```bash
ironmem write-rules --harness codex
```

The protocol tells Codex to use `add_drawer` with `logical_key` for mutable
current context and to treat collab plan/task/checkpoint drawers as operational
artifacts that can be reviewed with `ironmem memory gc --dry-run` before any
`--apply` pruning.

Result shape:

```markdown
<!-- AGENTS.md -->
... user content ...
<!-- BEGIN IRONMEM MEMORY PROTOCOL -->
<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->
Before answering questions about prior work, decisions, project history, or people, check search or KG tools first. Write important durable decisions back to memory. For mutable current task/project context, use add_drawer with logical_key so the latest state overwrites stale copies instead of accumulating forever. Treat collab-plans, collab-task-lists, and collab-checkpoints as operational artifacts; prefer compact durable summaries for long-term recall and prune stale operational drawers with ironmem memory gc --dry-run before --apply.
<!-- END IRONMEM MEMORY PROTOCOL -->
... user content ...
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

## Default Codex routing

The collab protocol passes model and reasoning overrides explicitly:

- implementation controller/workers: `gpt-5.6-luna` at `max`
- exploration, docs, and mechanical work: `gpt-5.6-luna` at `medium`
- planning and normal review: `gpt-5.6-terra` at `high`
- architecture/security escalation: `gpt-5.6-sol` at `high`

Sol is an escalation tier, not the routine default. Installing the plugin does
not modify personal `$CODEX_HOME/config.toml` agent roles; the protocol carries
its own defaults so behavior is consistent across callers.

## Notes

- The plugin wrapper builds `ironmem` automatically if the binary does not exist yet.
- The workspace root is inferred from `git rev-parse --show-toplevel` and falls back to the current directory.
