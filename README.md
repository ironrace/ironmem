# ironmem

[![CI](https://github.com/ironrace/ironmem/actions/workflows/ci.yml/badge.svg)](https://github.com/ironrace/ironmem/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ironmem.svg)](https://crates.io/crates/ironmem)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

`ironmem` is a Rust workspace for a local AI memory backend:

- `ironrace-core`: shared HNSW vector index
- `ironrace-embed`: ONNX sentence embeddings in pure Rust
- `ironmem`: MCP server exposing semantic search plus a knowledge graph

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
git clone https://github.com/ironrace/ironmem.git
cd ironmem
scripts/install-ironmem.sh
~/.ironrace/bin/ironmem setup
```

Start the MCP server in trusted mode (required for write tools):

```bash
IRONMEM_MCP_MODE=trusted ~/.ironrace/bin/ironmem serve
```

Smoke-test the live stdio server without downloading the model:

```bash
python3 scripts/mcp_smoke_test.py --binary ~/.ironrace/bin/ironmem
```

Add it to Codex:

```toml
[mcp_servers.ironmem]
command = "/absolute/path/to/ironmem"
args = ["serve"]

[mcp_servers.ironmem.env]
IRONMEM_MCP_MODE = "trusted"
```

Tagged releases upload prebuilt macOS and Linux binaries automatically. Until the first tagged release is published, building from source is the supported install path.

`scripts/install-ironmem.sh` also installs the bundled collab skill dependencies for both Codex and Claude Code:

- `writing-plans`
- `subagent-driven-development`
- `finishing-a-development-branch`
- `executing-plans`
- `using-git-worktrees`
- `using-superpowers`
- `requesting-code-review`
- `test-driven-development`

Existing identical skills are skipped. Existing divergent skills are left in place unless you pass `--force-skills`; use `--skip-skills` when you only want to replace the binary.
For Claude Code, the installer also installs the `code-reviewer` agent used by the vendored review flow.

## Current Status

- MCP server works over stdio with non-blocking startup (responds to `initialize` in <25 ms)
- Embedding and bootstrap run in a background thread; `status` returns `warming_up: true` until ready
- Search, taxonomy, graph, diary, and knowledge-graph tools exist
- Automatic bootstrap runs on first server or hook start
- Direct migration from `mempalace` Chroma stores is implemented
- Workspace mining and incremental re-mining are implemented
- Codex and Claude Code plugin packaging is included, including bundled collab skill dependencies
- `~/.ironrace/bin/ironmem` is the preferred installed binary location; plugin launch scripts check there first
- Bounded Claude↔Codex collaboration protocol (v1 planning + v3 coding) is available via the `collab_*` MCP tools, including long-poll `wait_my_turn` for autonomous operation — see [docs/COLLAB.md](docs/COLLAB.md)

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
[mcp_servers.ironmem.env]
IRONMEM_DB_PATH = "~/.ironmem/codex.sqlite3"
```

## Startup Behavior

`ironmem serve` uses a two-phase init so the harness is never left waiting at startup:

| Phase | What happens | Typical time |
|-------|-------------|--------------|
| Phase 1 | DB open + schema migration | ~50 ms |
| Phase 2 | ONNX model load + auto-bootstrap + mine (background thread) | 5–120 s |

Embedding-dependent tools (`search`, `add_drawer`, diary writes) return `{"warming_up": true}` until Phase 2 completes. Poll `status` and check `warming_up: false` before issuing write-heavy workloads.

## Benchmarking

Compare against a local `mempalace` checkout:

```bash
# Full comparison (requires ~/git-repos/mempalace)
python3 scripts/benchmark_vs_mempalace.py \
  --documents 100 \
  --queries 20 \
  --runs 2 \
  --output-json /tmp/ironmem-vs-mempalace.json

# ironmem only (no mempalace required)
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

### LLM rerank (opt-in)

Enable a Claude Haiku rerank pass over the top-K candidates by setting:

```bash
export IRONMEM_RERANK=llm_haiku
ironmem serve
```

| Env var | Default | Effect |
|---|---|---|
| `IRONMEM_RERANK` | (unset) | Set to `llm_haiku` to enable the LLM rerank stage. Strict string-enum — `1`/`true` do NOT enable. |
| `IRONMEM_RERANK_TOP_K` | `20` | How many top candidates feed the reranker. Smaller = faster. |
| `IRONMEM_LLM_RERANK_MODEL` | `claude-haiku-4-5` | Model alias passed to `claude --model`. |
| `IRONMEM_LLM_RERANK_TIMEOUT_MS` | `5000` | Wall-clock timeout per rerank call. |
| `IRONMEM_SHRINKAGE_RERANK` | `1` | Set to `0` to disable the existing lexical shrinkage rerank (eval-only). |
| `IRONMEM_SHRINKAGE_WORD_BOUNDARY` | `1` | Set to `0` to revert the shrinkage rerank's keyword/name matcher to legacy substring behavior. Default ON: word-boundary regex match with light English suffix tolerance (s\|es\|ed\|ing\|ion\|ions). |
| `IRONMEM_LLM_RERANK_BACKEND` | `cli` | `cli` shells out to the local `claude` CLI (subscription auth, ~1-3s per call). `api` POSTs directly to `api.anthropic.com/v1/messages` (faster, billed). |
| `IRONMEM_LLM_RERANK_MAX_TOKENS` | `8` | `max_tokens` for the API backend. Pick-one prompt at `temperature=0` emits a bare integer. Ignored by `cli` backend. |
| `ANTHROPIC_API_KEY` | (unset) | Required when `IRONMEM_LLM_RERANK_BACKEND=api`. The standard convention. |
| `IRONMEM_ANTHROPIC_API_KEY` | (unset) | Scoped fallback for users who keep `ANTHROPIC_API_KEY` unset so their `claude` CLI uses subscription auth. |

Requires the local `claude` CLI on `PATH` (Claude Code subscription provides auth — no API key needed). On `claude` CLI absent or subprocess error, the search returns the un-reranked candidates and a `WARN` line is logged — graceful degradation, never an error to the caller.

Expected p95 latency with rerank enabled: ~1-3 seconds per query (subprocess startup + Haiku inference). Acceptable for opt-in; off by default.

### Preference enrichment (off by default; experimental scaffolding)

Default OFF. The pref-enrich experiment did not meet its target lift on LongMemEval — see `docs/superpowers/specs/2026-04-30-pref-enrich-experiment-retro.md`. The infrastructure (PreferenceExtractor trait, pipeline collapse step, sentinel-prefix sibling drawers) is preserved for future synth-doc strategies.

| Variable | Default | Effect |
|---|---|---|
| `IRONMEM_PREF_ENRICH` | (unset, off) | Set to `1` to enable synthetic-preference-doc enrichment at ingest. |
| `IRONMEM_PREF_EXTRACTOR` | `regex` | `regex` (V4 pattern set) or `llm` (single-shot LLM summarize). |
| `IRONMEM_PREF_LLM_BACKEND` | `cli` | `cli` (claude subprocess) or `api` (direct ureq). |
| `IRONMEM_PREF_LLM_MODEL` | `claude-haiku-4-5` | Model alias for the LLM extractor. |
| `IRONMEM_PREF_LLM_TIMEOUT_MS` | `15000` | Wall-clock cap per LLM extraction call (capped at 60_000). |
| `IRONMEM_PREF_LLM_MAX_TOKENS` | `200` | `max_tokens` for the API backend. Ignored by `cli`. |

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The canonical version is in `crates/ironmem/Cargo.toml`. Plugin JSON files (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`) must match this version — enforced by CI. See [CHANGELOG.md](CHANGELOG.md) for release history.
