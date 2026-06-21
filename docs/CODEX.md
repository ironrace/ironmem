# Codex Guide

## Purpose

This guide explains how to use `ironmem` with Codex today, what is still missing, and how to compare it against `mempalace`.

For the bounded Claude↔Codex planning protocol, see [COLLAB.md](COLLAB.md).

## Current Support Level

What works now:

- Running `ironmem` as an MCP server over stdio with non-blocking startup (<25 ms to first response)
- Read and write MCP tools
- Semantic search
- Knowledge graph tools
- Restricted vs trusted access modes
- `mine` for workspace ingestion with incremental updates
- `hook` for session-start, stop, and precompact
- `metrics` — pure calc helpers + best-effort DB sinks shared by MCP response sizing and hook occupancy sampling (see `docs/METRICS_SPEC.md`)
- Codex plugin packaging
- Automatic migrate-or-init bootstrap on first use
- Stale `bootstrap.lock` files from crashed processes are auto-cleared on next startup

What hooks currently do on `stop` / `precompact`:

- **Transcript token persistence** — the full transcript is parsed and per-turn agent token usage
  is written as `source='transcript'`, `estimated=false` rows in `token_usage`. One row per distinct
  `message.id` for Claude stream-json; one cumulative row (`codex-final`) for Codex rollouts (cached
  tokens subtracted from input per §12 2026-06-20). Idempotent: re-running the same hook input
  does not double-count. Runs under `IRONMEM_METRICS` gate only, decoupled from `allows_writes`
  (same §113 pattern as occupancy sampling). See METRICS_SPEC.md §12 2026-06-20 for the full
  accounting rules and idempotency key format.
- **Occupancy sampling** — the last assistant message's input + cache_read token counts are sampled
  into `occupancy_samples` and merged into `session_summary`. Runs under `IRONMEM_METRICS` gate,
  decoupled from `allows_writes` (issue #113).
- **Transcript review capture** — assistant messages in the transcript are scanned in reverse
  chronological order for code-review-like content (severity labels, file references, decision
  keywords). The most recent review-like assistant message is stored as a drawer in the
  `reviews/` wing so it can be recalled in future sessions. Gated by `allows_writes`.
- **Metadata diary entry** — a structured summary line is written to the diary recording the hook
  name, harness, session ID, working directory, and transcript path, plus the review room if a
  review was captured. Gated by `allows_writes`.
- **Incremental re-mine** — workspace files changed since the last hook run are re-embedded.
  Gated by `allows_writes`.

`user-prompt-submit` (Claude Code only) — occupancy sampling only (no transcript token persistence;
no real message id is available in UPS).

What does not work yet:

- Hook behavior does not yet build a rich LLM-written session summary from transcript content

## Build

From the repo root:

```bash
scripts/install-ironmem.sh
~/.ironrace/bin/ironmem setup
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
[mcp_servers.ironmem]
command = "/absolute/path/to/.ironrace/bin/ironmem"
args = ["serve"]

[mcp_servers.ironmem.env]
IRONMEM_MCP_MODE = "trusted"
```

Leave `IRONMEM_DB_PATH` unset to use the shared default store
(`~/.ironrace-memory/memory.sqlite3`). Set it only when you want an isolated
store — for example a project-local path, or a Codex-only database as shown in
the isolation section below.

## Manual Validation

After registering the MCP server, validate the basics:

1. Start Codex and confirm the server appears in MCP listings.
2. Call `status`.
3. Add a small drawer with `add_drawer`.
4. Search for it with `search`.

## Shared Memory Across Harnesses

Codex and Claude Code share the **same database by default** (`~/.ironrace-memory/memory.sqlite3`). Memory written during a Claude session is immediately visible in Codex, and vice versa.

The database is kept up to date automatically through hooks:

| Hook | What happens |
|------|-------------|
| `session-start` | Bootstrap if first run; initial mine if workspace not yet indexed. On the Claude Code harness, also emits a compact memory-status block via `hookSpecificOutput.additionalContext` (drawer/wing/room counts, active collab session + phase, last-diary pointer, `MEMORY_PROTOCOL`); Codex receives no such output (silent degrade) |
| `user-prompt-submit` | Claude Code only — FTS/BM25 drawer lookup and optional context injection (see below). Codex registers no UserPromptSubmit hook |
| `stop` | Persist measured transcript token rows and occupancy samples under `IRONMEM_METRICS`; persist session summary to diary and re-mine changed files when writes are allowed |
| `precompact` | Persist measured transcript token rows and occupancy samples under `IRONMEM_METRICS`; snapshot pending session context and re-mine changed files when writes are allowed |

### UserPromptSubmit (Claude Code only)

On every prompt, ironmem runs an FTS/BM25-only drawer lookup (the embedder is
never loaded) and injects up to 3 sanitized one-line memory matches as
`hookSpecificOutput.additionalContext`. On overrun or no match it emits nothing
and exits 0. Codex registers no UserPromptSubmit hook.

Tunables (all fresh-read per invocation):

- `IRONMEM_PROMPT_HOOK_BUDGET_MS` — hard wall-clock budget, ms (default `150`,
  capped `1000`).
- `IRONMEM_PROMPT_HOOK_MAX_HITS` — max excerpts injected per prompt (default `3`,
  clamped `1`–`3`).
- `IRONMEM_PROMPT_HOOK_MIN_SCORE` — minimum BM25 score a hit must clear, higher =
  better (default `0.0`; any FTS match passes).
- `IRONMEM_PROMPT_HOOK_SUMMARY_MAX_BYTES` — byte cap per injected one-line excerpt
  (default `120`).

Incremental re-mining uses a SHA-256 manifest so only files whose content changed are re-embedded. Repeat hook runs on unchanged workspaces are fast.

SQLite WAL mode allows both harnesses to access the store concurrently without locking conflicts.

**Isolation:** To give a harness its own store, set `IRONMEM_DB_PATH` in its plugin config:

```toml
# Codex-only store
[mcp_servers.ironmem.env]
IRONMEM_DB_PATH = "~/.ironmem/codex.sqlite3"
```

```json
// Claude Code-only store — in .claude-plugin/.mcp.json env block
"IRONMEM_DB_PATH": "/Users/you/.ironmem/claude.sqlite3"
```

## Startup Behavior

`ironmem serve` uses a two-phase init so the harness is never left waiting at startup:

| Phase | What happens | Typical time |
|-------|-------------|--------------|
| Phase 1 | DB open + schema migration | ~50 ms |
| Phase 2 | ONNX model load + auto-bootstrap + mine (background thread) | 5–120 s |

Embedding-dependent tools (`search`, `add_drawer`, diary writes) return `{"warming_up": true}` until Phase 2 completes. The benchmark harness polls `status` until `warming_up: false` before starting measurements.

```json
// status response during warmup
{"warming_up": true, "total_drawers": 0, ...}

// status response once ready
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

`ironmem` now ships a `.codex-plugin/` directory with:

- `plugin.json`
- `hooks.json`
- wrapper scripts for the MCP server and hooks
- the `/collab` command shim
- Codex-specific README content
- bundled collab skill dependencies under `skills/`

The shared collab skill dependencies are bundled for Claude Code under `.claude-plugin/skills/`.
`scripts/install-ironmem.sh` installs the Codex copies into `$CODEX_HOME/skills` (default
`~/.codex/skills`) and the Claude copies into `$CLAUDE_HOME/skills` (default `~/.claude/skills`).
The shared required set is:

- `writing-plans`
- `subagent-driven-development`
- `finishing-a-development-branch`
- `executing-plans`
- `using-git-worktrees`
- `using-superpowers`
- `requesting-code-review`
- `test-driven-development`

Codex also receives `pr-review-toolkit`, which `/collab` uses during the
`CodeReviewFixGlobalPending` / `review_fix_global` turn before Codex fans
confirmed fixes out to subagents and Claude's `/ultrareview-local` audit runs.

Existing identical skills are skipped. Existing bundled skills, agents, commands, and prompts
that differ are updated to the packaged copies; `--skip-skills` skips this step entirely.
For Claude Code, the installer also provisions the `code-reviewer` agent used by the vendored
`subagent-driven-development` review flow into `$CLAUDE_HOME/agents`.

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

Codex adopts the memory protocol through a managed rules-file block sourced from
the `MEMORY_PROTOCOL` constant in `crates/ironmem/src/bootstrap.rs`:

```bash
ironmem write-rules --target AGENTS.md
```

This is explicit opt-in only; no hook or plugin path runs `write-rules`
automatically.

## Benchmarking Against MemPalace

This repo includes a benchmark harness at `scripts/benchmark_vs_mempalace.py`.

```bash
# Full comparison (requires ~/git-repos/mempalace)
python3 scripts/benchmark_vs_mempalace.py \
  --documents 100 \
  --queries 15 \
  --runs 2 \
  --output-json /tmp/ironmem-vs-mempalace.json

# ironmem only (no mempalace required)
python3 scripts/benchmark_vs_mempalace.py --ironmem-only --documents 100 --queries 20 --runs 3

# Capture server logs for debugging startup issues
python3 scripts/benchmark_vs_mempalace.py --ironmem-only --debug-stderr
```

What is measured per backend:

| Metric | Description |
|--------|-------------|
| startup p50/p95 | Time from process spawn to `initialize` response (connect only) |
| warmup p50/p95 | Time until `status` returns `warming_up: false` (model load + bootstrap) |
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

- `ironmem` uses a Rust ONNX embedding path; `mempalace` uses Python and Chroma
- The harness sets `IRONMEM_AUTO_BOOTSTRAP=0` and `IRONMEM_DISABLE_MIGRATION=1` automatically so one-time bootstrap cost is excluded from latency measurements; warmup time (model load) is tracked separately
- Storage is measured after a SQLite WAL `TRUNCATE` checkpoint for a fair comparison with Chroma-backed backends
- File mining is excluded — the benchmark targets common MCP tool surfaces only, because the two mining pipelines differ too much for a controlled comparison
- Search uses 5x overfetch (min 30 candidates) to maintain recall when needle documents are diluted by unrelated context

## Recommended Next Work

1. Extend benchmark coverage with larger datasets and repeated warm-cache runs
