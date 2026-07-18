# Codex Guide

## Purpose

`ironmem` gives Codex a private, local memory that persists across sessions and is shared with Claude Code — so Codex can recall what a repository contains and what was already decided instead of re-exploring it every time. This guide explains how to set that up with Codex today, what is still missing, and how to compare it against `mempalace`.

For the bounded Claude↔Codex planning protocol, see [COLLAB.md](COLLAB.md).

## Registry-Driven Hooks and Attribution

Codex is one registered harness in the `REGISTRY` constant
(`crates/ironmem/src/harness/mod.rs`). Its `HarnessSpec` entry records:

- **`id`**: `"codex"` — used as the harness slug in metrics and hook paths.
- **`binary`**: `"codex"` — the launcher binary looked up on `PATH`.
- **`rules_file`**: `"AGENTS.md"` — the target for `ironmem write-rules --harness codex`.
- **`rules_strategy`**: `"native"` — `AGENTS.md` is written with the canonical
  block directly.
- **`client_info_aliases`**: `["codex"]` — substring matched against
  `initialize.clientInfo.name` to attribute MCP sessions.
- **`env_aliases`**: `["codex"]` — accepted by `IRONMEM_HARNESS` for test overrides.
- **`additional_context_support`**: `false` — Codex has no
  `hookSpecificOutput.additionalContext` channel, so session-start memory
  injection and UserPromptSubmit context injection are Claude Code capabilities
  only. This is a capability flag in the registry, not a hard-coded prefix
  check; future harnesses that gain the channel can set this to `true`.
- **`occupancy_support`**: `true` — Codex hook output carries token counts
  that ironmem samples into `occupancy_samples`.
- **`transcript_parser`**: `Codex` — the Codex rollup format (one
  `codex-final` row per session, cached tokens subtracted from input).

Run `ironmem harnesses --format=json` to inspect the current registry at any
time. The registry also carries `grok` and `gemini` rows (`GROK.md`/`GEMINI.md`
via `@AGENTS.md` import, `write_rules_default: false`) — scaffolding for the
`ironmem grok`/`ironmem gemini` launchers, not yet default `write-rules`
targets. See [First run: one-command launchers](../README.md#first-run-one-command-launchers)
in the main README.

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
- **Symbol/import graph index** — local, offline, SQLite-backed code index for Rust and
  Python sources (migration 012); accessible via `ironmem symbols …` CLI or the
  `symbol_graph_*` MCP tools (see [Symbol Graph](#symbol-import-graph) below)

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
bash scripts/install-git-hooks.sh
```

The installer sets `core.hooksPath=.githooks` and writes fallback shims under
`.git/hooks/` to catch stale local hook bodies.

The hooks are diff-aware:

- collab protocol/template changes run the collab template lint
- Rust/workspace changes run fmt and clippy on commit, then workspace tests on push
- docs/config-only changes outside those surfaces skip heavy local gates

### `run_git_hook.py`: collect → resolve → execute

`scripts/run_git_hook.py` is the diff-aware dispatcher both hooks delegate to.
`main(phase)` wires it as three layers, each with one job and no layer
reaching backwards — the resolver never shells out, the collector never
decides which gates run, the executor never re-derives what the resolver
already decided:

| Layer | Entry point | Impure? | Job |
|---|---|---|---|
| Collect | `collect_pre_commit_changes()` / `collect_pre_push_changes(stdin)` | Yes — the only place the fail-closed `_run_git`/`_git_diff_paths_z` helpers are called | Turn Git's own diff output into a `ChangeSet`. Decides nothing about which gates run. |
| Resolve | `resolve_gates(phase, changes)` | No — pure and total | Turn `(phase, ChangeSet)` into the tuple of `Gate`s to run, in `GATES` declaration order. No I/O, no env, no clock, no subprocess. |
| Execute | `execute_gates(phase, changes)` | Yes — runs subprocesses | Call `resolve_gates` exactly once, then run each selected gate with a hardened `subprocess.run`, printing one line per gate (`run` / `skip (...)` / `fail (...)`) and stopping at the first non-zero exit. |

One more Git call site exists outside the hardened path above: `main()`
falls back to `_pre_push_manual_upstream_changes()` when a manual, direct
`python3 scripts/run_git_hook.py pre-push` invocation (no piped ref-update
stdin — never the real `git push`-invoked hook) yields a genuinely empty,
non-`unknown` `ChangeSet`. That fallback resolves `@{u}` and diffs it
through the separate, unhardened `git()` helper, not `_run_git`/
`_git_diff_paths_z`. That's deliberate, not an oversight: it's a
best-effort convenience for invocation outside Git's stdin contract, kept
unchanged from the pre-refactor fallback, and is not part of the
fail-closed contract this section otherwise describes.

#### The `ChangeSet` fail-closed interface

```python
@dataclasses.dataclass(frozen=True)
class ChangeSet:
    paths: tuple[str, ...]
    unknown: bool
    reason: str | None
```

- `paths=()` with `unknown=False` means **genuinely no changes** — nothing
  staged (pre-commit) or nothing in the pushed range (pre-push). It never
  means "collection broke."
- `unknown=True` means the collector could not determine the real change set
  — a Git subprocess call failed, pre-push stdin was malformed, a sha field
  wasn't hex, etc. `resolve_gates` treats `unknown=True` as an automatic
  escalation: **every gate declared for the phase runs**, regardless of
  `paths`. `reason` is always non-empty in this case, and `execute_gates`
  prints it first (`[git-hook] escalating: <reason>`) so a surprisingly full
  run explains itself instead of looking arbitrary.
- The subprocess boundary itself is fail-closed: `_run_git`'s success flag is
  False only when the `git` call could not be made at all (missing binary,
  undecodable output) — never based on git's own exit code, which callers
  interpret themselves — and any such failure always becomes
  `ChangeSet(unknown=True, ...)`, never a raised exception out of collection.

#### Classification table

`classify_path(path)` is pure and total (never raises, regardless of input
shape) and returns one of:

| Surface id | Predicate | Matches |
|---|---|---|
| `rust_workspace` | `is_rust_path` | `.rs` files, `Cargo.toml`/`Cargo.lock`/`build.rs`, anything under `.cargo/` |
| `collab_protocol` | `is_collab_protocol_path` | collab command/prompt/template files (`COLLAB_EXACT_PATHS`, `.claude-plugin/prompts/collab-turn-*`, `tests/collab_turn_templates/`) |
| `hook_self_test` | `is_hook_path` | the tracked hook scripts themselves — an exact-set membership check (`HOOK_EXACT_PATHS`), not a prefix: `.githooks/pre-commit`, `.githooks/pre-push`, `scripts/install-git-hooks.sh`, `scripts/run_git_hook.py`, `scripts/test_run_git_hook.py`. Contrast `collab_protocol` below, whose `collab-turn-*` genuinely is a prefix match. |
| `docs` | `is_docs_path` | any `.md` file, or any path whose leading `/`-split segment is `docs` |
| `UNKNOWN` | *(fallback — not in `SURFACES`)* | an unsafe-shaped path, or a path that matches no declared surface |

`docs` is a declared, first-class entry in `SURFACES` — **not a fallback** —
and that distinction is the point of the feature: an all-docs, all-safe-shape
change classifies cleanly to `docs` and selects only `always` gates (none
exist in today's manifest), which is cheaper and more precise than the
`UNKNOWN` path. `UNKNOWN` is the true fallback, returned only when a path is
unsafe-shaped (absolute, contains a `..` segment, a control byte other than
`\n`, empty, starts with `-`, or isn't even a `str`) or matches no entry in
`SURFACES` at all — and, like `changes.unknown`, it escalates `resolve_gates`
to running every gate for the phase.

#### Byte-exact paths — why stripping is forbidden

Every Git invocation in the collection layer that returns paths uses `-z`
(NUL-delimited) output (`_default_base`'s `symbolic-ref`/`merge-base` calls
are the exceptions — they return a ref name and a sha, never a path list,
so byte-exactness doesn't apply and they don't pass `-z`). That removes
`core.quotepath` escaping at the source, and —
critically — **a newline is a legal byte inside a Git filename**: with NUL as
the only delimiter, there is no delimiter newline to strip, so one is never
stripped. `_split_nul` drops exactly one trailing empty field (`-z`'s own
output framing, not path content) and touches nothing else.

`classify_path` and every surface predicate never call `.strip()`, unquote,
or case-fold a path. Rewriting an attacker-influenced path before classifying
it would let classification disagree with what Git actually staged — matching
byte-exact means what gets classified is what Git reported, never a
cleaned-up guess at it. Unsafe shapes are rejected straight to `UNKNOWN`
(which escalates to running every gate) — **never sanitized and
reclassified**.

#### How to add a gate

Two steps, nothing else:

1. Append a `Gate(...)` entry to the `GATES` tuple in
   `scripts/run_git_hook.py`.
2. If the gate declares a surface that has no existing example, add one to
   `_SURFACE_EXAMPLE_PATH_FOR_TEST` in `scripts/test_run_git_hook.py`.

`test_resolve_gates_reaches_every_manifest_gate` derives its parametrized
cases directly from `GATES` — it is not a hand-maintained list of gate names.
A gate added without a matching `_SURFACE_EXAMPLE_PATH_FOR_TEST` entry fails
that test loudly with a `KeyError` naming the missing surface, rather than
silently going unexercised. `GATES` order is execution order (declaration
order) and is never sorted at runtime.

#### The argv-literal rule

`Gate.argv` entries are string literals only, e.g.
`("cargo", "clippy", "--workspace", ...)`. No caller ever interpolates a
Git- or path-derived value (a changed filename, a sha, a branch name) into a
gate's `argv`. `execute_gates` runs
`subprocess.run(list(gate.argv), shell=False, ...)` with the argv taken
verbatim from the manifest, so the set of commands the hook can ever run is
fixed at manifest-authoring time — never influenced by what's in the diff.

#### The `GIT_*` environment scrub

`execute_gates` runs every gate with `env=` built by `_scrub_git_env`, which
drops every `GIT_*`-prefixed variable except an explicit keep-list
(`GIT_ASKPASS`, `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_TERMINAL_PROMPT`,
`GIT_TRACE*`). This exists because a pre-push hook exporting
`GIT_DIR`/`GIT_INDEX_FILE`/`GIT_WORK_TREE` let a `cargo test` tempdir Git
fixture inherit them and commit into the real repository (PR #186) — those
variables redirect a child Git invocation at a *different* repo/worktree/
index, and are always stripped.

`GIT_CONFIG_*` is deliberately **not** on the keep-list. `GIT_CONFIG_COUNT` +
`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` are the documented equivalent of
`git -c <key>=<value>` for arbitrary config — including `core.worktree`, the
config equivalent of `GIT_WORK_TREE` that this same scrub strips above. A
caller could otherwise set `GIT_CONFIG_COUNT=1
GIT_CONFIG_KEY_0=core.worktree GIT_CONFIG_VALUE_0=/real/repo` and reproduce
exactly the redirection this scrub exists to prevent, through the front
door. The keep-list is an allowlist, not a denylist: anything `GIT_*`-
prefixed that isn't explicitly named or prefix-matched is dropped, including
variables not yet invented.

#### Staged deletions are in scope

`collect_pre_commit_changes` runs `git diff --cached --name-only -z` with no
`--diff-filter`. `--diff-filter=ACMRTUXB` (which would exclude deletions) is
deliberately absent — deleting a `.rs` file or `Cargo.toml` is exactly the
kind of change that should still trigger the Rust gates. This is a ratified
decision, not a lost flag.

## Manual Codex MCP Setup

Add a server entry to your Codex MCP config.

Example `~/.codex/config.toml` fragment (one in-process server per client —
always works, no other moving parts):

```toml
[mcp_servers.ironmem]
command = "/absolute/path/to/.ironrace/bin/ironmem"
args = ["serve"]

[mcp_servers.ironmem.env]
IRONMEM_MCP_MODE = "trusted"
```

If Codex shares this repo with other MCP clients (Claude Code, a dashboard,
…), point Codex at the shared daemon proxy instead so they all use one
DB/embedding-model:

```toml
[mcp_servers.ironmem]
command = "/absolute/path/to/.ironrace/bin/ironmem"
args = ["serve", "--connect", "/absolute/path/to/.ironrace-memory/hook_state/daemon.sock"]

[mcp_servers.ironmem.env]
IRONMEM_MCP_MODE = "trusted"
```

The daemon is spawned automatically on first connect (single-flight, so
multiple clients racing to start it still converge on one), and shuts itself
down after `IRONMEM_DAEMON_IDLE_SECS` (default 300s) of no connections. `ironmem
codex` already writes this form for you, and upgrades a pre-existing bare
`["serve"]` entry in place. See
[Shared Daemon Mode](../README.md#shared-daemon-mode) in the main README for
the full flag/env-var reference, the fallback guarantee, and security notes.

**Access mode is daemon-process-global, not per-client.** `IRONMEM_MCP_MODE`
is read once, from whichever process's environment happened to spawn the
shared daemon first. Every OTHER client that later connects to that same
daemon gets that mode too, even if ITS OWN `IRONMEM_MCP_MODE` env differs —
there is currently no per-connection access-mode override. If you need
different clients to have different access modes, give them separate sockets
(`IRONMEM_DAEMON_SOCKET` or `--listen`/`--connect` pointed at distinct paths)
rather than relying on per-client env with a shared daemon. Because access
mode cannot be scoped per-client, review sub-agents get their lean tool
profile client-side instead; see `docs/REVIEW_AGENT_PROFILE.md`.

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
- `IRONMEM_DAEMON_SOCKET` overrides the shared daemon's default socket path
  (`<state_dir>/daemon.sock`, i.e. `~/.ironrace-memory/hook_state/daemon.sock`).
- `IRONMEM_DAEMON_IDLE_SECS` (default `300`) — seconds an idle shared daemon
  (zero active connections) waits before shutting itself down.
- `IRONMEM_NO_DAEMON` — set to any value other than empty/`0`/`false`/`no` to
  disable `serve --connect`'s auto-spawn (equivalent to always passing
  `--no-autospawn`); see [Shared Daemon Mode](../README.md#shared-daemon-mode).

## Codex Packaging Gap

`ironmem` now ships a `.codex-plugin/` directory with:

- `plugin.json`
- `hooks.json`
- `commands/collab.md`, the Codex `/collab` slash-command shim
- wrapper scripts for the MCP server and hooks
- Codex-specific README content
- protocol prompts under `prompts/`
- bundled collab skill dependencies under `skills/`

The shared collab skill dependencies are bundled for Claude Code under `.claude-plugin/skills/`.
`scripts/install-ironmem.sh` installs the Codex command into `$CODEX_HOME/commands`
(default `~/.codex/commands`), the Codex protocol prompts into `$CODEX_HOME/prompts`
(default `~/.codex/prompts`), the Codex skills into `$CODEX_HOME/skills`
(default `~/.codex/skills`), and the Claude copies into `$CLAUDE_HOME/skills`
(default `~/.claude/skills`).
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

Existing identical files are skipped. The installer records hidden packaged
baselines under each target root's `.ironmem-bases/` directory; on later
installs it three-way merges packaged updates into locally edited skills,
agents, commands, and prompts. If no baseline exists, the target is a symlink,
or a merge conflict occurs, the local file is left unchanged and the packaged
update is written next to it as `*.ironmem-packaged` (conflict drafts use
`*.ironmem-merge-conflict`). `--skip-skills` skips this step entirely.
For Claude Code, the installer also provisions the `code-reviewer` agent used by the vendored
`subagent-driven-development` review flow into `$CLAUDE_HOME/agents`.

### Codex model defaults

The bundled collab protocol uses explicit phase-based Codex routing: Luna at
`max` for implementation controllers/workers, Luna at `medium` for
exploration/docs/mechanical work, and Terra at `high` for planning and normal
review. Sol at `high` is reserved for an explicit architecture/security or
other high-risk escalation. Protocol dispatches pass the model and effort
explicitly instead of inheriting the caller's personal default.

The installer does not modify `$CODEX_HOME/config.toml` or user-defined agent
roles. Those personal settings remain available for ordinary Codex sessions;
the collab dispatcher and bundled Superpowers guidance carry the repository
defaults themselves.

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

`ironmem` uses a two-model flow for memory protocol rules:

- **Model A (canonical):** the source content is the
  `MEMORY_PROTOCOL` constant in `crates/ironmem/src/bootstrap.rs`, stamped as a
  managed block in `AGENTS.md`.
- **Model B (dependent):** harness-specific strategies derive dependent targets from
  the canonical rules via strategy-specific propagation (`Native`, `Import`, `Copy`).

`codex` uses Model A directly (native strategy), so the dependency file is `AGENTS.md` itself:

```bash
ironmem write-rules --harness codex
```

Codex's `AGENTS.md` after write-rules includes the canonical managed block:

```markdown
<!-- BEGIN IRONMEM MEMORY PROTOCOL -->
<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->
Before answering questions about prior work, decisions, project history, or people, check search or KG tools first. Write important durable decisions back to memory. For mutable current task/project context, use add_drawer with logical_key so the latest state overwrites stale copies instead of accumulating forever. Treat collab-plans, collab-task-lists, and collab-checkpoints as operational artifacts; prefer compact durable summaries for long-term recall and prune stale operational drawers with ironmem memory gc --dry-run before --apply.
<!-- END IRONMEM MEMORY PROTOCOL -->
```

This is explicit opt-in only; no hook or plugin path runs `write-rules` automatically.

## Memory Lifecycle

Use durable, append-only memory for decisions and facts. Use replaceable memory
for current task or project state:

- `add_drawer` without `logical_key` remains content-addressed and append-like.
- `add_drawer` with `logical_key` is key-addressed by
  wing/room/logical_key, so a later write updates the same drawer. This is the
  preferred shape for "current context" that would otherwise go stale.

Operational collab rooms are not permanent knowledge:

- `collab-checkpoints`
- `collab-plans`
- `collab-task-lists`

Run `ironmem memory gc --dry-run` to inspect stale operational drawers. Deletion
requires `ironmem memory gc --apply`. The default policy deletes checkpoint
candidates older than 60 days and unreferenced plan/task-list candidates older
than 180 days; linked plan/task-list drawers are skipped to preserve historical
collab session lookups.

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

## Symbol/Import Graph

ironmem includes a **local, offline code index** for Rust and Python sources
(migration 012, schema v12). The index is stored in the same SQLite database as
the rest of memory — no extra process or network required.

### Supported languages (v0)

**Rust (`.rs`) and Python (`.py`) only.** Other extensions are skipped; they do
not cause errors. Supported files larger than 1 MiB are skipped with a warning.
TypeScript, JavaScript, Go, etc. are explicitly unsupported in v0.

### Persisted metadata

Only declaration metadata is stored — **no full source bodies**:

| Field | Stored? | Notes |
|-------|---------|-------|
| Symbol kind, name, qualified name | Yes | `fn`, `struct`, `class`, etc. |
| Declaration signature | Yes, ≤ 512 bytes | Declaration header with inline bodies stripped |
| Span (start line, col; end line) | Yes | |
| Visibility | Yes | `pub`, `pub(crate)`, `private`, etc. |
| Import module, symbol, alias | Yes | |
| Raw import line | Yes, ≤ 512 bytes | |
| Full source file content | **No** | Never stored |

### Edge scope (v0)

| Edge kind | Meaning |
|-----------|---------|
| `import` | file → module (from `use`/`import` statements) |
| `contains` | symbol → parent symbol (nested items) |

Cross-symbol call/reference resolution is **not available in v0** — the
`code_symbol_edges` table is created for forward-compatibility but only
`import` and `contains` edges are emitted by the indexer.

### MCP access-mode implications

| Tool | Mode requirement |
|------|-----------------|
| `symbol_graph_index` | **write-mode only** (`IRONMEM_MCP_MODE=trusted`) |
| `symbol_lookup` | read-mode allowed (ReadOnly, Restricted, Trusted) |
| `symbol_imports` | read-mode allowed |
| `symbol_neighbors` | read-mode allowed |

All read tools enforce a hard result cap of **100 items** per call to prevent
unbounded responses. Raw FS/git errors are never returned to the client;
they are `eprintln!`'d server-side and the client receives a generic
validation error.

### CLI quick-start

```bash
# Index (incremental; re-runs only changed files by content-hash)
ironmem symbols index --repo /path/to/repo

# Look up symbols
ironmem symbols lookup --repo /path/to/repo "parse_file"
ironmem symbols lookup --repo /path/to/repo --kind struct "Config"

# Imports
ironmem symbols imports --repo /path/to/repo "std::collections"

# Edges
ironmem symbols neighbors --repo /path/to/repo "src/lib.rs"

# JSON output for scripting
ironmem symbols index --json --repo /path/to/repo
```

## Benchmark Caveats

- `ironmem` uses a Rust ONNX embedding path; `mempalace` uses Python and Chroma
- The harness sets `IRONMEM_AUTO_BOOTSTRAP=0` and `IRONMEM_DISABLE_MIGRATION=1` automatically so one-time bootstrap cost is excluded from latency measurements; warmup time (model load) is tracked separately
- Storage is measured after a SQLite WAL `TRUNCATE` checkpoint for a fair comparison with Chroma-backed backends
- File mining is excluded — the benchmark targets common MCP tool surfaces only, because the two mining pipelines differ too much for a controlled comparison
- Search uses 5x overfetch (min 30 candidates) to maintain recall when needle documents are diluted by unrelated context

## Recommended Next Work

1. Extend benchmark coverage with larger datasets and repeated warm-cache runs
