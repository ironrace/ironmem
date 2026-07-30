# ironmem for Claude Code

Persistent workspace memory for Claude Code using the local Rust `ironmem` binary.

## Behavior

- auto-migrates from `mempalace` on first use when a palace exists
- initializes a fresh local store otherwise
- mines the current workspace on first run
- incrementally updates memory on `Stop` and `PreCompact`
- bundles the collab skills used by the Claude/Codex handoff flow

## Memory protocol

The memory protocol uses a canonical-source model:

- **Model A:** `MEMORY_PROTOCOL` (in `crates/ironmem/src/bootstrap.rs`) is
  stamped into `AGENTS.md` as the canonical managed block.
- **Model B:** dependent files are derived from canonical rules by strategy. Claude
  uses `Import`, so `CLAUDE.md` contains a managed `@AGENTS.md` line.

Stamp with the explicit, opt-in command (no hook runs it for you):

```bash
ironmem write-rules --harness claude
```

The protocol tells Claude Code to use `add_drawer` with `logical_key` for
mutable current context and to treat collab plan/task/checkpoint drawers as
operational artifacts that can be reviewed with `ironmem memory gc --dry-run`
before any `--apply` pruning.

`--harness claude` writes both files:

```markdown
<!-- AGENTS.md (canonical source) -->
<!-- BEGIN IRONMEM MEMORY PROTOCOL -->
<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->
...
Before answering questions about prior work, decisions, project history, or people, check search or KG tools first. Write important durable decisions back to memory. For mutable current task/project context, use add_drawer with logical_key so the latest state overwrites stale copies instead of accumulating forever. Treat collab-plans, collab-task-lists, and collab-checkpoints as operational artifacts; prefer compact durable summaries for long-term recall and prune stale operational drawers with ironmem memory gc --dry-run before --apply.
<!-- END IRONMEM MEMORY PROTOCOL -->

<!-- CLAUDE.md (import target, preserved-user content around block) -->
<!-- BEGIN IRONMEM MEMORY PROTOCOL -->
<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->
@AGENTS.md
<!-- END IRONMEM MEMORY PROTOCOL -->
```

## Bundled skills

`scripts/install-ironmem.sh` installs these Claude Code skills into `$CLAUDE_HOME/skills`:

- `iron-spec`
- `iron-plan`
- `iron-build`
- `iron-tdd`

It also installs the `code-reviewer` agent into `$CLAUDE_HOME/agents`.

Existing bundled copies are updated on install; pass `--skip-skills` to leave
local skills, prompts, commands, and agents untouched.
