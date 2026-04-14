# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Non-blocking MCP startup: `App::new_server_ready()` opens only the DB in Phase 1 (<50 ms), while `run_background_memory_init()` loads the ONNX model, runs bootstrap, and signals `memory_ready` in a background thread
- `warming_up` flag on `ironmem_status` response and early-return guard in `ironmem_search`, `ironmem_add_drawer`, and diary writes — callers can poll status until `warming_up: false` before issuing write-heavy workloads
- Embedder hot-swap: the serving `App` swaps in the real embedder (and triggers HNSW rebuild) the first time a tool call is made after the background thread signals ready
- `--ironmem-only` flag on the benchmark harness to run without a mempalace checkout
- `--debug-stderr` flag on the benchmark harness to redirect server stderr to `/tmp` log files
- Warmup timing tracked separately from connect latency in benchmark output
- p95 latencies reported alongside p50 in benchmark summary
- Human-readable storage sizes (KB / MB / GB) in benchmark output
- Shared diary persistence helper so hook-written session summaries and diary tool entries use the same append-only write path
- Runtime QA coverage for bootstrap races, malformed bootstrap state recovery, CLI `init -> mine -> serve -> hook` smoke flows, malformed stdio protocol handling, failed re-mine recovery, and migration corruption/idempotency scenarios
- `IRONMEM_EMBED_MODE=noop` for process-level tests and controlled local smoke runs without the ONNX model
- `IRONMEM_DISABLE_MIGRATION=1` to explicitly disable first-run mempalace migration
- Hidden-path mining coverage, including tests for default exclusion and explicit opt-in behavior

### Changed

- Search overfetch increased from 3x to 5x (minimum 30 candidates) so needle documents are not dropped when their embeddings are diluted by unrelated document context
- Benchmark harness sets `IRONMEM_AUTO_BOOTSTRAP=0` and `IRONMEM_DISABLE_MIGRATION=1` automatically so one-time bootstrap cost does not pollute latency measurements
- Benchmark storage measurement now forces a SQLite WAL `TRUNCATE` checkpoint before measuring disk size for a fair comparison with Chroma-backed backends
- Plugin launch scripts (`.claude-plugin/bin/ironmem-mcp.sh`, `.codex-plugin/bin/ironmem-mcp.sh`) check `~/.ironrace/bin/ironmem` first, then the repo release binary, then the debug binary
- Bootstrap no longer infers a workspace from `cwd`; explicit workspace roots are required for auto-mining
- `serve` now fails closed on bootstrap errors instead of starting with partial or skipped initialization
- Re-mining replaces a file's drawers transactionally after embeddings are computed, so transient failures do not delete previously indexed content
- Migration from ChromaDB now imports drawers and knowledge-graph data transactionally and no longer falls back to a home-directory KG when migrating from an explicit external store
- Hook session summaries now land in the same readable diary stream as normal diary writes
- Benchmark/docs wording now reflects current reality: `mine` and `hook` are implemented, and file-mining is excluded from the benchmark harness by design rather than because the feature is missing
- Search and graph comments now describe KG score adjustment and substring matching plainly instead of implying novelty or fuzzy matching
- Mining now skips hidden files and directories by default; set `IRONMEM_MINE_HIDDEN=1` to opt in to indexing dot-paths

### Fixed

- Stale `bootstrap.lock` files left by SIGTERM'd processes are now detected and auto-cleared on the next startup instead of blocking bootstrap indefinitely
- Sanitized `cwd` and `transcript_path` values before hook diary persistence to prevent path-shaped content injection into durable summaries
- Rejected system directory prefixes for mining and migration inputs, and canonicalized mining roots before traversal
- Removed `.env` from the mining allowlist to reduce accidental credential ingestion
- Added bounded SQLite busy retries during startup schema work
- Serialized env-var-mutating bootstrap tests to prevent `HOME` / migration detection races from importing a real mempalace store into test databases
- Encapsulated direct database connection access behind `Database` methods to keep graph and migration code on supported transaction boundaries

### Removed

- `properties` field from the `entities` table and `Entity` struct — the column was never populated with meaningful data, never queried, and never surfaced through any tool; `upsert_entity` now uses `ON CONFLICT DO NOTHING` since there is no mutable entity state left to update

## [0.1.0] - 2026-04-13

### Added

- MCP server (`ironmem serve`) with JSON-RPC 2.0 over stdio
- Semantic search via HNSW index (all-MiniLM-L6-v2 ONNX embeddings, 384-dim)
- Knowledge graph with temporal triples — add, query, invalidate, timeline
- Memory graph traversal — BFS, tunnel detection, graph stats
- Diary read/write with wing-scoped entries
- Drawer CRUD — add, delete, list wings/rooms, full taxonomy
- Incremental workspace mining (`ironmem mine`) with SHA-256 manifest cache
- ChromaDB/mempalace migration (`ironmem migrate --from <path>`)
- Auto-bootstrap on first `serve` or `hook` — migrate-or-init + initial mine; disable with `IRONMEM_AUTO_BOOTSTRAP=0`
- `IRONMEM_WORKSPACE_ROOT` to pin the auto-mine target without passing it on the command line
- `IRONMEM_MIGRATE_FROM` to point migration at a custom ChromaDB store path
- `IRONMEM_DB_PATH`, `IRONMEM_MODEL_DIR`, `IRONMEM_MCP_MODE` for runtime config overrides
- Hook support for Claude Code and Codex: `session-start`, `stop`, `precompact`
- Three MCP access modes: `trusted`, `read-only`, `restricted`
- Input sanitization and content length limits on all write paths
- WAL audit log with automatic 30-day pruning
- SHA-256 checksum verification on model download
- Plugin packaging for Claude Code (`.claude-plugin/`)
- Plugin packaging for Codex (`.codex-plugin/`)
- Memory protocol guidance returned from `ironmem_status` and surfaced in plugin `defaultPrompt`
- Integration tests: MCP protocol contract, plugin metadata validation, mining end-to-end
- GitHub Actions CI: fmt check, clippy, cargo test, plugin JSON validation
