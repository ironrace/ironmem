# AGENTS.md

## Purpose

`ironrace-memory` is a Rust workspace for a local AI memory backend:

- `ironrace-core` — shared HNSW vector index
- `ironrace-embed` — ONNX sentence embeddings in pure Rust
- `ironrace-memory` — MCP server exposing semantic search plus a knowledge graph

## Shared Memory Protocol

When the `ironrace-memory` MCP server is available in the current harness, use it proactively so Codex and Claude Code share the same memory.

Default behavior:

- Codex and Claude Code read from and write to the same SQLite store by default: `~/.ironrace-memory/memory.sqlite3`
- Memory written in one harness should be treated as available to the other

Use the memory tools this way:

1. At session start, call `ironmem_status` to load the memory overview and check whether memory is still warming up.
2. Before answering questions about prior work, decisions, project history, people, or earlier sessions, call `ironmem_search` or the knowledge-graph tools first.
3. After important progress or decisions, write a durable summary back into memory.

Preferred tools:

- Overview: `ironmem_status`
- Recall: `ironmem_search`
- Structured facts: `ironmem_kg_query`, `ironmem_kg_stats`, related KG tools
- Durable notes: `ironmem_add_drawer`, diary tools, or other write tools that fit the context

## Warmup Rule

`ironmem serve` uses background warmup.

- If `ironmem_status` shows `warming_up: true`, avoid write-heavy memory actions until warmup completes.
- Poll `ironmem_status` and wait for `warming_up: false` before relying on embedding-dependent tools such as semantic search or drawer writes.

## Documentation Rules

- When behavior, setup, release flow, or public API changes, update the relevant docs in the same change.
- Keep `README.md`, `docs/CODEX.md`, `CONTRIBUTING.md`, plugin metadata, and workflow docs in sync.
- Documentation should be concise and direct; prefer concrete examples over vague guidance.

## Testing Rules

- Run the relevant Rust checks before considering work complete.
- For repo-wide changes, prefer:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  - `cargo test --workspace`
- If plugin metadata or release wiring changes, also run:
  - `bash scripts/check_versions.sh`
  - `python3 scripts/mcp_smoke_test.py --binary ./target/debug/ironmem`

## Security Rules

- Never commit secrets, API keys, or credentials.
- Any `unsafe` Rust code must include a `// SAFETY:` comment explaining why it is sound.
- Do not expose raw internal errors to external callers when a safer user-facing error is more appropriate.

## Git Workflow

- Use conventional commit prefixes such as `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `ci:`, and `chore:`.
- Keep commits focused on one logical change.
- PRs should target `main` and explain what changed and why.
