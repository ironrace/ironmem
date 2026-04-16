# Codex Guide

## Purpose

This guide explains how to use `ironrace-memory` with Codex today, what is still missing, and how to compare it against `mempalace`.

## Current Support Level

What works now:

- Running `ironmem` as an MCP server over stdio with non-blocking startup (<25 ms to first response)
- Read and write MCP tools
- Semantic search
- Knowledge graph tools
- Restricted vs trusted access modes
- `mine` for workspace ingestion with incremental updates
- `hook` for session-start, stop, and precompact
- Codex plugin packaging
- Automatic migrate-or-init bootstrap on first use
- Stale `bootstrap.lock` files from crashed processes are auto-cleared on next startup

What hooks currently do on `stop` / `precompact`:

- **Transcript review capture** — assistant messages in the transcript are scanned in reverse
  chronological order for code-review-like content (severity labels, file references, decision
  keywords). The most recent review-like assistant message is stored as a drawer in the
  `reviews/` wing so it can be recalled in future sessions.
- **Metadata diary entry** — a structured summary line is written to the diary recording the hook
  name, harness, session ID, working directory, and transcript path, plus the review room if a
  review was captured.
- **Incremental re-mine** — workspace files changed since the last hook run are re-embedded.

What does not work yet:

- There is still no standalone installer command; installs are source-build or release-binary based
- Hook behavior does not yet build a rich LLM-written session summary from transcript content

## Build

From the repo root:

```bash
cargo build -p ironrace-memory --bin ironmem
./target/debug/ironmem setup
```

`setup` prepares the embedding model under the default model cache. On a fresh machine it may download the model.

## Git Pre-Commit Hook

This repo includes tracked Git hooks so Codex, Claude Code, and manual terminal workflows all hit the same local gates.

Enable it once per clone:

```bash
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push
```

The hooks run:

- `pre-commit`: `cargo fmt --all -- --check`
- `pre-commit`: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `pre-push`: `cargo test --workspace`

## Manual Codex MCP Setup

Add a server entry to your Codex MCP config.

Example `~/.codex/config.toml` fragment:

```toml
[mcp_servers.ironrace_memory]
command = "/Users/jeffreycrum/git-repos/ironrace-memory/target/debug/ironmem"
args = ["serve"]

[mcp_servers.ironrace_memory.env]
IRONMEM_MCP_MODE = "trusted"
IRONMEM_DB_PATH = "/Users/jeffreycrum/.ironrace-memory/memory.sqlite3"
```

If you want a project-local store instead of the default home-directory location, point `IRONMEM_DB_PATH` at a repo-local path.

## Manual Validation

After registering the MCP server, validate the basics:

1. Start Codex and confirm the server appears in MCP listings.
2. Call `ironmem_status`.
3. Add a small drawer with `ironmem_add_drawer`.
4. Search for it with `ironmem_search`.

## Shared Memory Across Harnesses

Codex and Claude Code share the **same database by default** (`~/.ironrace-memory/memory.sqlite3`). Memory written during a Claude session is immediately visible in Codex, and vice versa.

The database is kept up to date automatically through hooks:

| Hook | What happens |
|------|-------------|
| `session-start` | Bootstrap if first run; initial mine if workspace not yet indexed |
| `stop` | Persist session summary to diary; re-mine files changed since last hook run |
| `precompact` | Snapshot pending session context; re-mine changed files |

Incremental re-mining uses a SHA-256 manifest so only files whose content changed are re-embedded. Repeat hook runs on unchanged workspaces are fast.

SQLite WAL mode allows both harnesses to access the store concurrently without locking conflicts.

**Isolation:** To give a harness its own store, set `IRONMEM_DB_PATH` in its plugin config:

```toml
# Codex-only store
[mcp_servers.ironrace_memory.env]
IRONMEM_DB_PATH = "~/.ironrace-memory/codex.sqlite3"
```

```json
// Claude Code-only store — in .claude-plugin/.mcp.json env block
"IRONMEM_DB_PATH": "/Users/you/.ironrace-memory/claude.sqlite3"
```

## Startup Behavior

`ironmem serve` uses a two-phase init so the harness is never left waiting at startup:

| Phase | What happens | Typical time |
|-------|-------------|--------------|
| Phase 1 | DB open + schema migration | ~50 ms |
| Phase 2 | ONNX model load + auto-bootstrap + mine (background thread) | 5–120 s |

Embedding-dependent tools (`ironmem_search`, `ironmem_add_drawer`, diary writes) return `{"warming_up": true}` until Phase 2 completes. The benchmark harness polls `ironmem_status` until `warming_up: false` before starting measurements.

```json
// ironmem_status response during warmup
{"warming_up": true, "total_drawers": 0, ...}

// ironmem_status response once ready
{"warming_up": false, "total_drawers": 42, ...}
```

## Operational Notes

- The binary default is `read-only` — running `ironmem serve` without setting `IRONMEM_MCP_MODE` disables all write tools. The plugin wrapper scripts default to `trusted` so plugin users are unaffected.
- `IRONMEM_MCP_MODE=trusted` enables writes (required for normal plugin use).
- `IRONMEM_MCP_MODE=read-only` disables write tools (binary default).
- `IRONMEM_MCP_MODE=restricted` disables writes and redacts sensitive returned content.
- Mining skips hidden files and directories by default. Set `IRONMEM_MINE_HIDDEN=1` only when you explicitly want dot-paths indexed.
- `IRONMEM_EMBED_MODE=noop` disables the ONNX embedder entirely (useful for process-level tests or smoke runs without the model).
- `IRONMEM_AUTO_BOOTSTRAP=0` disables the automatic bootstrap on `serve` start.
- `IRONMEM_DISABLE_MIGRATION=1` disables the first-run mempalace migration.

## Codex Packaging Gap

`ironrace-memory` now ships a `.codex-plugin/` directory with:

- `plugin.json`
- `hooks.json`
- wrapper scripts for the MCP server and hooks
- Codex-specific README content

The hook wrapper delegates to:

```bash
ironmem hook session-start --harness codex
ironmem hook stop --harness codex
ironmem hook precompact --harness codex
```

## Install-Time Migration and Bootstrap

Current behavior:

- If the user already has `mempalace`, installation detects that state and migrates automatically
- If the user does not have `mempalace`, installation initializes a fresh store automatically
- The embedding model is prepared automatically
- The active workspace is mined automatically on first use when the plugin wrapper can infer a workspace root
- Later hook runs update memory incrementally rather than re-mining everything

## Continuous Updates

Current behavior for Codex:

- first run: bootstrap, migrate-or-init, initial mine
- `PreCompact`: save summary and ingest changed files
- `Stop`: save durable summary and ingest changed files
- later sessions: query memory first when historical context matters

## Memory Usage Guidance

Codex should get a short protocol reminding it to use memory proactively.

Recommended text:

> Before answering questions about prior work, decisions, project history, or people, check `ironmem_search` or the KG tools first. After important progress or decisions, write durable summaries back into memory.

Best places to inject this:

- `.codex-plugin/plugin.json` default prompt metadata
- `ironmem_status` response
- Codex-facing README/setup docs

## Benchmarking Against MemPalace

This repo includes a benchmark harness at `scripts/benchmark_vs_mempalace.py`.

```bash
# Full comparison (requires ~/git-repos/mempalace)
python3 scripts/benchmark_vs_mempalace.py \
  --documents 100 \
  --queries 15 \
  --runs 2 \
  --output-json /tmp/ironmem-vs-mempalace.json

# ironrace-memory only (no mempalace required)
python3 scripts/benchmark_vs_mempalace.py --ironmem-only --documents 100 --queries 20 --runs 3

# Capture server logs for debugging startup issues
python3 scripts/benchmark_vs_mempalace.py --ironmem-only --debug-stderr
```

What is measured per backend:

| Metric | Description |
|--------|-------------|
| startup p50/p95 | Time from process spawn to `initialize` response (connect only) |
| warmup p50/p95 | Time until `ironmem_status` returns `warming_up: false` (model load + bootstrap) |
| add p50/p95 | `add_drawer` latency once embedder is ready |
| search p50/p95 | `search` latency with 5-needle recall check |
| status / taxonomy / delete p50 | Auxiliary tool latency |
| search hit rate | Fraction of queries where the planted needle appears in results |
| storage (post-checkpoint) | Disk bytes after WAL TRUNCATE checkpoint |

All flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--documents N` | 100 | Synthetic documents to ingest |
| `--queries N` | 20 | Searches per run |
| `--runs N` | 1 | Fresh runs per backend (storage wiped between runs) |
| `--seed N` | 42 | Dataset seed for reproducibility |
| `--ironmem-binary PATH` | `./target/debug/ironmem` | Path to ironmem binary |
| `--ironmem-model-dir PATH` | — | Override model directory |
| `--mempalace-repo PATH` | `~/git-repos/mempalace` | Path to mempalace repo |
| `--mempalace-python PATH` | current Python | Python interpreter for mempalace |
| `--ironmem-only` | false | Skip mempalace benchmark |
| `--debug-stderr` | false | Redirect server stderr to `/tmp/ironmem-*-stderr-*.log` |
| `--output-json PATH` | — | Write machine-readable results to a JSON file |
| `--keep-temp` | false | Keep temp benchmark workspace for inspection |

## Benchmark Caveats

- `ironrace-memory` uses a Rust ONNX embedding path; `mempalace` uses Python and Chroma
- The harness sets `IRONMEM_AUTO_BOOTSTRAP=0` and `IRONMEM_DISABLE_MIGRATION=1` automatically so one-time bootstrap cost is excluded from latency measurements; warmup time (model load) is tracked separately
- Storage is measured after a SQLite WAL `TRUNCATE` checkpoint for a fair comparison with Chroma-backed backends
- File mining is excluded — the benchmark targets common MCP tool surfaces only, because the two mining pipelines differ too much for a controlled comparison
- Search uses 5x overfetch (min 30 candidates) to maintain recall when needle documents are diluted by unrelated context

## Recommended Next Work

1. Extend benchmark coverage with larger datasets and repeated warm-cache runs
