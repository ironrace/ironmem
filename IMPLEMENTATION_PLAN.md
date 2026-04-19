# ironmem Integration Plan

## Goal

Make `ironmem` usable as a practical memory backend for both Claude Code and Codex, with parity on the minimum ergonomics that `mempalace` already ships today.

## Current State

What already exists:

- MCP server over stdio
- Read/write memory tools
- Knowledge graph and graph traversal
- Access modes for trusted, read-only, and restricted use
- Passing Rust tests
- Direct `mempalace` migration from Chroma-backed stores
- Workspace mining with incremental updates
- Codex and Claude plugin wrappers
- Hook entrypoint for session-start, stop, and precompact

What is missing for real harness adoption:

- No integration smoke tests against real harness runtimes
- No release-quality packaging or installer flow beyond wrapper scripts
- Hook summaries are metadata-based, not transcript-aware semantic summaries
- No CI for plugin metadata, hooks, or benchmark smoke runs

## Phase 1: Usable Manual Codex Integration ✅ COMPLETE

Objective: make Codex usable without plugin packaging.

Deliverables:

- [x] Document manual Codex MCP setup
- [x] Document model bootstrap and database path configuration
- [x] Document current limitations: `mine` and `hook` are not ready
- [x] Add a benchmark harness for repeatable comparison vs `mempalace`

Acceptance criteria:

- [x] A user can build `ironmem`
- [x] A user can register it as a Codex MCP server
- [x] A user can successfully call `initialize`, `tools/list`, `status`, and `search`
- [x] A user can compare `ironmem` and `mempalace` with one script

## Phase 1.5: Install-Time Bootstrap and Migration ✅ COMPLETE

Objective: make installation self-initializing rather than manual.

Required install behavior:

- [x] On install, detect whether an existing `mempalace` installation or data directory exists
- [x] If `mempalace` data exists, migrate automatically into `ironmem`
- [x] If no prior `mempalace` data exists, initialize a fresh store automatically
- [x] Run an initial mine automatically after successful install/bootstrap
- [x] Record bootstrap state so the install path is idempotent

Recommended detection order:

- [x] explicit environment override
- [x] project-local plugin state
- [x] default `mempalace` home directory
- [x] known local palace paths from existing Claude or Codex plugin config

Recommended bootstrap flow:

1. [x] Resolve target store path
2. [x] Detect prior `mempalace` store
3. [x] If present, run migration
4. [x] If absent, initialize fresh SQLite store
5. [x] Ensure embedding model is ready
6. [x] Run initial mine against the configured project root
7. [x] Persist a bootstrap marker so reinstall does not repeat destructive work

Acceptance criteria:

- [x] Fresh install on a clean machine creates a usable store without manual commands
- [x] Install on a machine with `mempalace` migrates data automatically
- [x] Reinstall is idempotent and does not duplicate migrated or mined content
- [x] Failed migration falls back safely with a clear recovery path

## Phase 2: Codex Plugin Parity ✅ COMPLETE

Objective: make Codex setup one-step rather than manual.

Missing pieces:

- [x] Add `.codex-plugin/plugin.json`
- [x] Add `.codex-plugin/hooks.json`
- [x] Add `.codex-plugin/README.md`
- [x] Add hook wrapper script under `.codex-plugin/hooks/`
- [ ] Optionally add skills or command docs under `.codex-plugin/skills/` _(deferred — optional)_

Recommended plugin shape:

- [x] MCP server command should point at the built `ironmem` binary
- [x] Hooks should route `SessionStart`, `Stop`, and `PreCompact` into `ironmem hook ...`
- [x] Plugin docs should clearly distinguish current supported behavior, optional setup, and required model bootstrap

Acceptance criteria:

- [x] Codex detects the plugin automatically when run inside the repo
- [ ] `codex --plugins` shows `ironmem` _(not yet verified against a live Codex runtime)_
- [x] Hooks invoke a stable wrapper script rather than raw inline shell

## Phase 3: Claude Code Plugin Parity ✅ COMPLETE

Objective: package the same server for Claude Code with equivalent lifecycle behavior.

Missing pieces:

- [x] Add `.claude-plugin/plugin.json`
- [x] Add `.claude-plugin/.mcp.json` (equivalent MCP config)
- [x] Add `.claude-plugin/hooks/hooks.json`
- [x] Add hook shell scripts
- [x] Add Claude-specific install docs
- [x] Add install bootstrap logic or wrapper invoked during plugin install/first run

Acceptance criteria:

- [x] Manual Claude MCP registration works
- [x] Plugin packaging works for user-scope or project-scope install
- [x] Claude Code and Codex use the same underlying `ironmem hook` entrypoint
- [x] Claude install performs migration-or-init automatically on first run

## Phase 4: Implement Hook Behavior ✅ COMPLETE (with known limitations)

Objective: make lifecycle automation real rather than documented fiction.

Current gap:

- [x] `ironmem hook <name>` returns a not-implemented error → now fully routed

Required hook behaviors:

- [x] `session-start`
  - [x] initialize state directory
  - [x] complete install bootstrap if not already completed
  - [x] run initial mine if the workspace has not yet been indexed
  - [x] emit a short status payload or bootstrap recommendation
- [x] `precompact`
  - [x] save a concise diary summary or pending-session snapshot
  - [x] mine recently changed files using content-hash based detection
- [x] `stop`
  - [x] persist final session summary
  - [x] ingest changed files from the active workspace
  - [x] update bootstrap/session state markers

Design notes:

- [x] Keep hooks deterministic and fast
- [x] Treat hooks as best-effort, never fatal to the host harness
- [x] Log failures to stderr and state files, not stdout protocol output
- [x] Separate full initial mine from incremental updates
- [x] Incremental updates use a content hash file cache so repeated stops are cheap

Acceptance criteria:

- [x] All three lifecycle hooks run without crashing
- [x] Hooks are safe to call repeatedly
- [x] Hooks do not block the host CLI for long-running work
- [x] Initial mining runs automatically on first use
- [x] Subsequent hook runs only process changed content

**Known gap:** Hook summaries are metadata-based (cwd, session_id, hook name), not transcript-semantic summaries derived from LLM-generated content. Rich summary extraction from `transcript_path` is not yet implemented.

## Phase 5: Implement File Mining ✅ COMPLETE (with known limitations)

Objective: make the CLI useful without requiring MCP write loops.

Current gap:

- [x] `mine_directory()` is a stub → now fully implemented

Required behavior:

- [x] Walk a directory with ignore support (`.gitignore`, `.ignore`, skip build dirs)
- [x] Chunk files deterministically (with overlap)
- [x] Infer `wing` and `room` from path or configuration
- [x] Upsert drawers idempotently (delete + reinsert on hash change)
- [ ] Optionally extract lightweight KG facts _(not yet implemented)_
- [x] Support incremental re-mining based on changed files only (manifest with SHA-256 hashes)

Recommended scope:

- [x] Support plain text, Markdown, source files, JSON, YAML, SQL, shell scripts, .env
- [x] Defer rich binary formats
- [x] Reuse the existing sanitization and deterministic drawer ID rules
- [x] Persist a mining manifest with file hashes and last successful run metadata

Acceptance criteria:

- [x] `ironmem mine <path>` ingests a real repo
- [x] Repeat runs are idempotent
- [x] Ingested content is searchable immediately afterward
- [x] Incremental mine is materially faster than full re-mine on unchanged repos

## Phase 6: Integration and Regression Tests ✅ COMPLETE

Objective: prevent drift between documented behavior and real harness behavior.

Tests added:

- [x] MCP handshake smoke test — `initialize` returns capabilities, error codes correct
- [x] Tool contract tests — `tools/list` contents, access mode filtering, `status` shape
- [x] Hook command unit tests (parses payload, builds response)
- [x] Mining end-to-end test on a fixture repo (ingest, idempotency, change detection, deletion)
- [x] Codex plugin metadata validation (`plugin.json`, `hooks.json` required fields)
- [x] Claude plugin metadata validation (`plugin.json`, `.mcp.json` required fields)

Acceptance criteria:

- [x] CI covers MCP startup and at least one real tool call
- [x] Plugin metadata changes are exercised in CI

**Implementation notes:**
- Added `Embedder::new_noop()` (enum-based, no ONNX model required) to enable model-free testing
- Added `App::open_for_test()` and `App::open_for_test_with_mode()` constructors
- Added `src/lib.rs` to expose modules to integration tests in `tests/`
- Made `dispatch` public in `server.rs` for direct protocol testing
- 78 total tests passing (59 unit + 6 MCP protocol + 4 mining + 5 plugin metadata + 4 noop embedder)

## Phase 7: Packaging and Release Readiness ✅ COMPLETE

Objective: reduce friction for non-authors.

Recommended additions:

- [x] Top-level README with quickstart
- [x] Release build instructions (`cargo build -p ironmem --bin ironmem`)
- [x] Versioning policy (semver, canonical in `Cargo.toml`, enforced by CI)
- [x] Changelog (`CHANGELOG.md`, Keep-a-Changelog format)
- [x] Installer/bootstrap wrapper shared by both Codex and Claude plugins
- [x] GitHub Actions (`.github/workflows/ci.yml`):
  - [x] Rust tests (`cargo test --workspace`)
  - [x] Format check + clippy (`-D warnings`)
  - [x] Binary build (`cargo build -p ironmem --bin ironmem`)
  - [x] Plugin metadata JSON validation + version consistency check
  - [ ] Smoke benchmarks _(deferred — requires real harness runtime)_

## Priority Order

1. ~~Manual Codex docs~~ ✅
2. ~~Benchmark harness~~ ✅
3. ~~Install-time bootstrap and migration~~ ✅
4. ~~Hook implementation~~ ✅
5. ~~File mining with incremental updates~~ ✅
6. ~~Codex plugin packaging~~ ✅
7. ~~Claude plugin packaging~~ ✅
8. CI and release workflow ← **next priority**

## Why This Order

- Codex can already use the server manually, so documentation delivers immediate value
- Benchmarking now gives a baseline before feature work changes behavior
- Automatic migration and bootstrap determine whether users actually adopt it
- Hooks and mining are the biggest functional gaps blocking real daily use
- Plugin packaging should come after the underlying commands are reliable

## Harness-Level Memory Protocol ✅ COMPLETE

Objective: make the host agent actually use the memory system instead of merely exposing tools.

Required behavior:

- [x] Add a short memory protocol line to the Codex and Claude plugin surfaces
- [x] Instruct the harness to consult memory before answering questions about prior work, decisions, people, projects, or historical context
- [x] Instruct the harness to write back important session outcomes at stop/precompact boundaries

Recommended implementation:

- [x] Include protocol text in plugin docs and default prompt metadata (`defaultPrompt` in `.codex-plugin/plugin.json`)
- [x] Return a concise protocol reminder from `status` (`MEMORY_PROTOCOL` constant in `bootstrap.rs`, surfaced via `tools.rs`)
- [x] Keep the prompt short and operational

Acceptance criteria:

- [x] A new Codex or Claude session gets explicit guidance to use memory
- [x] The guidance is short enough not to bloat prompt budget
- [x] The protocol is consistent across both harnesses
