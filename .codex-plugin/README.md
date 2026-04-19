# ironmem for Codex

Persistent workspace memory for Codex using the local Rust `ironmem` binary.

## What it does

- starts the MCP server
- auto-detects and migrates from `mempalace` on first use when available
- initializes a fresh store if no previous memory exists
- mines the current workspace on first run
- re-mines incrementally on `Stop` and `PreCompact`

## Memory protocol

Before answering questions about prior work, decisions, project history, or people, call `search` or the knowledge-graph tools first. After important progress or decisions, write durable summaries back into memory.

## Notes

- The plugin wrapper builds `ironmem` automatically if the binary does not exist yet.
- The workspace root is inferred from `git rev-parse --show-toplevel` and falls back to the current directory.
