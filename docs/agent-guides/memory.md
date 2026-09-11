# Shared memory and daemon transport

When the `ironmem` MCP server is available, Codex and Claude Code share `~/.ironrace-memory/memory.sqlite3` by default. Treat memory written by either harness as available to the other.

## Protocol

1. At session start, call `status` and inspect `readiness`.
2. Before answering about prior work, decisions, project history, people, or earlier sessions, use `search` or knowledge-graph tools.
3. After important progress or decisions, write a durable summary.
4. For mutable task or project state, use `add_drawer` with a stable `logical_key` so newer state replaces stale state.
5. Treat `collab-plans`, `collab-task-lists`, and `collab-checkpoints` as operational artifacts. Prefer concise durable summaries, and run `ironmem memory gc --dry-run` before any pruning.

Preferred tools: `status` for overview; `search` for recall; `kg_query` and `kg_stats` for structured facts; and `add_drawer`, diary tools, or other appropriate write tools for durable notes.

## Warmup

- Writes are safe during background warmup. `add_drawer`, diary writes, and `code_map_write` wait for readiness (up to `IRONMEM_WRITE_READINESS_TIMEOUT_SECS`, default 90 seconds); a successful response means the write landed.
- During warmup, `search` may return `{"warming_up": true, "results": []}`. Retry shortly; this is not “no matches.”
- Use `status.readiness`: `ready`, `warming_up`, or `failed`. Do not poll the `warming_up` boolean.
- `failed` is terminal until restart. Surface `readiness_error` and stop retrying; a failed-gate search reports an error.

## Shared daemon

`ironmem serve --listen` and `--connect` let attached harnesses share one application, database, and embedding model through a Unix socket. Bare `ironmem serve` remains the fallback. See [Shared Daemon Mode](../../README.md#shared-daemon-mode) for flags, `IRONMEM_DAEMON_IDLE_SECS`, `IRONMEM_NO_DAEMON`, and security notes. `ironmem doctor` reports reachability and proxy-command wiring.
