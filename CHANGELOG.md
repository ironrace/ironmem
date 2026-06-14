# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`ironrace-rerank` 0.3.4 → 0.4.0 (breaking, workspace-internal):**
  `LlmClient::call` now returns `Result<LlmResponse>` instead of
  `Result<String>`. `LlmResponse` carries the assistant `text` plus token
  `usage`, `cost_usd`, `model`, an `estimated` flag, and `prompt_chars`. New
  public types: `Usage`, `LlmResponse`, `RerankScoreResult`, `RerankScoreError`;
  `RerankerScorer::score_pairs` now returns `RerankScoreResult`. Both backends
  (`ClaudeCliClient`, `AnthropicApiClient`) parse real token counts (the CLI
  falls back to a chars/4 estimate, flagged `estimated`).

### Added

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
