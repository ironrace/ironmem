---
description: Join or start an IronMEM bounded Claude/Codex collab session from Codex.
argument-hint: "start <task> | join [--implementer=claude|codex] <session_id>"
---

# /collab

<!-- DERIVED FROM docs/COLLAB.md — this command is the Codex slash-command
shim for .codex-plugin/prompts/collab.md. Keep it in lockstep with
docs/COLLAB.md and .claude-plugin/commands/collab.md when the command surface
changes. -->

The user invoked `/collab` with arguments:

```text
$ARGUMENTS
```

You are Codex. This is the Codex slash-command entrypoint for the real
IronMEM collab protocol. It must behave like Claude's `/collab` command in
style and rigor, but from the Codex role: one invocation handles one
Codex-owned action and exits.

Before taking action, locate the full Codex protocol prompt at the first
existing path:

1. `$CODEX_HOME/prompts/collab.md`
2. `~/.codex/prompts/collab.md`
3. `.codex-plugin/prompts/collab.md` under the current repository
4. `$CODEX_PLUGIN_ROOT/prompts/collab.md`

Read that file completely. Follow it as the authoritative IronMEM collab
protocol, substituting the arguments above wherever the prompt says
`$ARGUMENTS`.

**Sandbox note (informational — this file is not the background-dispatch
entrypoint):** this INTERACTIVE shim is what a human runs; it never itself
launches another `codex exec` process, and Claude's background dispatch
does not go through this file either — it substitutes `$ARGUMENTS` directly
into `.codex-plugin/prompts/collab.md` or `collab-batch-impl.md` (see
`.claude-plugin/commands/collab.md`'s "Codex handoff — background `codex
exec`" step (b)). Either way, when you are running as a Codex process
dispatched for this protocol, your sandbox may include one extra writable
root beyond the checkout: the repo's common git directory, added via
`--add-dir` so `git commit`/`git push` succeed even from inside a linked
worktree (a linked worktree's `.git` metadata lives outside the worktree
directory itself). This does not loosen the sandbox mode itself — it is one
added root, nothing more. See `.claude-plugin/commands/collab.md` step (d)
for the exact resolution and rationale.

If no protocol prompt exists, report that the IronMEM Codex collab prompt is
not installed and ask the user to run `scripts/install-ironmem.sh` from the
IronMEM repo.

If the `mcp__ironmem__collab_*` tools are not directly callable in the current
session, use tool discovery for `ironmem collab` before proceeding.

Do not summarize the prompt back to the user. Proceed with the requested
`start` or `join` action using the IronMEM collab tools.
