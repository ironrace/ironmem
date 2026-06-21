# ironmem

[![CI](https://github.com/ironrace/ironmem/actions/workflows/ci.yml/badge.svg)](https://github.com/ironrace/ironmem/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ironrace/ironmem)](https://github.com/ironrace/ironmem/releases)
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
- [Cloudflare Pages Site](docs/CLOUDFLARE_PAGES.md)

Public site source lives in [`site/`](site/) and is configured for Cloudflare
Pages with [`wrangler.jsonc`](wrangler.jsonc).

## Contributor Hook

This repo includes tracked Git hooks for local commits and pushes.

Enable it once per clone:

```bash
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit .githooks/pre-push
```

The hooks run:

- `pre-commit`: `cargo fmt --all -- --check`, `python3 scripts/check_collab_turn_templates.py`, and `cargo clippy --workspace --all-targets --all-features -- -D warnings`
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

Prebuilt macOS (arm64) and Linux (x86_64) binaries, with SHA-256 checksums, are attached to every [tagged release](https://github.com/ironrace/ironmem/releases).

`scripts/install-ironmem.sh` also installs Codex's `/collab` command, the
Codex collab protocol prompts, and the bundled collab skill dependencies for
both Codex and Claude Code:

- `writing-plans`
- `subagent-driven-development`
- `finishing-a-development-branch`
- `executing-plans`
- `using-git-worktrees`
- `using-superpowers`
- `requesting-code-review`
- `test-driven-development`

Codex also receives the `pr-review-toolkit` skill used by the `/collab`
`review_fix_global` turn before Codex fans confirmed fixes out to
subagents and Claude runs `/ultrareview-local`.

Existing identical files are skipped. The installer records hidden packaged
baselines under each target root's `.ironmem-bases/` directory; on later
installs it three-way merges packaged updates into locally edited skills,
agents, commands, and prompts. If no baseline exists, the target is a symlink,
or a merge conflict occurs, the local file is left unchanged and the packaged
update is written next to it as `*.ironmem-packaged` (conflict drafts use
`*.ironmem-merge-conflict`). Use `--skip-skills` when you only want to replace
the binary or leave local copies untouched.
For Claude Code, the installer also installs the `code-reviewer` agent used by the vendored review flow.

## CLI

### `ironmem write-rules`

Stamp the canonical memory-protocol guidance into your rules files as an
idempotent, marker-delimited managed block:

```bash
# Write both CLAUDE.md and AGENTS.md in the current directory.
# With no --target, all targets are validated before any file is written.
ironmem write-rules

# Write a single file (--target accepts only CLAUDE.md or AGENTS.md),
# optionally in a different directory via --workspace.
ironmem write-rules --target AGENTS.md --workspace /path/to/repo
```

The block is sourced from a single in-source constant (`MEMORY_PROTOCOL`) and is
safe to re-run — it replaces only the managed block and never touches surrounding
content. **Explicit opt-in only: no hook ever runs this for you.**

### `ironmem doctor`

Validate a local install in one command. `doctor` reports the binary version,
database path and schema/migration status, embedding-model cache status, MCP
access mode, warmup readiness, and which harnesses (Claude Code, Codex) have the
`ironmem` MCP server registered:

```bash
ironmem doctor          # human-readable diagnostics
ironmem doctor --json   # machine-readable, for scripts/CI
```

It **diagnoses only** — it never modifies your config. Each line is one of
`[ OK ]`, `[INFO]`, `[WARN]`, or `[FAIL]`. The command exits non-zero **only**
when a blocking setup failure (`[FAIL]`) is found; warnings and info lines exit 0.

**Troubleshooting flow** — if memory isn't working, run `ironmem doctor` and fix
the first `[FAIL]`:

- **`model` not found** → run `ironmem setup` to download the embedding model.
- **`model` failed checksum** → delete the model directory and re-run `ironmem setup`.
- **`model` present but unreadable** → check file permissions on the model directory.
- **`database` cannot be opened/read** → the store may be corrupt; check the path
  and that no other process holds a lock.
- **`database` schema behind** (a `[WARN]`) → harmless; migrations apply
  automatically the next time the server starts.
- **`harness_*` not registered** (a `[WARN]`) → run `scripts/install-ironmem.sh`
  to register the MCP server with Claude Code and/or Codex. If a harness config
  is reported **unreadable** or **malformed** (also a `[WARN]`), fix the file by
  hand (permissions/encoding, or the JSON/TOML syntax) before re-registering.

Codex's config location honors `CODEX_HOME` (default `~/.codex/config.toml`);
Claude Code's is `~/.claude.json`. `doctor` reports each harness independently,
so you do **not** need both installed.

### `ironmem context`

Assemble a compact, bounded **task context pack** before you open a coding
session: relevant memory, known decisions, and per-area code-map freshness, all
trimmed to fit a token budget so you can paste it into a session as a starting
brief:

```bash
# Human-readable, with two code-map areas
ironmem context --repo . --task "refactor collab handoff validation" \
  --area collab --area metrics

# JSON for tooling, with an explicit token budget
ironmem context --repo . --task "explain metrics reporting" --budget 1500 --json
```

Flags:

- `--task <text>` (**required**) — task description that drives memory recall.
- `--repo <path>` (default `.`) — repository root for code-map lookup.
- `--area <name>` (repeatable) — code-map area to include. Areas are short
  names (e.g. `collab`), **not paths**; pass `--area` once per area.
- `--budget <tokens>` (default `2000`) — approximate output token budget.
  Memory hits are trimmed to fit; when any are dropped a truncation notice
  (`memory hits truncated to fit`) appears on the budget line.
- `--db <path>` — optional database path override (as with `report` / `doctor`).
- `--json` — emit JSON instead of text.

**Reading the output** — three sections:

- **Known decisions** — drawn from the knowledge graph for the requested areas.
- **Relevant memory** — semantic/lexical recall hits, each trimmed to a short
  snippet. With a small `--budget`, lower-ranked hits are dropped (the
  truncation notice fires).
- **Code maps** — per area, a **pointer** to where things live, **not** an
  authoritative fact. Always verify in source after navigating:
  - `FRESH` — the map matches the current `HEAD`; navigate, then confirm in
    source.
  - `STALE` — the listed files changed since the map was built; re-scout that
    area first.
  - `SCOUT REQUIRED` — no usable map (missing, an invalid area name, or an
    untrustable git state); explore the area before relying on memory for it.

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
- **UserPromptSubmit** (Claude Code only) — every prompt triggers a budget-bounded FTS/BM25 drawer lookup (embedder never loaded) that injects up to 3 sanitized untrusted-memory excerpts as `additionalContext`; see [docs/CODEX.md](docs/CODEX.md) for the `IRONMEM_PROMPT_HOOK_*` tunables
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

### Knowledge-graph fan-out caps (on by default)

Bound how many triples the KG layer serves and walks so a well-connected hub entity can't dump every relationship into MCP responses or the KG-boost loop. Both read fresh each call; the `kg_query` MCP tool also accepts a per-request `limit`.

| Variable | Default | Effect |
|---|---|---|
| `IRONMEM_KG_QUERY_LIMIT` | `50` | Max currently-valid triples `query_entity_current` returns (the `kg_query` tool and the KG-boost 1-hop fetch). Truncation is deterministic — ordered by `extracted_at DESC, id ASC`. `0` falls back to the default. |
| `IRONMEM_KG_BOOST_FANOUT` | `32` | Max distinct related entities the KG boost walks to across all mentioned entities, bounding the per-triple entity lookups a high-degree hub would otherwise trigger unbounded. `0` falls back to the default. |

### UserPromptSubmit FTS injection (Claude Code only)

On every prompt, ironmem runs an FTS/BM25-only drawer lookup (the embedder is never loaded) and injects up to 3 sanitized one-line untrusted-memory excerpts as `hookSpecificOutput.additionalContext`, under a hard wall-clock budget. On overrun, lock contention, or no qualifying hit it emits nothing and exits 0. Codex registers no UserPromptSubmit hook.

| Variable | Default | Effect |
|---|---|---|
| `IRONMEM_PROMPT_HOOK_BUDGET_MS` | `150` | Hard wall-clock budget for the whole hook, milliseconds. Non-positive/unparseable falls back to the default; capped at `1000`. |
| `IRONMEM_PROMPT_HOOK_MAX_HITS` | `3` | Max memory excerpts injected per prompt. Clamped to `1`–`3`. |
| `IRONMEM_PROMPT_HOOK_MIN_SCORE` | `0.0` | Minimum BM25 score a hit must clear (higher = better). `0.0` lets any FTS match through, since `MATCH` already filters relevance. |
| `IRONMEM_PROMPT_HOOK_SUMMARY_MAX_BYTES` | `120` | Byte cap for each injected one-line excerpt. |
| `IRONMEM_CONTEXT_WARN_PCT` | `0.60` | Context-occupancy fraction at which the hook injects a soft warning line (`>= warn`, `< handoff`). Unparseable or outside `0.0..=1.0` falls back to the default. If the resolved warn value `>=` handoff value, both revert to defaults to preserve the `warn < handoff` invariant. Occupancy uses `IRONMEM_CONTEXT_WINDOW` as the denominator. |
| `IRONMEM_CONTEXT_HANDOFF_PCT` | `0.80` | Context-occupancy fraction at which the hook injects the handoff instruction (`>= handoff`). Same parsing/clamping and `warn < handoff` invariant as above. |

### Metrics (instrumentation; on by default)

ironmem records lightweight per-call metrics (MCP response sizing + transcript occupancy) into the migration-008 tables. See `docs/METRICS_SPEC.md` (§5, §8). All writes are best-effort — a metrics failure never breaks an MCP response or a hook.

| Variable | Default | Effect |
|---|---|---|
| `IRONMEM_METRICS` | (unset, on) | Global kill switch. Set to `0`, `false`, `no`, or `off` to disable all metric writes. Any other value (including `1`) leaves metrics enabled. |
| `IRONMEM_CONTEXT_WINDOW` | `200000` | Occupancy denominator (tokens). Non-positive/unparseable values fall back to the default. Set to the harness's effective window for accurate `occupancy_pct`. |
| `IRONMEM_SESSION_ID` | (unset) | Override seam that pins the harness session id for `session_summary` co-keying when the MCP `initialize` request does not carry one. Primarily for testing. |
| `IRONMEM_HARNESS` | (unset) | Override seam pinning metrics harness attribution to `claude` or `codex` (otherwise learned from `initialize.clientInfo`). Primarily for testing. |

#### Reporting

`ironmem report` renders the recorded metrics (see `docs/METRICS_SPEC.md` §10 + the §7 cost table):

```bash
ironmem report                                  # human-readable text
ironmem report --json                           # stable JSON for tooling
ironmem report --task <tag> --since 2026-06-01  # scope to one task / start date (RFC3339 or YYYY-MM-DD)
```

It surfaces tokens-to-done by task and phase, the measured-vs-estimated split, iteration counts/outcome, and a merged-only headline. Cost is **§7-derived** (the stored provider figure is reported separately as `provider_reported_cost_usd`); `baseline_ready` / `baseline_task_count` track the Phase-6 ≥10-merged-task recording gate (§11.5).

### Excluded benchmark crates

These crates live under `benchmarks/` and are excluded from the Cargo workspace. They are standalone benchmark runners that never import ironmem crates at runtime.

- `benchmarks/provbench/baseline/` — §0c–§1 ProvBench LLM-as-invalidator baseline. See [benchmarks/provbench/SPEC.md](benchmarks/provbench/SPEC.md).
- `benchmarks/abeval/` — §11 A/B harness (corpus + dry-run smoke; no paid runs). See [benchmarks/abeval/README.md](benchmarks/abeval/README.md).

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The canonical version is in `crates/ironmem/Cargo.toml`. Plugin JSON files (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`) must match this version — enforced by CI. See [CHANGELOG.md](CHANGELOG.md) for release history.
