# Rename `ironrace-memory` → `ironmem` (Deferred Plan)

**Status:** deferred. Execute after `feat/collab-v1-bounded` merges to main.
**Branch name:** `refactor/rename-ironmem`, branched **off `main`** (not
off the collab branch — wait for that merge first).
**Estimated effort:** 60–90 minutes in one PR.

## Goals

1. Rename the Rust crate `ironrace-memory` → `ironmem`.
2. Rename the MCP server id from `ironrace-memory` → `ironmem`.
3. Drop the redundant `ironmem_` prefix from every MCP tool name (now
   that the server id is `ironmem`, the prefix is duplicated).
4. Update docs, CI, plugin manifests, and local config.

## Phase 1 — Crate + package rename (commit 1)

- `git mv crates/ironrace-memory crates/ironmem`.
- Workspace `Cargo.toml`: update member path.
- `crates/ironmem/Cargo.toml`: `name = "ironmem"`.
- Sed all Rust sources + tests: `ironrace_memory` → `ironmem`,
  `ironrace-memory` → `ironmem`.
- `.claude-plugin/plugin.json` + `.codex-plugin/plugin.json`:
  `"name": "ironmem"`.
- `.claude-plugin/.mcp.json` + launcher scripts: server id `ironmem`.
- `crates/ironmem/tests/plugin_metadata.rs`: expected name.
- Docs: `README.md`, `docs/CODEX.md`, `docs/COLLAB.md`,
  `CONTRIBUTING.md`, `IMPLEMENTATION_PLAN.md`, `AGENTS.md`.
- Scripts: `scripts/benchmark_*.py`, `check_versions.sh`,
  `check-versions.sh`, `mcp_smoke_test.py`.
- CI: `.github/workflows/{ci,release}.yml` — artifact names, cache keys.
- Migration SQL comment headers (cosmetic).

**Verify:** `cargo build --release && cargo test --all && cargo clippy --all-targets -- -D warnings && python3 scripts/mcp_smoke_test.py --binary ./target/release/ironmem`

## Phase 2 — Strip `ironmem_` prefix from tool names (commit 2)

Rename in `crates/ironmem/src/mcp/tools.rs`:

| Old | New |
|---|---|
| `ironmem_status` | `status` |
| `ironmem_search` | `search` |
| `ironmem_add_drawer` | `add_drawer` |
| `ironmem_delete_drawer` | `delete_drawer` |
| `ironmem_list_wings` | `list_wings` |
| `ironmem_list_rooms` | `list_rooms` |
| `ironmem_get_taxonomy` | `get_taxonomy` |
| `ironmem_kg_*` (6 tools) | `kg_*` |
| `ironmem_traverse` | `traverse` |
| `ironmem_find_tunnels` | `find_tunnels` |
| `ironmem_graph_stats` | `graph_stats` |
| `ironmem_diary_read` / `_write` | `diary_read` / `diary_write` |
| `ironmem_collab_*` (10 tools) | `collab_*` |

Same file: update `call_tool` match arms, `tool_known()`,
`tool_allowed_in_mode()`.

**Cross-file sweep:**

- `crates/ironmem/tests/mcp_protocol.rs`
- `crates/ironmem/tests/scenarios.rs`, `cli_smoke.rs`
- `docs/COLLAB.md`, `docs/CODEX.md`, `README.md`
- `scripts/mcp_smoke_test.py` (tool-list assertion)
- `~/.claude/commands/collab.md` —
  `mcp__ironrace-memory__ironmem_collab_*` → `mcp__ironmem__collab_*`
- `~/.codex/prompts/collab.md` — same prefix substitution
- `~/.claude/settings.local.json` — allowlist entries

**Verify:** full test suite + smoke test + start a live collab session.

## Phase 3 — Local user config (outside repo, after merge)

Solo-install user: safe to just overwrite.

- `~/.codex/config.toml`:
  `[mcp_servers.ironrace_memory]` → `[mcp_servers.ironmem]`.
- `~/.claude.json`: MCP server id in every project scope.
- Optional: rename data dir `~/.ironrace-memory/` → `~/.ironmem/` (or
  keep and set `IRONMEM_DB_PATH`).

## Things that do NOT change

- Binary name: `ironmem`.
- Install path: `~/.ironrace/bin/ironmem`.
- Claude hooks in `~/.claude/settings.json`: call binary directly, no
  MCP tool names referenced.
- Database schema (no data migration).

## Known risks

- **Tool-name assertions in CI smoke test** — most likely failure point;
  updated in Phase 2 plan.
- **`plugin_metadata.rs`** — fails loudly if `Cargo.toml` drift; good.
- **Allowlist drift** — `settings.local.json` with old tool names will
  prompt for permission on every new tool call until re-approved.
- **Published crate** — if ever published to crates.io, needs new publish.
