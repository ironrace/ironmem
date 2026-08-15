# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`collab_set_implementer` is now restricted to the session's current pilot
  (#264).** The tool previously gated only on phase, so either agent could
  rebind the implementer; it now runs the same caller-identity check as
  `collab_set_pilot`, before the phase gate, and rejects with
  `collab_set_implementer refused: caller '<agent>' is not the pilot of this
  session; only the current pilot '<pilot>' may reassign the implementer`. A
  copilot that needs the implementer changed must ask the pilot to make the
  call. Like `collab_set_pilot`, the check is caller-*asserted*: it stops an
  honest client from taking a turn it does not own, and is not process-bound
  authentication. Both `/collab` command templates now check the current pilot
  before attempting the call rather than letting the server refuse it.
  `collab_start` and `collab_start_code_review` additionally reject non-string
  `pilot`/`implementer` values (including explicit `null`) instead of silently
  falling back to the `claude` default, and a new session's `current_owner` is
  seeded from the resolved pilot rather than hardcoded to `claude`. See
  § Authorization / Phase / Ownership Matrix in `docs/COLLAB.md`.

### Added

- **Schema-enforced collab checkpoints and a head-consistency gate on
  `implementation_done` (#273).** Schema goes **v19 -> v21** (migrations 020
  and 021), adding a `collab_checkpoints` table that stores one row per
  session: reporting agent, status, `task_id`, `completed_task_ids`,
  `head_sha`, `gates_sha`/`gates_result`, and (for operator-attested rewrites)
  `attested_by`/`acknowledged_divergence`/`attestation_check`. A new
  `collab_checkpoint` MCP tool writes and upserts this row. `implementation_done`
  now hard-gates on `require_checkpoint_proof`: every task id the accepted task
  list declares must be covered by `completed_task_ids`, and the checkpoint's
  `head_sha` must equal the head the call reports, with gates recorded green at
  that same sha, before the call is admitted. The comparison against **live**
  git HEAD is a separate layer, on the `session_handoff` and `collab_resume`
  paths and at the checkpoint write — which is also where an operator
  attestation's `acknowledged_divergence` is resolved against the repository
  and labelled `verified` / `verified_without_span` /
  `unverified_repo_unreadable`. The legacy convention of
  recording batch progress in an `add_drawer` note is retired in favor of this
  schema-enforced record; `collab_status`, `collab_resume`, and the
  `session_handoff` block all render the checkpoint's state. See
  `docs/COLLAB.md` for the full checkpoint/attestation semantics.

- **Opt-in hybrid prompt recall (#235).** The UserPromptSubmit hook can ask an
  already-running shared daemon for vector candidates over its Unix socket and
  fuse them with local BM25 through weighted RRF. Off by default; enable with
  `IRONMEM_PROMPT_RECALL_HYBRID`, budget it with
  `IRONMEM_PROMPT_HOOK_HYBRID_BUDGET_MS` (default `60`), and cap candidates
  with `IRONMEM_PROMPT_HOOK_HYBRID_LIMIT` (default `5`, clamped `1`–`10`). The
  hook never initializes, spawns, or warms a daemon and never loads an
  embedder; the request runs on an abandonable worker with its own absolute
  deadline, and every failure falls back to the byte-identical local
  BM25/KG/diary result. Vector IDs are filtered against locally fetched drawers
  before fusion, so a daemon serving another database cannot consume injection
  slots.

### Documentation

- Documented all UserPromptSubmit recall knobs, defaults, clamps, opt-in and
  fallback behavior, ready-daemon-only hybrid semantics, and the possibility
  of shared-daemon LLM reranking work and cost continuing after hook fallback.

## [0.5.1] - 2026-07-29

### Added

- **Drawer supersession and advisory dedup hints (#211).** Schema **v17** adds
  `drawers.superseded_by` plus a partial `(wing, room) WHERE superseded_by IS
  NULL` index. `add_drawer` accepts `supersedes` (a current drawer in the same
  wing/room) and marks the predecessor inside the same transaction; the body is
  retained and still retrievable by ID. `search` accepts `include_superseded`
  (default `false`) and labels history rows with `superseded_by`; `get_drawer`
  returns `superseded_by` for any drawer. `add_drawer` responses may carry an
  advisory `dedup_hint` (`{id, score}`), and `dedup_hint_status: "unavailable"`
  when the check could not run. Supersession refuses collab-referenced and
  synthetic enrichment drawers, rejects a successor that is itself superseded
  (which would hide an entire lineage), reconnects a predecessor to its
  surviving successor (or restores it when the final successor is deleted),
  and treats re-filing a superseded body as a
  resurrection rather than a durable-but-hidden write.

### Fixed

- **Scoped BM25 search returned no lexical results (#211).** All three
  wing/room-scoped FTS5 branches referenced the table through an alias, which
  FTS5 rejects for `MATCH`/`bm25()`; the failure was swallowed at `debug` level
  and silently degraded scoped search to vector-only. The room-only branch also
  bound three parameters against SQL referencing `?4`. Both are fixed and
  covered for all four wing/room combinations, and a prepare failure that is
  not a missing FTS table now propagates instead of looking like zero hits.

- **Deterministic `/collab` MCP-response distributions and offline baseline gate
  (#212).** `ironmem report --json` now exposes p50/p95/max response-size
  distributions for every harness/tool group. `scripts/collab_baseline.py`
  captures and checks a committed JSON baseline without a live model or network;
  malformed schemas, missing groups, and p95 regressions fail closed. The
  committed reference is explicitly MCP-response sizing only because the
  available real session had no measured non-MCP collab token rows.

- **Uniform lean MCP profile for review sub-agents (#189).** Read-only review
  agents (`/ultrareview-local` lenses, `pr-review-toolkit` reviewers) now share
  one canonical tool profile — `Read`/`Grep`/`Glob`/`Bash`, no `mcp__ironmem__*`
  tools — documented in `docs/REVIEW_AGENT_PROFILE.md`. Claude Code enforces it
  per-agent via `tools:` frontmatter (the `code-reviewer` agent no longer
  inherits the full memory surface), guarded by a `plugin_metadata` regression
  test. Codex/Grok/Gemini, whose MCP surface is global rather than per-agent,
  rely on the reviewer-brief guard plus #190's thin proxies so the attached
  surface stays cheap.
- **Shared-daemon mode for `ironmem serve` (#190).** `serve --listen <socket>`
  runs as a long-lived daemon binding a Unix socket; `serve --connect <socket>`
  is a thin proxy that pumps bytes between the harness's stdio and that
  daemon, starting in milliseconds regardless of daemon startup cost. If no
  daemon is listening, `--connect` single-flight auto-spawns one under an
  atomic lockfile (`<socket>.lock`) so many clients launched at once converge
  on exactly one shared daemon and one shared database, instead of one
  `ironmem` process per client. Falls back transparently to the original
  in-process stdio server when `--no-autospawn` is set (or
  `IRONMEM_NO_DAEMON` is truthy) and no daemon is reachable. New env vars:
  `IRONMEM_DAEMON_SOCKET` (override the default `<state_dir>/daemon.sock`),
  `IRONMEM_DAEMON_IDLE_SECS` (override the 300s idle-shutdown window), and
  `IRONMEM_NO_DAEMON` (disable auto-spawn). `ironmem doctor` gained a daemon
  health probe and per-harness proxy-wiring checks.
- **`IRONMEM_WRITE_READINESS_TIMEOUT_SECS` env var.** Bounds how long a
  write-shaped MCP tool waits for startup readiness before giving up and
  returning `isError: true`. Defaults to 90s — generous enough to cover a cold
  ONNX model load plus bootstrap, short enough that a wedged startup surfaces
  as an error rather than an indefinite hang. Clamped to a 24-hour maximum;
  unparseable values fall back to the default.
- **`ironmem grok` / `ironmem gemini` launchers.** Registry-backed launchers
  for Grok CLI and Gemini CLI alongside the existing Claude/Codex launchers,
  all registering the shared-daemon proxy command by default.
- **Fresh MCP registrations now default to the shared-daemon proxy command**
  (`serve --connect <socket>`) instead of bare `serve`, for Claude, Codex,
  Gemini, and Grok. A pre-existing bare `["serve"]` entry from before this
  change is upgraded in place on the next registration; anything else
  (already-upgraded or hand-customized) is left untouched. Bare `serve`
  itself remains a fully-supported fallback.

### Changed

- **Breaking: `collab_status` returns plan and task-list references only
  (#207).** `verbose:true` no longer inlines accepted plan bodies; plan refs
  now carry `{drawer_id, hash, plan_file_path}` so bridge workers verify the
  approved file directly. `include_task_list:true` now returns the compact
  `{drawer_id, hash}` reference instead of task-list JSON; callers load the
  manifest through `get_drawer` and verify its hash.
- **Breaking: `search` response shape changed (#213).** The default response
  changed from `content` to `excerpt` + `content_mode`, with a stable `id`
  reference. Migrate by passing `full:true` for bounded full-content search
  results (subject to existing per-field and aggregate response caps), or by
  calling `get_drawer` with the result `id` for the complete body.
- **`AGENTS.md` is now canonical for harness rules.** `ironmem write-rules`
  treats `AGENTS.md` as the single source of truth and fans dependent harness
  files out from it via registry-backed `Native`/`Import`/`Copy` strategies.
  Note the migration behavior: selecting a non-native target (e.g.
  `--target CLAUDE.md` or `--harness claude`) also creates/updates `AGENTS.md`
  and rewrites the managed block in the dependent file down to an `@AGENTS.md`
  pointer. User content outside the managed markers is preserved; content
  *inside* the block is migrated.
- **Partial multi-file writes are now surfaced.** When a fan-out write fails
  after the canonical file was already written, the error names both the failed
  file and the already-updated files and prompts a re-run to reconcile, instead
  of discarding the partial-state report.

### Fixed

- **Write-shaped MCP tools no longer silently discard writes during warm-up.**
  `add_drawer`, `diary_write`, and `code_map_write` previously returned the same
  soft `{"warming_up": true}` body as `search` while startup was still in
  progress. That body is success-shaped, so a client saw an OK result for a
  write that never happened, and the memory was simply lost — the failure mode
  the flaky `daemon_autospawn_race` test was surfacing. These tools now block on
  a readiness gate and then perform the real write, or return `isError: true` if
  readiness resolves failed or the wait times out. A success-shaped result is
  once again equivalent to "the write happened." Requests that do not depend on
  readiness at all — unknown tool, mode rejection, malformed arguments — are
  still rejected up front rather than waiting out the timeout first.
- **Startup readiness now has a terminal failed state, and reads report it
  honestly.** Readiness was a bool, so a server that failed terminally at
  startup was indistinguishable from one still warming up: `search` kept
  returning an empty result set with "results will be available shortly", and a
  client following the documented poll-until-`warming_up: false` loop would spin
  forever against a server that was never coming up. Readiness is now a
  three-state gate (pending / ready / failed, first resolution wins) — `search`
  returns `isError: true` on a failed gate instead of the soft body, and
  `status`, which stays answerable in every state because it is the diagnostic
  endpoint, gained `readiness` (`"ready"` / `"warming_up"` / `"failed"`) and
  `readiness_error` alongside the retained `warming_up` bool. Startup error
  detail is sanitized into a client-facing reason rather than forwarded raw.
- **A warm-up write no longer head-of-line blocks later requests on the same
  connection.** Requests on a single connection are now pipelined, for both the
  stdio transport and daemon connections, so a write parked on the readiness
  gate no longer stalls every subsequent request behind it for the full
  readiness timeout — reads stay serviceable while a write waits. Responses may
  arrive out of request order; JSON-RPC 2.0 permits this and clients match
  responses to requests by `id`. Waiters park without consuming a blocking-pool
  thread each, so many concurrent parked writes cannot starve the runtime.
- **Writes hidden behind read-shaped tool names are now classified as writes.**
  Whether a call mutates is decided from its ARGUMENTS, not just its tool name.
  `collab_recv` with `auto_ack: true` acks every message it returns, and any
  collab call carrying a `handoff_token` claims the generation lease — both
  persist state while being named like queries. The two had different
  consequences, and only one was a mode-gating hole:
  - `auto_ack: true` was genuinely permitted under `IRONMEM_MCP_MODE=read-only`
    and wrote there. That bypass is now closed.
  - `handoff_token` was already refused in read-only mode by the guard in
    `ensure_actor_generation_current`, so there was **no** mode bypass to fix.
    What it did escape was the per-connection ordering barrier: classified as a
    read, a lease claim could overtake a write still parked on the readiness
    gate.

  Both are now gated and ordered as the writes they are. Plain `collab_recv`
  remains a read; only the write-triggering argument is refused, and refused
  explicitly rather than silently downgraded.
- **Overflowing the write backlog no longer breaks write ordering.** When more
  than 64 writes were queued on one connection the overflowing write was
  rejected, but writes arriving behind it still ran — so a `delete_drawer` could
  land without the `add_drawer` it was meant to follow, the exact inversion the
  ordering barrier exists to prevent. A rejection now blocks that connection's
  later writes until the backlog drains, making the documented guarantee literal:
  no mutation executes after a mutation that was refused. Reads are never
  refused by this rule and keep being answered throughout. The two refusals
  carry distinct messages so a client can tell which write broke its sequence.
- **A `collab_wait_my_turn` long poll no longer freezes the whole daemon.**
  The tool polls for up to 60 seconds, and it did so with `std::thread::sleep`
  inside the synchronous dispatch path — which, as the daemon's own design notes
  state, "stalls this thread, and with it every connection, for its duration."
  One agent waiting its turn therefore blocked every other connected client for
  up to a minute, violating the "dispatch is short" assumption the daemon's
  lock-free single-owner model is built on. The generation claim and each
  snapshot read remain short synchronous steps, but the wait between them is now
  asynchronous and yields the thread, so unrelated requests are served while an
  agent waits. Timing out is measured from when the request arrived, so queueing
  no longer extends the client's requested timeout.

## [0.5.0] - 2026-07-07

### Added

- **Memory lifecycle management.** Added `ironmem memory gc` with dry-run and
  apply modes for pruning stale operational/collab drawers while preserving
  referenced plan/task artifacts. The retention policy deletes old unreferenced
  operational drawers conservatively and reports planned actions as JSON for
  automation.
- **Mutable current-context drawers.** `add_drawer` now accepts a
  `logical_key`, allowing agents to overwrite durable "current context" notes
  instead of accumulating stale copies forever.
- **Shared memory protocol guidance.** Updated Codex, Claude, README, and
  project-agent docs so both harnesses proactively use ironmem for prior-work
  recall and write back durable summaries in the right memory surface.
- **`/ultrareview-local` hardened for bug-catching.** The orchestrator now
  captures the full diff text up front (including untracked files via
  `git add -N` in local mode), greps diff *content* — not just filenames — for
  conditional-agent triggers, and adds two new conditional lenses: a
  concurrency reviewer (races, TOCTOU, non-atomic read-modify-write,
  non-idempotent retries) and a performance reviewer (N+1, unbounded queries).
  Every agent brief now carries a shared output contract requiring a concrete
  failure scenario for CRITICAL/HIGH findings and a >80% confidence floor, plus
  a blast-radius requirement (verify callers of changed public symbols, with
  ironmem symbol-graph tools when available). A new adversarial verification
  phase (5.5) dispatches parallel verifiers that try to refute each
  CRITICAL/HIGH before it can drive the verdict; refuted findings are reported,
  not silently dropped. PR-mode validation now refuses to run tests when the
  working tree isn't at the PR head. Core agents gained matching modes:
  `code-reviewer` and `architect` get explicit diff-review modes (bug hunting
  instead of plan alignment / system design; `architect` gains read-only Bash
  so it can compute the diff), and `security-reviewer` gets multi-ecosystem
  scanners (`cargo audit`, `pip-audit`, `bandit`, `govulncheck`, `gitleaks`)
  and an explicit read-only rule under review commands.
- **Install script now bundles the full `/collab` review surface.**
  `scripts/install-ironmem.sh` installs `/ultrareview-local` (previously only a
  preflight warning pointed at a separate plugin) plus its three core review
  agents (`security-reviewer`, `architect`, `doc-reviewer`) alongside the
  existing `code-reviewer`. It also installs the eight `collab-turn-*.md`
  worker prompt templates to `~/.claude/prompts/`, and `/collab` now resolves
  templates repo-relative first with a fallback to that installed copy — so
  `/collab` can run against repos other than an ironrace-memory checkout. The
  `/ultrareview-local` preflight warning is removed as obsolete.

### Fixed

- **Linux CI runner stability.** GitHub Actions Linux jobs now pin
  `ubuntu-24.04` instead of the moving `ubuntu-latest` alias, and Linux-only
  steps key off `runner.os`.
- **Prompt-hook test flake.** Prompt-hook tests that depend on environment
  tunables now serialize those tunables before asserting deterministic recall
  output, fixing the intermittent Linux failure seen before this release.
- **`/collab` final review no longer re-runs the full gate suite after a
  successful push.** `CodeReviewFinalPending` now performs a cheap
  pushed-head proof (clean worktree, `HEAD == last_head_sha`, and local HEAD
  equal to upstream/origin) and uses the successful `review_local` push as gate
  evidence for that exact commit. If proof fails, the worker blocks for
  branch-drift triage instead of burning another full lint/typecheck/test/build
  cycle.
- **`/collab start` no longer records `main`/`master`/`trunk` as the
  session's branch.** The `branch` field is fixed at `collab_start` time
  with no update API. Previously, if `start` ran from the default branch
  (or a detached HEAD) — e.g. the user branches off manually right after
  starting the session — every later turn that trusts
  `collab_status.branch`, including Codex's pre-send harness
  (`git checkout <branch>; git reset --hard <last_head_sha>`), would check
  out and hard-reset local `main` to the session head, and the next push
  would land straight on `main`, bypassing PR review entirely. This is
  exactly what happened in a live session: Codex's global review turn pushed
  an unreviewed 10-commit security-hardening branch directly to `origin/main`
  with no PR. `start` now checks the current branch first: if it's already
  something other than `main`/`master`/`trunk` (e.g. from `using-git-worktrees`,
  or the user branched manually), it's used as-is — no redundant branch or
  worktree. Otherwise, `start` derives a `collab/<task-slug>` branch name
  (deduplicated against existing local/remote refs) and creates it inside a
  new **isolated git worktree** (same directory-selection convention as the
  `using-git-worktrees` skill: `.worktrees/` preferred, `CLAUDE.md`
  preference, else default — never an interactive ask), so the session's
  git operations can never collide with whatever the user's own terminal has
  checked out. The session's lifecycle ends at `CodingComplete`, before a
  human merges the PR, so collab can't observe the merge and never cleans
  the worktree up automatically; the terminal-phase report to the user now
  names the worktree path and points at the `engineering:git-worktree-manager`
  skill's `worktree_cleanup.py` (or `git worktree remove <path>`) as the
  manual follow-up once the PR merges. Fixed in lockstep across
  `docs/COLLAB.md`, `.claude-plugin/commands/collab.md`, and
  `.codex-plugin/prompts/collab.md`.

## [0.4.0] - 2026-06-22

### Changed

- Collab planning now skips the extra post-review planning loop: Claude merges
  the two blind drafts, Codex gets exactly one plan-review pass, then Claude
  asks for human approval only on the final Superpowers task plan. The
  PlanLocked bridge now parses that approved plan mechanically into `task_list`
  entries, and every task must be timeboxed to 20 minutes or less.
- **`ironrace-rerank` 0.3.4 → 0.4.0 (breaking, workspace-internal):**
  `LlmClient::call` now returns `Result<LlmResponse>` instead of
  `Result<String>`. `LlmResponse` carries the assistant `text` plus token
  `usage`, `cost_usd`, `model`, an `estimated` flag, and `prompt_chars`. New
  public types: `Usage`, `LlmResponse`, `RerankScoreResult`, `RerankScoreError`;
  `RerankerScorer::score_pairs` now returns `RerankScoreResult`. Both backends
  (`ClaudeCliClient`, `AnthropicApiClient`) parse real token counts (the CLI
  falls back to a chars/4 estimate, flagged `estimated`).
- CI now keeps `ironrace.dev` and the README in sync when user-facing features
  land, via a drift guard (issue #160).

### Added

- **`ironmem doctor` setup diagnostics (issue #142):** a new diagnose-only CLI
  command that validates the local install in one shot. It reports binary
  version, database path + schema/migration status, embedding-model cache
  status, MCP access mode, warmup readiness, and which harnesses (Claude Code
  via `~/.claude.json`, Codex via `$CODEX_HOME/config.toml`) have the `ironmem`
  MCP server registered — without requiring either to be installed. Each line is
  `[ OK ]`/`[INFO]`/`[WARN]`/`[FAIL]`; the command distinguishes blocking errors
  from warnings and exits non-zero **only** on a blocking setup failure.
  Supports `--json` for toolable output and never modifies user config. New
  public APIs: `ironmem::doctor` module, `Database::schema_version()` +
  `db::schema::LATEST_SCHEMA_VERSION`, and `ironrace_embed::embedder::{model_status, ModelStatus}`.
- **UserPromptSubmit FTS-injection hook (Claude Code):** a new `user-prompt-submit`
  hook runs an FTS/BM25-only drawer search on **every** prompt and injects up to 3
  sanitized one-line untrusted-memory excerpts via
  `HookResponse.hookSpecificOutput.additionalContext`, under a hard wall-clock
  `IRONMEM_PROMPT_HOOK_BUDGET_MS` budget (default 150 ms, capped 1000). The embedder
  is **never** loaded (no `App` construction); on overrun, lock contention,
  missing/empty prompt, or no qualifying hit it emits nothing and exits 0
  (fail-closed). Claude-Code-only — Codex registers no UserPromptSubmit hook and
  restricted mode injects nothing. Four new tunables: `IRONMEM_PROMPT_HOOK_BUDGET_MS`,
  `IRONMEM_PROMPT_HOOK_MAX_HITS` (default 3, clamped 1–3),
  `IRONMEM_PROMPT_HOOK_MIN_SCORE` (default 0.0), and
  `IRONMEM_PROMPT_HOOK_SUMMARY_MAX_BYTES` (default 120). New public DB API
  `Database::open_with_busy_timeout` opens an existing DB with a caller-bounded
  busy timeout and no migration for this latency-critical path.
- **SessionStart `additionalContext` injection (Claude Code):** the `session-start`
  hook now emits a compact memory-status block via
  `HookResponse.hookSpecificOutput.additionalContext` — drawer/wing/room counts
  (top-N by count), the active collab session + phase (read from the DB), a
  last-diary pointer (date + short id only, never the body), and
  `MEMORY_PROTOCOL` — so the model starts each session aware of memory instead of
  relying on a `status` call. Claude-Code-only; under `--harness codex` the field
  is omitted (silent degrade). New public type `HookSpecificOutput` and a new
  optional `HookResponse.hook_specific_output` field (omitted from JSON when
  absent, so existing hook output is unchanged). The block is capped (~400 tokens)
  with `MEMORY_PROTOCOL` given a reserved budget so it is always present.
- `ironmem report [--task <tag>] [--since <date>] [--json]` — renders METRICS_SPEC §10 (tokens-to-done by task/phase, measured-vs-estimated split, iteration counts/outcome, merged-only headline) with §7-derived cost; baseline-recording gate for Phase 6 (§11.5). The `status` MCP tool gains a one-line metrics summary.
- `ironmem` now records a `token_usage` row per real LLM call at the
  preference-extraction (`source = "pref_extract"`) and LLM-rerank
  (`source = "llm_rerank"`) call sites, via
  `db::metrics::new_token_usage_from_llm`. Usage is preserved even when a
  rerank answer fails to parse. Inserts are non-fatal — a recording failure
  logs a warning and never breaks `add_drawer`/`search`. Context columns
  (session/collab/phase/task) are intentionally left `None` pending a later
  attribution pass.
- **Local symbol/import graph index for code-aware retrieval (issue #148, PR #168):**
  a SQLite-backed graph (migration 012: `code_index_files`, `code_symbols`,
  `code_imports`, `code_symbol_edges`) over a git worktree. A regex/heuristic v0
  parser extracts Rust (`use/mod/fn/struct/enum/trait/impl/const/static/type/macro_rules!`)
  and Python (`import`, `from..import`, `def`/`async def`, `class`) declarations,
  each carrying `language`, `kind`, span, and `confidence`. The indexer is
  incremental (content-hash: new/changed/`--force` → reparse; unchanged → skip;
  absent → purge), fully offline, and persists only bounded declaration metadata
  (signatures/imports truncated to `MAX_SNIPPET_LEN`, never full source). New CLI
  `ironmem symbols index|lookup|imports|neighbors` and four MCP tools
  (`symbol_graph_index` write-gated; `symbol_lookup`/`symbol_imports`/`symbol_neighbors`
  read-mode), with path-safety hardening (out-of-repo/traversal rejected,
  repo-relative forward-slash paths, generic client-facing FS/git errors).
- **Local read-only dashboard (issue #149):** `ironmem dashboard` serves a local
  web view of memory drawers, code maps, and context-savings — with embed-model
  warming status, per-row code-map freshness badges, and per-section CLI
  remediation hints.
- **One-command Claude and Codex launchers (issue #143):** `ironmem claude` /
  `ironmem codex` subcommands that canonicalize the repo, idempotently register
  the `ironmem` MCP server for the target harness, warm the embedder, and launch
  the CLI.
- **Launcher context pre-injection (issue #147):** launcher-managed sessions get a
  compact memory/code-map context pack injected up front, behind a disclaimer
  header, with untrusted pack text sanitized (control-char/newline stripping,
  code-fence neutralization) and a context-truncation warning.
- **Task context packs (issue #144):** `ironmem` assembles per-task context packs
  backed by memory drawers and lazy code maps.
- **Product-facing exploration-savings summary (issue #145):** `ironmem report`
  gains a product-facing exploration value summary, with honest one-sided-sample
  handling.
- **Documentation:** benchmark methodology + first context-savings baseline
  (issue #146); using ironmem with any MCP client — Cursor, Cline, Windsurf
  (issue #159).

### Performance

- **Shrinkage rerank compile-once + lowercase hoisting** (`search/rerank.rs`):
  each query token's word-boundary matcher is now compiled exactly once and
  reused for both the IDF df-count and per-candidate scoring (previously
  recompiled per token in `idf_filter` *and* rebuilt in `shrinkage_rerank`),
  and each candidate document is lowercased once up front instead of
  token×candidate times. Quoted-phrase lowercasing is hoisted out of the
  per-candidate loop. Measured on a synthetic 200-candidate set (500 iters):
  release median **1035µs → 582µs (−44%)**, debug median
  **14302µs → 8532µs (−40%)**. Behavior unchanged — all existing search
  tests stay green. Closes the in-code `TODO(perf)`. (#85)

## [0.3.4] - 2026-06-04

### Changed

- Codex `/collab` now runs `/pr-review-toolkit:review-pr` during the
  `CodeReviewFixGlobalPending` / `review_fix_global` turn as the final
  Codex review pass before Claude's `/ultrareview-local` audit. The
  installer now bundles `pr-review-toolkit` as a Codex-only skill
  dependency for fresh `/collab` installs.

### Fixed

- docs(collab): document `CodeImplementPending+codex` batch dispatch with
  `model_reasoning_effort=xhigh` and the working `codex exec -c` override
  form.
- Fixed invalid YAML in `.github/workflows/ci.yml` caused by partially
  quoted absolute-path cargo commands, which made CI runs fail before
  scheduling any jobs.

## [0.3.3] - 2026-05-28

### Added

- New `collab_set_implementer` MCP tool lets an existing collab session
  reassign the v3 batch implementer during planning or active
  `CodeImplementPending`. When reassigned during `CodeImplementPending`,
  the server also moves `current_owner` to the selected implementer so
  `/collab join --implementer=<claude|codex> <session_id>` can hand off
  an in-progress batch cleanly.

### Changed

- Claude and Codex `/collab join` prompts now accept
  `--implementer=claude|codex`, resume from the newest ironmem
  `collab-checkpoints` entry, inspect git/code state, and scan the plan
  before continuing already-started implementation work.
- `/collab review` shortcut prompts now recover branch context from
  ironmem checkpoints and referenced writing-plans docs when available,
  then scan the code/diff before Codex performs the global review.

## [0.3.2] - 2026-05-28

### Changed

- Collab v3 implementation prompts now require durable ironmem
  `collab-checkpoints` entries during `CodeImplementPending`, letting a
  fresh Claude or Codex session resume from the last task boundary after
  token exhaustion or another mid-batch stop.
- Claude and Codex `/collab` prompt surfaces now search the checkpoint
  room before implementation work and resume from `next_task_id`, a
  started task, or `batch_complete` instead of depending on transcript
  context.

## [0.3.1] - 2026-05-28

### Added

- `scripts/install-ironmem.sh` now installs the four `/collab` dependencies
  fresh users were previously missing: the `/collab` command file
  (`~/.claude/commands/collab.md`), the Codex `/collab` and
  `collab-batch-impl` prompts (`~/.codex/prompts/`), the `ironmem` MCP
  server registration in `~/.claude.json` (via `jq`) and
  `~/.codex/config.toml` (appended block), and a preflight warning when
  `~/.claude/commands/ultrareview-local.md` is absent (it is not bundled
  with ironmem but is invoked by `/collab`'s `CodeReviewLocalPending`
  phase).
- New install flags: `--skip-wiring` and `--force-wiring`. `--skip-skills`
  / `--force-skills` now also cover the new command and prompt installs.

### Notes

- Hooks remain unmanaged by `install-ironmem.sh`: the bundled
  `.claude-plugin/hooks/ironmem-hook.sh` requires a plugin-tree layout
  this script does not produce, and the binary-direct hook entries in
  `~/.claude/settings.json` (calling `ironmem hook …` directly) are the
  supported wiring. `/collab` itself does not depend on the diary /
  auto-mining hooks.

## [0.3.0] - 2026-05-27

### Changed

- **Collab v1 planning gates reworked (prompt-layer only; no Rust /
  state-machine change).** Blind `draft` now sends autonomously — Codex
  starts grinding immediately on the owner-flip instead of waiting on a
  user think-time gate. The first `canonical` synthesis at
  `PlanSynthesisPending` with `review_round == 0` is now the user gate
  (Plan Mode + approval), since it is the first artifact that combines
  both drafts. Revision-round canonicals (`review_round >= 1`,
  re-entered on `request_changes`) run autonomously; the user's next
  gate is `final`. The v1 `final` gate and the v3 `final_review` PR
  gate are unchanged. Gating is prompt-enforced via the existing
  `review_round` field on `CollabSession` — server semantics are
  untouched.

- **Adjacent doc cleanups folded into the same change:** moved the
  72-line timing-instrumentation block from the runtime command file
  to the spec (inverts cross-reference; trims invocation tokens);
  rewrote four sections in `docs/COLLAB.md` still describing the old
  two-terminal model (§ Autonomous Planning Loop, § Prompt Templates,
  § Worked Example, top-of-doc bullets + Runtime Model ASCII);
  demoted § "Codex handoff via MCP" into a Fallback subsection merged
  under § Background `codex exec` dispatch; fixed stale
  `mcp__codex__codex` primary-dispatch references; recorded the
  `/ultrareview-local` anti-removal decision as `kept` under the
  existing overlap-audit clause.

### Added

- **ProvBench Plan A.2 — Python post-commit classification pipeline
  (2026-05-19, SPEC §11 row 2026-05-19).** Full per-commit Python
  classification replaces the Plan A.1 short-circuit that routed every
  changed Python file directly to `NeedsRevalidation`. Key changes:

  - **`is_python_path` short-circuit removed.** The Plan A.1 guard that
    bypassed per-fact matching for all Python-path facts is gone. Python
    facts now flow through the full replay loop.

  - **`classify_python_against_commit` dispatch.** `replay::classify_commit`
    detects `PostAst::Python(_)` and routes to a dedicated Python
    classification function rather than the Rust path.

  - **`matching_post_fact_python` — 5-arm matcher:**
    - `FunctionSignature` — body-hash compare + cross-file move via
      `lookup_python` 4-variant fallback.
    - `Field` — exact `qualified_path` lookup; absent → `NeedsRevalidation`.
    - `PublicSymbol` — `CommitSymbolIndex::lookup_python` with
      single-underscore-leaf filter (`_foo` does NOT match a public `foo`).
    - `TestAssertion` — ordinal end-to-end via `push_test_assertion_facts`;
      same ordinal pairing semantics as the Rust path.
    - `DocClaim` — stub, returns `None` (Python DocClaim is v1.4+ work).

  - **`PublicSymbol` single-underscore-leaf filter.** Applied symmetrically
    in `CommitSymbolIndex` index building (Python entries) and in
    `matching_post_fact_python` matching.

  - **`lookup_python` 4-variant fallback routing.**
    `ExactAtOriginalPath` → `UniqueFallbackAtPath` → `AmbiguousFallback`
    → `Absent`. Unique-but-no-body-hash cases route to `NeedsRevalidation`
    (not `Stale_Symbol_Renamed`).

  - **`RenameCandidate::new_python`** uses `.` splitting for
    qualified-name decomposition (not Rust's `::` splitter).

  - **Flask H3 (R3 dominance) closeable at the labeler layer.** The v1.2b
    held-out findings' H3 finding is now closeable; held-out re-run to
    confirm is the follow-up PR.

## [0.2.1] - 2026-05-17

### Changed

- **Collab v3 phase reorder — Codex global review precedes Claude local
  audit (2026-05-17, PR #56).** Forward-only protocol change.
  New phase sequence: `CodeImplementPending` →
  `CodeReviewFixGlobalPending` (Codex; reviews the raw post-implementation
  diff with no Claude pre-clean) → `CodeReviewLocalPending` (Claude;
  audits Codex's commits via `/ultrareview-local`, fixes
  CRITICAL/HIGH/MEDIUM inline) → `CodeReviewFinalPending` (Claude; PR
  creation) → `CodingComplete`. Wire-observable through
  `collab_status.phase` transitions.
  (A) State-machine match arms rewired in
  `crates/ironmem/src/collab/state_machine/mod.rs`; topic-to-event
  names unchanged. Owners per phase unchanged; positions in sequence
  change.
  (B) Pre-send harness reset rules scoped by harness owner: Claude
  resets to `last_head_sha` before `review_local` (Codex's only push
  in v3); skips reset before `task_list`, `implementation_done`, and
  `final_review`. Codex keeps its receive-side reset before
  `review_fix_global` (syncs to whatever Claude pushed at
  `implementation_done`).
  (C) `/ultrareview-local` role shifts to audit-of-Codex; anti-removal
  guardrail updated. Severity threshold for inline fixes extended to
  CRITICAL/HIGH/MEDIUM (was CRITICAL/HIGH).
  (D) Codex prompt framing updated: Codex sees the raw post-implementation
  diff, no Claude pre-clean. The next-receiving-side gate after Codex's
  `review_fix_global` is `CodeReviewLocalPending`, not
  `CodeReviewFinalPending`.
  (E) Shortcut ancestry validation extended in
  `crates/ironmem/src/mcp/tools/collab_session.rs` to fire at both
  `(CodeReviewFixGlobalPending, CodeReviewFixGlobal)` AND
  `(CodeReviewLocalPending, ReviewLocal)` when `task_list.is_none()`.
  New test `test_shortcut_review_local_ancestry_enforced` enforces
  branch-drift rejection.
  (F) **Deployment requirement** — operationally explicit: pause / avoid
  starting new coding-phase collab sessions while existing
  coding-active sessions are drained or aborted before rollout. No
  protocol-version migration; sessions surviving deploy follow new
  semantics from their stored phase forward.

- **Collab protocol — docs/prompts alignment with server enforcement
  (2026-05-16, PR #55).** Three doc/prompt-only changes; no Rust source touched
  (server behavior unchanged):
  (A) `docs/COLLAB.md`, `.claude-plugin/commands/collab.md`, and
  `.codex-plugin/prompts/collab.md` now name the
  `MAX_REVIEW_ROUNDS = 2` cap explicitly and cite
  `crates/ironmem/src/collab/state_machine/mod.rs:28` — Codex gets at
  most two v1 plan-review rounds, then the server force-finalizes to
  `PlanClaudeFinalizePending` regardless of verdict.
  (B) Timing-event names are now stable base identifiers with phase +
  round detail in structured `phase=<phase> round=<N>` key=value
  fields. `t4_phase_advanced_to_<phase>` is renamed to
  `t4_phase_advanced` (phase moves into `phase=`). Old suffix-shaped
  names (`<event>_round<N>`, `<event>_to_<phase>`) are documented as
  legacy artifacts and must not be emitted by current dispatchers;
  historical logs are not rewritten. Consumers of
  `/tmp/collab-eval-${session_id}.log` parsing on event-name suffix
  must switch to parsing the structured key=value fields.
  (C) Claude's dispatcher polling loop documents a bounded backoff
  curve for Codex-owned background phases — 10s default → 20s after
  60s of no progress → 30s after 300s (cap), reset on phase advance /
  new stdout / bg process exit / bg process error or signal. 600s
  hang detection unchanged. Scope: Codex bg phases only; does NOT
  affect Plan Mode idle gaps.
  Also documents two anti-removal guardrails:
  `/ultrareview-local`'s code-quality lens requires a written overlap
  audit before removal; SDD reviewer model-pinning recommendations
  belong in the SDD skill itself once pinning support exists, not in
  the collab protocol spec.

### Added

- **ProvBench §9.4 held-out evaluation — Round 1 (serde-rs/serde @
  T₀ `65e1a507`, v1.0.130).** First held-out evaluation of phase1
  v1.1 against a repo the rules were never tuned on (SPEC §13.2
  pre-registered, leakage-clean). Result: **FAIL §8 #3** — valid
  retention WLB 0.9062 < 0.95 required; pilot was 0.9716 (−6.5pp
  drop). §8 #4 latency p50 = 14 ms (PASS) and §8 #5 stale recall
  WLB = 0.9391 (PASS) generalize cleanly. Per-rule confusion
  attributes the §8 #3 miss to R4 (`span_hash_changed` line-presence
  probe): held-out false-Stale on GT=Valid is 162 vs pilot 17 (10×
  pilot rate). Per SPEC §10 no in-round retuning; SPEC §11 row
  records the FAIL. A future v1.2 with retuned R4 would re-run the
  leakage clock against pallets/flask (Round 2; pre-registered).
  Findings:
  `benchmarks/provbench/results/serde-heldout-2026-05-15-findings.md`.
- **ProvBench v1.2a — R4 Field-kind guard relaxation (ripgrep pilot,
  2026-05-15).** Phase1 `rule_set_version v1.2` (phase1 git SHA
  `97cef97`): R4 `span_hash_changed` `MIN_PROBE_NONWS_LEN` length
  floor dropped for `kind = "Field"` (single match arm relaxed in
  `phase1/src/rules/r4_span_hash_changed.rs`; all other kinds
  unchanged). Re-run on the ripgrep Phase 0c canary subset clears
  v1.1's pilot SPEC §8 thresholds verbatim (WLB valid 0.9729, p50
  2 ms, WLB stale 0.9537) with three v1.2a acceptance gates also
  cleared: §8 verbatim, no regression vs v1.1, Field false-Valid
  count `0` ≤ v1.1 + 20 slack. §10 admission consumed on ripgrep;
  no held-out evidence produced this round. Findings:
  `benchmarks/provbench/results/ripgrep-pilot-2026-05-15-v1.2a-findings.md`.
- **ProvBench v1.2b — Python labeler bring-up (Plan A, PR #50;
  merged at `c623298`).** Workspace-excluded `provbench-labeler`
  extended to label Python repos via `tree-sitter-python 0.25`
  (SPEC §13.1 pin). Pure-Rust extension; `tree-sitter` scope walker
  + lexical import graph (`resolve::python::PythonResolver`) — no
  Python runtime in ironmem. Same fact schema as Rust path
  (FunctionSignature, Field, PublicSymbol, TestAssertion;
  `DocClaim` for Python is a documented stub). Rust ripgrep canary
  byte-identical pre/post (SHA `d8de2d2a…` stripped of
  `labeler_git_sha`). Determinism enforced by new
  `tests/determinism_python.rs` (fixture, default-run) and
  `tests/determinism_flask.rs` (`#[ignore]`, full pallets/flask at
  T₀). Spot-check material (n=200, seed `0xC0DEBABEDEADBEEF`) at
  `benchmarks/provbench/results/python-labeler-2026-05-15-spotcheck.csv`.
- **ProvBench v1.2b — Python replay short-circuit fix (Plan A.1,
  PR #52; merged at `800d108`).** Plan A's Task 12 routed Python
  `Fact::FunctionSignature` through `push_observed_facts`
  (defaulting `function_signature_disambiguator: None`), then
  `replay/mod.rs` built `RustAst` for every fact-source path
  including `.py` files — `tree-sitter-rust` silently produced a
  garbled tree on Python, and `match_post.rs:60`'s `expect()` on
  the Rust-only disambiguator panicked during replay. Plan A's
  `#[ignore]` flask determinism test missed it (T₀-only, empty
  replay). Fix filters `post_asts` by `Language::for_path` (Rust
  paths only get `RustAst::parse`) and short-circuits non-byte-
  identical Python paths to `Label::NeedsRevalidation`. New
  `tests/python_replay_changed_file.rs` enforces the contract.
  Rust ripgrep canary remains byte-identical
  (`d8de2d2a…`).
- **ProvBench §9.4 held-out evaluation — Round 2 (pallets/flask @
  T₀ `2f0c62f5`, 2.0.0).** First Python held-out round. Labeler
  `800d108…` (Plan A.1) + phase1 `97cef97` (v1.2) on flask T₀
  with replay HEAD `9fcd34c9…` (T₀+401 first-parent commits).
  Stratified subset n=4,000 (vs serde's 12,820 — flask is fact-
  poor). Verdict **PASS-PASS-FAIL**: §8 #3 valid retention WLB
  `0.9981` (PASS; v1.2 R4 relaxation generalizes from pilot to
  Python — a stronger result than serde Round 1's FAIL); §8 #4
  latency p50 `0` ms (PASS vacuously — `wall_ms` not populated in
  predictions for this round; hygiene flag for v1.2c); §8 #5
  stale recall WLB `0.0` (FAIL — **structural**: Plan A.1 labeler
  emits 2,000 Valid + 2,000 NeedsRevalidation + 0 Stale_* ground
  truth on Python, so `stale_detection` recall collapses to 0/0
  by Wilson convention). The §8 #5 FAIL is uninformative about
  phase1's actual stale-detection ability on Python; v1.2c needs
  either labeler refinement (Stale_* emission on changed Python)
  or a corpus with pre-built Stale_* ground truth, or extending
  phase1 to emit a NeedsRevalidation decision. §10 attestation
  cleared 8/8. SPEC §11 row at SPEC.md:185. Findings:
  `benchmarks/provbench/results/flask-heldout-2026-05-15-findings.md`.
- **ProvBench v1.2b — SPEC §9.1 Python labeler spot-check PASS
  (PR #54; 2026-05-16).** First Python labeler-quality gate cleared
  at WLB 98.12% (200/200 agreements, same threshold as the Rust
  labeler's 2026-05-13 gate). New
  `benchmarks/provbench/autofilter_python.py` provides reusable
  labeler-independent triage for future Python rounds.

## [0.2.0] - 2026-05-14

### Added

- **provbench-phase1** (Phase 1): new workspace-excluded crate
  implementing the rules-based structural invalidator
  (`rule_set_version v1.0` → `v1.1`, frozen at phase1 git SHA
  `ccfc901be171`). 7-rule chain (`source_file_missing`,
  `blob_identical`, `symbol_missing`, `span_hash_changed`,
  `whitespace_or_comment_only`, `doc_claim`, `rename_candidate`),
  deterministic single-repo HEAD-only replay, per-rule confusion +
  audit trail in `rule_traces.jsonl`. Pilot v1.1 clears SPEC §8 #3 /
  #4 / #5 on the ripgrep Phase 0c canary (n=4,387; WLB valid 0.9716,
  p50 2 ms, WLB stale 0.9537).
- **provbench-scoring**: shared SPEC §7 math crate (Wilson intervals,
  three-way confusion, F1, Cohen κ bootstrap) split out of baseline
  so phase1 and baseline both consume the same scorer. `compare`
  subcommand produces side-by-side `metrics.json` with deltas.
- **provbench-baseline** (Phase 0c): new workspace-excluded crate
  implementing the LLM-as-invalidator baseline against
  `claude-sonnet-4-6` snapshot 2026-05-09 per SPEC §6.1. Three
  subcommands (`sample`, `run`, `score`). Operational $25 budget cap
  (preflight + live abort) under the spec's immutable $250 ceiling.
  Stratified sampler, atomic checkpointing, `--resume`, schema-derived
  preflight estimator, prompt caching at the static prefix,
  parse-error addendum retry, §7.1 three-way metrics + §9.2
  LLM-validator agreement with Wilson intervals + Cohen κ bootstrap.
- **provbench-labeler**: two new subcommands `emit-facts` and
  `emit-diffs` to produce the JSON artifacts consumed by the baseline
  runner.
- **provbench-labeler — `spotcheck --seed <u64>` (2026-05-12).** The
  stratified sampler now accepts an optional seed (decimal or `0x`-
  prefixed hex) so post-merge / anti-tuning validation runs can draw a
  fresh sample against a regenerated corpus. Omitting `--seed` uses
  the new `DEFAULT_SEED` public constant
  (`0xC0DEBABEDEADBEEF`, the historical value), preserving
  byte-identical replay for resuming an in-progress reviewer CSV. The
  CLI echoes the resolved seed and writes a `<out>.meta.json` sidecar
  recording `{corpus, seed, n, labeler_git_sha}` so the on-disk
  spot-check artifact is self-describing. The SPEC §9.1 acceptance
  gate must continue to use `DEFAULT_SEED`.
- `ironmem --version` and MCP `serverInfo.version` carry the
  git-describe suffix for in-development builds and drop it cleanly
  on a tagged commit.

### Changed

- **Breaking (wire):** MCP tool ids dropped the `ironmem_` prefix
  now that the server id itself is `ironmem`. For example,
  `ironmem_search` → `search`, `ironmem_collab_start` →
  `collab_start`. Clients invoking tools as `mcp__ironmem__ironmem_*`
  must update to `mcp__ironmem__*`.
- Renamed workspace crate `ironrace-memory` → `ironmem` and MCP
  server id → `ironmem`. The on-disk data directory
  `~/.ironrace-memory/` is preserved for user-data backcompat.
- `AgreementReport`'s `per_class` + `per_stale_subtype` moved
  `HashMap` → `BTreeMap` so byte-stable JSON serialization is
  structural, not platform-luck.
- `Database::migrate()` wraps the version-gated section in
  `BEGIN IMMEDIATE` so concurrent openers can't race
  `ALTER TABLE ADD COLUMN`.
- CI: macOS rustup-init shim workaround pinned via absolute-path
  cargo invocation; deterministic across all runner images.
- **ProvBench labeler — Phase 0b hardening pass 3 (2026-05-12).**
  Four labeling-correctness clusters fixed; SPEC v1 is unchanged:
  (A) visibility narrowing (`pub(crate)` / `pub(super)` / `pub(in path)` /
  private) is now classified as `StaleSourceChanged` per SPEC §5 rule
  ordering rather than `NeedsRevalidation`;
  (B) replay symbol resolution is commit-tree-local — `CommitSymbolIndex`
  built via tree-sitter per commit, eliminating the runtime RA dependency
  (RA tooling pin and `tests/replay_ra.rs` retained for future cross-crate
  / macro-expanded work);
  (C) rename detection requires a typed `RenameCandidate` with matching
  `kind` + `container` and a T₀-presence check to prevent false positives
  from pre-existing same-named symbols;
  (D) doc-claim matching is relocation-tolerant — post-state lookup uses
  `qualified_name` rather than byte-offset hash so claims that move lines
  are still matched correctly.
- **ProvBench labeler — Phase 0b hardening pass 2 (2026-05-09).**
  Deterministic `fact_id`s via pure-string path normalization (no
  `pwd`-sensitive canonicalization), fail-closed behavior on
  rust-analyzer indexing timeout, explicit invalid-UTF-8 error in the
  doc-claim extractor (no more silent zero-fact corpus on a corrupted
  README), structured CSV via the `csv` crate for the spot-check sample,
  and pinned `linux-x86_64` tooling hashes for the `ubuntu-latest` GitHub
  runner so CI matches the canonical `aarch64-darwin` freeze
  environment.

### Fixed

- **ProvBench labeler — Phase 0b hardening pass 5 (2026-05-13).**
  Three structural fixes addressing the post-pass-4 spot-check
  findings (`benchmarks/provbench/spotcheck/2026-05-13-post-pass4-findings.md`):
  (1) `FunctionSignature` post-commit pairing now uses a private
  replay-time disambiguator keyed on `(qualified_name,
  cfg_attribute_set, impl_receiver_type)` with a zero-based ordinal
  tiebreaker, mirroring pass-4's `TestAssertion` ordinal fix. When a
  T₀ fact's specific cfg/impl variant is deleted at a later commit
  while same-qualified-name survivors exist in other variants, the
  row routes to `NeedsRevalidation` (gray area for LLM follow-up)
  instead of mis-pairing against a survivor's span/hash and emitting
  `StaleSourceChanged`. ~9 sample rows fix.
  (2) `PublicSymbol` bare `pub use` re-exports (including
  `pub use … Original as Alias`) now preserve public-surface
  continuity → `Valid`, even when the post declaration span hashes
  differently from a T₀ definition span. Restricted-visibility uses
  (`pub(crate) use`, `pub(super) use`, `pub(in …) use`) remain
  narrowed → `StaleSourceChanged` via the pass-3 visibility-narrowing
  path. Glob re-exports (`pub use path::*;`) remain out of scope.
  ~2 sample rows fix.
  (3) `Fact::Field` post-commit matching now consults a private
  file-local `same_file_leaf_elsewhere` helper. When the T₀ field's
  exact `qualified_path` no longer resolves but the same leaf name
  appears in another struct or enum-variant in the same file, the
  row routes to `NeedsRevalidation` (file-local restructure gray
  area). Cross-file field-leaf tracking is intentionally not
  extended into `CommitSymbolIndex`. ~3 sample rows fix.
  The `Fact` enum, JSONL schema, and `fact_id` format are all
  byte-stable across this pass. `sample-eaf82d2.csv` remains the
  diagnostic ground-truth for the pass-4 gate FAIL; SPEC §9.1
  acceptance requires a freshly regenerated corpus + new-seed sample
  post-merge.

- **ProvBench labeler — Phase 0b hardening pass 4 (2026-05-13).**
  Two structural fixes addressing the post-pass-3 spot-check findings
  (`benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`):
  (1) `TestAssertion` post-commit pairing was matching by `test_fn`
  alone via `find_map`, returning the first assertion in the
  post-commit test fn for every T₀ fact in that fn. Non-first
  assertions in a multi-assertion `#[test]` body silently routed to
  `StaleSourceChanged` even in byte-identical files. Pairing now uses
  `(test_fn, zero-based ordinal)` via a private replay-time
  disambiguator on `ObservedFact`; the `Fact` enum, JSONL schema, and
  `fact_id` format are byte-stable. Blast radius across the ripgrep
  pilot corpus before this fix: 80.7% of `TestAssertion` fact_ids
  (667/827) were subject to misclassification.
  (2) Added a SPEC §5 byte-identical-file structural guardrail in
  `Replay::run_inner` step 3: when a fact's source path is
  byte-identical between T₀ and `commit_sha`, every fact at that path
  classifies `Valid` without invoking per-fact matching, symbol
  resolution, rename detection, or whitespace/comment diffing.
  Defense-in-depth: catches per-fact-matcher ambiguity for all five
  fact kinds (including `DocClaim` on byte-identical markdown), and
  structurally covers the lone `FunctionSignature::is_hidden` outlier
  from the pre-merge sample without chasing its per-fact root cause.
  `sample-e96c9fe.csv` was drawn against the buggy corpus and is
  diagnostic-only; the SPEC §9.1 acceptance gate must be re-run on a
  freshly regenerated corpus drawn with a NEW seed.

## [0.1.0] - 2026-04-15

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
- Non-blocking startup: DB opens in Phase 1 (<50 ms); ONNX model loads in a background thread with `warming_up` status flag
- Embedder hot-swap on first tool call after background init completes
- `IRONMEM_EMBED_MODE=noop` for smoke tests and CI without the ONNX model
- `IRONMEM_DISABLE_MIGRATION=1` to skip first-run mempalace migration
- Stale `bootstrap.lock` auto-recovery on startup
- MCP smoke test script (`scripts/mcp_smoke_test.py`)
- Tag-triggered release workflow with macOS and Linux binary archives
- Integration tests: MCP protocol contract, plugin metadata validation, mining end-to-end, bootstrap races, migration corruption/idempotency

### Changed

- Search overfetch increased from 3x to 5x (minimum 30 candidates)
- Mining skips hidden files and directories by default; set `IRONMEM_MINE_HIDDEN=1` to index dot-paths
- Bootstrap no longer infers workspace from `cwd`; explicit roots required for auto-mining
- `serve` fails closed on bootstrap errors instead of starting with partial initialization
- Re-mining replaces a file's drawers transactionally after embeddings are computed
- Migration from ChromaDB imports drawers and knowledge-graph data transactionally
- Hook session summaries land in the same diary stream as normal diary writes

### Fixed

- Sanitized `cwd` and `transcript_path` values before hook diary persistence
- Rejected system directory prefixes for mining and migration inputs
- Removed `.env` from the mining allowlist
- Added bounded SQLite busy retries during startup schema work
- Serialized env-var-mutating bootstrap tests to prevent race conditions

### Removed

- `properties` field from the `entities` table and `Entity` struct
