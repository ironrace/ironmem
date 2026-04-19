# ironmem for Claude Code

Persistent workspace memory for Claude Code using the local Rust `ironmem` binary.

## Behavior

- auto-migrates from `mempalace` on first use when a palace exists
- initializes a fresh local store otherwise
- mines the current workspace on first run
- incrementally updates memory on `Stop` and `PreCompact`

## Memory protocol

Before answering questions about prior work, decisions, project history, or people, check `search` or the KG tools first. After important progress or decisions, write durable summaries back into memory.
