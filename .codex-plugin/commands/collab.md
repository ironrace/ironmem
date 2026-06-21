---
description: Join or start an IronMEM bounded Claude/Codex collab session
argument-hint: "start <task> | join [--implementer=claude|codex] <session_id>"
---

# /collab

The user invoked `/collab` with arguments:

```text
$ARGUMENTS
```

Read `~/.codex/prompts/collab.md` completely before taking
action. Follow that prompt as the authoritative IronMEM collab protocol,
substituting the arguments above wherever the prompt says `$ARGUMENTS`.

If the `mcp__ironmem__collab_*` tools are not directly callable in the current
session, use tool discovery for `ironmem collab` before proceeding.

Do not summarize the prompt back to the user. Proceed with the requested
`start` or `join` action using the IronMEM collab tools.
