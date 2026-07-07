# ironmem

[![CI](https://github.com/ironrace/ironmem/actions/workflows/ci.yml/badge.svg)](https://github.com/ironrace/ironmem/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ironrace/ironmem)](https://github.com/ironrace/ironmem/releases)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**ironmem gives your AI coding agents a memory that survives the session.**
Instead of re-reading the same files and re-deriving the same context every time
you open Claude Code or Codex, ironmem keeps what was learned — decisions, file
locations, prior work — in a private local store shared across registered
harnesses.

What that means in practice:

- **Fewer repeated file reads.** Agents recall where things live and what was
  already decided, so they spend less of each session re-exploring the
  repository from scratch.
- **Local and private by default.** Everything lives in one local SQLite store
  on your machine (`~/.ironrace-memory/memory.sqlite3`). No cloud, no account,
  no Python runtime.
- **Shared across registered harnesses.** All harnesses read and write the same
  memory store. Claude Code and Codex are the two bundled defaults; the registry
  is extensible for additional harnesses. Context carried in a Claude session is
  available in Codex, and vice versa.

## When to use ironmem

Reach for ironmem when:

- You work in the same repositories across many agent sessions and keep
  re-explaining the same context.
- You switch between Claude Code and Codex and want them to share what they have
  learned.
- You want memory to stay on your machine rather than in a hosted service.

It is less useful for one-off questions, throwaway repos, or workflows where
every session genuinely starts from a blank slate.

## What it does not do yet

- It does not yet build a rich, LLM-written session summary from transcript
  content; summaries today are structured/metadata-based.
- It makes no numeric claims about exploration or token savings. See the
  [Benchmark Methodology](docs/BENCHMARKS.md) for how savings would be measured
  and the [Benchmarking](#benchmarking) harness to run it yourself.
- Install is from source or prebuilt release binaries today — there is no
  single-command package-manager install yet.

## Architecture

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
- [Benchmark Methodology](docs/BENCHMARKS.md)
- [Cloudflare Pages Site](docs/CLOUDFLARE_PAGES.md)

Public site source lives in [`site/`](site/) and is configured for Cloudflare
Pages with [`wrangler.jsonc`](wrangler.jsonc).

## Contributor Hook

This repo includes tracked Git hooks for local commits and pushes.

Enable it once per clone:

```bash
bash scripts/install-git-hooks.sh
```

The installer sets `core.hooksPath=.githooks` and writes fallback shims under
`.git/hooks/` so a local clone cannot silently keep using stale hook bodies.

The hooks are diff-aware:

- collab protocol/template changes run `python3 scripts/check_collab_turn_templates.py`
- Rust/workspace changes run `cargo fmt --all -- --check` and clippy on commit, then `cargo test --workspace` on push
- docs/config-only changes that do not affect those surfaces skip heavy local gates

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

### Use with any MCP client

ironmem's core tools — `search`, `status`, the knowledge-graph tools (`kg_query`, `kg_add`, `traverse`, …), diary reads/writes, `get_taxonomy`, and the drawer tools — speak MCP over stdio, so **any MCP-capable client works with no new code**. Cursor, Cline, Windsurf, and others accept a standard `mcpServers` block:

```json
{
  "mcpServers": {
    "ironmem": {
      "command": "/absolute/path/to/ironmem",
      "args": ["serve"],
      "env": { "IRONMEM_MCP_MODE": "trusted" }
    }
  }
}
```

Some clients want only the inner object (the value under `"ironmem"`) rather than the full `mcpServers` wrapper — adapt to your client's config format. Use an absolute path to the installed binary (`~/.ironrace/bin/ironmem`), and keep `IRONMEM_MCP_MODE` set to `trusted` if you want the write tools.

**Not available to generic clients.** The harness-driven automation listed under [Shared Memory Across Harnesses](#shared-memory-across-harnesses) — session-start memory injection, Stop/PreCompact mining, and UserPromptSubmit FTS injection — is wired into Claude Code and Codex specifically. A generic MCP client still gets the full read/search/write toolset on the shared store; it just won't fire those hooks automatically, which requires first-class harness support.

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

## First run: one-command launchers

Start an assistant with ironmem already attached — no manual MCP config:

```bash
ironmem claude .                       # launch Claude Code in the current repo
ironmem codex .                        # launch Codex in the current repo
ironmem claude . "fix the login bug"   # launch with an initial prompt
ironmem codex /path/to/repo "add tests"
```

Each launcher:

1. Canonicalizes the repo path and validates that it exists.
2. Validates that the target assistant (`claude` / `codex`) is on your `PATH`,
   and prints a clear error if it is not.
3. Ensures the ironmem MCP server is registered for that assistant
   (idempotent — existing manual setup is preserved untouched).
4. Warms the repo into memory on a best-effort basis (a warm failure is logged
   but does not block launch — the assistant's own MCP server bootstraps on
   `serve`).
5. **Pre-injects a compact context block** into the initial prompt when you
   pass one — relevant memory, known decisions, and per-area code-map freshness
   — so the assistant starts with context instead of exploring from scratch. The
   block is bounded to a token budget, sanitized (recalled memory is framed as
   untrusted reference text and cannot open a code fence in the prompt), and
   skipped when there is nothing relevant to add. This is best-effort: a build
   failure falls back to your bare prompt and never blocks launch.
6. Launches the assistant with the repo as its working directory.

Pre-injection is driven by your prompt (the task) and any `--area` flags:

```bash
ironmem claude . "fix the login bug" --area auth --area session
```

To preview exactly what would be injected, run the same inputs through the
context command:

```bash
ironmem context --repo . --task "fix the login bug" --area auth --area session
```

Tune or disable pre-injection:

- `--budget <tokens>` — change the context budget (default 2000).
- `--no-context` — skip pre-injection for a single launch.
- `IRONMEM_LAUNCHER_NO_CONTEXT=1` — disable pre-injection for the environment.

If you manage MCP configuration yourself, pass `--no-mcp-setup` to skip step 3:

```bash
ironmem claude . --no-mcp-setup
```

> A prompt that begins with `-` must be separated with `--`, e.g.
> `ironmem claude . -- "--version is broken"`.

The manual MCP setup path remains fully supported.

## CLI

### `ironmem write-rules`

Stamp the canonical memory-protocol guidance into your rules files as an
idempotent, marker-delimited managed block:

```bash
# Write all default-harness rules files (CLAUDE.md and AGENTS.md) in the
# current directory. All targets are validated before any file is written.
ironmem write-rules

# Write a single target by filename (validated against registered harness
# rules files at runtime).
ironmem write-rules --target AGENTS.md --workspace /path/to/repo

# Write the rules file for a specific harness (resolves to its rules file).
ironmem write-rules --harness codex   # writes AGENTS.md
ironmem write-rules --harness claude  # writes CLAUDE.md
```

`--target` and `--harness` are mutually exclusive. Both are validated at runtime
against the harness registry — only filenames registered in a harness entry are
accepted.

The block is sourced from a single in-source constant (`MEMORY_PROTOCOL`) and is
safe to re-run — it replaces only the managed block and never touches surrounding
content. **Explicit opt-in only: no hook ever runs this for you.**

### `ironmem harnesses`

List all registered harnesses and their key attributes:

```bash
ironmem harnesses             # human-readable one line per harness
ironmem harnesses --format=json  # machine-readable, for CI/packaging scripts
```

### `ironmem doctor`

Validate a local install in one command. `doctor` reports the binary version,
database path and schema/migration status, embedding-model cache status, MCP
access mode, warmup readiness, and which registered harnesses have the
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
  - `FRESH` — none of the map's source files changed since the map was built
    (the shown SHA is the build commit, not necessarily repo `HEAD`); use it to
    navigate, then confirm in source.
  - `STALE` — the listed files changed since the map was built; re-scout that
    area first.
  - `SCOUT REQUIRED` — no usable map (missing, an invalid area name, or an
    untrustable git state); explore the area before relying on memory for it.

### Memory lifecycle

Use append-only drawers for durable decisions and facts. For mutable "current
state" context, pass `logical_key` to `add_drawer`; the same
wing/room/logical_key rewrites one stable drawer ID instead of accumulating
stale copies:

```json
{
  "content": "Current task state: auth refactor is blocked on token tests.",
  "wing": "my-project",
  "room": "current-context",
  "logical_key": "task-state"
}
```

Prune stale operational collab artifacts with a dry-run first:

```bash
ironmem memory gc --dry-run
ironmem memory gc --apply
```

Defaults are conservative: `collab-checkpoints` older than 60 days are delete
candidates; unreferenced `collab-plans` and `collab-task-lists` older than 180
days are delete candidates. Referenced plan/task-list drawers are skipped so
historical `collab_status` lookups are not broken. Durable summaries,
project facts, source-mined drawers, diary entries, and knowledge-graph facts
are not pruned by this command.

### `ironmem symbols`

Index and query a **local symbol/import graph** built from your Rust and Python sources.
The graph is stored in SQLite alongside the memory store — no network required.

**Supported languages (v0): Rust (`.rs`) and Python (`.py`) only.**
TypeScript, JavaScript, and other extensions are skipped. Supported files larger
than 1 MiB are skipped with a warning.

```bash
# Index a repo (incremental by content-hash; re-runs only changed files)
ironmem symbols index --repo .

# Force a full re-index even when content is unchanged
ironmem symbols index --repo /path/to/repo --force

# Look up function/struct/class declarations by name
ironmem symbols lookup --repo /path/to/repo "parse_file"

# Filter by kind
ironmem symbols lookup --repo /path/to/repo --kind fn "handle_"

# Find imports by file path (repo-relative) or module name
ironmem symbols imports --repo /path/to/repo "std::collections"

# Graph edges (import: file → module; contains: symbol → parent symbol)
ironmem symbols neighbors --repo /path/to/repo "src/db/schema.rs"

# All commands support --json for structured output
ironmem symbols index --json --repo /path/to/repo
ironmem symbols lookup --json --repo /path/to/repo "MyStruct"
```

**Storage note:** only declaration metadata is persisted (`signature`, `raw` import lines,
spans, kind, visibility). No full source-file bodies are ever stored.
Max snippet length is 512 bytes per field.

**Edge scope (v0):** `import` (file → module from `use`/`import` statements) and
`contains` (symbol → parent symbol for nested items). Cross-symbol call/reference
resolution is not available in v0.

The same index is also accessible via MCP tools (`symbol_graph_index`,
`symbol_lookup`, `symbol_imports`, `symbol_neighbors`) so
AI agents can query the graph without a shell.

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

It surfaces tokens-to-done by task and phase, the measured-vs-estimated split, iteration counts/outcome, repeated MCP-response sizing (including top tools by collab session), and a merged-only headline. Cost is **§7-derived** (the stored provider figure is reported separately as `provider_reported_cost_usd`); `baseline_ready` / `baseline_task_count` track the Phase-6 ≥10-measured-task recording gate (§11.5).

### `ironmem dashboard`

Start a **local, loopback-only, read-only** HTTP server for inspecting the
configured SQLite store in a browser — useful for debugging memory content, code
maps, collab sessions, and metrics without raw SQL:

```bash
# Start on the default port (7384) at 127.0.0.1
ironmem dashboard

# Use a specific database and an ephemeral port (prints chosen URL)
ironmem dashboard --db /path/to/memory.sqlite3 --port 0

# Emit startup metadata as JSON (url, db_path, schema_version)
ironmem dashboard --port 0 --json
```

**Security model (enforced, not advisory):**

- Binds `127.0.0.1` (loopback) by default. A non-loopback `--host` is rejected
  unless `--allow-non-loopback` is explicitly passed (a warning is printed).
- The database is opened **read-only** — the dashboard never creates, modifies,
  or migrates the file. A missing or schema-mismatched database fails fast with
  a clear message (`db not found at <path>` / `schema version mismatch: expected N, found M`).
- Only `GET` and `HEAD` requests are served; all other methods return `405`.
- List/report responses are bounded with `?limit=` (default 50, max 500).
- No authentication; keep it loopback-only on shared or networked machines.

**Endpoints served:**

| Route | Description |
|---|---|
| `GET /` | Single-page HTML dashboard (Memory / Code Maps / Sessions / Reports) |
| `GET /api/summary` | Quick headline counts (total drawers, wings, KG stats, schema version) + `model_status` warming label |
| `GET /api/memory` | Drawer list with `?wing=`, `?room=`, `?limit=` filters; `?id=<drawer_id>` returns one full drawer |
| `GET /api/code-maps` | Code-map rows with `?repo=`, `?area=`, `?limit=` filters; each row carries a `freshness` badge |
| `GET /api/sessions` | Compact collab session summaries with `?limit=` (plan refs only, no full bodies) |
| `GET /api/report` | Metrics report JSON with optional `?task=`, `?since=`, `?limit=` filters |

**Warming status (`model_status`):** `/api/summary` reports embed-model cache
readiness as `ready` / `missing` / `corrupt` / `unreadable`. This answers *"can
it embed?"* (model files present and intact) — **not** whether memory is
populated (that's `total_drawers`). The UI surfaces both so warming is never
misread as content readiness. The status is checksummed **once at startup**
(the model is hundreds of MB, so it never runs per request); restart the
dashboard to re-check after a model finishes downloading.

**Code-map freshness:** each `/api/code-maps` row carries a `freshness` badge
computed with a hybrid strategy. When the map's stored canonical worktree path
resolves, the real freshness engine runs (`git diff` against `HEAD`) and reports
`fresh` / `stale` (with changed-file count) / `rescout`. When the path is absent
(worktree not checked out here), it falls back to a build-age bucket (`fresh`
<7d / `aging` <30d / `stale`) derived from `built_at`. When the path exists but
cannot be read (permission/transient I/O error), freshness is reported as
`rescout` rather than a misleading age signal. The git diff is memoized per
`(repo, head_sha)`, so listing many areas of one repo runs at most one `git
diff` per distinct build SHA. `head_sha` is always shown for provenance.

**Remediation hints:** each section links to the real CLI command that acts on
it — `ironmem mine <dir>` and `ironmem reembed` for memory, `ironmem mine <dir>`
+ `ironmem doctor` for stale/rescout maps (there is no `code-map refresh`
subcommand), `ironmem report` for sessions/metrics, and `ironmem doctor` /
`ironmem context` for cache health. These are static text — no user-controlled
data is rendered.

### Excluded benchmark crates

These crates live under `benchmarks/` and are excluded from the Cargo workspace. They are standalone benchmark runners that never import ironmem crates at runtime.

- `benchmarks/provbench/baseline/` — §0c–§1 ProvBench LLM-as-invalidator baseline. See [benchmarks/provbench/SPEC.md](benchmarks/provbench/SPEC.md).
- `benchmarks/abeval/` — §11 A/B harness (corpus + dry-run smoke; no paid runs). See [benchmarks/abeval/README.md](benchmarks/abeval/README.md).

## Versioning

This project uses [Semantic Versioning](https://semver.org/). The canonical version is in `crates/ironmem/Cargo.toml`. Plugin JSON files (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`) must match this version — enforced by CI. See [CHANGELOG.md](CHANGELOG.md) for release history.
