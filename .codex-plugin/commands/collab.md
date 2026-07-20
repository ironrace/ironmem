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
dispatched for this protocol, you run **unsandboxed** by explicit choice:
the dispatcher launches you with `-s danger-full-access`. You are dispatched
by the user, on the user's own machine, against the user's own repo, so the
sandbox adds no trust boundary — and it broke the protocol twice. A linked
worktree's git metadata and the shared object/ref database live outside any
workspace-scoped root (so `git commit` was denied), and sandbox denials are
not limited to the filesystem (Unix domain socket creation was denied under
workspace-write, failing `cargo test --workspace`'s daemon/doctor tests with
"Operation not permitted"). No set of extra writable roots can grant that.
See `.claude-plugin/commands/collab.md` step (e) for the launch lines and
the full rationale.

If no protocol prompt exists, report that the IronMEM Codex collab prompt is
not installed and ask the user to run `scripts/install-ironmem.sh` from the
IronMEM repo.

If the `mcp__ironmem__collab_*` tools are not directly callable in the current
session, use tool discovery for `ironmem collab` before proceeding.

Do not summarize the prompt back to the user. Proceed with the requested
`start` or `join` action using the IronMEM collab tools.
