# ironrace-memory

[![CI](https://github.com/ironrace/ironrace-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/ironrace/ironrace-memory/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ironrace-memory.svg)](https://crates.io/crates/ironrace-memory)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

`ironrace-memory` is a Rust workspace for a local AI memory backend:

- `ironrace-core`: shared HNSW vector index
- `ironrace-embed`: ONNX sentence embeddings in pure Rust
- `ironrace-memory`: MCP server exposing semantic search plus a knowledge graph

Codex and Claude Code plugin packaging is included. See [docs/CODEX.md](docs/CODEX.md) for setup instructions.

Key docs:

- [Contributing Guide](CONTRIBUTING.md)
- [Cross-Harness Implementation Plan](IMPLEMENTATION_PLAN.md)
- [Codex Guide](docs/CODEX.md)
- [Collab Guide](docs/COLLAB.md)

## Contributor Hook

This repo includes tracked Git hooks for local commits and pushes.

Enable it once per clone:

```bash
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push
```

The hooks run:

- `pre-commit`: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `pre-push`: `cargo test --workspace`

## Quickstart: Install and Run in 60 Seconds

Fastest path from source today:

```bash
git clone https://github.com/ironrace/ironrace-memory.git
cd ironrace-memory
cargo build --release -p ironrace-memory --bin ironmem
./target/release/ironmem setup
```

Start the MCP server in trusted mode (required for write tools):

```bash
IRONMEM_MCP_MODE=trusted ./target/release/ironmem serve
```

Smoke-test the live stdio server without downloading the model:

```bash
python3 scripts/mcp_smoke_test.py --binary ./target/release/ironmem
```

Add it to Codex:

```toml
[mcp_servers.ironrace_memory]
command = "/absolute/path/to/ironmem"
args = ["serve"]

[mcp_servers.ironrace_memory.env]
IRONMEM_MCP_MODE = "trusted"
IRONMEM_DB_PATH = "~/.ironrace-memory/memory.sqlite3"
```

Tagged releases upload prebuilt macOS and Linux binaries automatically. Until the first tagged release is published, building from source is the supported install path.

## Current Status

- MCP server works over stdio with non-blocking startup (responds to `initialize` in <25 ms)
- Embedding and bootstrap run in a background thread; `ironmem_status` returns `warming_up: true` until ready
- Search, taxonomy, graph, diary, and knowledge-graph tools exist
- Automatic bootstrap runs on first server or hook start
- Direct migration from `mempalace` Chroma stores is implemented
- Workspace mining and incremental re-mining are implemented
- Codex and Claude Code plugin packaging is included
- `~/.ironrace/bin/ironmem` is the preferred installed binary location; plugin launch scripts check there first
- Bounded Claude↔Codex planning protocol (v1) is available via the `ironmem_collab_*` MCP tools, including long-poll `wait_my_turn` for autonomous operation — see [docs/COLLAB.md](docs/COLLAB.md)

## Shared Memory Across Harnesses

Codex and Claude Code read from and write to the **same database by default** (`~/.ironrace-memory/memory.sqlite3`). Memory written in a Claude session is immediately visible in Codex, and vice versa — there is one unified store.

The DB is updated automatically as you work:

- **Session start** — bootstrap runs if this is the first time; the workspace is mined if it hasn't been indexed yet
- **Stop / PreCompact** — changed files are detected via SHA-256 manifest and re-mined incrementally; a session summary is appended to the diary
- **Later sessions** — only files whose content hash changed since the last hook run are re-embedded, so updates are fast

SQLite WAL mode handles concurrent access safely when both harnesses are running at the same time.

To give a harness its own isolated store, set `IRONMEM_DB_PATH` in its plugin config:

```toml
# ~/.codex/config.toml — Codex-only store
[mcp_servers.ironrace_memory.env]
IRONMEM_DB_PATH = "~/.ironrace-memory/codex.sqlite3"
```

## Startup Behavior

`ironmem serve` uses a two-phase init so the harness is never left waiting at startup:

| Phase | What happens | Typical time |
|-------|-------------|--------------|
| Phase 1 | DB open + schema migration | ~50 ms |
| Phase 2 | ONNX model load + auto-bootstrap + mine (background thread) | 5–120 s |

Embedding-dependent tools (`ironmem_search`, `ironmem_add_drawer`, diary writes) return `{"warming_up": true}` until Phase 2 completes. Poll `ironmem_status` and check `warming_up: false` before issuing write-heavy workloads.

## Benchmarking

Compare against a local `mempalace` checkout:

```bash
# Full comparison (requires ~/git-repos/mempalace)
python3 scripts/benchmark_vs_mempalace.py \
  --documents 100 \
  --queries 20 \
  --runs 2 \
  --output-json /tmp/ironmem-vs-mempalace.json

# ironrace-memory only (no mempalace required)
python3 scripts/benchmark_vs_mempalace.py \
  --ironmem-only \
  --documents 100 \
  --queries 20 \
  --runs 3

# Capture server logs for debugging
python3 scripts/benchmark_vs_mempalace.py --ironmem-only --debug-stderr
```

The harness measures startup latency (connect only), warmup time (model load + bootstrap), add/search/delete/status/taxonomy latency (p50 and p95), search hit rate, and post-WAL-checkpoint storage size. File mining is excluded — the benchmark targets common MCP tool surfaces only.

Key benchmark flags:

| Flag | Description |
|------|-------------|
| `--documents N` | Synthetic documents to ingest (default: 100) |
| `--queries N` | Searches per run (default: 20) |
| `--runs N` | Fresh runs per backend (default: 1) |
| `--seed N` | Dataset seed for reproducibility (default: 42) |
| `--ironmem-only` | Skip mempalace; useful without the Python stack |
| `--debug-stderr` | Write server stderr to `/tmp/ironmem-*-stderr-*.log` |
| `--output-json PATH` | Write machine-readable results to a JSON file |
| `--keep-temp` | Keep the temporary benchmark workspace for inspection |

### Benchmark Notes

- `IRONMEM_AUTO_BOOTSTRAP=0` is set automatically by the harness so one-time bootstrap cost does not pollute latency measurements
- Warmup time (model load) is tracked separately from connect latency
- Storage is measured after a SQLite WAL `TRUNCATE` checkpoint for a fair comparison
- Search uses 5x overfetch (minimum 30 candidates) to maintain recall when needle documents are diluted by unrelated context

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The canonical version is in `crates/ironrace-memory/Cargo.toml`. Plugin JSON files (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`) must match this version — enforced by CI. See [CHANGELOG.md](CHANGELOG.md) for release history.
